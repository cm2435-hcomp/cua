//! Process-wide v2 controller and invariant-preserving action pipeline.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::RwLock;

use super::{
    capabilities::{
        ActionKind, AddressingMode, CapabilityKey, CapabilityRegistry, Framework, PlatformName,
        RouteDecision, WindowStateKind,
    },
    contracts::{
        ActionId, ActionReceipt, AppRef, AxTreeMode, ClickCommand, ClickTarget, ClientId,
        DragCommand, EffectVerification, GetWindowStateRequest, LaunchAppRequest, LaunchResult,
        ListAppsRequest, ListWindowsRequest, ObservationId, PressKeyCommand, Readiness, Route,
        ScrollCommand, ScrollRequest, SecondaryActionCommand, SelectTextCommand, SetValueCommand,
        TypeTextCommand, VerificationLevel, WindowRef, WindowState,
    },
    errors::{ErrorCode, ErrorPhase, NativeError, PartialEvidence, PartialNativeDispatch},
    interaction::{MutationDeadline, ScopePlan, ScopeRequirements},
    observation::{
        revision_accessibility, InvalidationReason, ObservationRecord, ResolvedDrag,
        ResolvedScroll, ResolvedWindow,
    },
    platform::{
        ClickSpec, ElementScrollSpec, InteractionProvider, KeyboardActionProvider,
        LaunchPostureScope, LifecycleProvider, NativeDispatch, ObservationProvider, ObserveRequest,
        PlatformDriver, PointerActionProvider, ResolvedAction, SelectionSpec,
        SemanticActionProvider, WindowProvider,
    },
    settlement::{
        PendingSettlementEvidence, SettlementAttempt, SettlementEvidence, SettlementProfile,
        SettlementSignal,
    },
    target::{
        ProcessMutationLockRegistry, TargetControllerRegistry, TargetControllerState, TargetKey,
    },
};

const DEFAULT_TARGET_IDLE_TTL: Duration = Duration::from_secs(300);
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_MUTATION_WORK_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MUTATION_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub struct DriverController<P: PlatformDriver> {
    platform: Arc<P>,
    platform_name: PlatformName,
    os_version: String,
    capabilities: RwLock<CapabilityRegistry>,
    pub targets: Arc<TargetControllerRegistry<P>>,
    lock_timeout: Duration,
    mutation_work_timeout: Duration,
    mutation_teardown_timeout: Duration,
}

impl<P: PlatformDriver> DriverController<P> {
    pub fn new(
        platform: Arc<P>,
        platform_name: PlatformName,
        os_version: impl Into<String>,
    ) -> Self {
        let process_locks = Arc::new(ProcessMutationLockRegistry::default());
        let os_version = os_version.into();
        let capabilities = CapabilityRegistry::from_cells(platform.capability_cells(&os_version));
        Self {
            platform,
            platform_name,
            os_version,
            capabilities: RwLock::new(capabilities),
            targets: Arc::new(TargetControllerRegistry::new(
                process_locks,
                DEFAULT_TARGET_IDLE_TTL,
            )),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            mutation_work_timeout: DEFAULT_MUTATION_WORK_TIMEOUT,
            mutation_teardown_timeout: DEFAULT_MUTATION_TEARDOWN_TIMEOUT,
        }
    }

    /// Overrides the controller-owned mutation budget.
    ///
    /// `work_timeout` covers preflight, scope acquisition, native dispatch,
    /// and settlement. `teardown_timeout` is an additional reserve used only
    /// to release acquired leases while containment remains active.
    pub fn with_mutation_timeouts(
        mut self,
        work_timeout: Duration,
        teardown_timeout: Duration,
    ) -> Self {
        self.mutation_work_timeout = work_timeout;
        self.mutation_teardown_timeout = teardown_timeout;
        self
    }

    pub fn start_invalidation_loop(&self) -> tokio::task::JoinHandle<()> {
        let targets = Arc::clone(&self.targets);
        let platform = Arc::clone(&self.platform);
        let subscription = self.platform.subscribe_invalidations();
        tokio::spawn(async move {
            targets.invalidation_loop(platform, subscription).await;
        })
    }

    pub async fn check_readiness(&self) -> Result<Readiness, NativeError> {
        self.platform.lifecycle().readiness().await
    }

    pub async fn get_capabilities(
        &self,
    ) -> Result<super::capabilities::CapabilityManifest, NativeError> {
        let mut manifest = self.platform.lifecycle().capabilities().await?;
        manifest.platform = self.platform_name.clone();
        manifest.cells = self.capabilities.read().await.cells().collect();
        Ok(manifest)
    }

    pub async fn list_apps(&self, request: ListAppsRequest) -> Result<Vec<AppRef>, NativeError> {
        self.platform
            .lifecycle()
            .list_apps(request.query.unwrap_or_default())
            .await
    }

    pub async fn launch_app(&self, request: LaunchAppRequest) -> Result<LaunchResult, NativeError> {
        request.validate().map_err(NativeError::invalid)?;
        let action_id = ActionId::new();
        let mut posture_scope = LaunchPostureScope::for_action(action_id.clone());
        let launch = match self
            .platform
            .lifecycle()
            .launch_background(request.app, &mut posture_scope)
            .await
        {
            Ok(launch) => launch,
            Err(mut error) => {
                if posture_scope.side_effect_started() {
                    error.partial_evidence = Some(Box::new(PartialEvidence::Launch {
                        action_id,
                        app: posture_scope.partial_app,
                        windows: posture_scope.partial_windows,
                        posture: posture_scope.posture,
                        native_evidence: posture_scope.native_evidence,
                        pending_settlement: posture_scope.pending_settlement.map(Box::new),
                    }));
                }
                return Err(error);
            }
        };
        if !launch.posture.held {
            let mut error = NativeError::new(
                ErrorCode::PostureViolated,
                ErrorPhase::Verify,
                false,
                "background launch changed the user's foreground posture",
            )
            .with_detail("action_id", action_id.to_string());
            error.partial_evidence = Some(Box::new(PartialEvidence::Launch {
                action_id,
                app: Some(launch.app),
                windows: launch.windows,
                posture: launch.posture,
                native_evidence: posture_scope.native_evidence,
                pending_settlement: None,
            }));
            return Err(error);
        }
        Ok(LaunchResult {
            action_id,
            app: launch.app,
            windows: launch.windows,
            reused_running_app: launch.reused_running_app,
            verification: EffectVerification::EffectVerified,
            posture: launch.posture,
            settlement: launch.settlement,
        })
    }

