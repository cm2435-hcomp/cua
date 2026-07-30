use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use core_foundation::base::{CFRelease, CFTypeRef};
use cua_driver_core::api::{
    contracts::{MouseButton, Route, ScrollDirection, SelectionType, VerificationLevel},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{InteractionScope, NativeEvidence, NativeSideEffectBoundary},
    menu::{MenuMutationIntent, NativeMenuEvidence, NativeMenuIdentity},
    observation::{ResolvedElement, ResolvedWindowStamp},
    platform::{
        Candidate, ClickSpec, ElementScrollSpec, NativeDispatch, ResolvedAction, SelectionSpec,
    },
    settlement::SettlementSignal,
};
use serde_json::Value;

use crate::{
    ax::bindings::{
        self, copy_action_names_exact, copy_attr_value, copy_ax_windows, copy_children,
        copy_element_attr, copy_string_attr, copy_string_attr_exact, is_attribute_settable,
        kAXErrorAttributeUnsupported, kAXErrorSuccess, AXUIElementCreateApplication,
        AXUIElementRef, AxCfRange,
    },
    driver::{
        menu::resolve_menu_identity,
        observation::{
            discover_native_menu, RegisteredElementSnapshot, RetainedAxElement, RetainedCfValue,
        },
        target::MacTargetState,
        windows::MacWindowRegistry,
    },
};

use super::scroll::{direct_page_action, page_child_snapshot, PageRoute, AX_PRESS};

const AX_SHOW_MENU: &str = "AXShowMenu";
const AX_OPEN: &str = "AXOpen";
const MAX_OWNER_DEPTH: usize = 64;

#[derive(Clone)]
pub struct MacSemanticActions {
    windows: MacWindowRegistry,
}

/// Retained, side-effect-free semantic recipe produced before core consumes
/// the observation. Its internals stay platform-owned and cannot be rebuilt
/// from loose metadata at the dispatch boundary.
pub struct MacPreparedSemanticAction {
    kind: PreparedKind,
    signal_epoch: u64,
}

enum PreparedKind {
    AxAction {
        element: LiveAxElement,
        action: String,
        primitive: &'static str,
        opens_menu: bool,
        open_verification: Option<AxOpenVerification>,
    },
    PageScroll {
        route: PageRoute,
        direction: ScrollDirection,
        pages: u16,
    },
    SetValue {
        element: LiveAxElement,
        value: String,
    },
    SelectText {
        element: LiveAxElement,
        range: AxCfRange,
        expected_text: String,
    },
}

struct AxOpenVerification {
    pid: i32,
    owner_window_id: u32,
    prior_title: String,
    expected_title: String,
}

impl MacSemanticActions {
    pub fn new(windows: MacWindowRegistry) -> Self {
        Self { windows }
    }

    async fn element_click_candidate(
        &self,
        target: &mut MacTargetState,
        element: &ResolvedElement,
        spec: &ClickSpec,
    ) -> Result<Candidate<()>, NativeError> {
        let live = self.refetch_exact(target, element).await?;
        let Ok(action) = click_action(&live.snapshot.role, live.snapshot.subrole.as_deref(), spec)
        else {
            return Ok(Candidate::not_applicable(
                "element click shape has no exact semantic primitive",
            ));
        };
        if action == AX_PRESS
            && live
                .snapshot
                .actions
                .iter()
                .any(|candidate| candidate == AX_PRESS)
        {
            Ok(Candidate::Prepared(()))
        } else {
            Ok(Candidate::not_applicable(
                "element does not retain an exact usable AXPress action",
            ))
        }
    }

    fn element_scroll_candidate(
        &self,
        target: &MacTargetState,
        element: &ResolvedElement,
        scroll: &ElementScrollSpec,
    ) -> Result<Candidate<()>, NativeError> {
        if exact_semantic_pages(scroll.pages).is_none() {
            return Ok(Candidate::not_applicable(
                "fractional or oversized page count requires targeted page scroll",
            ));
        }
        let requested = self.registered_exact(target, element)?;
        let snapshots = target.elements.registered_snapshots();
        for candidate in retained_ancestor_chain(&requested, &snapshots)? {
            if direct_page_action(
                scroll.direction,
                &candidate.actions,
                candidate.orientation.as_deref(),
            )
            .is_some()
            {
                return Ok(Candidate::Prepared(()));
            }
            if is_scroll_container_role(&candidate.role)
                && page_child_snapshot(&candidate, &snapshots, scroll.direction)
                    .is_some_and(|child| child.actions.iter().any(|action| action == AX_PRESS))
            {
                return Ok(Candidate::Prepared(()));
            }
        }
        Ok(Candidate::not_applicable(
            "element and retained ancestors expose no exact semantic page action",
        ))
    }

