//! Exact window-stamped targeted pointer sequences for the v2 driver.

use std::{
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton, EventField, ScrollEventUnit},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use cua_driver_core::api::{
    contracts::{Modifier, MouseButton, Point, VerificationLevel},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{InteractionScope, NativeEvidence, NativeSideEffectBoundary, TargetCursorHandle},
    observation::{ResolvedPoint, ResolvedWindowStamp, SurfaceOwner},
    platform::{NativeDispatch, PointerActionProvider, ResolvedAction},
    settlement::SettlementSignal,
    Route,
};
use foreign_types::ForeignType;
use serde_json::Value;

use crate::{
    driver::{
        settlement::MacSignalJournal,
        target::MacTargetState,
        windows::{MacRelatedWindowFacts, MacWindowFacts, MacWindowRegistry},
    },
    input::skylight,
};

const CLICK_PRIMER_MOVE_MS: u64 = 15;
const CLICK_PRIMER_UP_MS: u64 = 16;
const CLICK_TARGET_START_MS: u64 = 116;
const CLICK_UP_DELAY_MS: u64 = 1;
const CLICK_PAIR_GAP_MS: u64 = 80;
const MAX_DRAG_STEPS: usize = 60;
const MIN_DRAG_STEPS: usize = 2;
const DRAG_POINTS_PER_STEP: f64 = 16.0;
const FIXED_POINT_SCALE: f64 = 65_536.0;
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
        if !skylight::is_targeted_pointer_route_available() {
            return Err(NativeError::unsupported(
                "recipe_unproven: complete SkyLight targeted-pointer post/window-stamp symbols are unavailable",
            )
            .with_detail("recipe_status", "recipe_unproven"));
        }

        let (sequence, final_cursor, route_name) = match action {
            ResolvedAction::PointClick { point, spec } => {
                let point = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), point)
                    .await?;
                let sequence = build_gesture(
                    point.clone(),
                    None,
                    spec.button,
                    spec.click_count,
                    &spec.modifiers,
                    0,
                )?;
                (sequence, point.logical, "macos_targeted_pointer_click")
            }
            ResolvedAction::Drag(drag) => {
                let start = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), &drag.start)
                    .await?;
                let end = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), &drag.end)
                    .await?;
                ensure_same_surface(&start, &end)?;
                if same_point(start.screen, end.screen) {
                    return Err(NativeError::invalid(
                        "drag endpoints must differ so the native sequence contains real motion",
                    ));
                }
                let sequence = build_gesture(
                    start,
                    Some(end.clone()),
                    drag.button,
                    1,
                    &drag.modifiers,
                    drag.duration_ms,
                )?;
                (sequence, end.logical, "macos_targeted_pointer_drag")
            }
            ResolvedAction::DeltaScroll(scroll) => {
                let point = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), &scroll.point)
                    .await?;
                let sequence = build_scroll(point.clone(), scroll.delta_x, scroll.delta_y)?;
                (sequence, point.logical, "macos_targeted_pointer_scroll")
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
        scope.logical_cursor.update(action.final_cursor);
        target
            .signals
            .record(SettlementSignal::PointerSequenceComplete);

        // Both SkyLight and public CGEvent process posting are void APIs. A
        // completed call proves only that the complete targeted sequence was
        // attempted; it is not a native delivery acknowledgement.
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
            "skylight_then_core_graphics_post_to_pid".into(),
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
        if !skylight::is_targeted_pointer_route_available() {
            return Err(NativeError::unsupported(
                "recipe_unproven: targeted pointer symbols disappeared before dispatch",
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
            if matches!(
                event.kind,
                PointerEventKind::PrimerDown | PointerEventKind::PrimerUp
            ) {
                continue;
            }
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
    Moved,
    PrimerDown,
    PrimerUp,
    Down,
    Dragged,
    Up,
    Scroll,
}

#[derive(Debug, Clone)]
struct PreparedPointerEvent {
    kind: PointerEventKind,
    point: NativePoint,
    button: MouseButton,
    click_state: u8,
    modifiers: Vec<Modifier>,
    scheduled_after: Duration,
    native_scroll_x: f64,
    native_scroll_y: f64,
}

fn build_gesture(
    start: NativePoint,
    drag_to: Option<NativePoint>,
    button: MouseButton,
    click_count: u8,
    modifiers: &[Modifier],
    duration_ms: u32,
) -> Result<PreparedSequence, NativeError> {
    if !(1..=3).contains(&click_count) {
        return Err(NativeError::invalid(
            "targeted pointer click_count must be between 1 and 3",
        ));
    }
    if drag_to.is_some() && click_count != 1 {
        return Err(NativeError::invalid(
            "a targeted drag is one gesture and cannot carry multiple click pairs",
        ));
    }
    let primer = NativePoint {
        screen: Point { x: -1.0, y: -1.0 },
        window_local: Point { x: -1.0, y: -1.0 },
        logical: start.logical,
        ..start.clone()
    };
    let mut events = vec![
        pointer_event(
            PointerEventKind::Moved,
            start.clone(),
            button,
            0,
            modifiers,
            0,
        ),
        pointer_event(
            PointerEventKind::PrimerDown,
            primer.clone(),
            MouseButton::Left,
            1,
            &[],
            CLICK_PRIMER_MOVE_MS,
        ),
        pointer_event(
            PointerEventKind::PrimerUp,
            primer,
            MouseButton::Left,
            1,
            &[],
            CLICK_PRIMER_UP_MS,
        ),
    ];

    if let Some(end) = drag_to {
        events.push(pointer_event(
            PointerEventKind::Down,
            start.clone(),
            button,
            1,
            modifiers,
            CLICK_TARGET_START_MS,
        ));
        let distance = ((end.screen.x - start.screen.x).powi(2)
            + (end.screen.y - start.screen.y).powi(2))
        .sqrt();
        let steps = ((distance / DRAG_POINTS_PER_STEP).ceil() as usize)
            .clamp(MIN_DRAG_STEPS, MAX_DRAG_STEPS);
        for index in 1..=steps {
            let fraction = index as f64 / (steps + 1) as f64;
            let point = interpolate_point(&start, &end, fraction);
            let delay = u64::from(duration_ms) * index as u64 / (steps + 1) as u64;
            events.push(pointer_event(
                PointerEventKind::Dragged,
                point,
                button,
                1,
                modifiers,
                CLICK_TARGET_START_MS + delay,
            ));
        }
        events.push(pointer_event(
            PointerEventKind::Up,
            end,
            button,
            1,
            modifiers,
            CLICK_TARGET_START_MS + u64::from(duration_ms),
        ));
        return Ok(PreparedSequence {
            target: start,
            events,
        });
    }

    for pair_index in 0..click_count {
        let down_at =
            CLICK_TARGET_START_MS + u64::from(pair_index) * (CLICK_UP_DELAY_MS + CLICK_PAIR_GAP_MS);
        let click_state = pair_index + 1;
        events.push(pointer_event(
            PointerEventKind::Down,
            start.clone(),
            button,
            click_state,
            modifiers,
            down_at,
        ));
        events.push(pointer_event(
            PointerEventKind::Up,
            start.clone(),
            button,
            click_state,
            modifiers,
            down_at + CLICK_UP_DELAY_MS,
        ));
    }
    Ok(PreparedSequence {
        target: start,
        events,
    })
}

fn build_scroll(
    point: NativePoint,
    public_delta_x: f64,
    public_delta_y: f64,
) -> Result<PreparedSequence, NativeError> {
    if !public_delta_x.is_finite() || !public_delta_y.is_finite() {
        return Err(NativeError::invalid(
            "targeted pointer scroll deltas must be finite",
        ));
    }
    if public_delta_x == 0.0 && public_delta_y == 0.0 {
        return Err(NativeError::invalid(
            "targeted pointer scroll deltas cannot both be zero",
        ));
    }
    // Core is positive-right/positive-down. Native pixel wheel deltas are the
    // opposite: positive reveals left/up content.
    let native_scroll_x = exact_fixed_delta(-public_delta_x)?;
    let native_scroll_y = exact_fixed_delta(-public_delta_y)?;
    let event = PreparedPointerEvent {
        kind: PointerEventKind::Scroll,
        point: point.clone(),
        button: MouseButton::Left,
        click_state: 0,
        modifiers: Vec::new(),
        scheduled_after: Duration::ZERO,
        native_scroll_x,
        native_scroll_y,
    };
    Ok(PreparedSequence {
        target: point,
        events: vec![event],
    })
}

fn exact_fixed_delta(value: f64) -> Result<f64, NativeError> {
    let scaled = value * FIXED_POINT_SCALE;
    if !scaled.is_finite()
        || scaled < f64::from(i32::MIN)
        || scaled > f64::from(i32::MAX)
        || (scaled.round() - scaled).abs() > f64::EPSILON * scaled.abs().max(1.0) * 4.0
    {
        return Err(NativeError::unsupported(
            "exact macOS pixel scroll route requires a signed 16.16-representable logical delta",
        )
        .with_detail("requested_native_delta", value));
    }
    Ok(scaled.round() / FIXED_POINT_SCALE)
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
        native_scroll_x: 0.0,
        native_scroll_y: 0.0,
    }
}

fn interpolate_point(start: &NativePoint, end: &NativePoint, fraction: f64) -> NativePoint {
    let interpolate = |left: f64, right: f64| left + (right - left) * fraction;
    NativePoint {
        screen: Point {
            x: interpolate(start.screen.x, end.screen.x),
            y: interpolate(start.screen.y, end.screen.y),
        },
        window_local: Point {
            x: interpolate(start.window_local.x, end.window_local.x),
            y: interpolate(start.window_local.y, end.window_local.y),
        },
        logical: Point {
            x: interpolate(start.logical.x, end.logical.x),
            y: interpolate(start.logical.y, end.logical.y),
        },
        ..start.clone()
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

fn ensure_same_surface(start: &NativePoint, end: &NativePoint) -> Result<(), NativeError> {
    if start.pid != end.pid
        || start.cg_window_id != end.cg_window_id
        || start.owner != end.owner
        || start.surface_id != end.surface_id
        || start.capture_revision != end.capture_revision
        || start.geometry_revision != end.geometry_revision
        || start.observation_epoch != end.observation_epoch
    {
        return Err(NativeError::stale(
            ErrorCode::SurfaceStale,
            "drag endpoints do not share one current captured surface and native owner",
        ));
    }
    Ok(())
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
        gesture_id: i64,
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
        gesture_id: i64,
        boundary: &mut NativeSideEffectBoundary<'_>,
        epoch: Option<&PointerEpochWitness>,
    ) -> Result<(), NativeError> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
            NativeError::new(
                ErrorCode::DispatchFailed,
                ErrorPhase::Dispatch,
                false,
                "CGEventSource creation failed for targeted pointer dispatch",
            )
        })?;
        let native = match event.kind {
            PointerEventKind::Scroll => build_native_scroll(source, event)?,
            _ => build_native_mouse(source, event)?,
        };
        stamp_targeted_event(&native, event, gesture_id)?;
        let pointer = native.as_ptr() as *mut std::ffi::c_void;
        let mut post = || {
            boundary.begin()?;
            post_both_targeted_transports(
                || skylight::post_to_pid(event.point.pid, pointer, false),
                || {
                    native.post_to_pid(event.point.pid);
                    Ok(())
                },
            )
        };
        match epoch {
            Some(epoch) => epoch.commit_first_post(post),
            None => post(),
        }
    }
}

