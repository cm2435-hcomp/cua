//! Exact focused-control preparation and background-only macOS keyboard routes.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use core_graphics::event::CGEventFlags;
use cua_driver_core::api::{
    capabilities::{
        ActionKind, AddressingMode, CapabilityCell, CapabilityKey, Framework, PlatformName,
        RouteDecision, WindowStateKind,
    },
    contracts::{Modifier, Route, VerificationLevel},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{InteractionScope, LeaseDecision, NativeEvidence, NativeSideEffectBoundary},
    observation::ResolvedFocus,
    platform::{KeyboardActionProvider, NativeDispatch, ResolvedAction},
    settlement::SettlementSignal,
};
use serde_json::{json, Value};

use crate::{
    ax::bindings::{
        self, copy_cf_range_attr, copy_string_attr_exact, focused_element_of_pid,
        is_attribute_settable, kAXErrorSuccess, AxCfRange,
    },
    input::keyboard::{
        chord_events, normalize_chord, post_prepared_targeted_event, prepare_targeted_event,
        unicode_events, NormalizedChord, NormalizedModifier, TargetedKeyEvent,
        TargetedKeyEventKind, TargetedPostPrimitive,
    },
};

use super::super::{
    observation::{RegisteredElementSnapshot, RetainedAxElement},
    target::MacTargetState,
    windows::{MacWindowFacts, MacWindowRegistry},
};

const EVENT_GAP: Duration = Duration::from_millis(8);

trait KeyboardEventPoster: Send + Sync {
    fn post(
        &self,
        pid: i32,
        event: &TargetedKeyEvent,
        boundary: &mut NativeSideEffectBoundary<'_>,
    ) -> Result<TargetedPostPrimitive, NativeError>;
}

#[derive(Default)]
struct SystemKeyboardEventPoster;

impl KeyboardEventPoster for SystemKeyboardEventPoster {
    fn post(
        &self,
        pid: i32,
        event: &TargetedKeyEvent,
        boundary: &mut NativeSideEffectBoundary<'_>,
    ) -> Result<TargetedPostPrimitive, NativeError> {
        let event = prepare_targeted_event(event)?;
        boundary.begin()?;
        Ok(post_prepared_targeted_event(pid, &event))
    }
}

#[derive(Clone)]
pub struct MacKeyboardActions {
    windows: MacWindowRegistry,
    poster: Arc<dyn KeyboardEventPoster>,
    event_gap: Duration,
}

pub struct MacPreparedKeyboardAction(PreparedKind);

enum PreparedKind {
    Chord {
        pid: i32,
        chord: NormalizedChord,
        focus: PreparedFocus,
    },
    SemanticInsertion {
        focus: PreparedFocus,
        text: String,
        expected_value: String,
        expected_selection: AxCfRange,
    },
    UnicodeInsertion {
        pid: i32,
        focus: PreparedFocus,
        text: String,
        expected: Option<ExpectedInsertion>,
    },
}

struct PreparedFocus {
    element: RetainedAxElement,
    snapshot: RegisteredElementSnapshot,
    ax_revision: String,
    observation_id: String,
    signal_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedInsertion {
    value: String,
    selection: AxCfRange,
}

impl MacKeyboardActions {
    pub fn new(windows: MacWindowRegistry) -> Self {
        Self {
            windows,
            poster: Arc::new(SystemKeyboardEventPoster),
            event_gap: EVENT_GAP,
        }
    }

    pub(crate) async fn prepare_action(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<MacPreparedKeyboardAction, NativeError> {
        let facts = self.windows.facts_for_stamp(&target.window).await?;
        let prepared = match action {
            ResolvedAction::PressKey { focus, stroke } => {
                require_scope(target, scope, Route::TargetedKeyboard, true)?;
                // Repeated here after the no-lease InteractionProvider check so
                // the retained plan never depends on mutable side state.
                let chord = normalize_chord(stroke)?;
                let (focus, _) = prepare_focus(target, focus, &facts, FocusProof::IdentityOnly)?;
                PreparedKind::Chord {
                    pid: facts.pid,
                    chord,
                    focus,
                }
            }
            ResolvedAction::TypeText { focus, text } => match scope.route {
                Route::Semantic => {
                    require_scope(target, scope, Route::Semantic, false)?;
                    let (focus, snapshot) =
                        prepare_focus(target, focus, &facts, FocusProof::TextState)?;
                    if snapshot.selected_text_settable != Some(true) {
                        return Err(NativeError::unsupported(
                            "focused macOS control has no exact writable AXSelectedText insertion route",
                        ));
                    }
                    let before = snapshot.string_value.as_deref().ok_or_else(|| {
                        NativeError::unsupported(
                            "semantic text insertion requires an exact CFString AXValue snapshot",
                        )
                    })?;
                    let selection = snapshot.selected_text_range.ok_or_else(|| {
                        NativeError::unsupported(
                            "semantic text insertion requires an exact AXSelectedTextRange snapshot",
                        )
                    })?;
                    let expected = apply_insertion(before, selection, text)?;
                    PreparedKind::SemanticInsertion {
                        focus,
                        text: text.clone(),
                        expected_value: expected.value,
                        expected_selection: expected.selection,
                    }
                }
                Route::TargetedKeyboard => {
                    require_scope(target, scope, Route::TargetedKeyboard, true)?;
                    let (focus, snapshot) =
                        prepare_focus(target, focus, &facts, FocusProof::TextState)?;
                    let expected = match (
                        snapshot.string_value.as_deref(),
                        snapshot.selected_text_range,
                    ) {
                        (Some(before), Some(selection)) => {
                            Some(apply_insertion(before, selection, text)?)
                        }
                        _ => None,
                    };
                    PreparedKind::UnicodeInsertion {
                        pid: facts.pid,
                        focus,
                        text: text.clone(),
                        expected,
                    }
                }
                Route::TargetedPointer => {
                    return Err(NativeError::new(
                        ErrorCode::Internal,
                        ErrorPhase::Preflight,
                        false,
                        "text action reached the targeted-pointer route",
                    ))
                }
            },
            _ => {
                return Err(NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Preflight,
                    false,
                    "non-keyboard action reached macOS keyboard preparation",
                ))
            }
        };
        Ok(MacPreparedKeyboardAction(prepared))
    }