    async fn prepare(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<MacPreparedSemanticAction, NativeError> {
        let prepared = match action {
            ResolvedAction::ElementClick { element, spec, .. } => {
                preflight_scope(target, scope, element)?;
                let live = self.refetch_exact(target, element).await?;
                let action =
                    click_action(&live.snapshot.role, live.snapshot.subrole.as_deref(), spec)?;
                require_exact_action(&live.snapshot.actions, action)?;
                PreparedKind::AxAction {
                    element: live,
                    action: action.to_owned(),
                    primitive: if action == AX_SHOW_MENU {
                        "macos_ax_show_menu"
                    } else {
                        "macos_ax_press"
                    },
                    opens_menu: action == AX_SHOW_MENU,
                    open_verification: None,
                }
            }
            ResolvedAction::ElementScroll { element, spec, .. } => {
                preflight_scope(target, scope, element)?;
                let pages = exact_semantic_pages(spec.pages).ok_or_else(|| {
                    NativeError::unsupported(
                        "semantic page scroll requires an integral page count within u16",
                    )
                })?;
                PreparedKind::PageScroll {
                    route: self.prepare_scroll_route(target, element, spec).await?,
                    direction: spec.direction,
                    pages,
                }
            }
            ResolvedAction::SetValue { element, value } => {
                preflight_scope(target, scope, element)?;
                let live = self.refetch_exact(target, element).await?;
                if live.snapshot.value_settable != Some(true)
                    || live.snapshot.string_value.is_none()
                {
                    return Err(NativeError::unsupported(
                        "macOS AXValue is not both settable and a CFString",
                    ));
                }
                PreparedKind::SetValue {
                    element: live,
                    value: value.clone(),
                }
            }
            ResolvedAction::SelectText { element, selection } => {
                preflight_scope(target, scope, element)?;
                let live = self.refetch_exact(target, element).await?;
                if live.snapshot.selected_text_range_settable != Some(true)
                    || live.snapshot.selected_text_range.is_none()
                {
                    return Err(NativeError::unsupported(
                        "macOS element has no exact writable AXSelectedTextRange route; text-marker-only selection is refused",
                    ));
                }
                let document = live.snapshot.string_value.as_deref().ok_or_else(|| {
                    NativeError::unsupported(
                        "macOS text selection requires an exact CFString AXValue snapshot",
                    )
                })?;
                let range = resolve_selection_range(document, selection)?;
                let expected_text = match selection.selection_type {
                    SelectionType::Text => selection.text.clone(),
                    SelectionType::CursorBefore | SelectionType::CursorAfter => String::new(),
                };
                PreparedKind::SelectText {
                    element: live,
                    range,
                    expected_text,
                }
            }
            ResolvedAction::Secondary { element, action } => {
                preflight_scope(target, scope, element)?;
                let live = self.refetch_exact(target, element).await?;
                require_exact_action(&live.snapshot.actions, action)?;
                let open_verification = if action == AX_OPEN {
                    self.prepare_ax_open_verification(
                        target.window.clone(),
                        live.snapshot.owner.clone(),
                        live.snapshot.owner_window_id,
                        live.snapshot.label.clone(),
                    )
                    .await
                } else {
                    None
                };
                PreparedKind::AxAction {
                    element: live,
                    action: action.clone(),
                    primitive: if action == AX_SHOW_MENU {
                        "macos_ax_show_menu"
                    } else {
                        "macos_ax_secondary_action"
                    },
                    opens_menu: action == AX_SHOW_MENU,
                    open_verification,
                }
            }
            ResolvedAction::DeltaScroll(_) => {
                return Err(NativeError::unsupported(
                    "platform has no exact semantic delta-scroll route",
                ))
            }
            ResolvedAction::TypeText { .. } => {
                return Err(NativeError::unsupported(
                    "platform has no semantic insertion-preserving text route",
                ))
            }
            ResolvedAction::PointClick { .. }
            | ResolvedAction::Drag(_)
            | ResolvedAction::PressKey { .. } => {
                return Err(NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Preflight,
                    false,
                    "non-semantic action reached macOS semantic prepare",
                ))
            }
        };
        Ok(MacPreparedSemanticAction {
            kind: prepared,
            signal_epoch: target.signals.epoch(),
        })
    }

    async fn prepare_scroll_route(
        &self,
        target: &mut MacTargetState,
        element: &ResolvedElement,
        scroll: &ElementScrollSpec,
    ) -> Result<PageRoute, NativeError> {
        let requested = self.registered_exact(target, element)?;
        let snapshots = target.elements.registered_snapshots();
        let candidates = retained_ancestor_chain(&requested, &snapshots)?;

        for candidate in candidates {
            if let Some(action) = direct_page_action(
                scroll.direction,
                &candidate.actions,
                candidate.orientation.as_deref(),
            ) {
                let live = self.refetch_registered_exact(target, candidate).await?;
                return Ok(PageRoute::Direct {
                    element: live,
                    action,
                });
            }
            if is_scroll_container_role(&candidate.role) {
                if let Some(page_child) =
                    page_child_snapshot(&candidate, &snapshots, scroll.direction)
                {
                    let page_child = self.refetch_registered_exact(target, page_child).await?;
                    require_exact_action(&page_child.snapshot.actions, AX_PRESS)?;
                    return Ok(PageRoute::ScrollbarPageChild {
                        element: page_child,
                        action: AX_PRESS,
                    });
                }
            }
        }
        Err(unsupported_scroll(scroll.direction))
    }