    pub async fn list_windows(
        &self,
        request: ListWindowsRequest,
    ) -> Result<Vec<WindowRef>, NativeError> {
        self.platform
            .windows()
            .list_windows(request.app.as_ref())
            .await
    }

    pub async fn get_window(
        &self,
        request: super::contracts::GetWindowRequest,
    ) -> Result<WindowRef, NativeError> {
        self.platform
            .windows()
            .rehydrate(&request.window_id, request.app.as_ref())
            .await
    }

    pub async fn get_window_state(
        &self,
        client_id: &ClientId,
        request: GetWindowStateRequest,
    ) -> Result<WindowState, NativeError> {
        let resolved = self.platform.windows().resolve(&request.window).await?;
        ensure_public_window_matches(&request.window, &resolved)?;
        let key = TargetKey::from_window(client_id.clone(), &resolved);
        let target = self
            .targets
            .get_or_create(&self.platform, key, resolved.clone())
            .await?;
        let _process_guard = tokio::time::timeout(
            self.lock_timeout,
            Arc::clone(&target.mutation_lock).lock_owned(),
        )
        .await
        .map_err(|_| target_busy(&resolved))?;
        target.ensure_valid()?;
        let mut state = target.state.lock().await;
        ensure_target_window_matches(&state.window, &resolved)?;
        let settlement = self.settle_if_dirty(&mut state, true, None).await?;
        let native = self
            .platform
            .observation()
            .observe(
                &mut state.platform,
                &resolved,
                ObserveRequest {
                    include_text: request.include_text,
                    include_screenshots: request.include_screenshots,
                },
            )
            .await?;
        native.window.geometry.validate()?;
        // A coherent platform retry may legitimately return a newer geometry
        // revision. Preserve every stable identity component and accept that
        // refreshed geometry only after the platform completed A/capture/B.
        ensure_target_window_matches(&resolved, &native.window)?;
        let observed_window = native.window.clone();
        state.window = observed_window.clone();

        let observation_id = ObservationId::new();
        let tree_bytes = native
            .accessibility
            .as_ref()
            .map(|accessibility| accessibility.normalized_tree.len())
            .unwrap_or(0);
        let revisioned = native
            .accessibility
            .map(|accessibility| {
                revision_accessibility(
                    &state.ax_revisions,
                    &observation_id,
                    accessibility,
                    request.ax_tree_mode,
                )
            })
            .transpose()?;

        let mut surfaces = HashMap::new();
        for surface in native.surfaces {
            surface.validate_for_window(&observed_window)?;
            if surfaces.insert(surface.id.clone(), surface).is_some() {
                return Err(NativeError::invalid(
                    "native observation returned duplicate surface ids",
                ));
            }
        }
        if let super::menu::NativeMenuObservation::Open { surface_ids, .. } = &native.menu {
            if surface_ids.iter().any(|id| !surfaces.contains_key(id)) {
                return Err(NativeError::stale(
                    ErrorCode::MenuStateStale,
                    "native menu observation referenced a surface outside this observation",
                ));
            }
        }
        state
            .menu
            .reconcile_observation(native.menu, &observation_id)?;
        let menu = state.menu.observation()?;
        let public_surfaces = surfaces.values().map(|surface| surface.public()).collect();
        let accessibility = revisioned.as_ref().map(|state| state.public.clone());
        let approximate_bytes = tree_bytes
            + surfaces
                .values()
                .map(|surface| surface.approximate_bytes)
                .sum::<usize>();
        let public = WindowState {
            observation_id: observation_id.clone(),
            window: observed_window.public.clone(),
            surfaces: public_surfaces,
            accessibility,
            menu: menu.clone(),
            settlement: settlement.clone(),
            captured_at_unix_ms: native.captured_at_unix_ms,
            warnings: native.warnings,
        };
        let accessibility_record = revisioned.as_ref().map(|state| state.record.clone());
        state.observations.insert(ObservationRecord {
            id: observation_id,
            window: observed_window.stamp(),
            captured_at: Instant::now(),
            surfaces,
            accessibility: accessibility_record,
            menu,
            settlement,
            state: super::observation::ObservationState::Current,
            approximate_bytes,
            artifacts: native.artifacts,
        })?;
        if let Some(revisioned) = revisioned {
            state.ax_revisions.commit(revisioned.prepared_revision);
        }
        target.touch();
        Ok(public)
    }