    pub(crate) async fn dispatch_action(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: MacPreparedKeyboardAction,
    ) -> Result<NativeDispatch, NativeError> {
        match action.0 {
            PreparedKind::Chord { pid, chord, focus } => {
                self.final_focus_validation(
                    target,
                    scope.owner.clone(),
                    scope.window.stamp(),
                    pid,
                    &focus,
                    FocusProof::IdentityOnly,
                )
                .await?;
                let evidence =
                    dispatch_chord(self.poster.as_ref(), self.event_gap, pid, &chord, boundary)
                        .map_err(|error| poison_unproved_cleanup(target, scope, error))?;
                Ok(dispatch_unverified(
                    "macos_targeted_key_chord",
                    focus,
                    evidence,
                ))
            }
            PreparedKind::UnicodeInsertion {
                pid,
                focus,
                text,
                expected,
            } => {
                self.final_focus_validation(
                    target,
                    scope.owner.clone(),
                    scope.window.stamp(),
                    pid,
                    &focus,
                    FocusProof::TextState,
                )
                .await?;
                let mut evidence = dispatch_unicode(
                    self.poster.as_ref(),
                    self.event_gap,
                    pid,
                    &text,
                    scope.deadline.work,
                    boundary,
                )
                .map_err(|error| poison_unproved_cleanup(target, scope, error))?;
                let readback = expected
                    .as_ref()
                    .map(|expected| exact_insertion_readback(focus.element.as_ptr(), expected));
                let readback_available = readback.is_some();
                let readback_matched = matches!(&readback, Some(Ok(true)));
                if let Some(Err(error)) = &readback {
                    evidence.insert(
                        "exact_readback_error".to_owned(),
                        Value::String(error.message.clone()),
                    );
                }
                evidence.insert(
                    "exact_readback_available".to_owned(),
                    readback_available.into(),
                );
                evidence.insert("exact_readback_matched".to_owned(), readback_matched.into());
                if matches!(&readback, Some(Ok(_))) {
                    target
                        .signals
                        .record(SettlementSignal::VerificationReadbackComplete);
                }
                // A same-thread AX readback is useful positive evidence when it
                // matches, but a targeted pid post has no delivery ack. Keep
                // the contract honest and let notification settlement finish.
                Ok(dispatch_unverified(
                    "macos_targeted_unicode_events",
                    focus,
                    evidence,
                ))
            }
            PreparedKind::SemanticInsertion {
                focus,
                text,
                expected_value,
                expected_selection,
            } => {
                let pid = self.windows.facts_for_stamp(&target.window).await?.pid;
                self.final_focus_validation(
                    target,
                    scope.owner.clone(),
                    scope.window.stamp(),
                    pid,
                    &focus,
                    FocusProof::TextState,
                )
                .await?;
                boundary.begin()?;
                let result = unsafe {
                    bindings::set_string_attr(focus.element.as_ptr(), "AXSelectedText", &text)
                };
                if result != kAXErrorSuccess {
                    return Err(NativeError::new(
                        ErrorCode::DispatchFailed,
                        ErrorPhase::Dispatch,
                        false,
                        "AXSelectedText insertion failed after entering the dispatch boundary",
                    )
                    .with_detail("ax_error", result)
                    .with_detail("possibly_partial_delivery", true));
                }
                let expected = ExpectedInsertion {
                    value: expected_value,
                    selection: expected_selection,
                };
                let readback = exact_insertion_readback(focus.element.as_ptr(), &expected)?;
                target
                    .signals
                    .record(SettlementSignal::VerificationReadbackComplete);
                if !readback {
                    return Err(NativeError::new(
                        ErrorCode::VerificationFailed,
                        ErrorPhase::Verify,
                        true,
                        "AXSelectedText insertion did not produce the exact value and caret readback",
                    )
                    .with_detail("possibly_partial_delivery", true));
                }
                let mut evidence = focus_evidence(&focus);
                evidence.insert(
                    "primitive".to_owned(),
                    Value::String("macos_ax_selected_text_insertion".to_owned()),
                );
                evidence.insert("clipboard_used".to_owned(), false.into());
                evidence.insert("exact_typed_readback".to_owned(), true.into());
                evidence.insert(
                    "inserted_utf16_units".to_owned(),
                    text.encode_utf16().count().into(),
                );
                Ok(NativeDispatch {
                    verification: VerificationLevel::EffectVerified,
                    evidence: NativeEvidence {
                        fields: evidence,
                        interaction_scope: None,
                    },
                    warnings: Vec::new(),
                    menu: None,
                })
            }
        }
    }

    async fn final_focus_validation(
        &self,
        target: &mut MacTargetState,
        scope_owner: cua_driver_core::api::observation::ResolvedWindowStamp,
        scope_window: cua_driver_core::api::observation::ResolvedWindowStamp,
        pid: i32,
        focus: &PreparedFocus,
        proof: FocusProof,
    ) -> Result<(), NativeError> {
        if target.invalidated() || scope_owner != target.window || scope_window != target.window {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "keyboard target window changed after action preparation",
            ));
        }
        let facts = self.windows.facts_for_stamp(&target.window).await?;
        if facts.pid != pid || facts.cg_window_id != focus.snapshot.owner_window_id {
            return Err(focus_drift(
                "keyboard pid/window identity changed before native dispatch",
            ));
        }
        if target.signals.epoch() != focus.signal_epoch {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "focus/content notification raced keyboard action preparation",
            ));
        }
        let live = unsafe { focused_element_of_pid(pid) }
            .map(|element| unsafe { RetainedAxElement::from_owned(element) })
            .ok_or_else(|| focus_drift("target process lost its exact focused AX element"))?;
        validate_focus_identity_proof(
            focus.snapshot.owner == target.window,
            focus.snapshot.owner_window_id,
            live.same_identity(&focus.element),
            unsafe { bindings::ax_get_window_id(live.as_ptr()) },
            facts.cg_window_id,
        )?;
        let live_role = unsafe { copy_string_attr_exact(live.as_ptr(), "AXRole") }
            .map_err(|error| ax_focus_error("AXRole", error))?;
        if !focus.snapshot.role_proven || live_role.as_deref() != Some(focus.snapshot.role.as_str())
        {
            return Err(focus_drift(
                "focused-control role changed before native dispatch",
            ));
        }
        if matches!(proof, FocusProof::TextState) {
            revalidate_text_state(&live, &focus.snapshot)?;
        }
        if target.signals.epoch() != focus.signal_epoch {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "focus/content notification raced final keyboard validation",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl KeyboardActionProvider<MacTargetState> for MacKeyboardActions {
    type PreparedAction = MacPreparedKeyboardAction;

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
        self.dispatch_action(target, scope, boundary, action).await
    }
}