    fn registered_exact(
        &self,
        target: &MacTargetState,
        element: &ResolvedElement,
    ) -> Result<RegisteredElementSnapshot, NativeError> {
        if target.invalidated() {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "macOS target invalidated before semantic element refetch",
            ));
        }
        if target.window != element.window.stamp() {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "resolved element target stamp no longer matches the locked macOS target",
            ));
        }
        let snapshot = target
            .elements
            .registered(&element.native, &element.element_id)
            .ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::ElementStale,
                    "resolved element has no exact retained macOS AX identity",
                )
            })?;
        if snapshot.owner != element.owner
            || element.role.as_deref() != Some(snapshot.role.as_str())
            || !same_actions(&snapshot.actions, &element.actions)
        {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "resolved element metadata disagrees with its retained macOS AX snapshot",
            ));
        }
        Ok(snapshot)
    }

    async fn dispatch(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: MacPreparedSemanticAction,
    ) -> Result<NativeDispatch, NativeError> {
        self.revalidate_prepared(target, scope.owner.clone(), scope.window.stamp(), &action)
            .await?;
        match action.kind {
            PreparedKind::AxAction {
                element,
                action,
                primitive,
                opens_menu,
                open_verification,
            } => {
                boundary.begin()?;
                let result = unsafe { bindings::perform_action(element.as_ptr(), &action) };
                let post_error_readback_attempts = if result == kAXErrorAttributeUnsupported {
                    match open_verification.as_ref() {
                        Some(proof) => {
                            wait_for_verified_ax_open_transition(proof, scope.deadline.work).await
                        }
                        None => None,
                    }
                } else {
                    None
                };
                let post_error_effect_verified = post_error_readback_attempts.is_some();
                if result != kAXErrorSuccess && !post_error_effect_verified {
                    return Err(ax_dispatch_error("exact AX action", result));
                }
                target.signals.record(SettlementSignal::AxAction);
                let mut native_dispatch = dispatch(
                    if post_error_effect_verified {
                        VerificationLevel::EffectVerified
                    } else {
                        VerificationLevel::DispatchVerified
                    },
                    primitive,
                    [
                        ("ax_action", Value::String(action.clone())),
                        ("ax_return_code", Value::from(result)),
                        (
                            "post_error_effect_verified",
                            Value::Bool(post_error_effect_verified),
                        ),
                        (
                            "post_error_readback_attempts",
                            Value::from(post_error_readback_attempts.unwrap_or(0)),
                        ),
                    ],
                );
                if post_error_effect_verified {
                    target
                        .signals
                        .record(SettlementSignal::VerificationReadbackComplete);
                    native_dispatch.evidence.fields.extend([
                        (
                            "owner_window_identity_preserved".to_owned(),
                            Value::Bool(true),
                        ),
                        ("destination_title_matched".to_owned(), Value::Bool(true)),
                    ]);
                }
                if let Some(intent) = &scope.menu_intent {
                    if let Err(mut error) =
                        target.arm_menu_suppression(&scope.action_id, intent.menu_id())
                    {
                        target.invalidate();
                        scope.invalidate_target();
                        error
                            .details
                            .insert("native_side_effect_started".to_owned(), true.into());
                        return Err(error);
                    }
                }
                native_dispatch.menu = match &scope.menu_intent {
                    Some(MenuMutationIntent::Opening { menu_id }) if opens_menu => Some(
                        self.wait_for_opened_menu(
                            scope.owner.clone(),
                            scope.action_id.clone(),
                            scope.deadline.work,
                            menu_id.clone(),
                        )
                        .await?,
                    ),
                    Some(MenuMutationIntent::Targeting { menu_id, identity }) if !opens_menu => {
                        Some(
                            self.menu_outcome_after_target(
                                scope.owner.clone(),
                                scope.action_id.clone(),
                                menu_id.clone(),
                                identity.clone(),
                            )
                            .await?,
                        )
                    }
                    Some(MenuMutationIntent::Dismissing { .. }) => {
                        return Err(NativeError::new(
                            ErrorCode::Internal,
                            ErrorPhase::Dispatch,
                            false,
                            "semantic AX action cannot satisfy a point-menu dismissal intent",
                        ))
                    }
                    Some(_) => {
                        return Err(NativeError::stale(
                            ErrorCode::MenuStateStale,
                            "semantic menu action and bound lifecycle intent disagree",
                        )
                        .with_detail("native_side_effect_started", true))
                    }
                    None => None,
                };
                match &native_dispatch.menu {
                    Some(NativeMenuEvidence::Opened { .. }) => {
                        target.signals.record(SettlementSignal::MenuOpened);
                    }
                    Some(NativeMenuEvidence::Dismissed { .. }) => {
                        target.signals.record(SettlementSignal::MenuDismissed);
                    }
                    Some(NativeMenuEvidence::Targeted { .. }) | None => {}
                }
                Ok(native_dispatch)
            }
            PreparedKind::PageScroll {
                route,
                direction,
                pages,
            } => {
                let (native, action, route_name) = match &route {
                    PageRoute::Direct { element, action } => {
                        (element.as_ptr(), *action, "nearest_direct_page_action")
                    }
                    PageRoute::ScrollbarPageChild { element, action } => (
                        element.as_ptr(),
                        *action,
                        "nearest_container_scrollbar_page_child",
                    ),
                };
                scope.native_evidence.fields.insert(
                    "semantic_scroll_requested_pages".to_owned(),
                    Value::from(pages),
                );
                scope.native_evidence.fields.insert(
                    "semantic_scroll_completed_pages".to_owned(),
                    Value::from(0_u16),
                );
                scope.native_evidence.fields.insert(
                    "semantic_scroll_direction".to_owned(),
                    Value::String(format!("{direction:?}").to_lowercase()),
                );
                scope.native_evidence.fields.insert(
                    "semantic_scroll_route".to_owned(),
                    Value::String(route_name.to_owned()),
                );
                let mut completed_pages = 0_u16;
                for _ in 0..pages {
                    boundary.begin()?;
                    let result = unsafe { bindings::perform_action(native, action) };
                    if result != kAXErrorSuccess {
                        return Err(NativeError::new(
                            ErrorCode::DispatchFailed,
                            ErrorPhase::Dispatch,
                            false,
                            "exact macOS AX page-scroll action failed after a partial dispatch",
                        )
                        .with_detail("ax_error", result)
                        .with_detail("completed_pages", completed_pages)
                        .with_detail("requested_pages", pages)
                        .with_detail("route", route_name));
                    }
                    completed_pages += 1;
                    scope.native_evidence.fields.insert(
                        "semantic_scroll_completed_pages".to_owned(),
                        Value::from(completed_pages),
                    );
                    target.signals.record(SettlementSignal::AxAction);
                }
                Ok(dispatch(
                    VerificationLevel::DispatchVerified,
                    "macos_ax_page_scroll",
                    [
                        ("route", Value::String(route_name.to_owned())),
                        ("completed_pages", Value::from(completed_pages)),
                    ],
                ))
            }
            PreparedKind::SetValue { element, value } => {
                boundary.begin()?;
                let result =
                    unsafe { bindings::set_string_attr(element.as_ptr(), "AXValue", &value) };
                if result != kAXErrorSuccess {
                    return Err(ax_dispatch_error("AXValue write", result));
                }
                let readback = unsafe { copy_string_attr_exact(element.as_ptr(), "AXValue") };
                target
                    .signals
                    .record(SettlementSignal::VerificationReadbackComplete);
                let readback = readback.map_err(|error| {
                    ax_verification_error("typed CFString AXValue readback", error)
                })?;
                if !exact_typed_cfstring_readback_matches(readback.as_deref(), &value) {
                    return Err(verification_error(
                        "macOS typed CFString AXValue readback did not exactly match the requested string",
                    ));
                }
                Ok(dispatch(
                    VerificationLevel::EffectVerified,
                    "macos_ax_string_value",
                    [("exact_typed_readback", Value::Bool(true))],
                ))
            }
            PreparedKind::SelectText {
                element,
                range,
                expected_text,
            } => {
                boundary.begin()?;
                let result = unsafe {
                    bindings::set_cf_range_attr(element.as_ptr(), "AXSelectedTextRange", range)
                };
                if result != kAXErrorSuccess {
                    return Err(ax_dispatch_error("AXSelectedTextRange write", result));
                }
                let range_readback = unsafe {
                    bindings::copy_cf_range_attr(element.as_ptr(), "AXSelectedTextRange")
                };
                let selected_text =
                    unsafe { copy_string_attr_exact(element.as_ptr(), "AXSelectedText") };
                target
                    .signals
                    .record(SettlementSignal::VerificationReadbackComplete);
                let range_readback = range_readback.map_err(|error| {
                    ax_verification_error("typed AXSelectedTextRange readback", error)
                })?;
                let selected_text = selected_text.map_err(|error| {
                    ax_verification_error("typed AXSelectedText readback", error)
                })?;
                if !exact_typed_selection_readback_matches(
                    range_readback,
                    selected_text.as_deref(),
                    range,
                    &expected_text,
                ) {
                    return Err(verification_error(
                        "macOS typed text selection range/text readback did not exactly match the requested selection",
                    ));
                }
                Ok(dispatch(
                    VerificationLevel::EffectVerified,
                    "macos_ax_utf16_selection",
                    [("exact_typed_readback", Value::Bool(true))],
                ))
            }
        }
    }

    async fn wait_for_opened_menu(
        &self,
        owner: cua_driver_core::api::observation::ResolvedWindowStamp,
        action_id: cua_driver_core::api::contracts::ActionId,
        deadline: Instant,
        menu_id: cua_driver_core::api::contracts::MenuId,
    ) -> Result<NativeMenuEvidence, NativeError> {
        loop {
            if let Some(identity) = self.discover_menu_identity(owner.clone()).await? {
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
                    "AXShowMenu dispatched but no exact native menu identity arrived before the action deadline",
                )
                .with_detail("native_side_effect_started", true));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn menu_outcome_after_target(
        &self,
        owner: cua_driver_core::api::observation::ResolvedWindowStamp,
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

    async fn discover_menu_identity(
        &self,
        owner: cua_driver_core::api::observation::ResolvedWindowStamp,
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

    async fn revalidate_prepared(
        &self,
        target: &mut MacTargetState,
        scope_owner: cua_driver_core::api::observation::ResolvedWindowStamp,
        scope_window: cua_driver_core::api::observation::ResolvedWindowStamp,
        action: &MacPreparedSemanticAction,
    ) -> Result<(), NativeError> {
        if target.invalidated() || scope_owner != target.window || scope_window != target.window {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "semantic target identity changed after native action preparation",
            ));
        }
        let epoch = target.signals.epoch();
        if epoch != action.signal_epoch {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "semantic AX/content notification raced the final native validation",
            ));
        }
        let snapshots: Vec<_> = match &action.kind {
            PreparedKind::AxAction { element, .. }
            | PreparedKind::SetValue { element, .. }
            | PreparedKind::SelectText { element, .. } => vec![element.snapshot.clone()],
            PreparedKind::PageScroll { route, .. } => match route {
                PageRoute::Direct { element, .. }
                | PageRoute::ScrollbarPageChild { element, .. } => {
                    vec![element.snapshot.clone()]
                }
            },
        };
        for snapshot in snapshots {
            self.refetch_registered_exact(target, snapshot).await?;
        }
        if target.signals.epoch() != epoch {
            return Err(NativeError::stale(
                ErrorCode::ObservationRaced,
                "semantic AX/content notification raced the exact dispatch refetch",
            ));
        }
        Ok(())
    }

    async fn prepare_ax_open_verification(
        &self,
        target_window: ResolvedWindowStamp,
        owner: ResolvedWindowStamp,
        observed_owner_window_id: u32,
        expected_title: Option<String>,
    ) -> Option<AxOpenVerification> {
        let expected_title = expected_title
            .as_deref()
            .filter(|title| !title.is_empty())?
            .to_owned();
        let (pid, owner_window_id) = if owner == target_window {
            let facts = self.windows.facts_for_stamp(&target_window).await.ok()?;
            (facts.pid, facts.cg_window_id)
        } else {
            let facts = self
                .windows
                .facts_for_related_stamp(&owner, &target_window)
                .await
                .ok()?;
            (facts.pid, facts.cg_window_id)
        };
        if owner_window_id != observed_owner_window_id {
            return None;
        }
        let window = exact_ax_window(pid, owner_window_id).ok()?;
        let prior_title = unsafe { copy_string_attr_exact(window.as_ptr(), "AXTitle") }.ok()??;
        Some(AxOpenVerification {
            pid,
            owner_window_id,
            prior_title,
            expected_title,
        })
    }

    async fn refetch_exact(
        &self,
        target: &mut MacTargetState,
        element: &ResolvedElement,
    ) -> Result<LiveAxElement, NativeError> {
        let snapshot = self.registered_exact(target, element)?;
        self.refetch_registered_exact(target, snapshot).await
    }

    async fn refetch_registered_exact(
        &self,
        target: &mut MacTargetState,
        snapshot: RegisteredElementSnapshot,
    ) -> Result<LiveAxElement, NativeError> {
        let (pid, owner_window_id) = if snapshot.owner == target.window {
            let facts = self.windows.facts_for_stamp(&target.window).await?;
            (facts.pid, facts.cg_window_id)
        } else {
            let facts = self
                .windows
                .facts_for_related_stamp(&snapshot.owner, &target.window)
                .await?;
            (facts.pid, facts.cg_window_id)
        };
        if owner_window_id != snapshot.owner_window_id {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "retained AX element owner window no longer matches its exact observation owner",
            ));
        }

        let refetched = if let Some(expected_parent) = &snapshot.parent {
            let current_parent =
                unsafe { copy_element_attr(snapshot.element.as_ptr(), "AXParent") }
                    .map_err(|error| ax_stale_error("AXParent refetch", error))?
                    .ok_or_else(|| {
                        NativeError::stale(
                            ErrorCode::ElementStale,
                            "retained AX element lost its observed parent",
                        )
                    })?;
            let current_parent = unsafe { RetainedAxElement::from_owned(current_parent) };
            if !current_parent.same_identity(expected_parent) {
                return Err(NativeError::stale(
                    ErrorCode::ElementStale,
                    "retained AX element parent identity changed",
                ));
            }
            exact_child_of(&current_parent, &snapshot.element)?
        } else {
            let window = exact_ax_window(pid, owner_window_id)?;
            if !window.same_identity(&snapshot.element) {
                return Err(NativeError::stale(
                    ErrorCode::ElementStale,
                    "retained root AX identity no longer matches the exact owner window",
                ));
            }
            window
        };
        if !refetched.same_identity(&snapshot.element) {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "CFEqual rejected the retained/refetched AX element identity",
            ));
        }
        if current_owner_window_id(&refetched)? != owner_window_id {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "live AX parent chain no longer reaches the observed owner window",
            ));
        }

        let (live_role, live_role_proven) =
            match unsafe { copy_string_attr_exact(refetched.as_ptr(), "AXRole") } {
                Ok(Some(role)) => (Some(role), true),
                _ => (None, false),
            };
        let live_subrole = unsafe { copy_string_attr(refetched.as_ptr(), "AXSubrole") };
        let live_orientation = unsafe { copy_string_attr(refetched.as_ptr(), "AXOrientation") };
        let (live_value_proof, live_value_query_proven) =
            match unsafe { copy_attr_value(refetched.as_ptr(), "AXValue") } {
                Ok(Some(value)) => (Some(unsafe { RetainedCfValue::from_owned(value) }), true),
                Ok(None) => (None, true),
                Err(_) => (None, false),
            };
        let live_value = live_value_proof
            .as_ref()
            .and_then(RetainedCfValue::as_string);
        let live_value_settable = live_value_proof
            .as_ref()
            .and_then(|_| unsafe { is_attribute_settable(refetched.as_ptr(), "AXValue").ok() });
        let live_range = live_value.as_ref().and_then(|_| unsafe {
            bindings::copy_cf_range_attr(refetched.as_ptr(), "AXSelectedTextRange")
                .ok()
                .flatten()
        });
        let live_range_settable = live_range.as_ref().and_then(|_| unsafe {
            is_attribute_settable(refetched.as_ptr(), "AXSelectedTextRange").ok()
        });
        let (live_actions, live_actions_proven) =
            match unsafe { copy_action_names_exact(refetched.as_ptr()) } {
                Ok(actions) => (actions, true),
                Err(_) => (Vec::new(), false),
            };
        if !snapshot.role_proven
            || !live_role_proven
            || live_role.as_deref() != Some(snapshot.role.as_str())
            || live_subrole != snapshot.subrole
            || live_orientation != snapshot.orientation
            || !snapshot.value_query_proven
            || !live_value_query_proven
            || !same_values(&live_value_proof, &snapshot.value_proof)
            || live_value != snapshot.string_value
            || live_value_settable != snapshot.value_settable
            || live_range != snapshot.selected_text_range
            || live_range_settable != snapshot.selected_text_range_settable
            || !snapshot.actions_proven
            || !live_actions_proven
            || !same_actions(&live_actions, &snapshot.actions)
        {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "live macOS AX role/actions/value/settable/parent/selection proof changed",
            ));
        }
        Ok(LiveAxElement {
            native: refetched,
            snapshot,
        })
    }
}

