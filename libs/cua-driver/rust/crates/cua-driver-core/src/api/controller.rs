//! Process-wide v2 controller and invariant-preserving action pipeline.

use std::{
    collections::{BTreeMap, HashMap},
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
    dispatch::{DispatchGuardRegistry, DispatchScope},
    errors::{ErrorCode, ErrorPhase, NativeError, PartialEvidence, PartialNativeDispatch},
    interaction::{
        MutationDeadline, NativeEvidence, NativeSideEffectBoundary, ScopePlan, ScopeRequirements,
    },
    menu::{MenuLifecycle, MenuMutationIntent},
    observation::{
        revision_accessibility, ObservationRecord, ResolvedDrag, ResolvedScroll, ResolvedWindow,
    },
    platform::{
        Candidate, ClickSpec, ElementScrollSpec, InteractionProvider, KeyboardActionProvider,
        LaunchScope, LifecycleProvider, NativeDispatch, ObservationProvider, ObserveRequest,
        PlatformDriver, PointerActionProvider, ResolvedAction, SelectionSpec,
        SemanticActionProvider, WindowProvider,
    },
    settlement::{
        PendingSettlementEvidence, SettlementAttempt, SettlementEvidence, SettlementProfile,
        SettlementSignal,
    },
    target::{TargetControllerRegistry, TargetControllerState, TargetKey},
};

const DEFAULT_TARGET_IDLE_TTL: Duration = Duration::from_secs(300);
const DEFAULT_MUTATION_WORK_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MUTATION_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

enum PreparedDispatch<P: PlatformDriver> {
    Semantic(<P::Semantic as SemanticActionProvider<P::TargetState>>::PreparedAction),
    Pointer(<P::Pointer as PointerActionProvider<P::TargetState>>::PreparedAction),
    Keyboard(<P::Keyboard as KeyboardActionProvider<P::TargetState>>::PreparedAction),
}

pub struct DriverController<P: PlatformDriver> {
    platform: Arc<P>,
    platform_name: PlatformName,
    os_version: String,
    capabilities: RwLock<CapabilityRegistry>,
    pub targets: Arc<TargetControllerRegistry<P>>,
    dispatch_guards: DispatchGuardRegistry,
    mutation_work_timeout: Duration,
    mutation_teardown_timeout: Duration,
}

impl<P: PlatformDriver> DriverController<P> {
    pub fn new(
        platform: Arc<P>,
        platform_name: PlatformName,
        os_version: impl Into<String>,
    ) -> Self {
        let os_version = os_version.into();
        let capabilities = CapabilityRegistry::from_cells(platform.capability_cells(&os_version));
        Self {
            platform,
            platform_name,
            os_version,
            capabilities: RwLock::new(capabilities),
            targets: Arc::new(TargetControllerRegistry::new(DEFAULT_TARGET_IDLE_TTL)),
            dispatch_guards: DispatchGuardRegistry::default(),
            mutation_work_timeout: DEFAULT_MUTATION_WORK_TIMEOUT,
            mutation_teardown_timeout: DEFAULT_MUTATION_TEARDOWN_TIMEOUT,
        }
    }