fn post_both_targeted_transports(
    skylight_post: impl FnOnce() -> bool,
    core_graphics_post: impl FnOnce() -> Result<(), NativeError>,
) -> Result<(), NativeError> {
    let skylight_succeeded = skylight_post();
    let core_graphics_result = core_graphics_post();
    let core_graphics_succeeded = core_graphics_result.is_ok();

    let skylight_error = (!skylight_succeeded).then(|| {
        NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            false,
            "SLEventPostToPid became unavailable after pointer preflight",
        )
    });
    let mut failures = Vec::new();
    if let Some(error) = skylight_error {
        failures.push(error);
    }
    if let Err(error) = core_graphics_result {
        failures.push(error);
    }
    let Some(error) = NativeError::primary(failures) else {
        return Ok(());
    };
    Err(error
        .with_detail("skylight_attempted", true)
        .with_detail("skylight_succeeded", skylight_succeeded)
        .with_detail("core_graphics_attempted", true)
        .with_detail("core_graphics_call_completed", core_graphics_succeeded))
}

fn build_native_mouse(
    source: CGEventSource,
    event: &PreparedPointerEvent,
) -> Result<CGEvent, NativeError> {
    let (event_type, button) = native_mouse_shape(event.kind, event.button)?;
    let native = CGEvent::new_mouse_event(
        source,
        event_type,
        CGPoint::new(event.point.screen.x, event.point.screen.y),
        button,
    )
    .map_err(|_| {
        NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            false,
            "CGEvent mouse event construction failed",
        )
    })?;
    let flags = modifier_flags(&event.modifiers);
    if flags != CGEventFlags::CGEventFlagNull {
        native.set_flags(flags);
    }
    Ok(native)
}