#[derive(Clone)]
pub(super) struct LiveAxElement {
    native: RetainedAxElement,
    pub(super) snapshot: RegisteredElementSnapshot,
}

impl LiveAxElement {
    pub(super) fn as_ptr(&self) -> AXUIElementRef {
        self.native.as_ptr()
    }
}

fn preflight_scope(
    target: &MacTargetState,
    scope: &InteractionScope,
    element: &ResolvedElement,
) -> Result<(), NativeError> {
    if scope.route != Route::Semantic
        || scope.owner != target.window
        || scope.window.stamp() != target.window
        || element.window.stamp() != target.window
    {
        return Err(NativeError::new(
            ErrorCode::Internal,
            ErrorPhase::Preflight,
            false,
            "semantic AX action entered without the exact locked interaction scope",
        ));
    }
    Ok(())
}

fn click_action(
    _role: &str,
    _subrole: Option<&str>,
    click: &ClickSpec,
) -> Result<&'static str, NativeError> {
    if click.click_count != 1 || !click.modifiers.is_empty() {
        return Err(NativeError::unsupported(
            "semantic macOS click supports exactly one click with no modifiers",
        ));
    }
    match click.button {
        MouseButton::Left => Ok(AX_PRESS),
        MouseButton::Right => Ok(AX_SHOW_MENU),
        MouseButton::Middle => Err(NativeError::unsupported(
            "semantic macOS click has no exact middle-button route",
        )),
    }
}

