//! Exact window-stamped targeted pointer sequences for the v2 driver.

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use cua_driver_core::api::{
    contracts::{Modifier, MouseButton, Point, VerificationLevel},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{InteractionScope, NativeEvidence, NativeSideEffectBoundary, TargetCursorHandle},
    observation::{ResolvedPoint, ResolvedWindowStamp, SurfaceOwner},
    platform::{NativeDispatch, PointerActionProvider, ResolvedAction},
    settlement::SettlementSignal,
    Route,
};
use serde_json::Value;

use crate::{
    driver::{
        settlement::MacSignalJournal,
        target::MacTargetState,
        windows::{MacRelatedWindowFacts, MacWindowFacts, MacWindowRegistry},
    },
    focus_steal,
    input::synthesized_event::{self, MouseEventKind as NativeMouseEventKind, MouseEventSpec},
};

const CLICK_UP_DELAY_MS: u64 = 100;
const POINT_EPSILON: f64 = 0.001;

#[derive(Clone)]
pub struct MacPointerActions {
    windows: MacWindowRegistry,
    sink: Arc<dyn TargetedPointerSink>,
}

impl MacPointerActions {
    pub fn new(windows: MacWindowRegistry) -> Self {
        Self {
            windows,
            sink: Arc::new(SystemTargetedPointerSink),
        }
    }

    async fn prepare_point(
        &self,
        target_stamp: ResolvedWindowStamp,
        scope_owner: ResolvedWindowStamp,
        point: &ResolvedPoint,
    ) -> Result<NativePoint, NativeError> {
        ensure_point_matches_target(&target_stamp, &scope_owner, point)?;
        let (pid, cg_window_id, owner_bounds) = match &point.surface_owner {
            SurfaceOwner::Target(owner) => {
                if owner != &target_stamp {
                    return Err(stale_surface(
                        point,
                        "captured target surface owner no longer matches the target controller",
                    ));
                }
                let facts = self.windows.facts_for_stamp(owner).await?;
                target_point_facts(point, facts)?
            }
            SurfaceOwner::RelatedTransient { owner, parent } => {
                if parent != &target_stamp {
                    return Err(stale_surface(
                        point,
                        "captured transient parent no longer matches the target controller",
                    ));
                }
                let facts = self.windows.facts_for_related_stamp(owner, parent).await?;
                related_point_facts(point, facts)?
            }
        };
        let window_local = Point {
            x: point.screen_point.x - owner_bounds.x,
            y: point.screen_point.y - owner_bounds.y,
        };
        if window_local.x < 0.0
            || window_local.y < 0.0
            || window_local.x >= owner_bounds.width
            || window_local.y >= owner_bounds.height
        {
            return Err(stale_surface(
                point,
                "captured point lies outside its live native surface owner",
            ));
        }
        let derived_screen = Point {
            x: owner_bounds.x + window_local.x,
            y: owner_bounds.y + window_local.y,
        };
        if !same_point(derived_screen, point.screen_point) {
            return Err(stale_surface(
                point,
                "captured point transform no longer reproduces its live screen point",
            ));
        }
        let observation_epoch = point.observation_epoch.ok_or_else(|| {
            stale_surface(
                point,
                "captured point has no exact native observation journal epoch",
            )
        })?;
        Ok(NativePoint {
            screen: point.screen_point,
            window_local,
            logical: point.window_point,
            pid,
            cg_window_id,
            owner_bounds,
            owner: surface_owner_stamp(&point.surface_owner).clone(),
            surface_id: point.surface_id.to_string(),
            capture_revision: point.capture_revision.to_string(),
            geometry_revision: point.geometry_revision.to_string(),
            observation_epoch: observation_epoch.0,
        })
    }