    /// Overrides the controller-owned mutation budget.
    ///
    /// `work_timeout` covers preflight, scope acquisition, native dispatch,
    /// and settlement. `teardown_timeout` is an additional reserve used only
    /// to release acquired platform resources.
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
        let _dispatch_permit = self.dispatch_guards.try_acquire(DispatchScope::Desktop)?;
        let mut launch_scope = LaunchScope::for_action(action_id.clone());
        let launch = match self
            .platform
            .lifecycle()
            .launch_background(request.app, &mut launch_scope)
            .await
        {
            Ok(launch) => launch,
            Err(mut error) => {
                if launch_scope.side_effect_started() {
                    error.partial_evidence = Some(Box::new(PartialEvidence::Launch {
                        action_id,
                        app: launch_scope.partial_app,
                        windows: launch_scope.partial_windows,
                        native_evidence: launch_scope.native_evidence,
                        pending_settlement: launch_scope.pending_settlement.map(Box::new),
                    }));
                }
                return Err(error);
            }
        };
        Ok(LaunchResult {
            action_id,
            app: launch.app,
            windows: launch.windows,
            reused_running_app: launch.reused_running_app,
            verification: EffectVerification::EffectVerified,
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
        tracing::debug!(
            target_instance_id = %target.instance_id,
            operation = "get_window_state",
            "v2 target controller acquired"
        );
        let _dispatch_permit = self
            .dispatch_guards
            .try_acquire(DispatchScope::Process(resolved.process.clone()))?;
        target.ensure_valid()?;
        let mut state = target
            .state
            .try_lock()
            .map_err(|_| target_busy(&resolved))?;
        ensure_target_window_matches(&state.window, &resolved)?;
        let settlement = self.settle_if_dirty(&mut state, true, None).await?;
        let mut native = self
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
        if let MenuLifecycle::Open {
            id,
            native_identity,
            ..
        } = state.menu.lifecycle()
        {
            for surface in &mut native.surfaces {
                if surface.kind == super::contracts::SurfaceKind::Menu
                    && surface.owner_window.id != observed_window.public.id
                {
                    surface.menu_id = Some(id.clone());
                }
            }
            if let Some(accessibility) = &mut native.accessibility {
                for element in &mut accessibility.elements {
                    if element.owner.native_window == native_identity.window {
                        element.menu_id = Some(id.clone());
                    }
                }
            }
        }

        let observation_id = ObservationId::new();
        let tree_bytes = native
            .accessibility
            .as_ref()
            .map(|accessibility| accessibility.normalized_tree.len())
            .unwrap_or(0);
        let ax_base_revision = state.ax_revisions.last_revision().map(ToString::to_string);
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
        let ax_result_revision = revisioned
            .as_ref()
            .map(|revisioned| revisioned.public.tree_update.revision().to_string());

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
        tracing::info!(
            target_instance_id = %target.instance_id,
            ax_base_revision = ax_base_revision.as_deref().unwrap_or("none"),
            ax_result_revision = ax_result_revision.as_deref().unwrap_or("none"),
            settlement_profile = %public.settlement.profile,
            "v2 window observation committed"
        );
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
                Ok(ResolvedAction::Drag(Box::new(ResolvedDrag {
                    start,
                    end,
                    duration_ms: command.request.duration_ms,
                    button: command.request.button,
                    modifiers: command.request.modifiers,
                })))
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
                    source: element.clone(),
                    element: Box::new(state.observations.resolve_element(window, &element)?),
                    point: None,
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
        if command.request.text.is_empty() {
            return self
                .type_text_noop(client_id, command.window, observation_id)
                .await;
        }
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

    async fn type_text_noop(
        &self,
        client_id: &ClientId,
        public_window: WindowRef,
        observation_id: ObservationId,
    ) -> Result<ActionReceipt, NativeError> {
        let resolved = self.platform.windows().resolve(&public_window).await?;
        ensure_public_window_matches(&public_window, &resolved)?;
        let key = TargetKey::from_window(client_id.clone(), &resolved);
        let target = self.targets.get(&key).await?;
        target.ensure_valid()?;
        let mut state = target
            .state
            .try_lock()
            .map_err(|_| target_busy(&resolved))?;
        ensure_target_window_matches(&state.window, &resolved)?;
        let settlement = state
            .settlement
            .settled_evidence()
            .cloned()
            .ok_or_else(|| {
                let mut error = NativeError::new(
                    ErrorCode::UiNotSettled,
                    ErrorPhase::Settle,
                    true,
                    "target is still dirty; observe it to resume settlement before another mutation",
                );
                error.pending_settlement = state.settlement.pending_evidence().map(Box::new);
                error
            })?;
        state
            .observations
            .validate_focus(&resolved, &observation_id)?;
        let action_id = ActionId::new();
        let native_evidence = NativeEvidence {
            fields: BTreeMap::from([
                ("route_detail".to_owned(), "empty_text_noop".into()),
                ("primitive".to_owned(), "empty_text_noop".into()),
                ("native_side_effect_started".to_owned(), false.into()),
                ("hardware_cursor_warp_attempted".to_owned(), false.into()),
            ]),
            interaction_scope: None,
        };
        let receipt = ActionReceipt {
            action_id,
            window: resolved.public,
            consumed_observation_id: observation_id,
            route: Route::Semantic,
            verification: VerificationLevel::EffectVerified,
            settlement,
            native_evidence,
            warnings: Vec::new(),
        };
        target.touch();
        Ok(receipt)
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
        target.ensure_valid()?;
        let mut state = target
            .state
            .try_lock()
            .map_err(|_| target_busy(&resolved))?;
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
        // Freshness and handle resolution precede live route selection. The
        // capability registry is release evidence only and cannot select or
        // refuse a route.
        let prepared = prepare(&mut state, &resolved)?;
        validate_current_menu_target(&state, &prepared)?;
        let targeted_menu_id = resolved_action_menu_id(&prepared).cloned();
        let (route, prepared, effective_addressing, candidate_detail) = match prepared {
            ResolvedAction::ElementClick {
                source,
                element,
                spec,
            } => match self
                .platform
                .semantic()
                .element_click_candidate(&mut state.platform, &element, &spec)
                .await?
            {
                Candidate::Prepared(()) => (
                    Route::Semantic,
                    ResolvedAction::ElementClick {
                        source,
                        element,
                        spec,
                    },
                    AddressingMode::Element,
                    "semantic_element_click".to_owned(),
                ),
                Candidate::NotApplicable { reason } => {
                    let point = state
                        .observations
                        .resolve_element_point(&resolved, &source)?;
                    (
                        Route::TargetedPointer,
                        ResolvedAction::PointClick { point, spec },
                        AddressingMode::CapturedPoint,
                        format!("semantic_not_applicable:{reason}"),
                    )
                }
            },
            ResolvedAction::ElementScroll {
                source,
                element,
                point: _,
                spec,
            } => match self
                .platform
                .semantic()
                .element_scroll_candidate(&mut state.platform, &element, &spec)
                .await?
            {
                Candidate::Prepared(()) => (
                    Route::Semantic,
                    ResolvedAction::ElementScroll {
                        source,
                        element,
                        point: None,
                        spec,
                    },
                    AddressingMode::Element,
                    "semantic_page_scroll".to_owned(),
                ),
                Candidate::NotApplicable { reason } => {
                    let point = state
                        .observations
                        .resolve_element_point(&resolved, &source)?;
                    (
                        Route::TargetedPointer,
                        ResolvedAction::ElementScroll {
                            source,
                            element,
                            point: Some(point),
                            spec,
                        },
                        AddressingMode::CapturedPoint,
                        format!("semantic_not_applicable:{reason}"),
                    )
                }
            },
            ResolvedAction::TypeText { focus, text } => match self
                .platform
                .keyboard()
                .semantic_type_text_candidate(&mut state.platform, &focus)
                .await?
            {
                Candidate::Prepared(()) => (
                    Route::Semantic,
                    ResolvedAction::TypeText { focus, text },
                    AddressingMode::ObservedFocus,
                    "semantic_focused_text_insertion".to_owned(),
                ),
                Candidate::NotApplicable { reason } => (
                    Route::TargetedKeyboard,
                    ResolvedAction::TypeText { focus, text },
                    AddressingMode::ObservedFocus,
                    format!("semantic_not_applicable:{reason}"),
                ),
            },
            prepared => {
                let route = route_for_live_action(&prepared);
                (
                    route,
                    adapt_action_route(&mut state, &resolved, route, prepared)?,
                    addressing,
                    "direct_live_provider".to_owned(),
                )
            }
        };
        let evidence_key = capability_key(
            &self.platform_name,
            &self.os_version,
            &action,
            effective_addressing,
            &resolved,
        );
        let capability_evidence = self
            .capabilities
            .read()
            .await
            .evidence(&evidence_key)
            .cloned();
        let action_id = ActionId::new();
        tracing::info!(
            target_instance_id = %target.instance_id,
            action_id = %action_id,
            action_kind = ?action,
            route = ?route,
            "v2 mutation target and route acquired"
        );
        let deadline =
            mutation_deadline(self.mutation_work_timeout, self.mutation_teardown_timeout)?;
        let menu_intent = if menu_opening {
            let menu_id =
                state
                    .menu
                    .begin_open(action_id.clone(), resolved.public.clone(), resolved.stamp());
            Some(MenuMutationIntent::Opening { menu_id })
        } else if let Some(menu_id) = targeted_menu_id.clone() {
            let identity = state.menu.native_identity().cloned().ok_or_else(|| {
                NativeError::stale(
                    ErrorCode::MenuStateStale,
                    "targeted menu action has no current native identity",
                )
            })?;
            state.menu.begin_target(&menu_id, action_id.clone())?;
            Some(MenuMutationIntent::Targeting { menu_id, identity })
        } else if matches!(prepared, ResolvedAction::PointClick { .. })
            || is_escape_key_action(&prepared)
        {
            match state.menu.lifecycle() {
                MenuLifecycle::Open { id, .. } => {
                    let menu_id = id.clone();
                    let identity = state
                        .menu
                        .native_identity()
                        .cloned()
                        .expect("open menu retains exact native identity");
                    state.menu.begin_dismiss(action_id.clone())?;
                    Some(MenuMutationIntent::Dismissing { menu_id, identity })
                }
                MenuLifecycle::Closed => None,
                _ => {
                    return Err(NativeError::stale(
                        ErrorCode::MenuStateStale,
                        "a point mutation cannot start while a prior menu transition is unresolved",
                    ))
                }
            }
        } else {
            match state.menu.lifecycle() {
                MenuLifecycle::Closed => None,
                MenuLifecycle::Open { .. } => None,
                _ => {
                    return Err(NativeError::stale(
                        ErrorCode::MenuStateStale,
                        "mutation cannot start while a prior menu transition is unresolved",
                    ))
                }
            }
        };
        let mut requirements = ScopeRequirements::for_route(route);
        // The signed helper's element-click pipeline always passes through
        // SyntheticAppFocusEnforcer before AXPress or pointer fallback. An
        // inactive AppKit menu can acknowledge AXPress while doing nothing
        // unless the target process first believes it has key focus.
        if matches!(prepared, ResolvedAction::ElementClick { .. }) {
            requirements.target_belief = true;
        }
        requirements.menu_dismissal = menu_intent.is_some();
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
            Ok(Ok(plan)) => plan,
            Ok(Err(error)) => {
                if menu_intent.is_some() {
                    state.menu.abort_transition(&action_id)?;
                }
                return Err(error);
            }
            Err(_) => {
                if menu_intent.is_some() {
                    state.menu.abort_transition(&action_id)?;
                }
                return Err(mutation_deadline_error(
                    &action_id,
                    ErrorPhase::Preflight,
                    "preflight",
                ));
            }
        };
        if let Err(error) =
            ensure_scope_plan_matches(&scope_plan, &action_id, &resolved, route, deadline)
        {
            if menu_intent.is_some() {
                state.menu.abort_transition(&action_id)?;
            }
            return Err(error);
        }
        let dispatch_scope_kind = scope_plan.dispatch_scope;
        let dispatch_scope =
            DispatchScope::materialize(dispatch_scope_kind, key.clone(), resolved.process.clone());
        let _dispatch_permit = match self.dispatch_guards.try_acquire(dispatch_scope) {
            Ok(permit) => permit,
            Err(error) => {
                if menu_intent.is_some() {
                    state.menu.abort_transition(&action_id)?;
                }
                return Err(error);
            }
        };
        if let Some(intent) = menu_intent.clone() {
            tracing::debug!(
                target_instance_id = %target.instance_id,
                action_id = %action_id,
                menu_transition = ?intent,
                "v2 menu lifecycle transition"
            );
            scope_plan.bind_menu_intent(intent);
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
            Ok(Err(mut error)) => {
                if menu_intent.is_some() {
                    state.menu.abort_transition(&action_id)?;
                }
                if error.target_invalidated() {
                    target.invalidate();
                    drop(state);
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
                }
                return Err(error);
            }
            Err(_) => {
                if menu_intent.is_some() {
                    state.menu.abort_transition(&action_id)?;
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
        scope.native_evidence.fields.insert(
            "dispatch_scope".to_owned(),
            serde_json::to_value(dispatch_scope_kind)
                .expect("typed dispatch scope kind must serialize"),
        );
        scope.bind_target_validity(target.validity_handle());
        let dispatch_plan_result = {
            let platform = &mut state.platform;
            tokio::time::timeout_at(
                deadline.work.into(),
                self.prepare_dispatch(platform, &mut scope, &prepared),
            )
            .await
        };
        let dispatch_plan = match dispatch_plan_result {
            Ok(Ok(plan)) => plan,
            Ok(Err(error)) => {
                let teardown = scope.release();
                if menu_intent.is_some() {
                    state.menu.abort_transition(&action_id)?;
                }
                let teardown_failed = !teardown.failures.is_empty();
                if teardown_failed {
                    target.invalidate();
                }
                let mut failures = vec![error];
                failures.extend(teardown.failures);
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
                    .expect("native action prepare failure is nonempty");
                primary
                    .details
                    .insert("interaction_scope".to_owned(), scope_evidence);
                return Err(primary);
            }
            Err(_) => {
                let mut failures = vec![mutation_deadline_error(
                    &action_id,
                    ErrorPhase::Preflight,
                    "native_action_prepare",
                )];
                let teardown = scope.release();
                if menu_intent.is_some() {
                    state.menu.abort_transition(&action_id)?;
                }
                let teardown_failed = !teardown.failures.is_empty();
                if teardown_failed {
                    target.invalidate();
                }
                failures.extend(teardown.failures);
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
                    .expect("native action prepare timeout failure is nonempty"));
            }
        };
        let profile = settlement_profile(
            &action,
            route,
            menu_intent.as_ref(),
            requires_effect_verification,
            action_allows_target_disappearance(&prepared),
        );
        let retire_target_after_success = profile.target_may_disappear;
        // Refresh the store's current proof immediately before handing the
        // provider its explicit first-native-side-effect boundary. The target
        // lock prevents any concurrent core mutation of this record.
        if let Err(error) = state.observations.current(&observation_id, &resolved) {
            if menu_intent.is_some() {
                state.menu.abort_transition(&action_id)?;
            }
            let teardown = scope.release();
            let mut failures = vec![error];
            failures.extend(teardown.failures);
            return Err(NativeError::primary(failures)
                .expect("observation revalidation failure is nonempty"));
        }
        let (dispatch_result, side_effect_started) = {
            let TargetControllerState {
                platform,
                observations,
                settlement,
                ..
            } = &mut *state;
            let mut boundary = NativeSideEffectBoundary::new(
                observations,
                settlement,
                observation_id.clone(),
                action_id.clone(),
                profile,
            );
            let result = tokio::time::timeout_at(
                deadline.work.into(),
                self.dispatch(platform, &mut scope, &mut boundary, prepared, dispatch_plan),
            )
            .await;
            (result, boundary.started())
        };
        if !side_effect_started {
            if menu_intent.is_some() {
                state.menu.abort_transition(&action_id)?;
            }
            let provider_failure = match dispatch_result {
                Ok(Err(error)) => error,
                Err(_) => mutation_deadline_error(
                    &action_id,
                    ErrorPhase::Preflight,
                    "final_native_validation",
                )
                .with_detail("native_side_effect_started", false),
                Ok(Ok(_)) => NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Dispatch,
                    false,
                    "native provider returned success without entering the first-side-effect boundary",
                ),
            };
            let provider_invalidated = provider_failure.target_invalidated();
            let teardown = scope.release();
            let teardown_failed = !teardown.failures.is_empty();
            if provider_invalidated || teardown_failed {
                target.invalidate();
            }
            let mut failures = vec![provider_failure];
            failures.extend(teardown.failures);
            let scope_evidence = serde_json::to_value(&scope.native_evidence)
                .expect("typed scope evidence must serialize");
            if provider_invalidated || teardown_failed {
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
            let mut primary =
                NativeError::primary(failures).expect("pre-side-effect failure is nonempty");
            primary
                .details
                .insert("interaction_scope".to_owned(), scope_evidence);
            primary
                .details
                .insert("native_side_effect_started".to_owned(), false.into());
            return Err(primary);
        }
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
                        if error.target_invalidated() {
                            target.invalidate();
                        }
                        failures.push(error);
                        None
                    }
                }
            }
            Err(_) => {
                dispatch_timed_out = true;
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
                failures.push(error);
            }
        } else if menu_intent.is_some() && dispatch.is_some() {
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
        let target_retired =
            retire_target_after_success && dispatch.is_some() && settlement.is_some();
        if target_retired {
            // A successful exact close leaves no native object for this
            // controller's observers or synthetic focus belief to own.
            target.invalidate();
        }
        let target_invalidated = target.ensure_valid().is_err();
        if teardown_failed {
            target.invalidate();
        }
        failures.extend(teardown.failures);
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
        let route_detail = native_evidence
            .fields
            .get("pointer_route")
            .cloned()
            .unwrap_or_else(|| candidate_detail.clone().into());
        native_evidence
            .fields
            .insert("route_detail".to_owned(), route_detail);
        native_evidence.fields.insert(
            "capability_evidence".to_owned(),
            capability_evidence_status(capability_evidence.as_ref(), route).into(),
        );
        native_evidence.fields.insert(
            "target_controller_retired".to_owned(),
            target_retired.into(),
        );
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
            native_evidence: native_evidence.clone(),
            pending_settlement,
        };
        if teardown_failed || target_invalidated {
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
            tracing::warn!(
                target_instance_id = %target.instance_id,
                action_id = %action_id,
                action_kind = ?action,
                route = ?route,
                verification = ?dispatch.as_ref().map(|dispatch| dispatch.verification),
                settlement_outcome = settlement.as_ref().map_or("pending_or_failed", |_| "settled"),
                error_code = ?error.code,
                "v2 mutation completed with typed failure"
            );
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
                settlement,
                native_evidence,
                warnings: dispatch.warnings,
            }),
            _ => None,
        };
        target.touch();
        if let Some(receipt) = &receipt {
            tracing::info!(
                target_instance_id = %target.instance_id,
                action_id = %receipt.action_id,
                action_kind = ?action,
                route = ?receipt.route,
                verification = ?receipt.verification,
                settlement_profile = %receipt.settlement.profile,
                settlement_outcome = "settled",
                "v2 mutation completed"
            );
        }
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

    async fn prepare_dispatch(
        &self,
        target: &mut P::TargetState,
        scope: &mut super::interaction::InteractionScope,
        mutation: &ResolvedAction,
    ) -> Result<PreparedDispatch<P>, NativeError> {
        match mutation {
            ResolvedAction::PressKey { .. } | ResolvedAction::TypeText { .. } => self
                .platform
                .keyboard()
                .prepare(target, scope, mutation)
                .await
                .map(PreparedDispatch::Keyboard),
            _ if scope.route == Route::Semantic => self
                .platform
                .semantic()
                .prepare(target, scope, mutation)
                .await
                .map(PreparedDispatch::Semantic),
            ResolvedAction::PointClick { .. }
            | ResolvedAction::Drag(_)
            | ResolvedAction::ElementScroll { point: Some(_), .. }
            | ResolvedAction::DeltaScroll(_)
                if scope.route == Route::TargetedPointer =>
            {
                self.platform
                    .pointer()
                    .prepare(target, scope, mutation)
                    .await
                    .map(PreparedDispatch::Pointer)
            }
            _ => Err(NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Preflight,
                false,
                "resolved action and selected route have no native prepare provider",
            )),
        }
    }

    async fn dispatch(
        &self,
        target: &mut P::TargetState,
        scope: &mut super::interaction::InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        mutation: ResolvedAction,
        plan: PreparedDispatch<P>,
    ) -> Result<NativeDispatch, NativeError> {
        match (mutation, plan) {
            (
                ResolvedAction::PressKey { .. } | ResolvedAction::TypeText { .. },
                PreparedDispatch::Keyboard(plan),
            ) if matches!(scope.route, Route::Semantic | Route::TargetedKeyboard) => {
                self.platform
                    .keyboard()
                    .dispatch(target, scope, boundary, plan)
                    .await
            }
            (
                ResolvedAction::PointClick { .. }
                | ResolvedAction::Drag(_)
                | ResolvedAction::ElementScroll { point: Some(_), .. }
                | ResolvedAction::DeltaScroll(_),
                PreparedDispatch::Pointer(plan),
            ) if scope.route == Route::TargetedPointer => {
                self.platform
                    .pointer()
                    .dispatch(target, scope, boundary, plan)
                    .await
            }
            (_, PreparedDispatch::Semantic(plan)) if scope.route == Route::Semantic => {
                self.platform
                    .semantic()
                    .dispatch(target, scope, boundary, plan)
                    .await
            }
            _ => Err(NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Dispatch,
                false,
                "prepared native action did not match the resolved action provider and route",
            )),
        }
    }
}