fn build_native_scroll(
    source: CGEventSource,
    event: &PreparedPointerEvent,
) -> Result<CGEvent, NativeError> {
    let native_y = event.native_scroll_y.round();
    let native_x = event.native_scroll_x.round();
    if native_y < f64::from(i32::MIN)
        || native_y > f64::from(i32::MAX)
        || native_x < f64::from(i32::MIN)
        || native_x > f64::from(i32::MAX)
    {
        return Err(NativeError::unsupported(
            "macOS pixel scroll delta exceeds the native event field range",
        ));
    }
    let native = CGEvent::new_scroll_event(
        source,
        ScrollEventUnit::PIXEL,
        2,
        native_y as i32,
        native_x as i32,
        0,
    )
    .map_err(|_| {
        NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            false,
            "CGEvent pixel scroll construction failed",
        )
    })?;
    native.set_double_value_field(
        EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1,
        event.native_scroll_y,
    );
    native.set_double_value_field(
        EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2,
        event.native_scroll_x,
    );
    native.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 1);
    unsafe {
        CGEventSetLocation(
            native.as_ptr() as *mut std::ffi::c_void,
            event.point.screen.x,
            event.point.screen.y,
        );
    }
    Ok(native)
}

fn stamp_targeted_event(
    event: &CGEvent,
    spec: &PreparedPointerEvent,
    gesture_id: i64,
) -> Result<(), NativeError> {
    let pointer = event.as_ptr() as *mut std::ffi::c_void;
    if !skylight::set_window_location(
        pointer,
        spec.point.window_local.x,
        spec.point.window_local.y,
    ) {
        return Err(missing_stamp("CGEventSetWindowLocation"));
    }
    for (field, value) in [
        (0, phase(spec.kind)),
        (1, i64::from(spec.click_state)),
        (3, button_number(spec.button)),
        (7, 3),
        (40, i64::from(spec.point.pid)),
        (51, i64::from(spec.point.cg_window_id)),
        (58, gesture_id),
        (91, i64::from(spec.point.cg_window_id)),
        (92, i64::from(spec.point.cg_window_id)),
    ] {
        if !skylight::set_integer_field(pointer, field, value) {
            return Err(missing_stamp("SLEventSetIntegerValueField"));
        }
    }
    Ok(())
}