    pub async fn click(
        &self,
        client_id: &ClientId,
        command: ClickCommand,
    ) -> Result<ActionReceipt, NativeError> {
        command.request.validate().map_err(NativeError::invalid)?;
        let observation_id = command.request.target.observation_id().clone();
        let addressing = match command.request.target {
            ClickTarget::Point { .. } => AddressingMode::CapturedPoint,
            ClickTarget::Element { .. } => AddressingMode::Element,
        };
        let menu_opening = command.request.button == super::contracts::MouseButton::Right;
        self.mutate(
            client_id,
            command.window,
            observation_id,
            ActionKind::Click,
            addressing,
            menu_opening,
            false,
            move |state, window| match command.request.target {
                ClickTarget::Point {
                    observation_id,
                    surface_id,
                    point,
                } => Ok(ResolvedAction::PointClick {
                    point: state.observations.resolve_point(
                        window,
                        &observation_id,
                        &surface_id,
                        point,
                    )?,
                    spec: ClickSpec {
                        button: command.request.button,
                        click_count: command.request.click_count,
                        modifiers: command.request.modifiers,
                    },
                }),
                ClickTarget::Element { element } => Ok(ResolvedAction::ElementClick {
                    source: element.clone(),
                    element: state.observations.resolve_element(window, &element)?,
                    spec: ClickSpec {
                        button: command.request.button,
                        click_count: command.request.click_count,
                        modifiers: command.request.modifiers,
                    },
                }),
            },
        )
        .await
    }

    pub async fn drag(
        &self,
        client_id: &ClientId,
        command: DragCommand,
    ) -> Result<ActionReceipt, NativeError> {
        command.request.validate().map_err(NativeError::invalid)?;
        let observation_id = command.request.observation_id.clone();
        self.mutate(
            client_id,
            command.window,
            observation_id.clone(),
            ActionKind::Drag,
            AddressingMode::CapturedPoint,
            false,
            false,
            move |state, window| {
                let start = state.observations.resolve_point(
                    window,
                    &observation_id,
                    &command.request.surface_id,
                    command.request.start,
                )?;
                let end = state.observations.resolve_point(
                    window,
                    &observation_id,
                    &command.request.surface_id,
                    command.request.end,
                )?;
                Ok(ResolvedAction::Drag(ResolvedDrag {
                    start,
                    end,
                    duration_ms: command.request.duration_ms,
                    button: command.request.button,
                    modifiers: command.request.modifiers,
                }))
            },
        )
        .await
    }

    pub async fn scroll(
        &self,
        client_id: &ClientId,
        command: ScrollCommand,
    ) -> Result<ActionReceipt, NativeError> {
        command.request.validate().map_err(NativeError::invalid)?;
        let observation_id = command.request.observation_id().clone();
        let addressing = match command.request {
            ScrollRequest::Delta { .. } => AddressingMode::CapturedPoint,
            ScrollRequest::Element { .. } => AddressingMode::Element,
        };
        self.mutate(
            client_id,
            command.window,
            observation_id,
            ActionKind::Scroll,
            addressing,
            false,
            false,
            move |state, window| match command.request {
                ScrollRequest::Delta {
                    observation_id,
                    surface_id,
                    point,
                    delta_x,
                    delta_y,
                } => Ok(ResolvedAction::DeltaScroll(ResolvedScroll {
                    point: state.observations.resolve_point(
                        window,
                        &observation_id,
                        &surface_id,
                        point,
                    )?,
                    delta_x,
                    delta_y,
                })),
                ScrollRequest::Element {
                    element,
                    direction,
                    pages,
                } => Ok(ResolvedAction::ElementScroll {
                    element: state.observations.resolve_element(window, &element)?,
                    spec: ElementScrollSpec { direction, pages },
                }),
            },
        )
        .await
    }

    pub async fn press_key(
        &self,
        client_id: &ClientId,
        command: PressKeyCommand,
    ) -> Result<ActionReceipt, NativeError> {
        command.request.validate().map_err(NativeError::invalid)?;
        let observation_id = command.request.observation_id.clone();
        self.mutate(
            client_id,
            command.window,
            observation_id.clone(),
            ActionKind::PressKey,
            AddressingMode::ObservedFocus,
            false,
            false,
            move |state, window| {
                let focus = state.observations.validate_focus(window, &observation_id)?;
                Ok(ResolvedAction::PressKey {
                    focus,
                    stroke: command.request.stroke,
                })
            },
        )
        .await
    }

    pub async fn type_text(
        &self,
        client_id: &ClientId,
        command: TypeTextCommand,
    ) -> Result<ActionReceipt, NativeError> {
        command.request.validate().map_err(NativeError::invalid)?;
        let observation_id = command.request.observation_id.clone();
        self.mutate(
            client_id,
            command.window,
            observation_id.clone(),
            ActionKind::TypeText,
            AddressingMode::ObservedFocus,
            false,
            false,
            move |state, window| {
                let focus = state.observations.validate_focus(window, &observation_id)?;
                Ok(ResolvedAction::TypeText {
                    focus,
                    text: command.request.text,
                })
            },
        )
        .await
    }

    pub async fn set_value(
        &self,
        client_id: &ClientId,
        command: SetValueCommand,
    ) -> Result<ActionReceipt, NativeError> {
        command.request.validate().map_err(NativeError::invalid)?;
        let observation_id = command.request.element.observation_id.clone();
        self.mutate(
            client_id,
            command.window,
            observation_id,
            ActionKind::SetValue,
            AddressingMode::Element,
            false,
            true,
            move |state, window| {
                Ok(ResolvedAction::SetValue {
                    element: state
                        .observations
                        .resolve_element(window, &command.request.element)?,
                    value: command.request.value,
                })
            },
        )
        .await
    }

    pub async fn select_text(
        &self,
        client_id: &ClientId,
        command: SelectTextCommand,
    ) -> Result<ActionReceipt, NativeError> {
        command.request.validate().map_err(NativeError::invalid)?;
        let observation_id = command.request.element.observation_id.clone();
        self.mutate(
            client_id,
            command.window,
            observation_id,
            ActionKind::SelectText,
            AddressingMode::Element,
            false,
            true,
            move |state, window| {
                Ok(ResolvedAction::SelectText {
                    element: state
                        .observations
                        .resolve_element(window, &command.request.element)?,
                    selection: SelectionSpec {
                        text: command.request.text,
                        prefix: command.request.prefix,
                        suffix: command.request.suffix,
                        selection_type: command.request.selection_type,
                    },
                })
            },
        )
        .await
    }