#[derive(Clone, Copy)]
enum FocusProof {
    IdentityOnly,
    TextState,
}

fn prepare_focus(
    target: &MacTargetState,
    focus: &ResolvedFocus,
    facts: &MacWindowFacts,
    proof: FocusProof,
) -> Result<(PreparedFocus, RegisteredElementSnapshot), NativeError> {
    let epoch = target.signals.epoch();
    let element_id = focus_element_id(focus_contract(target, focus)?)?;
    let snapshot = target
        .elements
        .registered_by_id(element_id)
        .ok_or_else(missing_focus_identity)?;
    let live = unsafe { focused_element_of_pid(facts.pid) }
        .map(|element| unsafe { RetainedAxElement::from_owned(element) })
        .ok_or_else(|| focus_drift("target process has no exact focused AX element"))?;
    validate_focus_identity_proof(
        snapshot.owner == target.window,
        snapshot.owner_window_id,
        live.same_identity(&snapshot.element),
        unsafe { bindings::ax_get_window_id(live.as_ptr()) },
        facts.cg_window_id,
    )?;
    let live_role = unsafe { copy_string_attr_exact(live.as_ptr(), "AXRole") }
        .map_err(|error| ax_focus_error("AXRole", error))?;
    if !snapshot.role_proven || live_role.as_deref() != Some(snapshot.role.as_str()) {
        return Err(focus_drift(
            "live focused-control role changed since the supplied observation",
        ));
    }
    if matches!(proof, FocusProof::TextState) {
        revalidate_text_state(&live, &snapshot)?;
    }
    if target.signals.epoch() != epoch {
        return Err(NativeError::stale(
            ErrorCode::ObservationRaced,
            "target focus/content notification raced keyboard preparation",
        ));
    }
    Ok((
        PreparedFocus {
            element: live,
            snapshot: snapshot.clone(),
            ax_revision: focus
                .ax_revision
                .as_ref()
                .expect("focus contract requires AX revision")
                .as_str()
                .to_owned(),
            observation_id: focus.observation_id.to_string(),
            signal_epoch: epoch,
        },
        snapshot,
    ))
}

fn validate_focus_identity_proof(
    observed_owner_matches: bool,
    observed_window_id: u32,
    live_identity_matches: bool,
    live_window_id: Option<u32>,
    target_window_id: u32,
) -> Result<(), NativeError> {
    if !observed_owner_matches || observed_window_id != target_window_id {
        return Err(focus_drift(
            "observed focused control no longer belongs to the exact target window",
        ));
    }
    if !live_identity_matches || live_window_id != Some(target_window_id) {
        return Err(focus_drift(
            "live AX focus identity changed since the supplied observation",
        ));
    }
    Ok(())
}

fn revalidate_text_state(
    live: &RetainedAxElement,
    snapshot: &RegisteredElementSnapshot,
) -> Result<(), NativeError> {
    if snapshot.value_query_proven {
        let live_value = unsafe { copy_string_attr_exact(live.as_ptr(), "AXValue") }
            .map_err(|error| ax_focus_error("AXValue", error))?;
        if live_value != snapshot.string_value {
            return Err(focus_drift(
                "focused-control value changed since the supplied observation",
            ));
        }
    }
    if let Some(expected_range) = snapshot.selected_text_range {
        let live_range = unsafe { copy_cf_range_attr(live.as_ptr(), "AXSelectedTextRange") }
            .map_err(|error| ax_focus_error("AXSelectedTextRange", error))?;
        if live_range != Some(expected_range) {
            return Err(focus_drift(
                "focused-control selection changed since the supplied observation",
            ));
        }
    }
    if let Some(expected_settable) = snapshot.selected_text_settable {
        let live_settable = unsafe { is_attribute_settable(live.as_ptr(), "AXSelectedText") }
            .map_err(|error| ax_focus_error("AXSelectedText settable", error))?;
        if live_settable != expected_settable {
            return Err(focus_drift(
                "focused-control insertion capability changed since observation",
            ));
        }
    }
    Ok(())
}

fn focus_contract<'a>(
    target: &MacTargetState,
    focus: &'a ResolvedFocus,
) -> Result<&'a ResolvedFocus, NativeError> {
    if target.invalidated() || focus.window.stamp() != target.window {
        return Err(focus_drift(
            "resolved focus target no longer matches the locked macOS target",
        ));
    }
    if focus.ax_revision.is_none() {
        return Err(focus_drift(
            "keyboard action requires an exact observed AX focus revision",
        ));
    }
    Ok(focus)
}

fn focus_element_id(
    focus: &ResolvedFocus,
) -> Result<&cua_driver_core::api::contracts::ElementId, NativeError> {
    focus.focused_element.as_ref().ok_or_else(|| {
        NativeError::unsupported(
            "keyboard action requires an exact focused element in the current observation",
        )
    })
}