fn is_scroll_container_role(role: &str) -> bool {
    matches!(
        role,
        "AXScrollArea"
            | "AXWebArea"
            | "AXList"
            | "AXTable"
            | "AXOutline"
            | "AXBrowser"
            | "AXTextArea"
    )
}

fn exact_semantic_pages(pages: f64) -> Option<u16> {
    if pages.is_finite() && pages > 0.0 && pages.fract() == 0.0 && pages <= f64::from(u16::MAX) {
        Some(pages as u16)
    } else {
        None
    }
}

fn require_exact_action(actions: &[String], action: &str) -> Result<(), NativeError> {
    if actions.iter().any(|candidate| candidate == action) {
        Ok(())
    } else {
        Err(NativeError::unsupported(format!(
            "live macOS AX element does not expose exact action {action}"
        )))
    }
}

fn verified_ax_open_transition(proof: &AxOpenVerification) -> bool {
    let Ok(window) = exact_ax_window(proof.pid, proof.owner_window_id) else {
        return false;
    };
    let Ok(Some(current_title)) = (unsafe { copy_string_attr_exact(window.as_ptr(), "AXTitle") })
    else {
        return false;
    };
    ax_open_title_transition_matches(&proof.prior_title, &current_title, &proof.expected_title)
}

async fn wait_for_verified_ax_open_transition(
    proof: &AxOpenVerification,
    deadline: Instant,
) -> Option<u32> {
    let mut attempts = 0_u32;
    loop {
        attempts += 1;
        if verified_ax_open_transition(proof) {
            return Some(attempts);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn ax_open_title_transition_matches(
    prior_title: &str,
    current_title: &str,
    expected_title: &str,
) -> bool {
    current_title != prior_title && current_title == expected_title
}

fn resolve_selection_range(
    document: &str,
    selection: &SelectionSpec,
) -> Result<AxCfRange, NativeError> {
    let matches: Vec<_> = document
        .char_indices()
        .filter_map(|(start, _)| {
            let tail = &document[start..];
            if !tail.starts_with(&selection.text) {
                return None;
            }
            let end = start + selection.text.len();
            let prefix_matches = selection
                .prefix
                .as_ref()
                .is_none_or(|prefix| document[..start].ends_with(prefix));
            let suffix_matches = selection
                .suffix
                .as_ref()
                .is_none_or(|suffix| document[end..].starts_with(suffix));
            (prefix_matches && suffix_matches).then_some((start, end))
        })
        .collect();
    let (start, end) = match matches.as_slice() {
        [] => {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "requested text/context does not occur in the exact AXValue snapshot",
            ))
        }
        [only] => *only,
        _ => {
            return Err(NativeError::new(
                ErrorCode::InvalidRequest,
                ErrorPhase::Preflight,
                false,
                "requested text/context is ambiguous in the exact AXValue snapshot",
            ))
        }
    };
    let utf16_start = document[..start].encode_utf16().count();
    let utf16_end = document[..end].encode_utf16().count();
    let (location, length) = match selection.selection_type {
        SelectionType::Text => (utf16_start, utf16_end - utf16_start),
        SelectionType::CursorBefore => (utf16_start, 0),
        SelectionType::CursorAfter => (utf16_end, 0),
    };
    AxCfRange::from_utf16(location, length).ok_or_else(|| {
        NativeError::unsupported("requested UTF-16 text range exceeds macOS CFRange limits")
    })
}

fn retained_ancestor_chain(
    requested: &RegisteredElementSnapshot,
    elements: &[RegisteredElementSnapshot],
) -> Result<Vec<RegisteredElementSnapshot>, NativeError> {
    let mut chain = vec![requested.clone()];
    let mut parent = requested.parent.clone();
    for _ in 0..MAX_OWNER_DEPTH {
        let Some(parent_identity) = parent else {
            return Ok(chain);
        };
        if chain
            .iter()
            .any(|candidate| candidate.element.same_identity(&parent_identity))
        {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "retained AX scroll ancestor chain contains an identity cycle",
            ));
        }
        let mut matches = elements
            .iter()
            .filter(|candidate| candidate.element.same_identity(&parent_identity));
        let Some(next) = matches.next().cloned() else {
            // The observed owner window may have an AXApplication parent that
            // is intentionally outside the window-scoped registry.
            return Ok(chain);
        };
        if matches.next().is_some() || next.owner != requested.owner {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "retained AX scroll ancestor identity was not unique in the exact owner",
            ));
        }
        parent = next.parent.clone();
        chain.push(next);
    }
    if parent.is_some() {
        return Err(NativeError::stale(
            ErrorCode::ElementStale,
            "retained AX scroll ancestor chain exceeded its bounded identity proof",
        ));
    }
    Ok(chain)
}