fn route_for_live_action(action: &ResolvedAction) -> Route {
    match action {
        ResolvedAction::PointClick { .. }
        | ResolvedAction::Drag(_)
        | ResolvedAction::DeltaScroll(_) => Route::TargetedPointer,
        ResolvedAction::PressKey { .. } => Route::TargetedKeyboard,
        ResolvedAction::TypeText { .. } => {
            unreachable!("type_text owns an exact semantic-to-targeted route ladder")
        }
        ResolvedAction::ElementClick { .. }
        | ResolvedAction::ElementScroll { .. }
        | ResolvedAction::SetValue { .. }
        | ResolvedAction::SelectText { .. }
        | ResolvedAction::Secondary { .. } => Route::Semantic,
    }
}

fn is_escape_key_action(action: &ResolvedAction) -> bool {
    matches!(
        action,
        ResolvedAction::PressKey { stroke, .. }
            if stroke.modifiers.is_empty()
                && matches!(stroke.key.trim().to_ascii_lowercase().as_str(), "escape" | "esc")
    )
}

fn capability_evidence_status(decision: Option<&RouteDecision>, selected: Route) -> &'static str {
    match decision {
        None => "unmeasured",
        Some(RouteDecision::Supported { route }) if *route == selected => {
            "published_supported_matching"
        }
        Some(RouteDecision::Supported { .. }) => "published_supported_different_nonblocking",
        Some(RouteDecision::Unsupported { .. }) => "published_unsupported_nonblocking",
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
        ResolvedAction::ElementScroll { point: Some(_), .. } if route == Route::TargetedPointer => {
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
        | ResolvedAction::SetValue { element, .. }
        | ResolvedAction::SelectText { element, .. }
        | ResolvedAction::Secondary { element, .. } => element.menu_id.as_ref(),
        ResolvedAction::ElementScroll { element, .. } => element.menu_id.as_ref(),
        ResolvedAction::PointClick { point, .. } => point.menu_id.as_ref(),
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
        | ResolvedAction::SetValue { element, .. }
        | ResolvedAction::SelectText { element, .. }
        | ResolvedAction::Secondary { element, .. } => element.menu_id.as_ref(),
        ResolvedAction::ElementScroll { element, .. } => element.menu_id.as_ref(),
        ResolvedAction::PointClick { point, .. } => point.menu_id.as_ref(),
        _ => None,
    }
}