fn native_mouse_shape(
    kind: PointerEventKind,
    button: MouseButton,
) -> Result<(CGEventType, CGMouseButton), NativeError> {
    let native_button = match button {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Right => CGMouseButton::Right,
        MouseButton::Middle => CGMouseButton::Center,
    };
    let event_type = match (kind, button) {
        (PointerEventKind::Moved, _) => CGEventType::MouseMoved,
        (PointerEventKind::PrimerDown, _) | (PointerEventKind::Down, MouseButton::Left) => {
            CGEventType::LeftMouseDown
        }
        (PointerEventKind::PrimerUp, _) | (PointerEventKind::Up, MouseButton::Left) => {
            CGEventType::LeftMouseUp
        }
        (PointerEventKind::Down, MouseButton::Right) => CGEventType::RightMouseDown,
        (PointerEventKind::Up, MouseButton::Right) => CGEventType::RightMouseUp,
        (PointerEventKind::Down, MouseButton::Middle) => CGEventType::OtherMouseDown,
        (PointerEventKind::Up, MouseButton::Middle) => CGEventType::OtherMouseUp,
        (PointerEventKind::Dragged, MouseButton::Left) => CGEventType::LeftMouseDragged,
        (PointerEventKind::Dragged, MouseButton::Right) => CGEventType::RightMouseDragged,
        (PointerEventKind::Dragged, MouseButton::Middle) => CGEventType::OtherMouseDragged,
        (PointerEventKind::Scroll, _) => {
            return Err(NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Dispatch,
                false,
                "scroll event reached mouse event construction",
            ))
        }
    };
    Ok((event_type, native_button))
}

