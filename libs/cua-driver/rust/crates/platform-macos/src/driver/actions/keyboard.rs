//! Exact focused-control preparation and background-only macOS keyboard routes.

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use async_trait::async_trait;
use cua_driver_core::api::{
    capabilities::{
        ActionKind, AddressingMode, CapabilityCell, CapabilityKey, Framework, PlatformName,
        RouteDecision, WindowStateKind,
    },
    contracts::{Route, VerificationLevel},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{InteractionScope, LeaseDecision, NativeEvidence, NativeSideEffectBoundary},
    observation::ResolvedFocus,
    platform::{KeyboardActionProvider, NativeDispatch, ResolvedAction},
    settlement::SettlementSignal,
};
use serde_json::{json, Value};

use crate::{
    ax::bindings::{
        self, copy_bool_attr_exact, copy_cf_range_attr, copy_element_attr, copy_string_attr_exact,
        focused_element_of_pid, is_attribute_settable, kAXErrorSuccess, AxCfRange,
    },
    input::keyboard::{
        normalize_chord, post_prepared_targeted_event, prepare_chord_sequence,
        prepare_unicode_sequence, NormalizedChord, PreparedKeySequence, PreparedTargetedKeyEvent,
        TargetedKeyEventKind,
    },
};

use super::super::{
    observation::{RegisteredElementSnapshot, RetainedAxElement},
    target::MacTargetState,
    windows::{MacWindowFacts, MacWindowRegistry},
};

trait KeyboardEventPoster: Send + Sync {
    fn post_to_pid(&self, pid: i32, event: &PreparedTargetedKeyEvent);
}

#[derive(Default)]
struct SystemKeyboardEventPoster;

impl KeyboardEventPoster for SystemKeyboardEventPoster {
    fn post_to_pid(&self, pid: i32, event: &PreparedTargetedKeyEvent) {
        post_prepared_targeted_event(pid, event);
    }
}

#[derive(Clone)]
pub struct MacKeyboardActions {
    windows: MacWindowRegistry,
    poster: Arc<dyn KeyboardEventPoster>,
}

pub struct MacPreparedKeyboardAction(PreparedKind);

enum PreparedKind {
    Chord {
        target: MacChordTarget,
        chord: NormalizedChord,
    },
    SemanticInsertion {
        target: MacKeyboardEventTarget,
        text: String,
        expected_value: String,
        expected_selection: AxCfRange,
    },
    UnicodeInsertion {
        target: MacKeyboardEventTarget,
        text: String,
        expected: Option<ExpectedInsertion>,
    },
}

enum MacChordTarget {
    Focused(Box<MacKeyboardEventTarget>),
    Application(MacApplicationKeyboardTarget),
}

struct MacApplicationKeyboardTarget {
    application_pid: i32,
    cg_window_id: u32,
    ax_revision: String,
    observation_id: String,
    signal_epoch: u64,
}