fn require_scope(
    target: &MacTargetState,
    scope: &InteractionScope,
    route: Route,
    target_belief: bool,
) -> Result<(), NativeError> {
    let belief_ok = if target_belief {
        scope.leases.target_belief == LeaseDecision::Acquired
    } else {
        scope.leases.target_belief == LeaseDecision::NotApplicable
    };
    if scope.route != route
        || scope.owner != target.window
        || scope.window.stamp() != target.window
        || !belief_ok
    {
        return Err(NativeError::new(
            ErrorCode::Internal,
            ErrorPhase::Preflight,
            false,
            "keyboard action entered without its exact locked interaction scope",
        ));
    }
    Ok(())
}

/// Deliberately contains no await point. Once the first modifier-down is
/// posted, controller cancellation cannot drop this future before every
/// modifier-up attempt has completed.
fn dispatch_chord(
    poster: &dyn KeyboardEventPoster,
    event_gap: Duration,
    pid: i32,
    chord: &NormalizedChord,
    boundary: &mut NativeSideEffectBoundary<'_>,
) -> Result<BTreeMap<String, Value>, NativeError> {
    let events = chord_events(chord);
    let first_release = chord.modifiers.len() + 2;
    let mut attempted = 0_usize;
    let mut delivered = 0_usize;
    let mut possibly_down = Vec::new();
    let mut main_key_possibly_down = false;
    let mut primitives = BTreeSet::new();

    for event in &events[..first_release] {
        attempted += 1;
        if let TargetedKeyEventKind::ModifierDown(modifier) = event.kind {
            possibly_down.push(normalized_modifier_for(chord, modifier));
        }
        if event.kind == TargetedKeyEventKind::KeyDown {
            // A void post can return an error after native delivery, so key
            // cleanup ownership starts before the down attempt.
            main_key_possibly_down = true;
        }
        match poster.post(pid, event, boundary) {
            Ok(primitive) => {
                delivered += 1;
                primitives.insert(primitive_name(primitive));
                if event.kind == TargetedKeyEventKind::KeyUp {
                    main_key_possibly_down = false;
                }
                wait_gap_blocking(event_gap);
            }
            Err(error) => {
                let main_cleanup = release_main_key(
                    poster,
                    event_gap,
                    pid,
                    chord.key_code,
                    event.flags,
                    main_key_possibly_down,
                    boundary,
                );
                let cleanup = release_modifiers(
                    poster,
                    event_gap,
                    pid,
                    &possibly_down,
                    event.flags,
                    boundary,
                );
                attempted += usize::from(main_cleanup.attempted) + cleanup.attempted.len();
                delivered += usize::from(main_cleanup.succeeded) + cleanup.succeeded.len();
                return Err(chord_failure(
                    error,
                    attempted,
                    delivered,
                    main_cleanup,
                    cleanup,
                ));
            }
        }
    }

    let cleanup = release_modifiers(
        poster,
        event_gap,
        pid,
        &possibly_down,
        events
            .get(first_release.saturating_sub(1))
            .map_or(CGEventFlags::CGEventFlagNull, |event| event.flags),
        boundary,
    );
    attempted += cleanup.attempted.len();
    delivered += cleanup.succeeded.len();
    primitives.extend(cleanup.primitives.iter().copied());
    if !cleanup.failures.is_empty() {
        let error = NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            false,
            "one or more modifier-up posts failed",
        );
        return Err(chord_failure(
            error,
            attempted,
            delivered,
            MainKeyCleanup::default(),
            cleanup,
        ));
    }

    Ok(BTreeMap::from([
        ("event_count_attempted".to_owned(), attempted.into()),
        ("event_count_posted".to_owned(), delivered.into()),
        (
            "modifier_releases_attempted".to_owned(),
            json!(modifier_names(&possibly_down)),
        ),
        ("main_key_cleanup_attempted".to_owned(), false.into()),
        (
            "post_primitives".to_owned(),
            json!(primitives.into_iter().collect::<Vec<_>>()),
        ),
        ("clipboard_used".to_owned(), false.into()),
    ]))
}

#[derive(Default)]
struct MainKeyCleanup {
    attempted: bool,
    succeeded: bool,
    failure: Option<NativeError>,
    primitive: Option<&'static str>,
}

fn release_main_key(
    poster: &dyn KeyboardEventPoster,
    event_gap: Duration,
    pid: i32,
    key_code: u16,
    flags: CGEventFlags,
    possibly_down: bool,
    boundary: &mut NativeSideEffectBoundary<'_>,
) -> MainKeyCleanup {
    if !possibly_down {
        return MainKeyCleanup::default();
    }
    let event = TargetedKeyEvent {
        kind: TargetedKeyEventKind::KeyUp,
        key_code,
        key_down: false,
        flags,
        text: None,
    };
    let result = poster.post(pid, &event, boundary);
    wait_gap_blocking(event_gap);
    match result {
        Ok(primitive) => MainKeyCleanup {
            attempted: true,
            succeeded: true,
            failure: None,
            primitive: Some(primitive_name(primitive)),
        },
        Err(error) => MainKeyCleanup {
            attempted: true,
            succeeded: false,
            failure: Some(error),
            primitive: None,
        },
    }
}