    pub async fn perform_secondary_action(
        &self,
        client_id: &ClientId,
        command: SecondaryActionCommand,
    ) -> Result<ActionReceipt, NativeError> {
        command.request.validate().map_err(NativeError::invalid)?;
        let observation_id = command.request.element.observation_id.clone();
        let menu_opening = is_menu_action(&command.request.action);
        self.mutate(
            client_id,
            command.window,
            observation_id,
            ActionKind::PerformSecondaryAction,
            AddressingMode::Element,
            menu_opening,
            false,
            move |state, window| {
                let element = state
                    .observations
                    .resolve_element(window, &command.request.element)?;
                if !element
                    .actions
                    .iter()
                    .any(|action| action == &command.request.action)
                {
                    return Err(NativeError::stale(
                        ErrorCode::ElementStale,
                        "secondary action was not exposed by the observed element",
                    ));
                }
                Ok(ResolvedAction::Secondary {
                    element,
                    action: command.request.action,
                })
            },
        )
        .await
    }

    pub async fn close_connection(&self, client_id: &ClientId) -> Result<usize, NativeError> {
        self.targets
            .close_connection(&self.platform, client_id)
            .await
    }

    pub async fn expire_idle_targets(&self) -> Result<usize, NativeError> {
        self.targets.expire_idle(&self.platform).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn mutate<F>(
        &self,
        client_id: &ClientId,
        public_window: WindowRef,
        observation_id: ObservationId,
        action: ActionKind,
        addressing: AddressingMode,
        menu_opening: bool,
        requires_effect_verification: bool,
        prepare: F,
    ) -> Result<ActionReceipt, NativeError>
    where
        F: FnOnce(
            &mut TargetControllerState<P>,
            &ResolvedWindow,
        ) -> Result<ResolvedAction, NativeError>,
    {
        let resolved = self.platform.windows().resolve(&public_window).await?;
        ensure_public_window_matches(&public_window, &resolved)?;
        let key = TargetKey::from_window(client_id.clone(), &resolved);
        let target = self.targets.get(&key).await?;
        let _process_guard = tokio::time::timeout(
            self.lock_timeout,
            Arc::clone(&target.mutation_lock).lock_owned(),
        )
        .await
        .map_err(|_| target_busy(&resolved))?;
        target.ensure_valid()?;
        let mut state = target.state.lock().await;
        ensure_target_window_matches(&state.window, &resolved)?;
        if state.settlement.settled_evidence().is_none() {
            let mut error = NativeError::new(
                ErrorCode::UiNotSettled,
                ErrorPhase::Settle,
                true,
                "target is still dirty; observe it to resume settlement before another mutation",
            );
            error.pending_settlement = state.settlement.pending_evidence().map(Box::new);
            return Err(error);
        }
        // Freshness and handle resolution precede capability routing so an
        // unsupported cell cannot hide a stale/cross-window request.
        let prepared = prepare(&mut state, &resolved)?;
        validate_current_menu_target(&state, &prepared)?;
        let targeted_menu_id = resolved_action_menu_id(&prepared).cloned();
        let capability_key = CapabilityKey {
            platform: self.platform_name.clone(),
            os_version: self.os_version.clone(),
            action: action.clone(),
            addressing,
            framework: resolved.framework.clone(),
            window_state: resolved.state.clone(),
        };
        let route = match self.capabilities.read().await.decision(&capability_key) {
            RouteDecision::Supported { route } => route,
            RouteDecision::Unsupported { reason } => {
                return Err(NativeError::unsupported(reason)
                    .with_detail("action", format!("{action:?}"))
                    .with_detail("framework", format!("{:?}", resolved.framework)))
            }
        };
        let prepared = adapt_action_route(&mut state, &resolved, route, prepared)?;
        let action_id = ActionId::new();
        let deadline =
            mutation_deadline(self.mutation_work_timeout, self.mutation_teardown_timeout)?;
        let mut requirements = ScopeRequirements::for_route(route);
        requirements.menu_dismissal = menu_opening;
        let preflight_result = {
            let TargetControllerState {
                platform, focus, ..
            } = &mut *state;
            tokio::time::timeout_at(
                deadline.work.into(),
                self.platform.interaction().preflight(
                    platform,
                    focus,
                    &action_id,
                    &resolved,
                    route,
                    &prepared,
                    deadline,
                    requirements,
                ),
            )
            .await
        };
        let mut scope_plan = match preflight_result {
            Ok(result) => result?,
            Err(_) => {
                return Err(mutation_deadline_error(
                    &action_id,
                    ErrorPhase::Preflight,
                    "preflight",
                ));
            }
        };
        ensure_scope_plan_matches(&scope_plan, &action_id, &resolved, route, deadline)?;
        let pending_menu_id = menu_opening.then(|| {
            state
                .menu
                .begin_open(action_id.clone(), resolved.public.clone(), resolved.stamp())
        });
        if let Some(menu_id) = pending_menu_id.clone() {
            scope_plan.bind_opening_menu(menu_id);
        }
        let cursor = state.logical_cursor.clone();
        let scope_result = {
            let TargetControllerState {
                platform, focus, ..
            } = &mut *state;
            tokio::time::timeout_at(
                deadline.work.into(),
                self.platform
                    .interaction()
                    .acquire_scope(platform, focus, scope_plan, cursor),
            )
            .await
        };
        let mut scope = match scope_result {
            Ok(Ok(scope)) => scope,
            Ok(Err(error)) => {
                if menu_opening {
                    state.menu.close();
                }
                return Err(error);
            }
            Err(_) => {
                if menu_opening {
                    state.menu.close();
                }
                // The timed-out acquisition future has been dropped, so all
                // provider-local RAII guards have had their cleanup chance.
                // Core has no complete teardown evidence, though, and cannot
                // safely reuse this target controller.
                target.invalidate();
                drop(state);
                let mut error =
                    mutation_deadline_error(&action_id, ErrorPhase::Preflight, "scope_acquisition")
                        .with_detail("cleanup_state", "uncertain");
                match tokio::time::timeout_at(
                    deadline.teardown.into(),
                    self.targets.remove_invalid_target(&self.platform, &key),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(removal_error)) => error = error.with_related(&removal_error),
                    Err(_) => {
                        error = error.with_related(&mutation_deadline_error(
                            &action_id,
                            ErrorPhase::Verify,
                            "poisoned_target_teardown",
                        ));
                    }
                }
                return Err(error);
            }
        };
        scope.bind_target_validity(target.validity_handle());
        let semantic_plan = if route == Route::Semantic {
            let prepare_result = {
                let platform = &mut state.platform;
                tokio::time::timeout_at(
                    deadline.work.into(),
                    self.platform
                        .semantic()
                        .prepare(platform, &mut scope, &prepared),
                )
                .await
            };
            match prepare_result {
                Ok(Ok(plan)) => Some(plan),
                Ok(Err(error)) => {
                    let teardown = scope.release();
                    let teardown_failed = !teardown.failures.is_empty();
                    if teardown_failed {
                        target.invalidate();
                    }
                    let mut failures = vec![error];
                    failures.extend(teardown.failures);
                    if let Some(error) = NativeError::from_posture(&scope.posture) {
                        failures.push(error);
                    }
                    let scope_evidence = serde_json::to_value(&scope.native_evidence)
                        .expect("typed scope evidence must serialize");
                    if teardown_failed {
                        drop(state);
                        match tokio::time::timeout_at(
                            deadline.teardown.into(),
                            self.targets.remove_invalid_target(&self.platform, &key),
                        )
                        .await
                        {
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) => failures.push(error),
                            Err(_) => failures.push(mutation_deadline_error(
                                &action_id,
                                ErrorPhase::Verify,
                                "poisoned_target_teardown",
                            )),
                        }
                    }
                    let mut primary = NativeError::primary(failures)
                        .expect("semantic prepare failure is nonempty");
                    primary
                        .details
                        .insert("interaction_scope".to_owned(), scope_evidence);
                    return Err(primary);
                }
                Err(_) => {
                    let mut failures = vec![mutation_deadline_error(
                        &action_id,
                        ErrorPhase::Preflight,
                        "semantic_prepare",
                    )];
                    let teardown = scope.release();
                    let teardown_failed = !teardown.failures.is_empty();
                    if teardown_failed {
                        target.invalidate();
                    }
                    failures.extend(teardown.failures);
                    if let Some(error) = NativeError::from_posture(&scope.posture) {
                        failures.push(error);
                    }
                    if teardown_failed {
                        drop(state);
                        match tokio::time::timeout_at(
                            deadline.teardown.into(),
                            self.targets.remove_invalid_target(&self.platform, &key),
                        )
                        .await
                        {
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) => failures.push(error),
                            Err(_) => failures.push(mutation_deadline_error(
                                &action_id,
                                ErrorPhase::Verify,
                                "poisoned_target_teardown",
                            )),
                        }
                    }
                    return Err(NativeError::primary(failures)
                        .expect("semantic prepare timeout failure is nonempty"));
                }
            }
        } else {
            None
        };
        if !menu_opening {
            if let Some(menu_id) = &targeted_menu_id {
                if let Err(error) = state.menu.begin_target(menu_id, action_id.clone()) {
                    let teardown = scope.release();
                    let teardown_failed = !teardown.failures.is_empty();
                    if teardown_failed {
                        target.invalidate();
                    }
                    let mut failures = vec![error];
                    failures.extend(teardown.failures);
                    if let Some(error) = NativeError::from_posture(&scope.posture) {
                        failures.push(error);
                    }
                    let scope_evidence = serde_json::to_value(&scope.native_evidence)
                        .expect("typed scope evidence must serialize");
                    if teardown_failed {
                        drop(state);
                        match tokio::time::timeout_at(
                            deadline.teardown.into(),
                            self.targets.remove_invalid_target(&self.platform, &key),
                        )
                        .await
                        {
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) => failures.push(error),
                            Err(_) => failures.push(mutation_deadline_error(
                                &action_id,
                                ErrorPhase::Verify,
                                "poisoned_target_teardown",
                            )),
                        }
                    }
                    let mut primary = NativeError::primary(failures).expect("non-empty failures");
                    primary
                        .details
                        .insert("interaction_scope".to_owned(), scope_evidence);
                    return Err(primary);
                }
            }
        }

        // Core owns the dispatch boundary. Once provider code is entered the
        // observation is consumed and target dirty even if the task is
        // cancelled while awaiting native dispatch.
        state
            .observations
            .consume(&observation_id, action_id.clone())?;
        state
            .observations
            .invalidate_all(InvalidationReason::MutationDispatched);
        let profile = settlement_profile(&action, menu_opening, requires_effect_verification);
        state.settlement.mark_dirty(action_id.clone(), profile)?;
        let dispatch_result = {
            let platform = &mut state.platform;
            tokio::time::timeout_at(
                deadline.work.into(),
                self.dispatch(platform, &mut scope, prepared, semantic_plan),
            )
            .await
        };
        let mut failures = Vec::new();
        let mut dispatch_timed_out = false;
        let dispatch = match dispatch_result {
            Ok(result) => {
                // This signal proves that provider control returned. A timed
                // out/cancelled provider must never manufacture it.
                state
                    .settlement
                    .record_signal(SettlementSignal::DispatchComplete)?;
                match result {
                    Ok(dispatch) => Some(dispatch),
                    Err(error) => {
                        if menu_opening || targeted_menu_id.is_some() {
                            state.menu.close();
                        }
                        failures.push(error);
                        None
                    }
                }
            }
            Err(_) => {
                dispatch_timed_out = true;
                if menu_opening || targeted_menu_id.is_some() {
                    state.menu.close();
                }
                failures.push(mutation_deadline_error(
                    &action_id,
                    ErrorPhase::Dispatch,
                    "dispatch",
                ));
                None
            }
        };

        if let Some(menu_evidence) = dispatch
            .as_ref()
            .and_then(|dispatch| dispatch.menu.as_ref())
        {
            if let Err(error) =
                state
                    .menu
                    .record_dispatch_evidence(&action_id, &resolved.stamp(), menu_evidence)
            {
                state.menu.close();
                failures.push(error);
            }
        } else if (menu_opening || targeted_menu_id.is_some()) && dispatch.is_some() {
            state.menu.close();
            failures.push(NativeError::stale(
                ErrorCode::MenuStateStale,
                "menu dispatch returned no exact native menu/action/owner identity evidence",
            ));
        }

        let mut settlement = None;
        let mut pending_settlement = if dispatch_timed_out {
            state.settlement.pending_evidence().map(Box::new)
        } else {
            None
        };
        if !dispatch_timed_out {
            match self
                .settle_if_dirty(&mut state, false, Some(deadline.work))
                .await
            {
                Ok(evidence) => settlement = Some(evidence),
                Err(error) => {
                    pending_settlement = error.pending_settlement.clone();
                    failures.push(error);
                }
            }
        }

        let teardown = scope.release();
        let teardown_failed = !teardown.failures.is_empty();
        if teardown_failed {
            target.invalidate();
        }
        failures.extend(teardown.failures);
        if let Some(error) = NativeError::from_posture(&scope.posture) {
            failures.push(error);
        }
        if requires_effect_verification
            && dispatch
                .as_ref()
                .is_some_and(|dispatch| dispatch.verification != VerificationLevel::EffectVerified)
        {
            failures.push(NativeError::new(
                ErrorCode::VerificationFailed,
                ErrorPhase::Verify,
                true,
                "semantic mutation did not provide its promised exact readback",
            ));
        }

        let mut native_evidence = scope.native_evidence.clone();
        if let Some(dispatch) = &dispatch {
            native_evidence
                .fields
                .extend(dispatch.evidence.fields.clone());
        }
        let partial = PartialEvidence::Action {
            action_id: action_id.clone(),
            window: resolved.public.clone(),
            consumed_observation_id: observation_id.clone(),
            route,
            dispatch: dispatch.as_ref().map(|dispatch| PartialNativeDispatch {
                verification: dispatch.verification,
                native_evidence: dispatch.evidence.clone(),
                warnings: dispatch.warnings.clone(),
            }),
            posture: scope.posture.clone(),
            native_evidence: scope.native_evidence.clone(),
            pending_settlement,
        };
        if teardown_failed {
            drop(state);
            match tokio::time::timeout_at(
                deadline.teardown.into(),
                self.targets.remove_invalid_target(&self.platform, &key),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => failures.push(error),
                Err(_) => failures.push(mutation_deadline_error(
                    &action_id,
                    ErrorPhase::Verify,
                    "poisoned_target_teardown",
                )),
            }
        }
        if let Some(mut error) = NativeError::primary(failures) {
            error.partial_evidence = Some(Box::new(partial));
            return Err(error);
        }

        let receipt = match (dispatch, settlement) {
            (Some(dispatch), Some(settlement)) => Some(ActionReceipt {
                action_id,
                window: resolved.public,
                consumed_observation_id: observation_id,
                route,
                verification: dispatch.verification,
                posture: scope.posture.clone(),
                settlement,
                native_evidence,
                warnings: dispatch.warnings,
            }),
            _ => None,
        };
        target.touch();
        receipt.ok_or_else(|| {
            NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Verify,
                false,
                "mutation completed without either a receipt or a typed failure",
            )
        })
    }

    async fn settle_if_dirty(
        &self,
        state: &mut TargetControllerState<P>,
        resumed_from_prior_call: bool,
        mutation_work_deadline: Option<Instant>,
    ) -> Result<SettlementEvidence, NativeError> {
        let Some(dirty) = state.settlement.begin(resumed_from_prior_call) else {
            return state
                .settlement
                .settled_evidence()
                .cloned()
                .ok_or_else(|| NativeError::invalid("settlement state has no settled evidence"));
        };
        let profile_deadline = Instant::now() + Duration::from_millis(dirty.profile.deadline_ms);
        let deadline = mutation_work_deadline
            .map(|mutation_deadline| profile_deadline.min(mutation_deadline))
            .unwrap_or(profile_deadline);
        let settlement_result = tokio::time::timeout_at(
            deadline.into(),
            self.platform
                .observation()
                .settle(&mut state.platform, &dirty, deadline),
        )
        .await;
        match settlement_result {
            Err(_) => {
                let pending: PendingSettlementEvidence = dirty.pending_evidence();
                state.settlement.preserve_pending(&pending)?;
                let mut error = NativeError::new(
                    ErrorCode::UiNotSettled,
                    ErrorPhase::Settle,
                    true,
                    "native settlement exceeded the controller-owned deadline",
                )
                .with_detail("deadline_stage", "settlement");
                error.pending_settlement = Some(Box::new(pending));
                Err(error)
            }
            Ok(Ok(SettlementAttempt::Settled(evidence))) => state.settlement.complete(evidence),
            Ok(Ok(SettlementAttempt::Pending(pending))) => {
                state.settlement.preserve_pending(&pending)?;
                let mut error = NativeError::new(
                    ErrorCode::UiNotSettled,
                    ErrorPhase::Settle,
                    true,
                    "target did not settle before the action-specific deadline",
                );
                error.pending_settlement = Some(Box::new(pending));
                Err(error)
            }
            Ok(Err(mut error)) => {
                let pending: PendingSettlementEvidence = dirty.pending_evidence();
                state.settlement.preserve_pending(&pending)?;
                error = NativeError::new(
                    ErrorCode::UiNotSettled,
                    ErrorPhase::Settle,
                    true,
                    format!("native settlement failed: {}", error.message),
                )
                .with_related(&error);
                error.pending_settlement = Some(Box::new(pending));
                Err(error)
            }
        }
    }

    async fn dispatch(
        &self,
        target: &mut P::TargetState,
        scope: &mut super::interaction::InteractionScope,
        mutation: ResolvedAction,
        semantic_plan: Option<
            <P::Semantic as SemanticActionProvider<P::TargetState>>::PreparedAction,
        >,
    ) -> Result<NativeDispatch, NativeError> {
        if scope.route == Route::Semantic {
            let plan = semantic_plan.ok_or_else(|| {
                NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Dispatch,
                    false,
                    "semantic dispatch entered without its retained prepare plan",
                )
            })?;
            return self.platform.semantic().dispatch(target, scope, plan).await;
        }
        if semantic_plan.is_some() {
            return Err(NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Dispatch,
                false,
                "non-semantic dispatch received a semantic prepare plan",
            ));
        }
        match mutation {
            ResolvedAction::PointClick { point, spec } => {
                self.platform.pointer().click(scope, point, spec).await
            }
            ResolvedAction::Drag(drag) => self.platform.pointer().drag(scope, drag).await,
            ResolvedAction::DeltaScroll(scroll) => {
                self.platform.pointer().scroll(scope, scroll).await
            }
            ResolvedAction::PressKey { focus, stroke } => {
                self.platform
                    .keyboard()
                    .press_key(scope, &focus, stroke)
                    .await
            }
            ResolvedAction::TypeText { focus, text } => {
                self.platform
                    .keyboard()
                    .type_text(scope, &focus, &text)
                    .await
            }
            ResolvedAction::ElementClick { .. }
            | ResolvedAction::ElementScroll { .. }
            | ResolvedAction::SetValue { .. }
            | ResolvedAction::SelectText { .. }
            | ResolvedAction::Secondary { .. } => Err(NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Dispatch,
                false,
                "element semantic action reached a non-semantic dispatch route",
            )),
        }
    }
}