fn capability_key(
    platform: &PlatformName,
    os_version: &str,
    action: &ActionKind,
    addressing: AddressingMode,
    window: &ResolvedWindow,
) -> CapabilityKey {
    CapabilityKey {
        platform: platform.clone(),
        os_version: os_version.to_owned(),
        action: action.clone(),
        addressing,
        framework: window.framework.clone(),
        window_state: window.state.clone(),
    }
}

fn ensure_public_window_matches(
    requested: &WindowRef,
    resolved: &ResolvedWindow,
) -> Result<(), NativeError> {
    if requested.id != resolved.public.id
        || requested.app.id != resolved.public.app.id
        || requested.generation != resolved.public.generation
    {
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
        || target.public.generation != resolved.public.generation
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
    route: Route,
    menu_intent: Option<&MenuMutationIntent>,
    exact_readback: bool,
    target_may_disappear: bool,
) -> SettlementProfile {
    if target_may_disappear {
        let mut profile = SettlementProfile::requiring(
            "performsecondaryaction_target_close",
            [SettlementSignal::WindowListChanged],
        )
        .with_relevant_signals([SettlementSignal::WindowListChanged])
        .allowing_target_disappearance();
        // Window disappearance is the terminal signal, but AppKit can still
        // be completing its close transition after that signal arrives. Keep
        // the signed helper's post-element-action interval before allowing a
        // replacement target in the same process to begin.
        profile.quiet_window_ms = 250;
        return profile;
    }
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
    if matches!(menu_intent, Some(MenuMutationIntent::Opening { .. })) {
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
    if matches!(menu_intent, Some(MenuMutationIntent::Dismissing { .. })) {
        return SettlementProfile::dispatch_only(format!("{action:?}_menu_dismiss").to_lowercase())
            .with_relevant_signals([
                SettlementSignal::MenuDismissed,
                SettlementSignal::WindowListChanged,
                SettlementSignal::FocusChanged,
            ]);
    }
    if matches!(menu_intent, Some(MenuMutationIntent::Targeting { .. })) {
        let mut profile =
            SettlementProfile::dispatch_only(format!("{action:?}_menu_target").to_lowercase())
                .with_relevant_signals([
                    SettlementSignal::AxValueChanged,
                    SettlementSignal::MenuDismissed,
                    SettlementSignal::WindowListChanged,
                    SettlementSignal::FocusChanged,
                ]);
        // The signed helper leaves a 250 ms post-click settling interval before
        // returning refreshed state. Finder applies AXSelected + AXPress
        // asynchronously; the ordinary 30 ms quiet window can otherwise
        // publish the menu's disappearing AXWindow as though it were live.
        profile.quiet_window_ms = 250;
        return profile;
    }
    let relevant = match action {
        ActionKind::Click => vec![
            SettlementSignal::PointerSequenceComplete,
            SettlementSignal::AxAction,
            SettlementSignal::AxValueChanged,
            SettlementSignal::FocusChanged,
            SettlementSignal::WindowListChanged,
            SettlementSignal::MenuOpened,
            SettlementSignal::MenuDismissed,
        ],
        ActionKind::Drag => vec![
            SettlementSignal::PointerSequenceComplete,
            SettlementSignal::AxAction,
            SettlementSignal::AxValueChanged,
            SettlementSignal::WindowGeometryChanged,
        ],
        ActionKind::Scroll => vec![
            SettlementSignal::PointerSequenceComplete,
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
    let name = format!("{action:?}").to_lowercase();
    let mut profile = if route == Route::TargetedPointer {
        SettlementProfile::requiring(name, [SettlementSignal::PointerSequenceComplete])
            .with_relevant_signals(relevant)
    } else {
        SettlementProfile::dispatch_only(name).with_relevant_signals(relevant)
    };
    if matches!(action, ActionKind::Click) {
        // The signed helper keeps a 250 ms post-click interval before it
        // publishes refreshed state. This covers delayed native selection and
        // menu transitions without app-specific sleeps in the public harness.
        profile.quiet_window_ms = 250;
    }
    profile
}

fn action_allows_target_disappearance(action: &ResolvedAction) -> bool {
    matches!(
        action,
        ResolvedAction::Secondary { element, action }
            if secondary_action_allows_target_disappearance(element.subrole.as_deref(), action)
    )
}

fn secondary_action_allows_target_disappearance(subrole: Option<&str>, action: &str) -> bool {
    subrole == Some("AXCloseButton") && action == "AXPress"
}

fn target_busy(window: &ResolvedWindow) -> NativeError {
    NativeError::new(
        ErrorCode::TargetBusy,
        ErrorPhase::Preflight,
        true,
        "the exact target controller is already handling another request",
    )
    .with_detail("native_side_effect_started", false)
    .with_detail("requested_scope", "target_state")
    .with_detail("window_id", window.public.id.to_string())
    .with_detail("app_id", window.public.app.id.to_string())
}

fn is_menu_action(action: &str) -> bool {
    matches!(action, "show_menu" | "AXShowMenu")
}

#[allow(dead_code)]
fn _type_assertions(_: Framework, _: WindowStateKind, _: AxTreeMode) {}

#[cfg(test)]
mod target_disappearance_tests {
    use super::*;
    use crate::api::{MenuId, NativeMenuIdentity, NativeProcessHandle, NativeWindowHandle};

    #[test]
    fn only_exact_close_button_press_allows_target_disappearance() {
        assert!(secondary_action_allows_target_disappearance(
            Some("AXCloseButton"),
            "AXPress"
        ));
        assert!(!secondary_action_allows_target_disappearance(
            Some("AXCloseButton"),
            "AXShowMenu"
        ));
        assert!(!secondary_action_allows_target_disappearance(
            Some("AXMinimizeButton"),
            "AXPress"
        ));
        assert!(!secondary_action_allows_target_disappearance(
            None, "AXPress"
        ));
    }

    #[test]
    fn close_profile_requires_window_disappearance_evidence() {
        let profile = settlement_profile(
            &ActionKind::PerformSecondaryAction,
            Route::Semantic,
            None,
            false,
            true,
        );
        assert!(profile.target_may_disappear);
        assert_eq!(
            profile.required_terminal_signals,
            [SettlementSignal::WindowListChanged].into()
        );
        assert_eq!(profile.quiet_window_ms, 250);
    }

    #[test]
    fn menu_target_profile_waits_for_signed_helper_quiet_interval() {
        let menu_id = MenuId::new();
        let profile = settlement_profile(
            &ActionKind::Click,
            Route::Semantic,
            Some(&MenuMutationIntent::Targeting {
                menu_id,
                identity: NativeMenuIdentity {
                    process: NativeProcessHandle::new("menu-process").unwrap(),
                    window: NativeWindowHandle::new("menu-window").unwrap(),
                    generation: super::super::contracts::WindowGeneration(1),
                },
            }),
            false,
            false,
        );
        assert_eq!(profile.name, "click_menu_target");
        assert_eq!(profile.quiet_window_ms, 250);
    }

    #[test]
    fn ordinary_click_profile_waits_for_signed_helper_quiet_interval() {
        let profile = settlement_profile(
            &ActionKind::Click,
            Route::TargetedPointer,
            None,
            false,
            false,
        );
        assert_eq!(profile.name, "click");
        assert_eq!(profile.quiet_window_ms, 250);
    }
}