fn dispatch_unicode(
    poster: &dyn KeyboardEventPoster,
    event_gap: Duration,
    pid: i32,
    text: &str,
    deadline: Instant,
    boundary: &mut NativeSideEffectBoundary<'_>,
) -> Result<BTreeMap<String, Value>, NativeError> {
    let events = unicode_events(text);
    let mut primitives = BTreeSet::new();
    let mut attempted = 0_usize;
    let mut posted = 0_usize;
    let mut completed_pairs = 0_usize;
    for pair in events.chunks_exact(2) {
        if Instant::now() >= deadline {
            return Err(NativeError::new(
                ErrorCode::DispatchFailed,
                ErrorPhase::Dispatch,
                false,
                "targeted Unicode sequence reached its controller deadline between complete key pairs",
            )
            .with_detail("deadline_stage", "unicode_pair_boundary")
            .with_detail("completed_pairs", completed_pairs)
            .with_detail("event_count_attempted", attempted)
            .with_detail("event_count_posted", posted));
        }
        let down = &pair[0];
        let up = &pair[1];
        attempted += 1;
        let down_result = poster.post(pid, down, boundary);
        if let Ok(primitive) = down_result.as_ref() {
            posted += 1;
            primitives.insert(primitive_name(*primitive));
        }
        wait_gap_blocking(event_gap);

        // The pair is one non-cancellable synchronous region. Even when the
        // down post reports failure, it may have reached the void native API,
        // so up is always attempted before control returns.
        attempted += 1;
        let up_result = poster.post(pid, up, boundary);
        if let Ok(primitive) = up_result.as_ref() {
            posted += 1;
            primitives.insert(primitive_name(*primitive));
        }
        wait_gap_blocking(event_gap);

        if down_result.is_err() || up_result.is_err() {
            let cleanup_unproved = up_result.is_err();
            let mut failure = NativeError::new(
                ErrorCode::DispatchFailed,
                ErrorPhase::Dispatch,
                false,
                "targeted Unicode pair failed after possible partial delivery; key-up was attempted",
            )
            .with_detail("possibly_partial_delivery", boundary.started())
            .with_detail("event_count_attempted", attempted)
            .with_detail("event_count_posted", posted)
            .with_detail("unicode_key_up_attempted", true)
            .with_detail("unicode_key_up_succeeded", up_result.is_ok());
            if let Err(error) = down_result {
                failure = failure.with_related(&error);
            }
            if let Err(error) = up_result {
                failure = failure.with_related(&error);
            }
            if cleanup_unproved {
                failure = failure.with_target_invalidated();
            }
            return Err(failure);
        }
        completed_pairs += 1;
    }
    Ok(BTreeMap::from([
        ("event_count_attempted".to_owned(), attempted.into()),
        ("event_count_posted".to_owned(), posted.into()),
        ("completed_pairs".to_owned(), completed_pairs.into()),
        (
            "post_primitives".to_owned(),
            json!(primitives.into_iter().collect::<Vec<_>>()),
        ),
        ("clipboard_used".to_owned(), false.into()),
    ]))
}

#[derive(Default)]
struct ModifierCleanup {
    attempted: Vec<Modifier>,
    succeeded: Vec<Modifier>,
    failures: Vec<NativeError>,
    primitives: BTreeSet<&'static str>,
}

fn release_modifiers(
    poster: &dyn KeyboardEventPoster,
    event_gap: Duration,
    pid: i32,
    modifiers: &[NormalizedModifier],
    mut flags: core_graphics::event::CGEventFlags,
    boundary: &mut NativeSideEffectBoundary<'_>,
) -> ModifierCleanup {
    let mut cleanup = ModifierCleanup::default();
    for modifier in modifiers.iter().rev() {
        flags.remove(modifier.flag);
        let event = TargetedKeyEvent {
            kind: TargetedKeyEventKind::ModifierUp(modifier.modifier),
            key_code: modifier.key_code,
            key_down: false,
            flags,
            text: None,
        };
        cleanup.attempted.push(modifier.modifier);
        match poster.post(pid, &event, boundary) {
            Ok(primitive) => {
                cleanup.succeeded.push(modifier.modifier);
                cleanup.primitives.insert(primitive_name(primitive));
            }
            Err(error) => cleanup.failures.push(error),
        }
        wait_gap_blocking(event_gap);
    }
    cleanup
}

fn chord_failure(
    error: NativeError,
    attempted: usize,
    delivered: usize,
    main_cleanup: MainKeyCleanup,
    cleanup: ModifierCleanup,
) -> NativeError {
    let mut failure = NativeError::new(
        ErrorCode::DispatchFailed,
        ErrorPhase::Dispatch,
        false,
        "targeted key chord failed after possible partial delivery; every possible modifier release was attempted",
    )
    .with_detail("possibly_partial_delivery", delivered > 0)
    .with_detail("event_count_attempted", attempted)
    .with_detail("event_count_posted", delivered)
    .with_detail("main_key_cleanup_attempted", main_cleanup.attempted)
    .with_detail("main_key_cleanup_succeeded", main_cleanup.succeeded)
    .with_detail(
        "modifier_releases_attempted",
        json!(modifier_names_raw(&cleanup.attempted)),
    )
    .with_detail(
        "modifier_releases_succeeded",
        json!(modifier_names_raw(&cleanup.succeeded)),
    )
    .with_detail("modifier_release_failures", cleanup.failures.len())
    .with_related(&error);
    let cleanup_unproved =
        (main_cleanup.attempted && !main_cleanup.succeeded) || !cleanup.failures.is_empty();
    if let Some(primitive) = main_cleanup.primitive {
        failure = failure.with_detail("main_key_cleanup_primitive", primitive);
    }
    if let Some(main_error) = main_cleanup.failure {
        failure = failure.with_related(&main_error);
    }
    for release_error in cleanup.failures {
        failure = failure.with_related(&release_error);
    }
    if cleanup_unproved {
        failure = failure.with_target_invalidated();
    }
    failure
}

fn poison_unproved_cleanup(
    target: &mut MacTargetState,
    scope: &InteractionScope,
    error: NativeError,
) -> NativeError {
    if error.target_invalidated() {
        target.invalidate();
        scope.invalidate_target();
    }
    error
}

fn apply_insertion(
    before: &str,
    selection: AxCfRange,
    inserted: &str,
) -> Result<ExpectedInsertion, NativeError> {
    let location = usize::try_from(selection.location)
        .map_err(|_| invalid_selection("selection location is negative"))?;
    let length = usize::try_from(selection.length)
        .map_err(|_| invalid_selection("selection length is negative"))?;
    let end = location
        .checked_add(length)
        .ok_or_else(|| invalid_selection("selection range overflowed"))?;
    let start_byte = byte_index_at_utf16(before, location)
        .ok_or_else(|| invalid_selection("selection start is not a UTF-16 boundary"))?;
    let end_byte = byte_index_at_utf16(before, end)
        .ok_or_else(|| invalid_selection("selection end is not a UTF-16 boundary"))?;
    let mut value = String::with_capacity(before.len() + inserted.len());
    value.push_str(&before[..start_byte]);
    value.push_str(inserted);
    value.push_str(&before[end_byte..]);
    let caret = location
        .checked_add(inserted.encode_utf16().count())
        .ok_or_else(|| invalid_selection("inserted caret location overflowed"))?;
    Ok(ExpectedInsertion {
        value,
        selection: AxCfRange::from_utf16(caret, 0)
            .ok_or_else(|| invalid_selection("inserted caret exceeds CFRange"))?,
    })
}