fn adapt_action_route<P: PlatformDriver>(
    state: &mut TargetControllerState<P>,
    window: &ResolvedWindow,
    route: Route,
    action: ResolvedAction,
) -> Result<ResolvedAction, NativeError> {
    match action {
        ResolvedAction::ElementClick {
            source,
            element,
            spec,
        } if route == Route::Semantic => Ok(ResolvedAction::ElementClick {
            source,
            element,
            spec,
        }),
        ResolvedAction::ElementClick { source, spec, .. } if route == Route::TargetedPointer => {
            Ok(ResolvedAction::PointClick {
                point: state.observations.resolve_element_point(window, &source)?,
                spec,
            })
        }
        ResolvedAction::PointClick { .. } | ResolvedAction::Drag(_)
            if route == Route::TargetedPointer =>
        {
            Ok(action)
        }
        ResolvedAction::ElementScroll { .. }
        | ResolvedAction::SetValue { .. }
        | ResolvedAction::SelectText { .. }
        | ResolvedAction::Secondary { .. }
            if route == Route::Semantic =>
        {
            Ok(action)
        }
        ResolvedAction::DeltaScroll(_)
            if matches!(route, Route::Semantic | Route::TargetedPointer) =>
        {
            Ok(action)
        }
        ResolvedAction::PressKey { .. } if route == Route::TargetedKeyboard => Ok(action),
        ResolvedAction::TypeText { .. }
            if matches!(route, Route::Semantic | Route::TargetedKeyboard) =>
        {
            Ok(action)
        }
        _ => Err(NativeError::unsupported(format!(
            "resolved action is incompatible with selected route {route:?}"
        ))),
    }
}