    async fn prepare_action(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<MacPreparedPointerAction, NativeError> {
        if target.invalidated() || scope.route != Route::TargetedPointer {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "targeted pointer prepare requires a live targeted-pointer scope",
            ));
        }
        let (sequence, final_cursor, route_name) = match action {
            ResolvedAction::PointClick { point, spec } => {
                if spec.button != MouseButton::Left
                    || spec.click_count != 1
                    || !spec.modifiers.is_empty()
                {
                    return Err(NativeError::unsupported(
                        "recipe_unproven: the canonical macOS pointer route currently proves only an unmodified single left click",
                    )
                    .with_detail("recipe_status", "recipe_unproven"));
                }
                let point = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), point)
                    .await?;
                let sequence = build_click(point.clone());
                (sequence, point.logical, "macos_targeted_pointer_click")
            }
            ResolvedAction::Drag(_) | ResolvedAction::DeltaScroll(_) => {
                return Err(NativeError::unsupported(
                    "recipe_unproven: drag and scroll are not published on the canonical macOS pointer route",
                )
                .with_detail("recipe_status", "recipe_unproven"));
            }
            _ => {
                return Err(NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Preflight,
                    false,
                    "non-pointer action reached macOS pointer prepare",
                ))
            }
        };
        ensure_sequence_fits_deadline(&sequence, scope.deadline.work)?;
        Ok(MacPreparedPointerAction {
            sequence,
            final_cursor,
            route_name,
        })
    }

    async fn dispatch_action(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: MacPreparedPointerAction,
    ) -> Result<NativeDispatch, NativeError> {
        if target.invalidated() || scope.owner != target.window {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "targeted pointer dispatch target changed after prepare",
            ));
        }
        self.revalidate_sequence_target(
            target.window.clone(),
            scope.owner.clone(),
            scope.window.stamp(),
            &action.sequence,
        )
        .await?;
        if target.invalidated()
            || scope.owner != target.window
            || target.signals.epoch() != action.sequence.target.observation_epoch
        {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "pointer target or observation epoch changed during final live-facts validation",
            ));
        }
        let epoch_witness = PointerEpochWitness {
            journal: target.signals.clone(),
            expected: action.sequence.target.observation_epoch,
        };
        let mut evidence = PointerDispatchEvidence::new(&action);
        let result = dispatch_sequence(
            self.sink.as_ref(),
            &action.sequence,
            scope.deadline.work,
            &scope.logical_cursor,
            &mut evidence,
            boundary,
            &epoch_witness,
        );
        evidence.merge_into(&mut scope.native_evidence);
        if let Err(mut error) = result {
            if evidence.cleanup_attempted && !evidence.cleanup_succeeded {
                target.invalidate();
                scope.invalidate_target();
            }
            error.details.insert(
                "pointer_dispatch".to_owned(),
                serde_json::to_value(&scope.native_evidence)
                    .expect("typed pointer dispatch evidence must serialize"),
            );
            return Err(error);
        }
        focus_steal::record_synthesized_action(action.sequence.target.pid);
        scope.logical_cursor.update(action.final_cursor);
        target
            .signals
            .record(SettlementSignal::PointerSequenceComplete);

        // The helper's CGEventPostToPid transport is a void API. A completed
        // call proves only that the complete targeted sequence was attempted;
        // it is not a native delivery acknowledgement.
        let verification = VerificationLevel::DispatchUnverified;
        let mut native = NativeEvidence::default();
        native
            .fields
            .insert("pointer_route".to_owned(), action.route_name.into());
        native.fields.insert(
            "pointer_event_count".to_owned(),
            Value::from(action.sequence.events.len()),
        );
        native.fields.insert(
            "pointer_transport".to_owned(),
            "appkit_event_core_graphics_post_to_pid_once".into(),
        );
        native.fields.insert(
            "surface_id".to_owned(),
            action.sequence.target.surface_id.clone().into(),
        );
        native.fields.insert(
            "capture_revision".to_owned(),
            action.sequence.target.capture_revision.clone().into(),
        );
        native.fields.insert(
            "geometry_revision".to_owned(),
            action.sequence.target.geometry_revision.clone().into(),
        );
        native.fields.insert(
            "observation_epoch".to_owned(),
            action.sequence.target.observation_epoch.into(),
        );
        Ok(NativeDispatch {
            verification,
            evidence: native,
            warnings: Vec::new(),
            menu: None,
        })
    }

    async fn revalidate_sequence_target(
        &self,
        target_window: ResolvedWindowStamp,
        scope_owner: ResolvedWindowStamp,
        scope_window: ResolvedWindowStamp,
        sequence: &PreparedSequence,
    ) -> Result<(), NativeError> {
        if scope_owner != target_window
            || scope_window != target_window
            || sequence.target.geometry_revision != target_window.geometry_revision.as_str()
        {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "pointer target/window geometry changed after native action preparation",
            ));
        }
        let (pid, cg_window_id, bounds, stamp) = if sequence.target.owner == target_window {
            let facts = self.windows.facts_for_stamp(&target_window).await?;
            (facts.pid, facts.cg_window_id, facts.bounds, facts.stamp)
        } else {
            let facts = self
                .windows
                .facts_for_related_stamp(&sequence.target.owner, &target_window)
                .await?;
            (facts.pid, facts.cg_window_id, facts.bounds, facts.stamp)
        };
        if pid != sequence.target.pid
            || cg_window_id != sequence.target.cg_window_id
            || bounds != sequence.target.owner_bounds
            || stamp != sequence.target.owner
        {
            return Err(NativeError::stale(
                ErrorCode::SurfaceStale,
                "pointer surface owner facts or geometry changed before the first native post",
            )
            .with_detail("surface_id", sequence.target.surface_id.clone())
            .with_detail("capture_revision", sequence.target.capture_revision.clone())
            .with_detail(
                "geometry_revision",
                sequence.target.geometry_revision.clone(),
            ));
        }
        for event in &sequence.events {
            let expected_screen = Point {
                x: bounds.x + event.point.window_local.x,
                y: bounds.y + event.point.window_local.y,
            };
            if event.point.pid != pid
                || event.point.cg_window_id != cg_window_id
                || event.point.owner != stamp
                || event.point.owner_bounds != bounds
                || !same_point(expected_screen, event.point.screen)
            {
                return Err(NativeError::stale(
                    ErrorCode::SurfaceStale,
                    "prepared pointer event no longer matches final live surface geometry",
                ));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl PointerActionProvider<MacTargetState> for MacPointerActions {
    type PreparedAction = MacPreparedPointerAction;

    async fn prepare(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<Self::PreparedAction, NativeError> {
        self.prepare_action(target, scope, action).await
    }

    async fn dispatch(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: Self::PreparedAction,
    ) -> Result<NativeDispatch, NativeError> {
        // The complete sequence is intentionally synchronous. Core cannot
        // cancel between a button-down and its paired cleanup button-up.
        self.dispatch_action(target, scope, boundary, action).await
    }
}

pub struct MacPreparedPointerAction {
    sequence: PreparedSequence,
    final_cursor: Point,
    route_name: &'static str,
}

#[derive(Debug, Clone)]
struct PreparedSequence {
    target: NativePoint,
    events: Vec<PreparedPointerEvent>,
}

#[derive(Debug, Clone)]
struct NativePoint {
    screen: Point,
    window_local: Point,
    logical: Point,
    pid: i32,
    cg_window_id: u32,
    owner_bounds: cua_driver_core::api::contracts::Rect,
    owner: ResolvedWindowStamp,
    surface_id: String,
    capture_revision: String,
    geometry_revision: String,
    observation_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerEventKind {
    Down,
    Up,
}

#[derive(Debug, Clone)]
struct PreparedPointerEvent {
    kind: PointerEventKind,
    point: NativePoint,
    button: MouseButton,
    click_state: u8,
    modifiers: Vec<Modifier>,
    scheduled_after: Duration,
}

fn build_click(point: NativePoint) -> PreparedSequence {
    let events = vec![
        pointer_event(
            PointerEventKind::Down,
            point.clone(),
            MouseButton::Left,
            1,
            &[],
            0,
        ),
        pointer_event(
            PointerEventKind::Up,
            point.clone(),
            MouseButton::Left,
            1,
            &[],
            CLICK_UP_DELAY_MS,
        ),
    ];
    PreparedSequence {
        target: point,
        events,
    }
}

fn pointer_event(
    kind: PointerEventKind,
    point: NativePoint,
    button: MouseButton,
    click_state: u8,
    modifiers: &[Modifier],
    scheduled_after_ms: u64,
) -> PreparedPointerEvent {
    PreparedPointerEvent {
        kind,
        point,
        button,
        click_state,
        modifiers: modifiers.to_vec(),
        scheduled_after: Duration::from_millis(scheduled_after_ms),
    }
}

fn ensure_sequence_fits_deadline(
    sequence: &PreparedSequence,
    deadline: Instant,
) -> Result<(), NativeError> {
    let duration = sequence
        .events
        .last()
        .map_or(Duration::ZERO, |event| event.scheduled_after);
    let completes_at = Instant::now().checked_add(duration).ok_or_else(|| {
        NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Preflight,
            false,
            "pointer sequence duration exceeds the platform clock range",
        )
    })?;
    if completes_at > deadline {
        return Err(NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Preflight,
            true,
            "pointer sequence cannot complete inside the controller-owned work deadline",
        )
        .with_detail("sequence_duration_ms", duration.as_millis() as u64));
    }
    Ok(())
}

fn ensure_point_matches_target(
    target: &ResolvedWindowStamp,
    scope_owner: &ResolvedWindowStamp,
    point: &ResolvedPoint,
) -> Result<(), NativeError> {
    if &point.window.stamp() != target
        || scope_owner != target
        || point.geometry_revision != target.geometry_revision
    {
        return Err(stale_surface(
            point,
            "point/window/geometry revision no longer matches the target controller",
        ));
    }
    let expected_screen = Point {
        x: point.window.geometry.bounds.x + point.window_point.x,
        y: point.window.geometry.bounds.y + point.window_point.y,
    };
    if !same_point(expected_screen, point.screen_point) {
        return Err(stale_surface(
            point,
            "captured point no longer matches its observation-owned window transform",
        ));
    }
    Ok(())
}

fn target_point_facts(
    point: &ResolvedPoint,
    facts: MacWindowFacts,
) -> Result<(i32, u32, cua_driver_core::api::contracts::Rect), NativeError> {
    if facts.stamp != point.window.stamp() {
        return Err(stale_surface(
            point,
            "live target window facts changed after point observation",
        ));
    }
    Ok((facts.pid, facts.cg_window_id, facts.bounds))
}

fn related_point_facts(
    point: &ResolvedPoint,
    facts: MacRelatedWindowFacts,
) -> Result<(i32, u32, cua_driver_core::api::contracts::Rect), NativeError> {
    if &facts.stamp != surface_owner_stamp(&point.surface_owner) {
        return Err(stale_surface(
            point,
            "live transient window facts changed after point observation",
        ));
    }
    Ok((facts.pid, facts.cg_window_id, facts.bounds))
}

fn surface_owner_stamp(owner: &SurfaceOwner) -> &ResolvedWindowStamp {
    match owner {
        SurfaceOwner::Target(owner) | SurfaceOwner::RelatedTransient { owner, .. } => owner,
    }
}

fn stale_surface(point: &ResolvedPoint, message: &'static str) -> NativeError {
    NativeError::stale(ErrorCode::SurfaceStale, message)
        .with_detail("surface_id", point.surface_id.to_string())
        .with_detail("capture_revision", point.capture_revision.to_string())
        .with_detail("geometry_revision", point.geometry_revision.to_string())
}

fn same_point(left: Point, right: Point) -> bool {
    (left.x - right.x).abs() <= POINT_EPSILON && (left.y - right.y).abs() <= POINT_EPSILON
}

trait TargetedPointerSink: Send + Sync {
    fn post(
        &self,
        event: &PreparedPointerEvent,
        boundary: &mut NativeSideEffectBoundary<'_>,
        epoch: Option<&PointerEpochWitness>,
    ) -> Result<(), NativeError>;
}

#[derive(Clone)]
struct PointerEpochWitness {
    journal: MacSignalJournal,
    expected: u64,
}

impl PointerEpochWitness {
    fn commit_first_post<T>(
        &self,
        post: impl FnOnce() -> Result<T, NativeError>,
    ) -> Result<T, NativeError> {
        match self.journal.commit_if_epoch(self.expected, post)? {
            Some(result) => Ok(result),
            None => Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "native signal journal advanced after the captured-point observation",
            )
            .with_detail("observed_epoch", self.expected)
            .with_detail("current_epoch", self.journal.epoch())),
        }
    }
}