fn byte_index_at_utf16(value: &str, target: usize) -> Option<usize> {
    if target == 0 {
        return Some(0);
    }
    let mut utf16 = 0_usize;
    for (byte, character) in value.char_indices() {
        if utf16 == target {
            return Some(byte);
        }
        utf16 = utf16.checked_add(character.len_utf16())?;
        if utf16 > target {
            return None;
        }
    }
    (utf16 == target).then_some(value.len())
}

fn exact_insertion_readback(
    element: bindings::AXUIElementRef,
    expected: &ExpectedInsertion,
) -> Result<bool, NativeError> {
    let value = unsafe { copy_string_attr_exact(element, "AXValue") }
        .map_err(|error| ax_readback_error("AXValue", error))?;
    let selection = unsafe { copy_cf_range_attr(element, "AXSelectedTextRange") }
        .map_err(|error| ax_readback_error("AXSelectedTextRange", error))?;
    Ok(value.as_deref() == Some(expected.value.as_str()) && selection == Some(expected.selection))
}

fn dispatch_unverified(
    primitive: &str,
    focus: PreparedFocus,
    mut fields: BTreeMap<String, Value>,
) -> NativeDispatch {
    fields.extend(focus_evidence(&focus));
    fields.insert("primitive".to_owned(), Value::String(primitive.to_owned()));
    NativeDispatch {
        verification: VerificationLevel::DispatchUnverified,
        evidence: NativeEvidence {
            fields,
            interaction_scope: None,
        },
        warnings: Vec::new(),
        menu: None,
    }
}

fn focus_evidence(focus: &PreparedFocus) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "focus_ax_revision".to_owned(),
            Value::String(focus.ax_revision.clone()),
        ),
        (
            "focus_observation_id".to_owned(),
            Value::String(focus.observation_id.clone()),
        ),
        ("focus_identity_revalidated".to_owned(), true.into()),
    ])
}

fn normalized_modifier_for(chord: &NormalizedChord, modifier: Modifier) -> NormalizedModifier {
    chord
        .modifiers
        .iter()
        .find(|candidate| candidate.modifier == modifier)
        .copied()
        .expect("modifier-down event came from normalized chord")
}

fn modifier_names(modifiers: &[NormalizedModifier]) -> Vec<&'static str> {
    modifiers
        .iter()
        .map(|modifier| modifier_name(modifier.modifier))
        .collect()
}

fn modifier_names_raw(modifiers: &[Modifier]) -> Vec<&'static str> {
    modifiers.iter().copied().map(modifier_name).collect()
}

fn modifier_name(modifier: Modifier) -> &'static str {
    match modifier {
        Modifier::Shift => "shift",
        Modifier::Control => "control",
        Modifier::Alt => "alt",
        Modifier::Meta => "meta",
    }
}

fn primitive_name(primitive: TargetedPostPrimitive) -> &'static str {
    match primitive {
        TargetedPostPrimitive::SkyLightAuthenticated => "skylight_authenticated_pid",
        TargetedPostPrimitive::CoreGraphicsPid => "core_graphics_pid",
    }
}

fn wait_gap_blocking(gap: Duration) {
    if !gap.is_zero() {
        std::thread::sleep(gap);
    }
}

fn missing_focus_identity() -> NativeError {
    focus_drift("observed focused element is missing from the retained AX registry")
}

fn focus_drift(message: &str) -> NativeError {
    NativeError::stale(ErrorCode::AxRevisionMismatch, message)
}

fn invalid_selection(message: &str) -> NativeError {
    NativeError::stale(ErrorCode::AxRevisionMismatch, message)
}

fn ax_focus_error(attribute: &str, error: bindings::AXError) -> NativeError {
    focus_drift(&format!(
        "exact focused-control {attribute} revalidation failed with AX error {error}"
    ))
}

fn ax_readback_error(attribute: &str, error: bindings::AXError) -> NativeError {
    NativeError::new(
        ErrorCode::VerificationFailed,
        ErrorPhase::Verify,
        true,
        format!("exact {attribute} insertion readback failed with AX error {error}"),
    )
}