fn validate_current_menu_target<P: PlatformDriver>(
    state: &TargetControllerState<P>,
    action: &ResolvedAction,
) -> Result<(), NativeError> {
    let menu_id = match action {
        ResolvedAction::ElementClick { element, .. }
        | ResolvedAction::ElementScroll { element, .. }
        | ResolvedAction::SetValue { element, .. }
        | ResolvedAction::SelectText { element, .. }
        | ResolvedAction::Secondary { element, .. } => element.menu_id.as_ref(),
        _ => None,
    };
    if let Some(menu_id) = menu_id {
        state.menu.validate_current_menu_id(menu_id)?;
    }
    Ok(())
}

fn resolved_action_menu_id(action: &ResolvedAction) -> Option<&super::contracts::MenuId> {
    match action {
        ResolvedAction::ElementClick { element, .. }
        | ResolvedAction::ElementScroll { element, .. }
        | ResolvedAction::SetValue { element, .. }
        | ResolvedAction::SelectText { element, .. }
        | ResolvedAction::Secondary { element, .. } => element.menu_id.as_ref(),
        _ => None,
    }
}

fn ensure_public_window_matches(
    requested: &WindowRef,
    resolved: &ResolvedWindow,
) -> Result<(), NativeError> {
    if requested.id != resolved.public.id || requested.app.id != resolved.public.app.id {
        return Err(NativeError::stale(
            ErrorCode::WindowIdentityChanged,
            "resolved window does not match the supplied app/window identity",
        ));
    }
    resolved.geometry.validate()?;
    Ok(())
}