fn exact_child_of(
    parent: &RetainedAxElement,
    expected: &RetainedAxElement,
) -> Result<RetainedAxElement, NativeError> {
    let children: Vec<_> = unsafe { copy_children(parent.as_ptr()) }
        .into_iter()
        .map(|child| unsafe { RetainedAxElement::from_owned(child) })
        .collect();
    let mut matches = children
        .iter()
        .filter(|child| child.same_identity(expected));
    let matched = matches.next().cloned();
    if matches.next().is_some() {
        return Err(NativeError::stale(
            ErrorCode::ElementStale,
            "AX parent returned duplicate CFEqual child identities",
        ));
    }
    matched.ok_or_else(|| {
        NativeError::stale(
            ErrorCode::ElementStale,
            "retained AX element is no longer an exact child of its observed parent",
        )
    })
}

fn exact_ax_window(pid: i32, window_id: u32) -> Result<RetainedAxElement, NativeError> {
    let application = unsafe { AXUIElementCreateApplication(pid) };
    if application.is_null() {
        return Err(NativeError::stale(
            ErrorCode::ElementStale,
            "macOS AX application root disappeared during exact refetch",
        ));
    }
    let windows: Vec<_> = unsafe { copy_ax_windows(application) }
        .into_iter()
        .map(|window| unsafe { RetainedAxElement::from_owned(window) })
        .collect();
    unsafe { CFRelease(application as CFTypeRef) };
    let mut matches = windows.iter().filter(|window| {
        (unsafe { bindings::ax_get_window_id(window.as_ptr()) }) == Some(window_id)
    });
    let matched = matches.next().cloned();
    if matches.next().is_some() {
        return Err(NativeError::stale(
            ErrorCode::ElementStale,
            "macOS AX returned multiple windows for one exact WindowServer id",
        ));
    }
    matched.ok_or_else(|| {
        NativeError::stale(
            ErrorCode::ElementStale,
            "exact macOS AX owner window disappeared during refetch",
        )
    })
}