pub(crate) fn keyboard_capability_cells(os_version: &str) -> Vec<CapabilityCell> {
    let frameworks = [
        Framework::Unknown,
        Framework::AppKit,
        Framework::Chromium,
        Framework::WebKit,
        Framework::Electron,
        Framework::Catalyst,
    ];
    let states = [
        WindowStateKind::Visible,
        WindowStateKind::Occluded,
        WindowStateKind::Minimized,
        WindowStateKind::OffSpace,
        WindowStateKind::Unknown,
    ];
    let mut cells = Vec::with_capacity(frameworks.len() * states.len() * 2);
    for framework in frameworks {
        for state in &states {
            let type_text = match state {
                WindowStateKind::Visible | WindowStateKind::Occluded
                    if matches!(
                        framework,
                        Framework::Unknown
                            | Framework::AppKit
                            | Framework::Chromium
                            | Framework::WebKit
                    ) =>
                {
                    RouteDecision::Supported {
                        route: Route::Semantic,
                    }
                }
                WindowStateKind::Visible | WindowStateKind::Occluded => {
                    RouteDecision::Unsupported {
                        reason: "recipe_unproven: no exact background text recipe is published for this framework before manual host QA".to_owned(),
                    }
                }
                _ => RouteDecision::Unsupported {
                    reason:
                        "macOS keyboard/text actions require an exact current-Space non-minimized window state"
                            .to_owned(),
                },
            };
            cells.push(CapabilityCell {
                key: CapabilityKey {
                    platform: PlatformName::Macos,
                    os_version: os_version.to_owned(),
                    action: ActionKind::TypeText,
                    addressing: AddressingMode::ObservedFocus,
                    framework: framework.clone(),
                    window_state: state.clone(),
                },
                decision: type_text,
            });
            cells.push(CapabilityCell {
                key: CapabilityKey {
                    platform: PlatformName::Macos,
                    os_version: os_version.to_owned(),
                    action: ActionKind::PressKey,
                    addressing: AddressingMode::ObservedFocus,
                    framework: framework.clone(),
                    window_state: state.clone(),
                },
                decision: RouteDecision::Unsupported {
                    reason: "recipe_unproven: targeted key chords are implemented but no exact target-belief recipe is published before manual host QA".to_owned(),
                },
            });
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use cua_driver_core::api::contracts::KeyStroke;

    use super::*;

    struct FailingPoster {
        calls: Mutex<Vec<TargetedKeyEventKind>>,
        fail_at: usize,
    }

    impl KeyboardEventPoster for FailingPoster {
        fn post(
            &self,
            _pid: i32,
            event: &TargetedKeyEvent,
            _boundary: &mut NativeSideEffectBoundary<'_>,
        ) -> Result<TargetedPostPrimitive, NativeError> {
            let mut calls = self.calls.lock().unwrap();
            let index = calls.len();
            calls.push(event.kind);
            if index == self.fail_at {
                Err(NativeError::new(
                    ErrorCode::DispatchFailed,
                    ErrorPhase::Dispatch,
                    false,
                    "injected event-post failure",
                ))
            } else {
                Ok(TargetedPostPrimitive::CoreGraphicsPid)
            }
        }
    }

    struct FailingSetPoster {
        calls: Mutex<Vec<TargetedKeyEventKind>>,
        fail_at: BTreeSet<usize>,
    }

    impl KeyboardEventPoster for FailingSetPoster {
        fn post(
            &self,
            _pid: i32,
            event: &TargetedKeyEvent,
            _boundary: &mut NativeSideEffectBoundary<'_>,
        ) -> Result<TargetedPostPrimitive, NativeError> {
            let mut calls = self.calls.lock().unwrap();
            let index = calls.len();
            calls.push(event.kind);
            if self.fail_at.contains(&index) {
                Err(NativeError::new(
                    ErrorCode::DispatchFailed,
                    ErrorPhase::Dispatch,
                    false,
                    "injected event-post failure",
                ))
            } else {
                Ok(TargetedPostPrimitive::CoreGraphicsPid)
            }
        }
    }

    #[test]
    fn every_partial_chord_failure_attempts_all_possible_modifier_releases() {
        let chord = normalize_chord(&KeyStroke {
            key: "k".to_owned(),
            modifiers: vec![Modifier::Control, Modifier::Alt, Modifier::Meta],
        })
        .unwrap();
        let pre_release_events = chord.modifiers.len() + 2;
        for fail_at in 0..pre_release_events {
            let poster = FailingPoster {
                calls: Mutex::new(Vec::new()),
                fail_at,
            };
            let mut observations = cua_driver_core::api::observation::ObservationStore::default();
            let mut settlement = cua_driver_core::api::settlement::SettlementState::default();
            let mut boundary = NativeSideEffectBoundary::new(
                &mut observations,
                &mut settlement,
                cua_driver_core::api::contracts::ObservationId::parse("unused-observation")
                    .unwrap(),
                cua_driver_core::api::contracts::ActionId::parse("unused-action").unwrap(),
                cua_driver_core::api::settlement::SettlementProfile::dispatch_only("test"),
            );
            let error =
                dispatch_chord(&poster, Duration::ZERO, 42, &chord, &mut boundary).unwrap_err();
            assert_eq!(error.code, ErrorCode::DispatchFailed);
            let calls = poster.calls.lock().unwrap();
            let attempted_modifiers: Vec<_> = calls[..=fail_at]
                .iter()
                .filter_map(|kind| match kind {
                    TargetedKeyEventKind::ModifierDown(modifier) => Some(*modifier),
                    _ => None,
                })
                .collect();
            let releases: Vec<_> = calls[fail_at + 1..]
                .iter()
                .filter_map(|kind| match kind {
                    TargetedKeyEventKind::ModifierUp(modifier) => Some(*modifier),
                    _ => None,
                })
                .collect();
            assert_eq!(
                releases,
                attempted_modifiers.into_iter().rev().collect::<Vec<_>>()
            );
        }

        let poster = FailingPoster {
            calls: Mutex::new(Vec::new()),
            fail_at: pre_release_events,
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
        let error = dispatch_chord(&poster, Duration::ZERO, 42, &chord, &mut boundary).unwrap_err();
        assert_eq!(error.code, ErrorCode::DispatchFailed);
        let calls = poster.calls.lock().unwrap();
        assert_eq!(calls.len(), pre_release_events + chord.modifiers.len());
        assert_eq!(
            calls
                .iter()
                .filter(|kind| matches!(kind, TargetedKeyEventKind::ModifierUp(_)))
                .count(),
            chord.modifiers.len()
        );
        assert!(error.target_invalidated());
    }

    #[test]
    fn unicode_pair_always_attempts_up_and_poison_depends_on_up_cleanup() {
        for (fail_at, poisoned) in [(0, false), (1, true)] {
            let poster = FailingPoster {
                calls: Mutex::new(Vec::new()),
                fail_at,
            };
            let mut observations = cua_driver_core::api::observation::ObservationStore::default();
            let mut settlement = cua_driver_core::api::settlement::SettlementState::default();
            let mut boundary = NativeSideEffectBoundary::new(
                &mut observations,
                &mut settlement,
                cua_driver_core::api::contracts::ObservationId::parse("unused-observation")
                    .unwrap(),
                cua_driver_core::api::contracts::ActionId::parse("unused-action").unwrap(),
                cua_driver_core::api::settlement::SettlementProfile::dispatch_only("test"),
            );
            let error = dispatch_unicode(
                &poster,
                Duration::ZERO,
                42,
                "x",
                Instant::now() + Duration::from_secs(1),
                &mut boundary,
            )
            .unwrap_err();
            assert_eq!(
                *poster.calls.lock().unwrap(),
                vec![
                    TargetedKeyEventKind::UnicodeDown,
                    TargetedKeyEventKind::UnicodeUp,
                ]
            );
            assert_eq!(error.target_invalidated(), poisoned);
        }
    }

    #[test]
    fn failed_main_key_up_is_retried_before_modifiers_and_poisoned_if_unproved() {
        let chord = normalize_chord(&KeyStroke {
            key: "k".to_owned(),
            modifiers: vec![Modifier::Control],
        })
        .unwrap();
        for (fail_at, poisoned) in [([2].as_slice(), false), ([2, 3].as_slice(), true)] {
            let poster = FailingSetPoster {
                calls: Mutex::new(Vec::new()),
                fail_at: fail_at.iter().copied().collect(),
            };
            let mut observations = cua_driver_core::api::observation::ObservationStore::default();
            let mut settlement = cua_driver_core::api::settlement::SettlementState::default();
            let mut boundary = NativeSideEffectBoundary::new(
                &mut observations,
                &mut settlement,
                cua_driver_core::api::contracts::ObservationId::parse("unused-observation")
                    .unwrap(),
                cua_driver_core::api::contracts::ActionId::parse("unused-action").unwrap(),
                cua_driver_core::api::settlement::SettlementProfile::dispatch_only("test"),
            );
            let error =
                dispatch_chord(&poster, Duration::ZERO, 42, &chord, &mut boundary).unwrap_err();
            assert_eq!(
                *poster.calls.lock().unwrap(),
                vec![
                    TargetedKeyEventKind::ModifierDown(Modifier::Control),
                    TargetedKeyEventKind::KeyDown,
                    TargetedKeyEventKind::KeyUp,
                    TargetedKeyEventKind::KeyUp,
                    TargetedKeyEventKind::ModifierUp(Modifier::Control),
                ]
            );
            assert_eq!(error.target_invalidated(), poisoned);
        }
    }

    #[test]
    fn unicode_deadline_is_checked_between_atomic_pairs() {
        let poster = FailingPoster {
            calls: Mutex::new(Vec::new()),
            fail_at: usize::MAX,
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
        let error = dispatch_unicode(
            &poster,
            Duration::from_millis(2),
            42,
            "xy",
            Instant::now() + Duration::from_millis(1),
            &mut boundary,
        )
        .unwrap_err();
        assert_eq!(error.details["completed_pairs"], 1);
        assert_eq!(
            *poster.calls.lock().unwrap(),
            vec![
                TargetedKeyEventKind::UnicodeDown,
                TargetedKeyEventKind::UnicodeUp,
            ]
        );
    }

    #[test]
    fn insertion_replaces_only_the_observed_utf16_selection() {
        let before = "ab😀cd";
        let inserted = apply_insertion(before, AxCfRange::from_utf16(2, 2).unwrap(), "Z").unwrap();
        assert_eq!(inserted.value, "abZcd");
        assert_eq!(inserted.selection, AxCfRange::from_utf16(3, 0).unwrap());

        let inserted = apply_insertion(before, AxCfRange::from_utf16(4, 0).unwrap(), "!").unwrap();
        assert_eq!(inserted.value, "ab😀!cd");
        assert_eq!(inserted.selection, AxCfRange::from_utf16(5, 0).unwrap());

        let error = apply_insertion(before, AxCfRange::from_utf16(3, 0).unwrap(), "x").unwrap_err();
        assert_eq!(error.code, ErrorCode::AxRevisionMismatch);
    }

    #[test]
    fn changed_focused_element_or_owner_window_refuses_before_dispatch() {
        let changed_element =
            validate_focus_identity_proof(true, 7, false, Some(7), 7).unwrap_err();
        assert_eq!(changed_element.code, ErrorCode::AxRevisionMismatch);
        assert_eq!(changed_element.phase, ErrorPhase::Preflight);

        let changed_owner = validate_focus_identity_proof(true, 8, true, Some(8), 7).unwrap_err();
        assert_eq!(changed_owner.code, ErrorCode::AxRevisionMismatch);
        assert_eq!(changed_owner.phase, ErrorPhase::Preflight);
    }

    #[test]
    fn capabilities_publish_only_semantic_insertion_before_keyboard_host_qa() {
        let cells = keyboard_capability_cells("fixture");
        assert!(cells
            .iter()
            .filter(|cell| matches!(cell.decision, RouteDecision::Supported { .. }))
            .all(|cell| {
                cell.key.action == ActionKind::TypeText
                    && matches!(
                        cell.key.window_state,
                        WindowStateKind::Visible | WindowStateKind::Occluded
                    )
                    && matches!(
                        cell.decision,
                        RouteDecision::Supported {
                            route: Route::Semantic
                        }
                    )
            }));
        assert!(cells
            .iter()
            .filter(|cell| cell.key.action == ActionKind::PressKey)
            .all(|cell| matches!(cell.decision, RouteDecision::Unsupported { .. })));
        assert!(cells
            .iter()
            .filter(|cell| {
                cell.key.action == ActionKind::TypeText && cell.key.framework == Framework::Unknown
            })
            .all(|cell| {
                matches!(
                    (&cell.key.window_state, &cell.decision),
                    (
                        WindowStateKind::Visible | WindowStateKind::Occluded,
                        RouteDecision::Supported {
                            route: Route::Semantic
                        }
                    ) | (
                        WindowStateKind::Minimized
                            | WindowStateKind::OffSpace
                            | WindowStateKind::Unknown,
                        RouteDecision::Unsupported { .. }
                    )
                )
            }));
    }
}