struct SystemTargetedPointerSink;

impl TargetedPointerSink for SystemTargetedPointerSink {
    fn post(
        &self,
        event: &PreparedPointerEvent,
        boundary: &mut NativeSideEffectBoundary<'_>,
        epoch: Option<&PointerEpochWitness>,
    ) -> Result<(), NativeError> {
        let mut post = || {
            boundary.begin()?;
            synthesized_event::post_mouse_event(&MouseEventSpec {
                pid: event.point.pid,
                cg_window_id: event.point.cg_window_id,
                screen: event.point.screen,
                window_local: event.point.window_local,
                button: event.button,
                click_count: event.click_state,
                modifiers: &event.modifiers,
                kind: match event.kind {
                    PointerEventKind::Down => NativeMouseEventKind::Down,
                    PointerEventKind::Up => NativeMouseEventKind::Up,
                },
            })
        };
        match epoch {
            Some(epoch) => epoch.commit_first_post(post),
            None => post(),
        }
    }
}

fn dispatch_sequence(
    sink: &dyn TargetedPointerSink,
    sequence: &PreparedSequence,
    deadline: Instant,
    logical_cursor: &TargetCursorHandle,
    evidence: &mut PointerDispatchEvidence,
    boundary: &mut NativeSideEffectBoundary<'_>,
    epoch: &PointerEpochWitness,
) -> Result<(), NativeError> {
    let started = Instant::now();
    let mut pressed: Option<(MouseButton, NativePoint)> = None;

    for event in &sequence.events {
        if let Err(error) = wait_until(started + event.scheduled_after, deadline) {
            let dispatch_started = boundary.started() || evidence.completed_events > 0;
            let cleanup = if dispatch_started {
                cleanup_pressed(sink, pressed.take(), boundary)
            } else {
                CleanupResult::not_needed()
            };
            evidence.cleanup_attempted = cleanup.attempted;
            evidence.cleanup_succeeded = cleanup.succeeded;
            evidence.modifiers_cleared = cleanup.succeeded;
            evidence.may_have_partially_landed = dispatch_started;
            return Err(error_with_cleanup(
                error,
                cleanup,
                evidence.completed_events,
            ));
        }
        if event.kind == PointerEventKind::Down {
            // Posting APIs are void. A returned error may still follow partial
            // native delivery, so cleanup must conservatively assume down.
            pressed = Some((event.button, event.point.clone()));
        }
        let first_post_epoch = (evidence.completed_events == 0).then_some(epoch);
        if let Err(error) = sink.post(event, boundary, first_post_epoch) {
            let dispatch_started = boundary.started() || evidence.completed_events > 0;
            let cleanup = if dispatch_started {
                cleanup_pressed(sink, pressed.take(), boundary)
            } else {
                CleanupResult::not_needed()
            };
            evidence.cleanup_attempted = cleanup.attempted;
            evidence.cleanup_succeeded = cleanup.succeeded;
            evidence.modifiers_cleared = cleanup.succeeded;
            evidence.may_have_partially_landed = dispatch_started;
            return Err(error_with_cleanup(
                error,
                cleanup,
                evidence.completed_events,
            ));
        }
        evidence.completed_events += 1;
        evidence.may_have_partially_landed = true;
        match event.kind {
            PointerEventKind::Down => {}
            PointerEventKind::Up => {
                pressed = None;
                logical_cursor.update(event.point.logical);
            }
        }
    }
    evidence.may_have_partially_landed = false;
    evidence.cleanup_succeeded = true;
    evidence.modifiers_cleared = true;
    Ok(())
}