fn current_owner_window_id(element: &RetainedAxElement) -> Result<u32, NativeError> {
    let mut current = element.clone();
    for _ in 0..MAX_OWNER_DEPTH {
        if let Some(window_id) = unsafe { bindings::ax_get_window_id(current.as_ptr()) } {
            return Ok(window_id);
        }
        let parent = unsafe { copy_element_attr(current.as_ptr(), "AXParent") }
            .map_err(|error| ax_stale_error("AX owner parent-chain refetch", error))?
            .ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::ElementStale,
                    "live AX parent chain ended before an exact owner window",
                )
            })?;
        let parent = unsafe { RetainedAxElement::from_owned(parent) };
        if parent.same_identity(&current) {
            return Err(NativeError::stale(
                ErrorCode::ElementStale,
                "live AX parent chain contains an identity cycle",
            ));
        }
        current = parent;
    }
    Err(NativeError::stale(
        ErrorCode::ElementStale,
        "live AX parent chain exceeded the bounded owner-depth proof",
    ))
}

fn same_actions(left: &[String], right: &[String]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort();
    left.dedup();
    right.sort();
    right.dedup();
    left == right
}

fn same_values(left: &Option<RetainedCfValue>, right: &Option<RetainedCfValue>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.same_value(right),
        _ => false,
    }
}

/// Exact typed CFString comparison. Rust string equality is byte exact for
/// these decoded scalar values; this deliberately performs no Unicode
/// normalization or case folding.
fn exact_typed_cfstring_readback_matches(readback: Option<&str>, expected: &str) -> bool {
    readback == Some(expected)
}

/// Exact typed CFRange plus CFString comparison, with no normalization.
fn exact_typed_selection_readback_matches(
    range_readback: Option<AxCfRange>,
    text_readback: Option<&str>,
    expected_range: AxCfRange,
    expected_text: &str,
) -> bool {
    range_readback == Some(expected_range) && text_readback == Some(expected_text)
}

fn unsupported_scroll(direction: ScrollDirection) -> NativeError {
    NativeError::unsupported(format!(
        "macOS element exposes neither an exact {direction:?} page action nor one unique exact scrollbar page child"
    ))
}

fn dispatch<const N: usize>(
    verification: VerificationLevel,
    primitive: &str,
    fields: [(&str, Value); N],
) -> NativeDispatch {
    let mut evidence =
        BTreeMap::from([("primitive".to_owned(), Value::String(primitive.to_owned()))]);
    evidence.extend(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    );
    NativeDispatch {
        verification,
        evidence: NativeEvidence {
            fields: evidence,
            interaction_scope: None,
        },
        warnings: Vec::new(),
        // Menu provenance comes only from the canonical menu lifecycle. An AX
        // dispatch result alone never proves which menu appeared.
        menu: None,
    }
}

fn ax_stale_error(operation: &str, error: i32) -> NativeError {
    NativeError::stale(
        ErrorCode::ElementStale,
        format!("{operation} failed while proving exact macOS AX identity"),
    )
    .with_detail("ax_error", error)
}

fn ax_dispatch_error(operation: &str, error: i32) -> NativeError {
    NativeError::new(
        ErrorCode::DispatchFailed,
        ErrorPhase::Dispatch,
        false,
        format!("{operation} failed"),
    )
    .with_detail("ax_error", error)
}

fn ax_verification_error(operation: &str, error: i32) -> NativeError {
    verification_error(format!("{operation} failed")).with_detail("ax_error", error)
}

fn verification_error(message: impl Into<String>) -> NativeError {
    NativeError::new(
        ErrorCode::VerificationFailed,
        ErrorPhase::Verify,
        true,
        message,
    )
}

#[async_trait]
impl cua_driver_core::api::platform::SemanticActionProvider<MacTargetState> for MacSemanticActions {
    type PreparedAction = MacPreparedSemanticAction;

