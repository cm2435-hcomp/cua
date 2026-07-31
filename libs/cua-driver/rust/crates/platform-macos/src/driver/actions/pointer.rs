//! Exact window-stamped targeted pointer sequences for the v2 driver.

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use cua_driver_core::api::{
    contracts::{Modifier, MouseButton, Point, ScrollDirection, VerificationLevel},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{InteractionScope, NativeEvidence, NativeSideEffectBoundary, TargetCursorHandle},
    menu::{MenuMutationIntent, NativeMenuEvidence, NativeMenuIdentity},
    observation::{ResolvedPoint, ResolvedWindowStamp, SurfaceOwner},
    platform::{NativeDispatch, PointerActionProvider, ResolvedAction},
    settlement::SettlementSignal,
    Route,
};
use serde_json::Value;

use crate::{
    driver::{
        menu::resolve_menu_identity,
        observation::discover_native_menu,
        settlement::MacSignalJournal,
        target::MacTargetState,
        windows::{MacRelatedWindowFacts, MacWindowFacts, MacWindowRegistry},
    },
    focus_steal,
    input::synthesized_event::{
        self, MouseEventKind as NativeMouseEventKind, MouseEventSpec, PixelScrollEventSpec,
    },
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
                let point = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), point)
                    .await?;
                let sequence = build_click(
                    point.clone(),
                    spec.button,
                    spec.click_count,
                    &spec.modifiers,
                );
                (sequence, point.logical, "macos_targeted_pointer_click")
            }
            ResolvedAction::Drag(drag) => {
                let start = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), &drag.start)
                    .await?;
                let end = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), &drag.end)
                    .await?;
                ensure_same_drag_surface(&start, &end)?;
                let sequence = build_drag(
                    start,
                    end.clone(),
                    drag.button,
                    &drag.modifiers,
                    drag.duration_ms,
                );
                (sequence, end.logical, "macos_targeted_pointer_drag")
            }
            ResolvedAction::ElementScroll {
                element,
                point: Some(point),
                spec,
                ..
            } => {
                let point = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), point)
                    .await?;
                let bounds = element.bounds.ok_or_else(|| {
                    NativeError::unsupported(
                        "signed-compatible page scroll requires exact current element bounds",
                    )
                    .with_detail("recipe_status", "element_bounds_required")
                })?;
                let (delta_x, delta_y) =
                    signed_page_scroll_delta(bounds, spec.direction, spec.pages)?;
                let sequence = build_delta_scroll(point.clone(), delta_x, delta_y);
                (
                    sequence,
                    point.logical,
                    "macos_signed_page_scroll_post_to_pid",
                )
            }
            ResolvedAction::DeltaScroll(scroll) => {
                let point = self
                    .prepare_point(target.window.clone(), scope.owner.clone(), &scroll.point)
                    .await?;
                validate_integral_scroll_delta(scroll.delta_x, "delta_x")?;
                validate_integral_scroll_delta(scroll.delta_y, "delta_y")?;
                let sequence = build_delta_scroll(point.clone(), scroll.delta_x, scroll.delta_y);
                (
                    sequence,
                    point.logical,
                    "macos_targeted_pointer_pixel_scroll",
                )
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
        let MacPreparedPointerAction {
            sequence,
            final_cursor,
            route_name,
        } = action;
        self.revalidate_sequence_target(
            target.window.clone(),
            scope.owner.clone(),
            scope.window.stamp(),
            &sequence,
        )
        .await?;
        if target.invalidated()
            || scope.owner != target.window
            || target.signals.epoch() != sequence.target.observation_epoch
        {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "pointer target or observation epoch changed during final live-facts validation",
            ));
        }
        let epoch_witness = PointerEpochWitness {
            journal: target.signals.clone(),
            expected: sequence.target.observation_epoch,
        };
        let mut evidence = PointerDispatchEvidence::new();
        let result = dispatch_sequence(
            self.sink.as_ref(),
            &sequence,
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
        if let Some(
            intent @ (MenuMutationIntent::Opening { .. } | MenuMutationIntent::Targeting { .. }),
        ) = &scope.menu_intent
        {
            if let Err(mut error) = target.arm_menu_suppression(&scope.action_id, intent.menu_id())
            {
                target.invalidate();
                scope.invalidate_target();
                error
                    .details
                    .insert("native_side_effect_started".to_owned(), true.into());
                return Err(error);
            }
        }
        let menu = match &scope.menu_intent {
            Some(MenuMutationIntent::Opening { menu_id }) => Some(
                self.wait_for_opened_menu(
                    scope.owner.clone(),
                    scope.action_id.clone(),
                    scope.deadline.work,
                    menu_id.clone(),
                )
                .await?,
            ),
            Some(MenuMutationIntent::Targeting { menu_id, identity }) => Some(
                self.menu_outcome_after_target(
                    scope.owner.clone(),
                    scope.action_id.clone(),
                    menu_id.clone(),
                    identity.clone(),
                )
                .await?,
            ),
            Some(MenuMutationIntent::Dismissing { menu_id, identity }) => Some(
                self.wait_for_dismissed_menu(
                    scope.owner.clone(),
                    scope.action_id.clone(),
                    scope.deadline.work,
                    menu_id.clone(),
                    identity.clone(),
                )
                .await?,
            ),
            None => None,
        };
        match &menu {
            Some(NativeMenuEvidence::Opened { .. }) => {
                target.signals.record(SettlementSignal::MenuOpened);
            }
            Some(NativeMenuEvidence::Dismissed { .. }) => {
                target.signals.record(SettlementSignal::MenuDismissed);
            }
            Some(NativeMenuEvidence::Targeted { .. }) | None => {}
        }
        focus_steal::record_synthesized_action(sequence.target.pid);
        scope.logical_cursor.update(final_cursor);
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
            .insert("pointer_route".to_owned(), route_name.into());
        native.fields.insert(
            "pointer_event_count".to_owned(),
            Value::from(sequence.events.len()),
        );
        native.fields.insert(
            "pointer_transport".to_owned(),
            "appkit_event_core_graphics_post_to_pid_once".into(),
        );
        native.fields.insert(
            "surface_id".to_owned(),
            sequence.target.surface_id.clone().into(),
        );
        native.fields.insert(
            "capture_revision".to_owned(),
            sequence.target.capture_revision.clone().into(),
        );
        native.fields.insert(
            "geometry_revision".to_owned(),
            sequence.target.geometry_revision.clone().into(),
        );
        native.fields.insert(
            "observation_epoch".to_owned(),
            sequence.target.observation_epoch.into(),
        );
        append_sequence_evidence(&sequence, &mut native);
        Ok(NativeDispatch {
            verification,
            evidence: native,
            warnings: Vec::new(),
            menu,
        })
    }

    async fn wait_for_opened_menu(
        &self,
        owner: ResolvedWindowStamp,
        action_id: cua_driver_core::api::contracts::ActionId,
        deadline: Instant,
        menu_id: cua_driver_core::api::contracts::MenuId,
    ) -> Result<NativeMenuEvidence, NativeError> {
        let parent = self.windows.facts_for_stamp(&owner).await?;
        loop {
            let pid = parent.pid;
            let window_id = parent.cg_window_id;
            let discovered =
                tokio::task::spawn_blocking(move || discover_native_menu(pid, window_id))
                    .await
                    .map_err(|error| {
                        NativeError::new(
                            ErrorCode::MenuStateStale,
                            ErrorPhase::Dispatch,
                            false,
                            format!("native menu discovery task failed: {error}"),
                        )
                    })??;
            if let Some((menu_window_id, menu_element)) = discovered {
                let identity =
                    resolve_menu_identity(&self.windows, &parent, menu_window_id, &menu_element)
                        .await?;
                return Ok(NativeMenuEvidence::Opened {
                    menu_id,
                    opened_by_action_id: action_id,
                    owner,
                    identity,
                    surface_ids: Vec::new(),
                    focused_item: None,
                });
            }
            if Instant::now() >= deadline {
                return Err(NativeError::stale(
                    ErrorCode::MenuStateStale,
                    "targeted right click posted but no exact native menu identity arrived before the action deadline",
                )
                .with_detail("native_side_effect_started", true));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn menu_outcome_after_target(
        &self,
        owner: ResolvedWindowStamp,
        action_id: cua_driver_core::api::contracts::ActionId,
        menu_id: cua_driver_core::api::contracts::MenuId,
        prior_identity: NativeMenuIdentity,
    ) -> Result<NativeMenuEvidence, NativeError> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(identity) = self.discover_menu_identity(owner.clone()).await? {
            return Ok(NativeMenuEvidence::Targeted {
                menu_id,
                action_id,
                owner,
                identity,
            });
        }
        Ok(NativeMenuEvidence::Dismissed {
            menu_id,
            action_id,
            owner,
            identity: prior_identity,
        })
    }

    async fn wait_for_dismissed_menu(
        &self,
        owner: ResolvedWindowStamp,
        action_id: cua_driver_core::api::contracts::ActionId,
        deadline: Instant,
        menu_id: cua_driver_core::api::contracts::MenuId,
        prior_identity: NativeMenuIdentity,
    ) -> Result<NativeMenuEvidence, NativeError> {
        loop {
            if self.discover_menu_identity(owner.clone()).await?.is_none() {
                return Ok(NativeMenuEvidence::Dismissed {
                    menu_id,
                    action_id,
                    owner,
                    identity: prior_identity,
                });
            }
            if Instant::now() >= deadline {
                return Err(NativeError::stale(
                    ErrorCode::MenuStateStale,
                    "parent-surface click posted but the exact native menu remained open through the action deadline",
                )
                .with_detail("native_side_effect_started", true));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn discover_menu_identity(
        &self,
        owner: ResolvedWindowStamp,
    ) -> Result<Option<NativeMenuIdentity>, NativeError> {
        let parent = self.windows.facts_for_stamp(&owner).await?;
        let pid = parent.pid;
        let window_id = parent.cg_window_id;
        let discovered = tokio::task::spawn_blocking(move || discover_native_menu(pid, window_id))
            .await
            .map_err(|error| {
                NativeError::new(
                    ErrorCode::MenuStateStale,
                    ErrorPhase::Dispatch,
                    false,
                    format!("native menu discovery task failed: {error}"),
                )
            })??;
        let Some((menu_window_id, menu_element)) = discovered else {
            return Ok(None);
        };
        Ok(Some(
            resolve_menu_identity(&self.windows, &parent, menu_window_id, &menu_element).await?,
        ))
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
            let event_point = event.point();
            let expected_screen = Point {
                x: bounds.x + event_point.window_local.x,
                y: bounds.y + event_point.window_local.y,
            };
            if event_point.pid != pid
                || event_point.cg_window_id != cg_window_id
                || event_point.owner != stamp
                || event_point.owner_bounds != bounds
                || !same_point(expected_screen, event_point.screen)
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

fn append_sequence_evidence(sequence: &PreparedSequence, evidence: &mut NativeEvidence) {
    evidence.fields.insert(
        "pointer_screen_x".to_owned(),
        sequence.target.screen.x.into(),
    );
    evidence.fields.insert(
        "pointer_screen_y".to_owned(),
        sequence.target.screen.y.into(),
    );
    evidence.fields.insert(
        "pointer_window_x".to_owned(),
        sequence.target.window_local.x.into(),
    );
    evidence.fields.insert(
        "pointer_window_y".to_owned(),
        sequence.target.window_local.y.into(),
    );
    let event_kinds: Vec<Value> = sequence
        .events
        .iter()
        .map(|event| match event {
            PreparedPointerEvent::Mouse(mouse) => Value::from(match mouse.kind {
                PreparedMouseEventKind::Down => "mouse_down",
                PreparedMouseEventKind::Dragged => "mouse_dragged",
                PreparedMouseEventKind::Up => "mouse_up",
            }),
            PreparedPointerEvent::PixelScroll(_) => Value::from("pixel_scroll"),
        })
        .collect();
    evidence
        .fields
        .insert("pointer_event_kinds".to_owned(), Value::Array(event_kinds));
    evidence.fields.insert(
        "pointer_native_post_count".to_owned(),
        sequence.events.len().into(),
    );
    if let Some(PreparedPointerEvent::Mouse(mouse)) = sequence.events.first() {
        evidence.fields.insert(
            "pointer_button".to_owned(),
            Value::from(match mouse.button {
                MouseButton::Left => "left",
                MouseButton::Right => "right",
                MouseButton::Middle => "middle",
            }),
        );
        let click_count = sequence
            .events
            .iter()
            .filter_map(|event| match event {
                PreparedPointerEvent::Mouse(mouse)
                    if mouse.kind == PreparedMouseEventKind::Down =>
                {
                    Some(mouse.click_state)
                }
                _ => None,
            })
            .max()
            .unwrap_or_default();
        evidence
            .fields
            .insert("pointer_click_count".to_owned(), click_count.into());
    }
    if let Some(PreparedPointerEvent::PixelScroll(scroll)) = sequence.events.first() {
        evidence
            .fields
            .insert("scroll_requested_delta_x".to_owned(), scroll.delta_x.into());
        evidence
            .fields
            .insert("scroll_requested_delta_y".to_owned(), scroll.delta_y.into());
        let posted_delta_x = -scroll.delta_x;
        let posted_delta_y = -scroll.delta_y;
        evidence
            .fields
            .insert("scroll_posted_delta_x".to_owned(), posted_delta_x.into());
        evidence
            .fields
            .insert("scroll_posted_delta_y".to_owned(), posted_delta_y.into());
        evidence
            .fields
            .insert("scroll_units".to_owned(), "pixel".into());
        evidence.fields.insert(
            "scroll_native_primitive".to_owned(),
            "CGEventCreateScrollWheelEvent".into(),
        );
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
enum PreparedMouseEventKind {
    Down,
    Dragged,
    Up,
}

#[derive(Debug, Clone)]
enum PreparedPointerEvent {
    Mouse(PreparedMouseEvent),
    PixelScroll(PreparedPixelScrollEvent),
}

impl PreparedPointerEvent {
    fn point(&self) -> &NativePoint {
        match self {
            Self::Mouse(event) => &event.point,
            Self::PixelScroll(event) => &event.point,
        }
    }

    fn scheduled_after(&self) -> Duration {
        match self {
            Self::Mouse(event) => event.scheduled_after,
            Self::PixelScroll(event) => event.scheduled_after,
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedMouseEvent {
    kind: PreparedMouseEventKind,
    point: NativePoint,
    button: MouseButton,
    click_state: u8,
    modifiers: Vec<Modifier>,
    scheduled_after: Duration,
}

#[derive(Debug, Clone)]
struct PreparedPixelScrollEvent {
    point: NativePoint,
    delta_x: f64,
    delta_y: f64,
    scheduled_after: Duration,
}

fn build_click(
    point: NativePoint,
    button: MouseButton,
    click_count: u8,
    modifiers: &[Modifier],
) -> PreparedSequence {
    let mut events = Vec::with_capacity(usize::from(click_count) * 2);
    for pair in 1..=click_count {
        let down_index = u64::from(pair - 1) * 2;
        events.push(mouse_event(
            PreparedMouseEventKind::Down,
            point.clone(),
            button,
            pair,
            modifiers,
            down_index * CLICK_UP_DELAY_MS,
        ));
        events.push(mouse_event(
            PreparedMouseEventKind::Up,
            point.clone(),
            button,
            pair,
            modifiers,
            (down_index + 1) * CLICK_UP_DELAY_MS,
        ));
    }
    PreparedSequence {
        target: point,
        events,
    }
}

fn build_drag(
    start: NativePoint,
    end: NativePoint,
    button: MouseButton,
    modifiers: &[Modifier],
    duration_ms: u32,
) -> PreparedSequence {
    let midpoint = interpolate_native_point(&start, &end, 0.5);
    let duration_ms = u64::from(duration_ms);
    let events = vec![
        mouse_event(
            PreparedMouseEventKind::Down,
            start.clone(),
            button,
            1,
            modifiers,
            0,
        ),
        mouse_event(
            PreparedMouseEventKind::Dragged,
            start,
            button,
            0,
            modifiers,
            0,
        ),
        mouse_event(
            PreparedMouseEventKind::Dragged,
            midpoint,
            button,
            0,
            modifiers,
            duration_ms / 2,
        ),
        mouse_event(
            PreparedMouseEventKind::Dragged,
            end.clone(),
            button,
            0,
            modifiers,
            duration_ms,
        ),
        mouse_event(
            PreparedMouseEventKind::Up,
            end.clone(),
            button,
            1,
            modifiers,
            duration_ms,
        ),
    ];
    PreparedSequence {
        target: end,
        events,
    }
}

fn build_delta_scroll(point: NativePoint, delta_x: f64, delta_y: f64) -> PreparedSequence {
    PreparedSequence {
        target: point.clone(),
        events: vec![PreparedPointerEvent::PixelScroll(
            PreparedPixelScrollEvent {
                point,
                delta_x,
                delta_y,
                scheduled_after: Duration::ZERO,
            },
        )],
    }
}

fn mouse_event(
    kind: PreparedMouseEventKind,
    point: NativePoint,
    button: MouseButton,
    click_state: u8,
    modifiers: &[Modifier],
    scheduled_after_ms: u64,
) -> PreparedPointerEvent {
    PreparedPointerEvent::Mouse(PreparedMouseEvent {
        kind,
        point,
        button,
        click_state,
        modifiers: modifiers.to_vec(),
        scheduled_after: Duration::from_millis(scheduled_after_ms),
    })
}

fn ensure_sequence_fits_deadline(
    sequence: &PreparedSequence,
    deadline: Instant,
) -> Result<(), NativeError> {
    let duration = sequence
        .events
        .last()
        .map_or(Duration::ZERO, PreparedPointerEvent::scheduled_after);
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

fn ensure_same_drag_surface(start: &NativePoint, end: &NativePoint) -> Result<(), NativeError> {
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
            "drag endpoints do not belong to the same captured native surface",
        ));
    }
    Ok(())
}

fn interpolate_native_point(start: &NativePoint, end: &NativePoint, amount: f64) -> NativePoint {
    let interpolate = |left: f64, right: f64| left + (right - left) * amount;
    let mut point = start.clone();
    point.screen = Point {
        x: interpolate(start.screen.x, end.screen.x),
        y: interpolate(start.screen.y, end.screen.y),
    };
    point.window_local = Point {
        x: interpolate(start.window_local.x, end.window_local.x),
        y: interpolate(start.window_local.y, end.window_local.y),
    };
    point.logical = Point {
        x: interpolate(start.logical.x, end.logical.x),
        y: interpolate(start.logical.y, end.logical.y),
    };
    point
}

fn validate_integral_scroll_delta(value: f64, field: &'static str) -> Result<(), NativeError> {
    if !value.is_finite()
        || value.fract() != 0.0
        || value <= f64::from(i32::MIN)
        || value > f64::from(i32::MAX)
    {
        return Err(NativeError::new(
            ErrorCode::UnsupportedInBackground,
            ErrorPhase::Preflight,
            false,
            "the recovered macOS pixel-scroll route requires an integral 32-bit logical delta",
        )
        .with_detail("recipe_status", "native_integer_delta_required")
        .with_detail("field", field)
        .with_detail("value", value));
    }
    Ok(())
}

fn signed_page_scroll_delta(
    bounds: cua_driver_core::api::contracts::Rect,
    direction: ScrollDirection,
    pages: f64,
) -> Result<(f64, f64), NativeError> {
    let relevant_dimension = match direction {
        ScrollDirection::Up | ScrollDirection::Down => bounds.height,
        ScrollDirection::Left | ScrollDirection::Right => bounds.width,
    };
    let magnitude = (pages * relevant_dimension.max(100.0)).round();
    validate_integral_scroll_delta(magnitude, "page_magnitude")?;
    Ok(match direction {
        ScrollDirection::Up => (0.0, -magnitude),
        ScrollDirection::Down => (0.0, magnitude),
        ScrollDirection::Left => (-magnitude, 0.0),
        ScrollDirection::Right => (magnitude, 0.0),
    })
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
            match event {
                PreparedPointerEvent::Mouse(event) => {
                    synthesized_event::post_mouse_event(&MouseEventSpec {
                        pid: event.point.pid,
                        cg_window_id: event.point.cg_window_id,
                        screen: event.point.screen,
                        window_local: event.point.window_local,
                        button: event.button,
                        click_count: event.click_state,
                        modifiers: &event.modifiers,
                        kind: match event.kind {
                            PreparedMouseEventKind::Down => NativeMouseEventKind::Down,
                            PreparedMouseEventKind::Dragged => NativeMouseEventKind::Dragged,
                            PreparedMouseEventKind::Up => NativeMouseEventKind::Up,
                        },
                    })
                }
                PreparedPointerEvent::PixelScroll(event) => {
                    synthesized_event::post_pixel_scroll_event(&PixelScrollEventSpec {
                        pid: event.point.pid,
                        cg_window_id: event.point.cg_window_id,
                        screen: event.point.screen,
                        window_local: event.point.window_local,
                        delta_x: event.delta_x,
                        delta_y: event.delta_y,
                    })
                }
            }
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
        if let Err(error) = wait_until(started + event.scheduled_after(), deadline) {
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
        if let PreparedPointerEvent::Mouse(mouse) = event {
            if mouse.kind == PreparedMouseEventKind::Down {
                // Posting APIs are void. A returned error may still follow
                // partial native delivery, so cleanup conservatively assumes
                // the button is pressed.
                pressed = Some((mouse.button, mouse.point.clone()));
            }
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
        match event {
            PreparedPointerEvent::Mouse(mouse) => match mouse.kind {
                PreparedMouseEventKind::Down | PreparedMouseEventKind::Dragged => {}
                PreparedMouseEventKind::Up => {
                    pressed = None;
                    logical_cursor.update(mouse.point.logical);
                }
            },
            PreparedPointerEvent::PixelScroll(scroll) => {
                logical_cursor.update(scroll.point.logical);
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
    let cleanup = mouse_event(PreparedMouseEventKind::Up, point, button, 1, &[], 0);
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
    fn new() -> Self {
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
            AppId, AppRef, CaptureRevision, GeometryRevision, Rect, SurfaceId, WindowGeneration,
            WindowId, WindowRef,
        },
        observation::{NativeProcessHandle, NativeWindowHandle},
    };

    use super::*;

    fn point(x: f64, y: f64) -> NativePoint {
        let app = AppRef {
            id: AppId::parse("app").unwrap(),
            canonical_id: None,
            name: None,
            pid: Some(10),
            running: true,
        };
        let public = WindowRef {
            id: WindowId::parse("window").unwrap(),
            app,
            title: None,
            usable: true,
            is_standard: Some(true),
            is_main: Some(true),
            z_index: Some(1),
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
    fn signed_page_scroll_uses_relevant_element_dimension_and_one_hundred_point_floor() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 80.0,
            height: 375.0,
        };
        assert_eq!(
            signed_page_scroll_delta(bounds, ScrollDirection::Down, 0.5).unwrap(),
            (0.0, 188.0)
        );
        assert_eq!(
            signed_page_scroll_delta(bounds, ScrollDirection::Up, 2.0).unwrap(),
            (0.0, -750.0)
        );
        assert_eq!(
            signed_page_scroll_delta(bounds, ScrollDirection::Right, 1.0).unwrap(),
            (100.0, 0.0)
        );
        assert_eq!(
            signed_page_scroll_delta(bounds, ScrollDirection::Left, 1.5).unwrap(),
            (-150.0, 0.0)
        );
    }

    fn as_mouse(event: &PreparedPointerEvent) -> &PreparedMouseEvent {
        match event {
            PreparedPointerEvent::Mouse(event) => event,
            PreparedPointerEvent::PixelScroll(_) => panic!("expected a prepared mouse event"),
        }
    }

    #[test]
    fn click_sequence_preserves_button_modifiers_and_incrementing_click_state() {
        let sequence = build_click(
            point(10.0, 20.0),
            MouseButton::Middle,
            3,
            &[Modifier::Shift],
        );
        assert_eq!(sequence.events.len(), 6);
        let events: Vec<_> = sequence.events.iter().map(as_mouse).collect();
        assert_eq!(events[0].kind, PreparedMouseEventKind::Down);
        assert_eq!(events[1].kind, PreparedMouseEventKind::Up);
        assert_eq!(events[0].scheduled_after, Duration::ZERO);
        assert_eq!(events[1].scheduled_after, Duration::from_millis(100));
        assert!(events.iter().all(|event| {
            event.button == MouseButton::Middle && event.modifiers == [Modifier::Shift]
        }));
        assert_eq!(
            events
                .chunks_exact(2)
                .map(|pair| (pair[0].click_state, pair[1].click_state))
                .collect::<Vec<_>>(),
            vec![(1, 1), (2, 2), (3, 3)]
        );
    }

    #[test]
    fn drag_sequence_contains_real_start_midpoint_and_destination_motion() {
        let sequence = build_drag(
            point(10.0, 20.0),
            point(30.0, 60.0),
            MouseButton::Right,
            &[Modifier::Alt],
            300,
        );
        let events: Vec<_> = sequence.events.iter().map(as_mouse).collect();
        assert_eq!(events.len(), 5);
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec![
                PreparedMouseEventKind::Down,
                PreparedMouseEventKind::Dragged,
                PreparedMouseEventKind::Dragged,
                PreparedMouseEventKind::Dragged,
                PreparedMouseEventKind::Up,
            ]
        );
        assert_eq!(events[2].point.screen, Point { x: 20.0, y: 40.0 });
        assert_eq!(events[2].scheduled_after, Duration::from_millis(150));
        assert_eq!(events[3].scheduled_after, Duration::from_millis(300));
        assert_eq!(events[4].scheduled_after, Duration::from_millis(300));
        assert!(events.iter().all(|event| {
            event.button == MouseButton::Right && event.modifiers == [Modifier::Alt]
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
        let sequence = build_click(point(0.0, 0.0), MouseButton::Left, 1, &[]);
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
        let cleanup = as_mouse(events.last().unwrap());
        assert_eq!(cleanup.kind, PreparedMouseEventKind::Up);
        assert!(cleanup.modifiers.is_empty());
        assert_eq!(cleanup.point.pid, sequence.target.pid);
        assert_eq!(cleanup.point.cg_window_id, sequence.target.cg_window_id);
    }

    #[test]
    fn advanced_observation_epoch_refuses_before_the_first_pointer_post() {
        let sequence = build_click(point(5.0, 6.0), MouseButton::Left, 1, &[]);
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
        let mut evidence = PointerDispatchEvidence::new();
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
        let sequence = build_click(point(10.0, 20.0), MouseButton::Left, 1, &[]);
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
        let mut evidence = PointerDispatchEvidence::new();
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