fn phase(kind: PointerEventKind) -> i64 {
    match kind {
        PointerEventKind::Moved | PointerEventKind::PrimerUp => 2,
        PointerEventKind::PrimerDown => 1,
        PointerEventKind::Down
        | PointerEventKind::Dragged
        | PointerEventKind::Up
        | PointerEventKind::Scroll => 3,
    }
}

fn button_number(button: MouseButton) -> i64 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

fn modifier_flags(modifiers: &[Modifier]) -> CGEventFlags {
    modifiers
        .iter()
        .fold(CGEventFlags::CGEventFlagNull, |flags, modifier| {
            flags
                | match modifier {
                    Modifier::Shift => CGEventFlags::CGEventFlagShift,
                    Modifier::Control => CGEventFlags::CGEventFlagControl,
                    Modifier::Alt => CGEventFlags::CGEventFlagAlternate,
                    Modifier::Meta => CGEventFlags::CGEventFlagCommand,
                }
        })
}

fn missing_stamp(symbol: &'static str) -> NativeError {
    NativeError::new(
        ErrorCode::DispatchFailed,
        ErrorPhase::Dispatch,
        false,
        "required targeted pointer event stamp became unavailable after preflight",
    )
    .with_detail("symbol", symbol)
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSetLocation(event: *mut std::ffi::c_void, x: f64, y: f64);
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
    let gesture_id = gesture_id(sequence);
    let mut pressed: Option<(MouseButton, NativePoint)> = None;

    for event in &sequence.events {
        if let Err(error) = wait_until(started + event.scheduled_after, deadline) {
            let dispatch_started = boundary.started() || evidence.completed_events > 0;
            let cleanup = if dispatch_started {
                cleanup_pressed(sink, pressed.take(), gesture_id, boundary)
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
        if matches!(
            event.kind,
            PointerEventKind::PrimerDown | PointerEventKind::Down
        ) {
            // Posting APIs are void. A returned error may still follow partial
            // native delivery, so cleanup must conservatively assume down.
            pressed = Some((event.button, event.point.clone()));
        }
        let first_post_epoch = (evidence.completed_events == 0).then_some(epoch);
        if let Err(error) = sink.post(event, gesture_id, boundary, first_post_epoch) {
            let dispatch_started = boundary.started() || evidence.completed_events > 0;
            let cleanup = if dispatch_started {
                cleanup_pressed(sink, pressed.take(), gesture_id, boundary)
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
            PointerEventKind::PrimerDown | PointerEventKind::Down => {}
            PointerEventKind::PrimerUp | PointerEventKind::Up => pressed = None,
            PointerEventKind::Moved | PointerEventKind::Dragged => {
                logical_cursor.update(event.point.logical);
            }
            PointerEventKind::Scroll => logical_cursor.update(event.point.logical),
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
    gesture_id: i64,
    boundary: &mut NativeSideEffectBoundary<'_>,
) -> CleanupResult {
    let Some((button, point)) = pressed else {
        return CleanupResult::not_needed();
    };
    let cleanup = pointer_event(PointerEventKind::Up, point, button, 1, &[], 0);
    CleanupResult {
        attempted: true,
        succeeded: sink.post(&cleanup, gesture_id, boundary, None).is_ok(),
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

fn gesture_id(sequence: &PreparedSequence) -> i64 {
    static NEXT_GESTURE_ID: AtomicI64 = AtomicI64::new(1);
    let sequence_id = NEXT_GESTURE_ID.fetch_add(1, Ordering::Relaxed);
    sequence_id
        ^ i64::from(sequence.target.cg_window_id)
        ^ i64::from(sequence.target.pid).rotate_left(17)
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
    fn one_builder_preserves_button_count_modifiers_and_drag_motion() {
        for (button, count) in [
            (MouseButton::Left, 1),
            (MouseButton::Right, 2),
            (MouseButton::Middle, 3),
        ] {
            let sequence = build_gesture(
                point(10.0, 20.0),
                None,
                button,
                count,
                &[Modifier::Shift, Modifier::Meta],
                0,
            )
            .unwrap();
            let target: Vec<_> = sequence
                .events
                .iter()
                .filter(|event| matches!(event.kind, PointerEventKind::Down | PointerEventKind::Up))
                .collect();
            assert_eq!(target.len(), usize::from(count) * 2);
            assert!(target.iter().all(|event| {
                event.button == button && event.modifiers == [Modifier::Shift, Modifier::Meta]
            }));
            assert_eq!(
                target
                    .chunks_exact(2)
                    .map(|pair| pair[0].click_state)
                    .collect::<Vec<_>>(),
                (1..=count).collect::<Vec<_>>()
            );
        }

        let drag = build_gesture(
            point(0.0, 0.0),
            Some(point(64.0, 32.0)),
            MouseButton::Right,
            1,
            &[Modifier::Alt],
            400,
        )
        .unwrap();
        assert!(matches!(drag.events[0].kind, PointerEventKind::Moved));
        let dragged: Vec<_> = drag
            .events
            .iter()
            .filter(|event| event.kind == PointerEventKind::Dragged)
            .collect();
        assert!(dragged.len() >= 2);
        assert!(dragged.windows(2).all(|pair| {
            pair[0].scheduled_after <= pair[1].scheduled_after
                && pair[0].point.screen.x < pair[1].point.screen.x
        }));
        assert!(matches!(
            drag.events.last().unwrap().kind,
            PointerEventKind::Up
        ));
    }

    #[test]
    fn scroll_converts_public_right_down_only_at_native_boundary() {
        let sequence = build_scroll(point(5.0, 6.0), 12.5, 7.25).unwrap();
        let event = &sequence.events[0];
        assert_eq!(event.native_scroll_x, -12.5);
        assert_eq!(event.native_scroll_y, -7.25);
        assert!(build_scroll(point(0.0, 0.0), 0.1, 0.0).is_err());
    }

    #[test]
    fn proved_pointer_transport_posts_skylight_then_core_graphics() {
        let calls = Mutex::new(Vec::new());
        post_both_targeted_transports(
            || {
                calls.lock().unwrap().push("skylight");
                true
            },
            || {
                calls.lock().unwrap().push("core_graphics");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["skylight", "core_graphics"]);
    }

    #[test]
    fn failed_skylight_post_still_attempts_core_graphics_and_reports_both() {
        let calls = Mutex::new(Vec::new());
        let error = post_both_targeted_transports(
            || {
                calls.lock().unwrap().push("skylight");
                false
            },
            || {
                calls.lock().unwrap().push("core_graphics");
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(*calls.lock().unwrap(), ["skylight", "core_graphics"]);
        assert_eq!(error.code, ErrorCode::DispatchFailed);
        assert_eq!(error.details["skylight_succeeded"], false);
        assert_eq!(error.details["core_graphics_attempted"], true);
        assert_eq!(error.details["core_graphics_call_completed"], true);
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
            _gesture_id: i64,
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
    fn partial_drag_posts_targeted_button_cleanup_without_cursor_warp() {
        let sequence = build_gesture(
            point(0.0, 0.0),
            Some(point(64.0, 0.0)),
            MouseButton::Left,
            1,
            &[Modifier::Shift],
            0,
        )
        .unwrap();
        let down_index = sequence
            .events
            .iter()
            .position(|event| event.kind == PointerEventKind::Down)
            .unwrap();
        let sink = FakeSink {
            events: Mutex::new(Vec::new()),
            fail_at: down_index + 1,
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
        let sequence = build_scroll(point(5.0, 6.0), 1.0, 0.0).unwrap();
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
        let sequence =
            build_gesture(point(10.0, 20.0), None, MouseButton::Left, 1, &[], 0).unwrap();
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