    async fn element_click_candidate(
        &self,
        target: &mut MacTargetState,
        element: &ResolvedElement,
        spec: &ClickSpec,
    ) -> Result<Candidate<()>, NativeError> {
        MacSemanticActions::element_click_candidate(self, target, element, spec).await
    }

    async fn element_scroll_candidate(
        &self,
        target: &mut MacTargetState,
        element: &ResolvedElement,
        spec: &ElementScrollSpec,
    ) -> Result<Candidate<()>, NativeError> {
        MacSemanticActions::element_scroll_candidate(self, target, element, spec)
    }

    async fn prepare(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<Self::PreparedAction, NativeError> {
        MacSemanticActions::prepare(self, target, scope, action).await
    }

    async fn dispatch(
        &self,
        target: &mut MacTargetState,
        scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: Self::PreparedAction,
    ) -> Result<NativeDispatch, NativeError> {
        MacSemanticActions::dispatch(self, target, scope, boundary, action).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::api::{contracts::Modifier, platform::ClickSpec};

    fn selection(
        text: &str,
        prefix: Option<&str>,
        suffix: Option<&str>,
        selection_type: SelectionType,
    ) -> SelectionSpec {
        SelectionSpec {
            text: text.to_owned(),
            prefix: prefix.map(str::to_owned),
            suffix: suffix.map(str::to_owned),
            selection_type,
        }
    }

    #[test]
    fn semantic_click_maps_exact_menu_actions_and_refuses_nonsemantic_shapes() {
        assert_eq!(
            click_action(
                "AXButton",
                None,
                &ClickSpec {
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: vec![],
                },
            )
            .unwrap(),
            AX_PRESS
        );
        assert_eq!(
            click_action(
                "AXButton",
                None,
                &ClickSpec {
                    button: MouseButton::Right,
                    click_count: 1,
                    modifiers: vec![],
                },
            )
            .unwrap(),
            AX_SHOW_MENU
        );
        for (role, subrole) in [
            ("AXPopUpButton", None),
            ("AXMenuButton", None),
            ("AXMenu", None),
            ("AXMenuItem", None),
            ("AXMenuBar", None),
            ("AXMenuBarItem", None),
            ("AXButton", Some("AXMenuButton")),
        ] {
            assert_eq!(
                click_action(
                    role,
                    subrole,
                    &ClickSpec {
                        button: MouseButton::Left,
                        click_count: 1,
                        modifiers: vec![],
                    },
                )
                .unwrap(),
                AX_PRESS
            );
        }
        for click in [
            ClickSpec {
                button: MouseButton::Middle,
                click_count: 1,
                modifiers: vec![],
            },
            ClickSpec {
                button: MouseButton::Left,
                click_count: 2,
                modifiers: vec![],
            },
            ClickSpec {
                button: MouseButton::Left,
                click_count: 1,
                modifiers: vec![Modifier::Shift],
            },
        ] {
            assert_eq!(
                click_action("AXButton", None, &click).unwrap_err().code,
                ErrorCode::UnsupportedInBackground
            );
        }
    }

    #[test]
    fn secondary_action_names_are_exact_and_case_sensitive() {
        let actions = vec!["AXConfirm".to_owned()];
        assert!(require_exact_action(&actions, "AXConfirm").is_ok());
        assert_eq!(
            require_exact_action(&actions, "axconfirm")
                .unwrap_err()
                .code,
            ErrorCode::UnsupportedInBackground
        );
    }

    #[test]
    fn ax_open_post_error_proof_requires_an_exact_title_transition() {
        assert!(ax_open_title_transition_matches("root", "alpha", "alpha"));
        assert!(!ax_open_title_transition_matches("root", "beta", "alpha"));
        assert!(!ax_open_title_transition_matches("alpha", "alpha", "alpha"));
        assert!(!ax_open_title_transition_matches("root", "", "alpha"));
    }

    #[test]
    fn selection_requires_one_contextual_match_and_uses_utf16_offsets() {
        let document = "😀 alpha target omega / target";
        let exact = selection(
            "target",
            Some("alpha "),
            Some(" omega"),
            SelectionType::Text,
        );
        assert_eq!(
            resolve_selection_range(document, &exact).unwrap(),
            AxCfRange {
                location: 9,
                length: 6
            }
        );
        let before = selection(
            "target",
            Some("alpha "),
            Some(" omega"),
            SelectionType::CursorBefore,
        );
        assert_eq!(
            resolve_selection_range(document, &before).unwrap(),
            AxCfRange {
                location: 9,
                length: 0
            }
        );
        let after = selection(
            "target",
            Some("alpha "),
            Some(" omega"),
            SelectionType::CursorAfter,
        );
        assert_eq!(
            resolve_selection_range(document, &after).unwrap(),
            AxCfRange {
                location: 15,
                length: 0
            }
        );
        assert_eq!(
            resolve_selection_range(
                document,
                &selection("target", None, None, SelectionType::Text),
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn overlapping_matches_are_not_silently_collapsed() {
        assert_eq!(
            resolve_selection_range("aaa", &selection("aa", None, None, SelectionType::Text),)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn typed_readback_is_exact_not_unicode_normalized_or_range_only() {
        assert!(exact_typed_cfstring_readback_matches(Some("é"), "é"));
        assert!(!exact_typed_cfstring_readback_matches(
            Some("e\u{301}"),
            "é"
        ));
        let expected = AxCfRange {
            location: 2,
            length: 1,
        };
        assert!(exact_typed_selection_readback_matches(
            Some(expected),
            Some("x"),
            expected,
            "x"
        ));
        assert!(!exact_typed_selection_readback_matches(
            Some(expected),
            Some("y"),
            expected,
            "x"
        ));
    }
}