struct MacKeyboardEventTarget {
    application_pid: i32,
    dispatch_pid: i32,
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
                let keyboard_target = if focus.focused_element.is_some() {
                    let (target, _) =
                        prepare_keyboard_target(target, focus, &facts, FocusProof::IdentityOnly)?;
                    MacChordTarget::Focused(Box::new(target))
                } else {
                    MacChordTarget::Application(prepare_application_keyboard_target(
                        target, focus, &facts,
                    )?)
                };
                PreparedKind::Chord {
                    target: keyboard_target,
                    chord,
                }
            }
            ResolvedAction::TypeText { focus, text } => match scope.route {
                Route::Semantic => {
                    require_scope(target, scope, Route::Semantic, false)?;
                    let (keyboard_target, snapshot) =
                        prepare_keyboard_target(target, focus, &facts, FocusProof::TextState)?;
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
                        target: keyboard_target,
                        text: text.clone(),
                        expected_value: expected.value,
                        expected_selection: expected.selection,
                    }
                }
                Route::TargetedKeyboard => {
                    require_scope(target, scope, Route::TargetedKeyboard, true)?;
                    let (keyboard_target, snapshot) =
                        prepare_keyboard_target(target, focus, &facts, FocusProof::TextState)?;
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
                        target: keyboard_target,
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
            PreparedKind::Chord {
                target: keyboard_target,
                chord,
            } => match keyboard_target {
                MacChordTarget::Focused(keyboard_target) => {
                    self.final_focus_validation(
                        target,
                        scope.owner.clone(),
                        scope.window.stamp(),
                        &keyboard_target,
                        FocusProof::IdentityOnly,
                    )
                    .await?;
                    let sequence = prepare_chord_sequence(&chord)
                        .map_err(|error| sequence_construction_error(error, boundary.started()))?;
                    let focus_write = focus_field_if_needed(&keyboard_target, boundary)?;
                    let evidence = dispatch_sequence(
                        self.poster.as_ref(),
                        keyboard_target.application_pid,
                        keyboard_target.dispatch_pid,
                        &sequence,
                        boundary,
                        focus_write,
                    )?;
                    Ok(dispatch_unverified(
                        "macos_targeted_key_chord",
                        *keyboard_target,
                        evidence,
                    ))
                }
                MacChordTarget::Application(keyboard_target) => {
                    self.final_application_validation(
                        target,
                        scope.owner.clone(),
                        scope.window.stamp(),
                        &keyboard_target,
                    )
                    .await?;
                    let sequence = prepare_chord_sequence(&chord)
                        .map_err(|error| sequence_construction_error(error, boundary.started()))?;
                    let evidence = dispatch_sequence(
                        self.poster.as_ref(),
                        keyboard_target.application_pid,
                        keyboard_target.application_pid,
                        &sequence,
                        boundary,
                        false,
                    )?;
                    Ok(dispatch_unverified_application(
                        "macos_targeted_application_key_chord",
                        keyboard_target,
                        evidence,
                    ))
                }
            },
            PreparedKind::UnicodeInsertion {
                target: keyboard_target,
                text,
                expected,
            } => {
                self.final_focus_validation(
                    target,
                    scope.owner.clone(),
                    scope.window.stamp(),
                    &keyboard_target,
                    FocusProof::TextState,
                )
                .await?;
                let focus_write = focus_field_if_needed(&keyboard_target, boundary)?;
                if Instant::now() >= scope.deadline.work {
                    return Err(NativeError::new(
                        ErrorCode::DispatchFailed,
                        if boundary.started() {
                            ErrorPhase::Dispatch
                        } else {
                            ErrorPhase::Preflight
                        },
                        true,
                        "targeted Unicode sequence reached its controller deadline before construction",
                    )
                    .with_detail("possibly_partial_delivery", boundary.started())
                    .with_detail("field_focus_write_attempted", focus_write));
                }
                let sequence = prepare_unicode_sequence(&text)
                    .map_err(|error| sequence_construction_error(error, boundary.started()))?;
                let mut evidence = dispatch_sequence(
                    self.poster.as_ref(),
                    keyboard_target.application_pid,
                    keyboard_target.dispatch_pid,
                    &sequence,
                    boundary,
                    focus_write,
                )?;
                let readback = expected.as_ref().map(|expected| {
                    exact_insertion_readback(keyboard_target.element.as_ptr(), expected)
                });
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
                // matches. A mismatch or unavailable readback is not a
                // negative delivery acknowledgement for an asynchronous web
                // control, so retain it as evidence and keep the signed
                // helper's void action semantics. The next observation owns
                // effect confirmation.
                Ok(dispatch_unverified(
                    "macos_targeted_unicode_events",
                    keyboard_target,
                    evidence,
                ))
            }
            PreparedKind::SemanticInsertion {
                target: keyboard_target,
                text,
                expected_value,
                expected_selection,
            } => {
                self.final_focus_validation(
                    target,
                    scope.owner.clone(),
                    scope.window.stamp(),
                    &keyboard_target,
                    FocusProof::TextState,
                )
                .await?;
                boundary.begin()?;
                let result = unsafe {
                    bindings::set_string_attr(
                        keyboard_target.element.as_ptr(),
                        "AXSelectedText",
                        &text,
                    )
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
                let readback =
                    exact_insertion_readback(keyboard_target.element.as_ptr(), &expected)?;
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
                let mut evidence = focus_evidence(&keyboard_target);
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
        keyboard_target: &MacKeyboardEventTarget,
        proof: FocusProof,
    ) -> Result<(), NativeError> {
        if target.invalidated() || scope_owner != target.window || scope_window != target.window {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "keyboard target window changed after action preparation",
            ));
        }
        let facts = self.windows.facts_for_stamp(&target.window).await?;
        if facts.pid != keyboard_target.application_pid
            || facts.cg_window_id != keyboard_target.snapshot.owner_window_id
        {
            return Err(focus_drift(
                "keyboard pid/window identity changed before native dispatch",
            ));
        }
        if target.signals.epoch() != keyboard_target.signal_epoch {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "focus/content notification raced keyboard action preparation",
            ));
        }
        let live = unsafe { focused_element_of_pid(keyboard_target.application_pid) }
            .map(|element| unsafe { RetainedAxElement::from_owned(element) })
            .ok_or_else(|| focus_drift("target process lost its exact focused AX element"))?;
        validate_focus_identity_proof(
            keyboard_target.snapshot.owner == target.window,
            keyboard_target.snapshot.owner_window_id,
            live.same_identity(&keyboard_target.snapshot.element),
            unsafe { bindings::ax_get_window_id(live.as_ptr()) },
            facts.cg_window_id,
        )?;
        let live_role = unsafe { copy_string_attr_exact(live.as_ptr(), "AXRole") }
            .map_err(|error| ax_focus_error("AXRole", error))?;
        if !keyboard_target.snapshot.role_proven
            || live_role.as_deref() != Some(keyboard_target.snapshot.role.as_str())
        {
            return Err(focus_drift(
                "focused-control role changed before native dispatch",
            ));
        }
        let (live_effective_element, live_dispatch_pid) =
            effective_keyboard_element(keyboard_target.application_pid, &live)?;
        if live_dispatch_pid != keyboard_target.dispatch_pid
            || !live_effective_element.same_identity(&keyboard_target.element)
        {
            return Err(focus_drift(
                "effective keyboard event target changed before native dispatch",
            ));
        }
        if matches!(proof, FocusProof::TextState) {
            revalidate_text_state(&live, &keyboard_target.snapshot)?;
        }
        if target.signals.epoch() != keyboard_target.signal_epoch {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "focus/content notification raced final keyboard validation",
            ));
        }
        Ok(())
    }

    async fn final_application_validation(
        &self,
        target: &mut MacTargetState,
        scope_owner: cua_driver_core::api::observation::ResolvedWindowStamp,
        scope_window: cua_driver_core::api::observation::ResolvedWindowStamp,
        keyboard_target: &MacApplicationKeyboardTarget,
    ) -> Result<(), NativeError> {
        if target.invalidated() || scope_owner != target.window || scope_window != target.window {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "application keyboard target window changed after action preparation",
            ));
        }
        let facts = self.windows.facts_for_stamp(&target.window).await?;
        if facts.pid != keyboard_target.application_pid
            || facts.cg_window_id != keyboard_target.cg_window_id
        {
            return Err(focus_drift(
                "application keyboard pid/window identity changed before native dispatch",
            ));
        }
        if target.signals.epoch() != keyboard_target.signal_epoch {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "focus/content notification raced application keyboard action preparation",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl KeyboardActionProvider<MacTargetState> for MacKeyboardActions {
    type PreparedAction = MacPreparedKeyboardAction;

    async fn semantic_type_text_candidate(
        &self,
        target: &mut MacTargetState,
        focus: &ResolvedFocus,
    ) -> Result<cua_driver_core::api::platform::Candidate<()>, NativeError> {
        let facts = self.windows.facts_for_stamp(&target.window).await?;
        let (_, snapshot) = prepare_keyboard_target(target, focus, &facts, FocusProof::TextState)?;
        let exact_insertion_available =
            snapshot.string_value.is_some() && snapshot.selected_text_range.is_some();
        Ok(
            if should_use_exact_focused_insertion(
                &focus.window.framework,
                snapshot.selected_text_settable,
                exact_insertion_available,
            ) {
                cua_driver_core::api::platform::Candidate::Prepared(())
            } else {
                cua_driver_core::api::platform::Candidate::not_applicable(
                    "focused control has no verified exact AXSelectedText insertion route",
                )
            },
        )
    }

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

fn prepare_keyboard_target(
    target: &MacTargetState,
    focus: &ResolvedFocus,
    facts: &MacWindowFacts,
    proof: FocusProof,
) -> Result<(MacKeyboardEventTarget, RegisteredElementSnapshot), NativeError> {
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
    let (effective_element, dispatch_pid) = effective_keyboard_element(facts.pid, &live)?;
    Ok((
        MacKeyboardEventTarget {
            application_pid: facts.pid,
            dispatch_pid,
            element: effective_element,
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

fn prepare_application_keyboard_target(
    target: &MacTargetState,
    focus: &ResolvedFocus,
    facts: &MacWindowFacts,
) -> Result<MacApplicationKeyboardTarget, NativeError> {
    let focus = focus_contract(target, focus)?;
    if focus.focused_element.is_some() {
        return Err(NativeError::new(
            ErrorCode::Internal,
            ErrorPhase::Preflight,
            false,
            "application keyboard target was prepared despite an exact focused element",
        ));
    }
    Ok(MacApplicationKeyboardTarget {
        application_pid: facts.pid,
        cg_window_id: facts.cg_window_id,
        ax_revision: focus
            .ax_revision
            .as_ref()
            .expect("focus contract requires AX revision")
            .as_str()
            .to_owned(),
        observation_id: focus.observation_id.to_string(),
        signal_epoch: target.signals.epoch(),
    })
}

fn effective_keyboard_element(
    application_pid: i32,
    focused: &RetainedAxElement,
) -> Result<(RetainedAxElement, i32), NativeError> {
    let focused_pid = unsafe { bindings::element_pid(focused.as_ptr()) }
        .map_err(|error| effective_pid_error(application_pid, error))?;
    if focused_pid != application_pid {
        return Ok((focused.clone(), focused_pid));
    }

    let mut current = focused.clone();
    for _ in 0..64 {
        let Some(parent) = (unsafe { copy_element_attr(current.as_ptr(), "AXParent") })
            .map_err(|error| effective_pid_error(application_pid, error))?
            .map(|parent| unsafe { RetainedAxElement::from_owned(parent) })
        else {
            return Ok((focused.clone(), focused_pid));
        };
        if parent.same_identity(&current) {
            return Err(NativeError::unsupported(
                "effective keyboard target ancestry contains a cycle",
            )
            .with_detail("application_pid", application_pid));
        }
        let parent_pid = unsafe { bindings::element_pid(parent.as_ptr()) }
            .map_err(|error| effective_pid_error(application_pid, error))?;
        if parent_pid != application_pid {
            return Ok((parent, parent_pid));
        }
        current = parent;
    }
    Err(NativeError::unsupported(
        "effective keyboard target ancestry exceeded the bounded AX parent walk",
    )
    .with_detail("application_pid", application_pid))
}

fn effective_pid_error(application_pid: i32, error: bindings::AXError) -> NativeError {
    NativeError::unsupported(
        "effective keyboard dispatch pid could not be proved from the focused AX target",
    )
    .with_detail("application_pid", application_pid)
    .with_detail("ax_error", error)
}

fn validate_focus_identity_proof(
    observed_owner_matches: bool,
    observed_window_id: u32,
    live_identity_matches: bool,
    live_window_id: Option<u32>,
    target_window_id: u32,
) -> Result<(), NativeError> {
    if !observed_owner_matches {
        return Err(focus_drift(
            "observed focused control owner stamp no longer matches the exact target window",
        )
        .with_detail("observed_window_id", observed_window_id)
        .with_detail("target_window_id", target_window_id));
    }
    if observed_window_id != target_window_id {
        return Err(
            focus_drift("observed focused control belongs to a different native window")
                .with_detail("observed_window_id", observed_window_id)
                .with_detail("target_window_id", target_window_id),
        );
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

fn should_use_exact_focused_insertion(
    framework: &Framework,
    selected_text_settable: Option<bool>,
    exact_insertion_available: bool,
) -> bool {
    selected_text_settable == Some(true)
        && exact_insertion_available
        && !matches!(framework, Framework::Chromium | Framework::Electron)
}

/// Deliberately contains no await point. `CGEventPostToPid` is void, so after
/// the boundary starts this loop cannot report or recover a partial post.
fn dispatch_sequence(
    poster: &dyn KeyboardEventPoster,
    application_pid: i32,
    dispatch_pid: i32,
    sequence: &PreparedKeySequence,
    boundary: &mut NativeSideEffectBoundary<'_>,
    field_focus_write: bool,
) -> Result<BTreeMap<String, Value>, NativeError> {
    boundary.begin()?;
    crate::focus_steal::record_synthesized_action(application_pid);
    let kinds = post_sequence(poster, dispatch_pid, sequence);
    Ok(sequence_evidence(
        application_pid,
        dispatch_pid,
        sequence.len(),
        kinds,
        field_focus_write,
    ))
}

fn post_sequence(
    poster: &dyn KeyboardEventPoster,
    dispatch_pid: i32,
    sequence: &PreparedKeySequence,
) -> Vec<&'static str> {
    let mut kinds = Vec::with_capacity(sequence.len());
    for (index, event) in sequence.events().iter().enumerate() {
        event.set_fresh_timestamp();
        poster.post_to_pid(dispatch_pid, event);
        kinds.push(event_kind_name(event.kind()));
        // `SynthesizedEvent.type(string:)` is sent through the helper's
        // delay-capable path: each four-event virtual-key group is atomic, but
        // consecutive characters are yielded to the target run loop. Repeated
        // key-code-zero Unicode groups are otherwise collapsed by AppKit after
        // the first character.
        if event.kind() == TargetedKeyEventKind::FlagsChangedRestore && index + 1 < sequence.len() {
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
    }
    kinds
}

fn sequence_evidence(
    application_pid: i32,
    dispatch_pid: i32,
    event_count: usize,
    event_kinds: Vec<&'static str>,
    field_focus_write: bool,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("event_count_attempted".to_owned(), event_count.into()),
        ("event_count_posted".to_owned(), event_count.into()),
        ("event_sequence".to_owned(), json!(event_kinds)),
        ("post_primitives".to_owned(), json!(["core_graphics_pid"])),
        ("application_pid".to_owned(), application_pid.into()),
        ("dispatch_pid".to_owned(), dispatch_pid.into()),
        (
            "out_of_process_target".to_owned(),
            (application_pid != dispatch_pid).into(),
        ),
        (
            "field_focus_write_attempted".to_owned(),
            field_focus_write.into(),
        ),
        ("delivery_acknowledged".to_owned(), false.into()),
        ("clipboard_used".to_owned(), false.into()),
    ])
}

fn focus_field_if_needed(
    target: &MacKeyboardEventTarget,
    boundary: &mut NativeSideEffectBoundary<'_>,
) -> Result<bool, NativeError> {
    let focused = unsafe { copy_bool_attr_exact(target.element.as_ptr(), "AXFocused") }
        .map_err(|error| ax_focus_error("AXFocused", error))?;
    let Some(false) = focused else {
        return Ok(false);
    };
    let settable = unsafe { is_attribute_settable(target.element.as_ptr(), "AXFocused") }
        .map_err(|error| ax_focus_error("AXFocused settable", error))?;
    if !settable {
        return Err(NativeError::unsupported(
            "exact keyboard target is not focused and AXFocused is not writable",
        )
        .with_detail("application_pid", target.application_pid)
        .with_detail("dispatch_pid", target.dispatch_pid)
        .with_detail("role", target.snapshot.role.clone()));
    }

    boundary.begin()?;
    let result = unsafe { bindings::set_bool_attr_true(target.element.as_ptr(), "AXFocused") };
    if result != kAXErrorSuccess {
        return Err(NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            true,
            "AXFocused restoration failed after entering the native side-effect boundary",
        )
        .with_detail("application_pid", target.application_pid)
        .with_detail("dispatch_pid", target.dispatch_pid)
        .with_detail("ax_error", result)
        .with_detail("field_focus_write_attempted", true)
        .with_detail("event_count_posted", 0)
        .with_detail("possibly_partial_delivery", true));
    }
    let restored =
        unsafe { copy_bool_attr_exact(target.element.as_ptr(), "AXFocused") }.map_err(|error| {
            NativeError::new(
                ErrorCode::DispatchFailed,
                ErrorPhase::Dispatch,
                true,
                "AXFocused restoration could not be read back",
            )
            .with_detail("application_pid", target.application_pid)
            .with_detail("dispatch_pid", target.dispatch_pid)
            .with_detail("ax_error", error)
            .with_detail("field_focus_write_attempted", true)
            .with_detail("event_count_posted", 0)
            .with_detail("possibly_partial_delivery", true)
        })?;
    if restored != Some(true) {
        return Err(NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            true,
            "AXFocused restoration was not acknowledged by the exact keyboard target",
        )
        .with_detail("application_pid", target.application_pid)
        .with_detail("dispatch_pid", target.dispatch_pid)
        .with_detail("field_focus_write_attempted", true)
        .with_detail("event_count_posted", 0)
        .with_detail("possibly_partial_delivery", true));
    }
    Ok(true)
}

fn sequence_construction_error(mut error: NativeError, side_effect_started: bool) -> NativeError {
    error.retryable = !side_effect_started;
    error.phase = if side_effect_started {
        ErrorPhase::Dispatch
    } else {
        ErrorPhase::Preflight
    };
    error
        .with_detail("field_focus_side_effect_started", side_effect_started)
        .with_detail("event_count_posted", 0)
        .with_detail("possibly_partial_delivery", side_effect_started)
}

fn event_kind_name(kind: TargetedKeyEventKind) -> &'static str {
    match kind {
        TargetedKeyEventKind::FlagsChangedRequested => "flags_changed_requested",
        TargetedKeyEventKind::KeyDown => "key_down",
        TargetedKeyEventKind::KeyUp => "key_up",
        TargetedKeyEventKind::FlagsChangedRestore => "flags_changed_restore",
        TargetedKeyEventKind::UnicodeDown => "unicode_down",
        TargetedKeyEventKind::UnicodeUp => "unicode_up",
    }
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
    target: MacKeyboardEventTarget,
    mut fields: BTreeMap<String, Value>,
) -> NativeDispatch {
    fields.extend(focus_evidence(&target));
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

fn dispatch_unverified_application(
    primitive: &str,
    target: MacApplicationKeyboardTarget,
    mut fields: BTreeMap<String, Value>,
) -> NativeDispatch {
    fields.extend(BTreeMap::from([
        (
            "focus_ax_revision".to_owned(),
            Value::String(target.ax_revision),
        ),
        (
            "focus_observation_id".to_owned(),
            Value::String(target.observation_id),
        ),
        ("focus_identity_revalidated".to_owned(), false.into()),
        ("application_scoped_chord".to_owned(), true.into()),
        ("application_pid".to_owned(), target.application_pid.into()),
        ("dispatch_pid".to_owned(), target.application_pid.into()),
        ("out_of_process_target".to_owned(), false.into()),
    ]));
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

fn focus_evidence(target: &MacKeyboardEventTarget) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "focus_ax_revision".to_owned(),
            Value::String(target.ax_revision.clone()),
        ),
        (
            "focus_observation_id".to_owned(),
            Value::String(target.observation_id.clone()),
        ),
        ("focus_identity_revalidated".to_owned(), true.into()),
        ("application_pid".to_owned(), target.application_pid.into()),
        ("dispatch_pid".to_owned(), target.dispatch_pid.into()),
        (
            "out_of_process_target".to_owned(),
            (target.application_pid != target.dispatch_pid).into(),
        ),
    ])
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
                    if framework == Framework::Chromium =>
                {
                    // Chromium exposes AXSelectedText as writable even when the
                    // renderer silently drops the write. Match the Swift
                    // driver's verified AX -> pid-post fallback by selecting
                    // the already-proven targeted Unicode route up front.
                    RouteDecision::Supported {
                        route: Route::TargetedKeyboard,
                    }
                }
                WindowStateKind::Visible | WindowStateKind::Occluded
                    if matches!(
                        framework,
                        Framework::Unknown
                            | Framework::AppKit
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
                decision: match state {
                    WindowStateKind::Visible | WindowStateKind::Occluded => {
                        RouteDecision::Supported {
                            route: Route::TargetedKeyboard,
                        }
                    }
                    _ => RouteDecision::Unsupported {
                        reason: "macOS targeted keyboard actions require an exact current-Space non-minimized window state".to_owned(),
                    },
                },
            });
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use cua_driver_core::api::contracts::{KeyStroke, Modifier};

    use super::*;

    struct RecordingPoster {
        calls: Mutex<Vec<(i32, TargetedKeyEventKind)>>,
    }

    impl KeyboardEventPoster for RecordingPoster {
        fn post_to_pid(&self, pid: i32, event: &PreparedTargetedKeyEvent) {
            self.calls.lock().unwrap().push((pid, event.kind()));
        }
    }

    #[test]
    fn prepared_chord_posts_once_per_event_to_the_effective_pid() {
        let chord = normalize_chord(&KeyStroke {
            key: "k".to_owned(),
            modifiers: vec![Modifier::Meta],
        })
        .unwrap();
        let sequence = prepare_chord_sequence(&chord).unwrap();
        let poster = RecordingPoster {
            calls: Mutex::new(Vec::new()),
        };
        let kinds = post_sequence(&poster, 42, &sequence);
        let evidence = sequence_evidence(41, 42, sequence.len(), kinds, false);
        assert_eq!(
            *poster.calls.lock().unwrap(),
            vec![
                (42, TargetedKeyEventKind::FlagsChangedRequested),
                (42, TargetedKeyEventKind::KeyDown),
                (42, TargetedKeyEventKind::KeyUp),
                (42, TargetedKeyEventKind::FlagsChangedRestore),
            ]
        );
        assert_eq!(evidence["application_pid"], 41);
        assert_eq!(evidence["dispatch_pid"], 42);
        assert_eq!(evidence["out_of_process_target"], true);
        assert_eq!(evidence["delivery_acknowledged"], false);
        assert_eq!(evidence["post_primitives"], json!(["core_graphics_pid"]));
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
    fn type_text_uses_exact_focused_insertion_before_framework_exceptions() {
        for framework in [Framework::Unknown, Framework::AppKit, Framework::WebKit] {
            assert!(should_use_exact_focused_insertion(
                &framework,
                Some(true),
                true
            ));
        }
        for framework in [Framework::Chromium, Framework::Electron] {
            assert!(!should_use_exact_focused_insertion(
                &framework,
                Some(true),
                true
            ));
        }
        assert!(!should_use_exact_focused_insertion(
            &Framework::Unknown,
            Some(false),
            true
        ));
        assert!(!should_use_exact_focused_insertion(
            &Framework::Unknown,
            Some(true),
            false
        ));
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
    fn capabilities_publish_generic_targeted_keyboard_including_unknown() {
        let cells = keyboard_capability_cells("fixture");
        assert!(cells
            .iter()
            .filter(|cell| cell.key.action == ActionKind::PressKey)
            .all(|cell| {
                matches!(
                    (&cell.key.window_state, &cell.decision),
                    (
                        WindowStateKind::Visible | WindowStateKind::Occluded,
                        RouteDecision::Supported {
                            route: Route::TargetedKeyboard
                        }
                    ) | (
                        WindowStateKind::Minimized
                            | WindowStateKind::OffSpace
                            | WindowStateKind::Unknown,
                        RouteDecision::Unsupported { .. }
                    )
                )
            }));
        let unknown = cells
            .iter()
            .find(|cell| {
                cell.key.action == ActionKind::PressKey
                    && cell.key.framework == Framework::Unknown
                    && cell.key.window_state == WindowStateKind::Visible
            })
            .unwrap();
        assert!(matches!(
            unknown.decision,
            RouteDecision::Supported {
                route: Route::TargetedKeyboard
            }
        ));

        let chromium_text = cells
            .iter()
            .find(|cell| {
                cell.key.action == ActionKind::TypeText
                    && cell.key.framework == Framework::Chromium
                    && cell.key.window_state == WindowStateKind::Visible
            })
            .unwrap();
        assert!(matches!(
            chromium_text.decision,
            RouteDecision::Supported {
                route: Route::TargetedKeyboard
            }
        ));

        let appkit_text = cells
            .iter()
            .find(|cell| {
                cell.key.action == ActionKind::TypeText
                    && cell.key.framework == Framework::AppKit
                    && cell.key.window_state == WindowStateKind::Visible
            })
            .unwrap();
        assert!(matches!(
            appkit_text.decision,
            RouteDecision::Supported {
                route: Route::Semantic
            }
        ));
    }
}