fn wait_until(scheduled: Instant, deadline: Instant) -> Result<(), NativeError> {
    let now = Instant::now();
    if now > deadline || scheduled > deadline {
        return Err(NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            false,
            "targeted pointer sequence exceeded the controller-owned work deadline",
        ));
    }
    if scheduled > now {
        thread::sleep(scheduled.saturating_duration_since(now));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct CleanupResult {
    attempted: bool,
    succeeded: bool,
}

impl CleanupResult {
    fn not_needed() -> Self {
        Self {
            attempted: false,
            succeeded: true,
        }
    }
}

fn cleanup_pressed(
    sink: &dyn TargetedPointerSink,
    pressed: Option<(MouseButton, NativePoint)>,
    boundary: &mut NativeSideEffectBoundary<'_>,
) -> CleanupResult {
    let Some((button, point)) = pressed else {
        return CleanupResult::not_needed();
    };
    let cleanup = pointer_event(PointerEventKind::Up, point, button, 1, &[], 0);
    CleanupResult {
        attempted: true,
        succeeded: sink.post(&cleanup, boundary, None).is_ok(),
    }
}

fn error_with_cleanup(
    mut error: NativeError,
    cleanup: CleanupResult,
    completed_events: usize,
) -> NativeError {
    error
        .details
        .insert("completed_events".to_owned(), completed_events.into());
    error
        .details
        .insert("cleanup_attempted".to_owned(), cleanup.attempted.into());
    error
        .details
        .insert("cleanup_succeeded".to_owned(), cleanup.succeeded.into());
    if cleanup.attempted && !cleanup.succeeded {
        error.retryable = false;
    }
    error
}

struct PointerDispatchEvidence {
    completed_events: usize,
    cleanup_attempted: bool,
    cleanup_succeeded: bool,
    modifiers_cleared: bool,
    may_have_partially_landed: bool,
    hardware_cursor_warp_attempted: bool,
}

impl PointerDispatchEvidence {
    fn new(_action: &MacPreparedPointerAction) -> Self {
        Self {
            completed_events: 0,
            cleanup_attempted: false,
            cleanup_succeeded: false,
            modifiers_cleared: false,
            may_have_partially_landed: false,
            hardware_cursor_warp_attempted: false,
        }
    }

    fn merge_into(&self, evidence: &mut NativeEvidence) {
        evidence.fields.insert(
            "pointer_completed_events".to_owned(),
            self.completed_events.into(),
        );
        evidence.fields.insert(
            "pointer_cleanup_attempted".to_owned(),
            self.cleanup_attempted.into(),
        );
        evidence.fields.insert(
            "pointer_cleanup_succeeded".to_owned(),
            self.cleanup_succeeded.into(),
        );
        evidence.fields.insert(
            "pointer_modifiers_cleared".to_owned(),
            self.modifiers_cleared.into(),
        );
        evidence.fields.insert(
            "pointer_dispatch_may_have_partially_landed".to_owned(),
            self.may_have_partially_landed.into(),
        );
        evidence.fields.insert(
            "hardware_cursor_warp_attempted".to_owned(),
            self.hardware_cursor_warp_attempted.into(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use cua_driver_core::api::{
        contracts::{
            AppId, AppRef, CaptureRevision, GeometryRevision, SurfaceId, WindowGeneration,
            WindowId, WindowRef,
        },
        observation::{NativeProcessHandle, NativeWindowHandle},
    };

    use super::*;

    fn point(x: f64, y: f64) -> NativePoint {
        let app = AppRef {
            id: AppId::parse("app").unwrap(),
            name: None,
            pid: Some(10),
            running: true,
        };
        let public = WindowRef {
            id: WindowId::parse("window").unwrap(),
            app,
            title: None,
        };
        NativePoint {
            screen: Point { x, y },
            window_local: Point { x, y },
            logical: Point { x, y },
            pid: 10,
            cg_window_id: 20,
            owner_bounds: cua_driver_core::api::contracts::Rect {
                x: 0.0,
                y: 0.0,
                width: 1_000.0,
                height: 1_000.0,
            },
            owner: ResolvedWindowStamp {
                app_id: public.app.id,
                window_id: public.id,
                generation: WindowGeneration(1),
                geometry_revision: GeometryRevision::parse("geometry").unwrap(),
                native_window: NativeWindowHandle::new("native").unwrap(),
                process: NativeProcessHandle::new("process").unwrap(),
            },
            surface_id: SurfaceId::parse("surface").unwrap().to_string(),
            capture_revision: CaptureRevision::parse("capture").unwrap().to_string(),
            geometry_revision: GeometryRevision::parse("geometry").unwrap().to_string(),
            observation_epoch: 0,
        }
    }

    #[test]
    fn canonical_click_is_one_down_up_pair_with_the_swift_interval() {
        let sequence = build_click(point(10.0, 20.0));
        assert_eq!(sequence.events.len(), 2);
        assert_eq!(sequence.events[0].kind, PointerEventKind::Down);
        assert_eq!(sequence.events[1].kind, PointerEventKind::Up);
        assert_eq!(sequence.events[0].scheduled_after, Duration::ZERO);
        assert_eq!(
            sequence.events[1].scheduled_after,
            Duration::from_millis(100)
        );
        assert!(sequence.events.iter().all(|event| {
            event.button == MouseButton::Left
                && event.click_state == 1
                && event.modifiers.is_empty()
        }));
    }

    struct FakeSink {
        events: Mutex<Vec<PreparedPointerEvent>>,
        fail_at: usize,
        signal_after_first: Option<MacSignalJournal>,
    }

    impl TargetedPointerSink for FakeSink {
        fn post(
            &self,
            event: &PreparedPointerEvent,
            _boundary: &mut NativeSideEffectBoundary<'_>,
            epoch: Option<&PointerEpochWitness>,
        ) -> Result<(), NativeError> {
            let post = || {
                let mut events = self.events.lock().unwrap();
                let index = events.len();
                events.push(event.clone());
                if index == self.fail_at {
                    Err(NativeError::new(
                        ErrorCode::DispatchFailed,
                        ErrorPhase::Dispatch,
                        false,
                        "injected pointer post failure",
                    ))
                } else {
                    Ok(index)
                }
            };
            let index = if let Some(epoch) = epoch {
                epoch.commit_first_post(post)?
            } else {
                post()?
            };
            if index == 0 {
                if let Some(journal) = &self.signal_after_first {
                    journal.record(SettlementSignal::FocusChanged);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn failed_up_posts_targeted_button_cleanup_without_cursor_warp() {
        let sequence = build_click(point(0.0, 0.0));
        let sink = FakeSink {
            events: Mutex::new(Vec::new()),
            fail_at: 1,
            signal_after_first: None,
        };
        let mut evidence = PointerDispatchEvidence {
            completed_events: 0,
            cleanup_attempted: false,
            cleanup_succeeded: false,
            modifiers_cleared: false,
            may_have_partially_landed: false,
            hardware_cursor_warp_attempted: false,
        };
        let mut observations = cua_driver_core::api::observation::ObservationStore::default();
        let mut settlement = cua_driver_core::api::settlement::SettlementState::default();
        let mut boundary = NativeSideEffectBoundary::new(
            &mut observations,
            &mut settlement,
            cua_driver_core::api::contracts::ObservationId::parse("unused-observation").unwrap(),
            cua_driver_core::api::contracts::ActionId::parse("unused-action").unwrap(),
            cua_driver_core::api::settlement::SettlementProfile::dispatch_only("test"),
        );
        let journal = MacSignalJournal::default();
        let epoch = PointerEpochWitness {
            expected: journal.epoch(),
            journal,
        };
        let error = dispatch_sequence(
            &sink,
            &sequence,
            Instant::now() + Duration::from_secs(1),
            &TargetCursorHandle::default(),
            &mut evidence,
            &mut boundary,
            &epoch,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::DispatchFailed);
        assert!(evidence.cleanup_attempted);
        assert!(evidence.cleanup_succeeded);
        assert!(evidence.modifiers_cleared);
        assert!(!evidence.hardware_cursor_warp_attempted);
        let events = sink.events.lock().unwrap();
        let cleanup = events.last().unwrap();
        assert_eq!(cleanup.kind, PointerEventKind::Up);
        assert!(cleanup.modifiers.is_empty());
        assert_eq!(cleanup.point.pid, sequence.target.pid);
        assert_eq!(cleanup.point.cg_window_id, sequence.target.cg_window_id);
    }

    #[test]
    fn advanced_observation_epoch_refuses_before_the_first_pointer_post() {
        let sequence = build_click(point(5.0, 6.0));
        let sink = FakeSink {
            events: Mutex::new(Vec::new()),
            fail_at: usize::MAX,
            signal_after_first: None,
        };
        let journal = MacSignalJournal::default();
        let epoch = PointerEpochWitness {
            expected: journal.epoch(),
            journal: journal.clone(),
        };
        journal.record(SettlementSignal::FocusChanged);
        let mut evidence = PointerDispatchEvidence::new(&MacPreparedPointerAction {
            final_cursor: sequence.target.logical,
            route_name: "test",
            sequence: sequence.clone(),
        });
        let mut observations = cua_driver_core::api::observation::ObservationStore::default();
        let mut settlement = cua_driver_core::api::settlement::SettlementState::default();
        let mut boundary = NativeSideEffectBoundary::new(
            &mut observations,
            &mut settlement,
            cua_driver_core::api::contracts::ObservationId::parse("unused-observation").unwrap(),
            cua_driver_core::api::contracts::ActionId::parse("unused-action").unwrap(),
            cua_driver_core::api::settlement::SettlementProfile::dispatch_only("test"),
        );
        let error = dispatch_sequence(
            &sink,
            &sequence,
            Instant::now() + Duration::from_secs(1),
            &TargetCursorHandle::default(),
            &mut evidence,
            &mut boundary,
            &epoch,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ObservationRaced);
        assert!(error.retryable);
        assert!(!boundary.started());
        assert!(!evidence.cleanup_attempted);
        assert!(!evidence.may_have_partially_landed);
        assert!(sink.events.lock().unwrap().is_empty());
    }

    #[test]
    fn signal_from_first_post_does_not_abort_the_owned_pointer_sequence() {
        let sequence = build_click(point(10.0, 20.0));
        let journal = MacSignalJournal::default();
        let sink = FakeSink {
            events: Mutex::new(Vec::new()),
            fail_at: usize::MAX,
            signal_after_first: Some(journal.clone()),
        };
        let epoch = PointerEpochWitness {
            expected: journal.epoch(),
            journal: journal.clone(),
        };
        let mut evidence = PointerDispatchEvidence::new(&MacPreparedPointerAction {
            final_cursor: sequence.target.logical,
            route_name: "test",
            sequence: sequence.clone(),
        });
        let mut observations = cua_driver_core::api::observation::ObservationStore::default();
        let mut settlement = cua_driver_core::api::settlement::SettlementState::default();
        let mut boundary = NativeSideEffectBoundary::new(
            &mut observations,
            &mut settlement,
            cua_driver_core::api::contracts::ObservationId::parse("unused-observation").unwrap(),
            cua_driver_core::api::contracts::ActionId::parse("unused-action").unwrap(),
            cua_driver_core::api::settlement::SettlementProfile::dispatch_only("test"),
        );

        dispatch_sequence(
            &sink,
            &sequence,
            Instant::now() + Duration::from_secs(1),
            &TargetCursorHandle::default(),
            &mut evidence,
            &mut boundary,
            &epoch,
        )
        .unwrap();

        assert_eq!(journal.epoch(), epoch.expected + 1);
        assert_eq!(sink.events.lock().unwrap().len(), sequence.events.len());
        assert_eq!(evidence.completed_events, sequence.events.len());
    }
}