fn ensure_target_window_matches(
    target: &ResolvedWindow,
    resolved: &ResolvedWindow,
) -> Result<(), NativeError> {
    if target.public.id != resolved.public.id
        || target.public.app.id != resolved.public.app.id
        || target.generation != resolved.generation
        || target.process != resolved.process
        || target.native != resolved.native
    {
        return Err(NativeError::stale(
            ErrorCode::WindowIdentityChanged,
            "live target no longer matches the target controller identity/generation",
        ));
    }
    Ok(())
}

fn ensure_scope_plan_matches<NativePlan>(
    plan: &ScopePlan<NativePlan>,
    action_id: &ActionId,
    window: &ResolvedWindow,
    route: Route,
    deadline: MutationDeadline,
) -> Result<(), NativeError> {
    if &plan.action_id != action_id
        || plan.window.stamp() != window.stamp()
        || plan.route != route
        || plan.deadline != deadline
    {
        return Err(NativeError::new(
            ErrorCode::Internal,
            ErrorPhase::Preflight,
            false,
            "interaction preflight returned a plan for a different action, route, target, or deadline",
        )
        .with_detail("expected_action_id", action_id.to_string())
        .with_detail("planned_action_id", plan.action_id.to_string())
        .with_detail("expected_route", format!("{route:?}"))
        .with_detail("planned_route", format!("{:?}", plan.route)));
    }
    Ok(())
}

fn mutation_deadline(
    work_timeout: Duration,
    teardown_timeout: Duration,
) -> Result<MutationDeadline, NativeError> {
    let started_at = Instant::now();
    let work = started_at.checked_add(work_timeout).ok_or_else(|| {
        NativeError::new(
            ErrorCode::Internal,
            ErrorPhase::Preflight,
            false,
            "configured mutation work timeout exceeds the platform clock range",
        )
    })?;
    let teardown = work.checked_add(teardown_timeout).ok_or_else(|| {
        NativeError::new(
            ErrorCode::Internal,
            ErrorPhase::Preflight,
            false,
            "configured mutation teardown timeout exceeds the platform clock range",
        )
    })?;
    MutationDeadline::new(work, teardown)
}

fn mutation_deadline_error(
    action_id: &ActionId,
    phase: ErrorPhase,
    stage: &'static str,
) -> NativeError {
    NativeError::new(
        ErrorCode::DispatchFailed,
        phase,
        false,
        format!("mutation exceeded the controller-owned deadline during {stage}"),
    )
    .with_detail("action_id", action_id.to_string())
    .with_detail("deadline_stage", stage)
}

fn settlement_profile(
    action: &ActionKind,
    menu_opening: bool,
    exact_readback: bool,
) -> SettlementProfile {
    if exact_readback {
        return SettlementProfile::requiring(
            format!("{action:?}_exact_readback").to_lowercase(),
            [SettlementSignal::VerificationReadbackComplete],
        )
        .with_relevant_signals([
            SettlementSignal::AxValueChanged,
            SettlementSignal::FocusChanged,
            SettlementSignal::VerificationReadbackComplete,
        ]);
    }
    if menu_opening {
        return SettlementProfile::requiring(
            format!("{action:?}_menu_open").to_lowercase(),
            [SettlementSignal::MenuOpened],
        )
        .with_relevant_signals([
            SettlementSignal::MenuOpened,
            SettlementSignal::MenuDismissed,
            SettlementSignal::WindowListChanged,
            SettlementSignal::FocusChanged,
        ]);
    }
    let relevant = match action {
        ActionKind::Click => vec![
            SettlementSignal::AxAction,
            SettlementSignal::AxValueChanged,
            SettlementSignal::FocusChanged,
            SettlementSignal::WindowListChanged,
            SettlementSignal::MenuOpened,
            SettlementSignal::MenuDismissed,
        ],
        ActionKind::Drag => vec![
            SettlementSignal::AxAction,
            SettlementSignal::AxValueChanged,
            SettlementSignal::WindowGeometryChanged,
        ],
        ActionKind::Scroll => vec![
            SettlementSignal::ScrollChanged,
            SettlementSignal::AxValueChanged,
        ],
        ActionKind::PressKey | ActionKind::TypeText | ActionKind::SelectText => vec![
            SettlementSignal::AxValueChanged,
            SettlementSignal::FocusChanged,
        ],
        ActionKind::SetValue => vec![
            SettlementSignal::AxValueChanged,
            SettlementSignal::VerificationReadbackComplete,
        ],
        ActionKind::PerformSecondaryAction => vec![
            SettlementSignal::AxAction,
            SettlementSignal::MenuOpened,
            SettlementSignal::MenuDismissed,
            SettlementSignal::WindowListChanged,
        ],
    };
    SettlementProfile::dispatch_only(format!("{action:?}").to_lowercase())
        .with_relevant_signals(relevant)
}

fn target_busy(window: &ResolvedWindow) -> NativeError {
    NativeError::new(
        ErrorCode::TargetBusy,
        ErrorPhase::Preflight,
        true,
        "timed out acquiring the bounded process mutation lock",
    )
    .with_detail("window_id", window.public.id.to_string())
    .with_detail("app_id", window.public.app.id.to_string())
}

fn is_menu_action(action: &str) -> bool {
    matches!(action, "show_menu" | "AXShowMenu")
}

#[allow(dead_code)]
fn _type_assertions(_: Framework, _: WindowStateKind, _: AxTreeMode) {}
