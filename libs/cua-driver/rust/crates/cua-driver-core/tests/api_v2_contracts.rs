use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use cua_driver_core::{
    api::*,
    protocol::{
        V2Command, V2RequestEnvelope, V2ResponseBody, V2ResponseEnvelope, V2_PROTOCOL_VERSION,
    },
};
use tokio::sync::Notify;

fn id<T>(value: &str, parse: impl FnOnce(String) -> Result<T, String>) -> T {
    parse(value.to_owned()).expect("valid test id")
}

fn test_mutation_deadline() -> MutationDeadline {
    let work = Instant::now() + Duration::from_secs(30);
    MutationDeadline::new(work, work + Duration::from_secs(5)).unwrap()
}

fn app_ref() -> AppRef {
    AppRef {
        id: id("app-1", AppId::parse),
        canonical_id: None,
        name: Some("Fixture".to_owned()),
        pid: Some(42),
        running: true,
    }
}

fn window_ref() -> WindowRef {
    WindowRef {
        id: id("window-1", WindowId::parse),
        app: app_ref(),
        generation: WindowGeneration(1),
        title: Some("Fixture window".to_owned()),
        usable: true,
        is_standard: Some(true),
        is_main: Some(true),
        z_index: Some(1),
    }
}

fn resolved_window() -> ResolvedWindow {
    ResolvedWindow {
        public: window_ref(),
        native: NativeWindowHandle::new("native-window-1").unwrap(),
        process: NativeProcessHandle::new("process-42").unwrap(),
        framework: Framework::AppKit,
        geometry: WindowGeometry {
            bounds: Rect {
                x: 100.0,
                y: 200.0,
                width: 640.0,
                height: 480.0,
            },
            scale_factor: 1.0,
            revision: id("geometry-1", GeometryRevision::parse),
        },
        state: WindowStateKind::Visible,
    }
}

fn alternate_window(id: &str, app_id: &str, pid: u32, process: &str) -> ResolvedWindow {
    let public = WindowRef {
        id: WindowId::parse(id).unwrap(),
        app: AppRef {
            id: AppId::parse(app_id).unwrap(),
            canonical_id: Some(format!("com.example.{app_id}")),
            name: Some(app_id.to_owned()),
            pid: Some(pid),
            running: true,
        },
        generation: WindowGeneration(1),
        title: Some(id.to_owned()),
        usable: true,
        is_standard: Some(true),
        is_main: Some(true),
        z_index: Some(1),
    };
    ResolvedWindow {
        public,
        native: NativeWindowHandle::new(format!("native-{id}")).unwrap(),
        process: NativeProcessHandle::new(process).unwrap(),
        framework: Framework::AppKit,
        geometry: WindowGeometry {
            bounds: Rect {
                x: 100.0,
                y: 200.0,
                width: 640.0,
                height: 480.0,
            },
            scale_factor: 1.0,
            revision: GeometryRevision::parse(format!("geometry-{id}")).unwrap(),
        },
        state: WindowStateKind::Visible,
    }
}

fn point_click_command(window: WindowRef, state: &WindowState) -> ClickCommand {
    ClickCommand {
        window,
        request: ClickRequest {
            target: ClickTarget::Point {
                observation_id: state.observation_id.clone(),
                surface_id: state.surfaces[0].id.clone(),
                point: Point { x: 20.0, y: 20.0 },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    }
}

#[test]
fn v2_protocol_rejects_schema_drift_and_version_mismatch() {
    let valid = serde_json::json!({
        "request_id": "request-1",
        "protocol_version": {"major": 2, "minor": 1},
        "method": "driver.v2.click",
        "params": {
            "window": window_ref(),
            "request": {
                "target": {
                    "kind": "point",
                    "observation_id": "observation-1",
                    "surface_id": "surface-1",
                    "point": {"x": 10.0, "y": 20.0}
                },
                "button": "left",
                "click_count": 1,
                "modifiers": []
            }
        }
    });
    let envelope: V2RequestEnvelope =
        serde_json::from_value(valid.clone()).expect("valid strict command");
    assert!(matches!(envelope.command, V2Command::Click(_)));
    envelope.validate_version().expect("supported version");

    let mut unknown = valid.clone();
    unknown["params"]["request"]["execution_mode"] = serde_json::json!("foreground");
    assert!(serde_json::from_value::<V2RequestEnvelope>(unknown).is_err());

    let mut ambiguous_union = valid.clone();
    ambiguous_union["params"]["request"]["target"]["element"] = serde_json::json!({
        "observation_id": "observation-1",
        "id": "element-1"
    });
    assert!(serde_json::from_value::<V2RequestEnvelope>(ambiguous_union).is_err());

    let mut wrong_version = valid;
    wrong_version["protocol_version"]["minor"] = serde_json::json!(0);
    let mismatch: V2RequestEnvelope = serde_json::from_value(wrong_version).unwrap();
    let error = mismatch.validate_version().unwrap_err();
    assert_eq!(error.code, ErrorCode::ProtocolMismatch);
    assert_eq!(error.phase, ErrorPhase::Validate);
    assert_eq!(
        serde_json::to_value(&error).unwrap()["code"],
        serde_json::json!("protocol_mismatch")
    );

    let both_result_and_error = serde_json::json!({
        "request_id": "request-1",
        "protocol_version": {"major": 2, "minor": 1},
        "result": {},
        "error": serde_json::to_value(&error).unwrap(),
    });
    assert!(
        serde_json::from_value::<V2ResponseEnvelope<serde_json::Value>>(both_result_and_error)
            .is_err()
    );

    let launch = LaunchResult {
        action_id: id("launch-action", ActionId::parse),
        app: app_ref(),
        windows: vec![window_ref()],
        reused_running_app: false,
        verification: EffectVerification::EffectVerified,
        settlement: SettlementEvidence::initial(),
    };
    let mut invalid_launch = serde_json::to_value(launch).unwrap();
    invalid_launch["verification"] = serde_json::json!("dispatch_verified");
    assert!(serde_json::from_value::<LaunchResult>(invalid_launch).is_err());
}

#[test]
fn ax_diff_round_trip_is_exact_and_malformed_ranges_fail_closed() {
    let base = "root id=1\n  button id=2 label=Old\n  text id=3";
    let current = "root id=1\n  button id=2 label=New\n  text id=3\n  text id=4";
    let operations = diff_lines(base, current);
    assert_eq!(apply_ax_diff(base, &operations).unwrap(), current);

    let malformed = vec![
        ReplaceAxLines {
            start_line: 0,
            delete_count: 2,
            lines: vec![],
        },
        ReplaceAxLines {
            start_line: 1,
            delete_count: 1,
            lines: vec!["overlap".to_owned()],
        },
    ];
    let error = apply_ax_diff(base, &malformed).unwrap_err();
    assert_eq!(error.code, ErrorCode::AxRevisionMismatch);

    let mut revisions = AxRevisionState::default();
    let prepared = revisions
        .prepare(current, AxTreeMode::DiffIfAvailable)
        .unwrap();
    assert!(
        revisions.last_revision().is_none(),
        "preparing an observation must not advance the delivered revision"
    );
    revisions.commit(prepared);
    assert!(revisions.last_revision().is_some());
    let no_op = revisions
        .prepare(current, AxTreeMode::DiffIfAvailable)
        .unwrap();
    let AxTreeUpdate::Diff {
        base_revision,
        revision,
        operations,
    } = &no_op.update
    else {
        panic!("a delivered base must produce a diff by default");
    };
    assert!(operations.is_empty());
    assert_ne!(base_revision, revision, "even a no-op gets a new revision");
    revisions.commit(no_op);
    revisions.invalidate_base();
    assert!(matches!(
        revisions
            .prepare(current, AxTreeMode::DiffIfAvailable)
            .unwrap()
            .update,
        AxTreeUpdate::Full { .. }
    ));

    let malformed_native = NativeAccessibilityUpdate {
        normalized_tree: current.to_owned(),
        elements: vec![NativeAccessibilityElement {
            id: id("element-1", ElementId::parse),
            native: NativeElementHandle::new("native-element-1").unwrap(),
            owner: resolved_window().stamp(),
            role: Some("button".to_owned()),
            subrole: None,
            label: None,
            value: None,
            bounds: Some(Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            }),
            actions: Vec::new(),
            menu_id: None,
        }],
        focused_element: Some(id("missing-element", ElementId::parse)),
        selected_text: None,
        selected_elements: Vec::new(),
        document_text: None,
    };
    let error = revision_accessibility(
        &AxRevisionState::default(),
        &id("observation-malformed", ObservationId::parse),
        malformed_native,
        AxTreeMode::DiffIfAvailable,
    )
    .err()
    .expect("dangling AX references must fail");
    assert_eq!(error.code, ErrorCode::AxRevisionMismatch);
}

#[test]
fn request_validation_rejects_unsafe_noops_and_non_finite_geometry() {
    let observation_id = id("observation-1", ObservationId::parse);
    let surface_id = id("surface-1", SurfaceId::parse);
    assert!(ClickRequest {
        target: ClickTarget::Point {
            observation_id: observation_id.clone(),
            surface_id: surface_id.clone(),
            point: Point {
                x: f64::NAN,
                y: 1.0,
            },
        },
        button: MouseButton::Left,
        click_count: 1,
        modifiers: Vec::new(),
    }
    .validate()
    .is_err());
    assert!(ScrollRequest::Delta {
        observation_id,
        surface_id,
        point: Point { x: 1.0, y: 1.0 },
        delta_x: f64::INFINITY,
        delta_y: 0.0,
    }
    .validate()
    .is_err());
    let scroll_element = ElementRef {
        observation_id: id("observation-scroll", ObservationId::parse),
        id: id("element-scroll", ElementId::parse),
    };
    assert!(ScrollRequest::Element {
        element: scroll_element.clone(),
        direction: ScrollDirection::Down,
        pages: 0.25,
    }
    .validate()
    .is_ok());
    for pages in [0.0, -1.0, f64::INFINITY, f64::NAN] {
        assert!(ScrollRequest::Element {
            element: scroll_element.clone(),
            direction: ScrollDirection::Down,
            pages,
        }
        .validate()
        .is_err());
    }
    assert!(PressKeyRequest {
        observation_id: id("observation-2", ObservationId::parse),
        stroke: KeyStroke {
            key: "  ".to_owned(),
            modifiers: Vec::new(),
        },
    }
    .validate()
    .is_err());
    assert!(SelectTextRequest {
        element: ElementRef {
            observation_id: id("observation-3", ObservationId::parse),
            id: id("element-1", ElementId::parse),
        },
        text: String::new(),
        prefix: Some(String::new()),
        suffix: None,
        selection_type: SelectionType::Text,
    }
    .validate()
    .is_err());
    assert!(SecondaryActionRequest {
        element: ElementRef {
            observation_id: id("observation-4", ObservationId::parse),
            id: id("element-2", ElementId::parse),
        },
        action: String::new(),
    }
    .validate()
    .is_err());
    assert!(Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 10.0,
    }
    .validate()
    .is_err());
}

#[derive(Default)]
struct FakeTargetState {
    semantic_dispatches: usize,
    verification_readback_complete: bool,
}

struct FakeFocus {
    shutdowns: Arc<AtomicUsize>,
}

#[async_trait]
impl TargetFocusCoordinator for FakeFocus {
    async fn shutdown(&mut self) -> Result<(), NativeError> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct EmptyInvalidations;

#[async_trait]
impl InvalidationSubscription for EmptyInvalidations {
    async fn next(&mut self) -> Option<TargetInvalidation> {
        None
    }
}

#[derive(Default)]
struct FakePlatform {
    observation_count: AtomicUsize,
    dispatch_count: AtomicUsize,
    target_creations: AtomicUsize,
    shutdowns: Arc<AtomicUsize>,
    ordering: Arc<Mutex<Vec<&'static str>>>,
    cleanup_count: Arc<AtomicUsize>,
    preflight_action_ids: Arc<Mutex<Vec<ActionId>>>,
    acquired_action_ids: Arc<Mutex<Vec<ActionId>>>,
    pointer_screen_points: Arc<Mutex<Vec<Point>>>,
    block_dispatch: std::sync::atomic::AtomicBool,
    block_before_boundary: std::sync::atomic::AtomicBool,
    dispatch_fail: std::sync::atomic::AtomicBool,
    dispatch_poison: std::sync::atomic::AtomicBool,
    semantic_candidate_unusable: std::sync::atomic::AtomicBool,
    semantic_prepare_fail: std::sync::atomic::AtomicBool,
    semantic_verification_fail: std::sync::atomic::AtomicBool,
    observation_role: Mutex<Option<String>>,
    cleanup_failure_count: AtomicUsize,
    settle_pending: std::sync::atomic::AtomicBool,
    block_settle: std::sync::atomic::AtomicBool,
    launch_fail: std::sync::atomic::AtomicBool,
    refresh_geometry_during_observe: std::sync::atomic::AtomicBool,
    dispatch_entered: Notify,
    settle_entered: Notify,
    blocked_dispatch_process: Mutex<Option<String>>,
}

enum FakePreparedSemantic {
    Click,
    SetValue,
    Secondary,
}

struct FakeScopeCleanup {
    ordering: Arc<Mutex<Vec<&'static str>>>,
    cleanup_count: Arc<AtomicUsize>,
    failure_count: usize,
    leases: ScopeLeaseTeardown,
}

impl ScopeCleanup for FakeScopeCleanup {
    fn cleanup(&mut self, _deadline: Instant) -> ScopeTeardownOutcome {
        self.cleanup_count.fetch_add(1, Ordering::SeqCst);
        self.ordering.lock().unwrap().push("scope_released");
        let failures = (0..self.failure_count)
            .map(|index| {
                NativeError::new(
                    if index == 0 {
                        ErrorCode::VerificationFailed
                    } else {
                        ErrorCode::Internal
                    },
                    ErrorPhase::Verify,
                    false,
                    format!("fake cleanup failure {index}"),
                )
            })
            .collect();
        ScopeTeardownOutcome {
            native_evidence: NativeEvidence {
                fields: BTreeMap::from([(
                    "fake_cleanup".to_owned(),
                    serde_json::json!("complete"),
                )]),
                ..NativeEvidence::default()
            },
            leases: self.leases.clone(),
            failures,
        }
    }
}

impl FakePlatform {
    fn record(&self, event: &'static str) {
        self.ordering.lock().unwrap().push(event);
    }
}

#[async_trait]
impl LifecycleProvider for FakePlatform {
    async fn readiness(&self) -> Result<Readiness, NativeError> {
        Ok(Readiness {
            ready: true,
            permissions: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
        })
    }

    async fn capabilities(&self) -> Result<CapabilityManifest, NativeError> {
        Ok(CapabilityManifest {
            platform: PlatformName::Macos,
            driver_version: "test".to_owned(),
            protocol_version: "2.1".to_owned(),
            permissions: BTreeMap::new(),
            cells: Vec::new(),
        })
    }

    async fn list_apps(&self, query: AppQuery) -> Result<Vec<AppRef>, NativeError> {
        if query.name_contains.as_deref() == Some("slow-protocol-read") {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(vec![app_ref()])
    }

    async fn launch_background(
        &self,
        _app: AppSelector,
        launch_scope: &mut LaunchScope,
    ) -> Result<NativeLaunch, NativeError> {
        launch_scope.begin_launch();
        launch_scope.record_partial_result(app_ref(), vec![window_ref()]);
        launch_scope.pending_settlement = Some(PendingSettlementEvidence {
            state: PendingSettlementState::Pending,
            trigger_action_id: launch_scope
                .action_id()
                .cloned()
                .expect("controller launch scopes carry their action id"),
            profile: "fake_launch".to_owned(),
            elapsed_ms: 1,
            observed_signals: vec![SettlementSignal::DispatchComplete],
            missing_signals: Vec::new(),
        });
        if self.launch_fail.load(Ordering::SeqCst) {
            return Err(NativeError::new(
                ErrorCode::AppLaunchFailed,
                ErrorPhase::Dispatch,
                true,
                "fake post-dispatch launch failure",
            ));
        }
        Ok(NativeLaunch {
            app: app_ref(),
            windows: vec![window_ref()],
            reused_running_app: false,
            settlement: SettlementEvidence::initial(),
        })
    }
}

#[tokio::test]
async fn post_dispatch_launch_failure_keeps_exact_partial_and_pending_evidence() {
    let platform = Arc::new(FakePlatform::default());
    platform.launch_fail.store(true, Ordering::SeqCst);
    let controller = DriverController::new(platform, PlatformName::Macos, "test-os");
    let error = controller
        .launch_app(LaunchAppRequest {
            app: AppSelector::BundleId {
                bundle_id: "com.example.fixture".to_owned(),
            },
        })
        .await
        .unwrap_err();

    let Some(PartialEvidence::Launch {
        action_id,
        app,
        windows,
        pending_settlement: Some(pending),
        ..
    }) = error.partial_evidence.as_deref()
    else {
        panic!("post-dispatch launch failure must keep structured launch evidence");
    };
    assert_eq!(app.as_ref(), Some(&app_ref()));
    assert_eq!(windows, &vec![window_ref()]);
    assert_eq!(pending.trigger_action_id, *action_id);
}

#[async_trait]
impl WindowProvider for FakePlatform {
    async fn list_windows(&self, _app: Option<&AppRef>) -> Result<Vec<WindowRef>, NativeError> {
        Ok(vec![window_ref()])
    }

    async fn rehydrate(
        &self,
        _id: &WindowId,
        _app: Option<&AppRef>,
    ) -> Result<WindowRef, NativeError> {
        Ok(window_ref())
    }

    async fn resolve(&self, window: &WindowRef) -> Result<ResolvedWindow, NativeError> {
        let resolved = if window.id == window_ref().id && window.app.id == app_ref().id {
            resolved_window()
        } else if window.id.as_str() == "window-2" && window.app.id.as_str() == "app-2" {
            alternate_window("window-2", "app-2", 84, "process-84")
        } else if window.id.as_str() == "window-3" && window.app.id == app_ref().id {
            alternate_window("window-3", "app-1", 42, "process-42")
        } else {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "fake window mismatch",
            ));
        };
        if window.generation != resolved.public.generation {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "fake window generation mismatch",
            ));
        }
        Ok(resolved)
    }
}

#[async_trait]
impl ObservationProvider<FakeTargetState> for FakePlatform {
    async fn settle(
        &self,
        _target: &mut FakeTargetState,
        dirty: &DirtyState,
        _deadline: Instant,
    ) -> Result<SettlementAttempt, NativeError> {
        self.record("settle");
        if self.block_settle.load(Ordering::SeqCst) {
            self.settle_entered.notify_waiters();
            std::future::pending::<()>().await;
        }
        if self.settle_pending.load(Ordering::SeqCst) {
            let mut pending = dirty.pending_evidence();
            pending.observed_signals.push(SettlementSignal::AxAction);
            return Ok(SettlementAttempt::Pending(pending));
        }
        let mut signals = dirty.observed_signals.clone();
        signals.insert(SettlementSignal::DispatchComplete);
        signals.extend(
            dirty
                .profile
                .required_terminal_signals
                .iter()
                .copied()
                .filter(|signal| {
                    *signal != SettlementSignal::VerificationReadbackComplete
                        || _target.verification_readback_complete
                }),
        );
        if !dirty.profile.required_terminal_signals.is_subset(&signals) {
            return Ok(SettlementAttempt::Pending(PendingSettlementEvidence {
                state: PendingSettlementState::Pending,
                trigger_action_id: dirty.action_id.clone(),
                profile: dirty.profile.name.clone(),
                elapsed_ms: 1,
                observed_signals: signals.iter().copied().collect(),
                missing_signals: dirty
                    .profile
                    .required_terminal_signals
                    .difference(&signals)
                    .copied()
                    .collect(),
            }));
        }
        Ok(SettlementAttempt::Settled(SettlementEvidence {
            state: SettledState::Settled,
            trigger_action_id: Some(dirty.action_id.clone()),
            profile: dirty.profile.name.clone(),
            elapsed_ms: 1,
            observed_signals: signals.into_iter().collect(),
            terminal_signal: "fake_terminal".to_owned(),
            quiet_window_ms: dirty.profile.quiet_window_ms,
            resumed_from_prior_call: dirty.resumed_from_prior_call,
        }))
    }

    async fn observe(
        &self,
        _target: &mut FakeTargetState,
        window: &ResolvedWindow,
        _request: ObserveRequest,
    ) -> Result<NativeObservationUpdate, NativeError> {
        let sequence = self.observation_count.fetch_add(1, Ordering::SeqCst);
        self.record("observe");
        let observation_suffix = sequence + 1;
        let element_id = id("element-1", ElementId::parse);
        let mut observed_window = window.clone();
        if self.refresh_geometry_during_observe.load(Ordering::SeqCst) {
            observed_window.geometry.bounds.x += 10.0;
            observed_window.geometry.revision = id("geometry-refreshed", GeometryRevision::parse);
        }
        Ok(NativeObservationUpdate {
            window: observed_window.clone(),
            surfaces: vec![SurfaceRecord {
                id: id(&format!("surface-{observation_suffix}"), SurfaceId::parse),
                kind: SurfaceKind::Window,
                owner_window: observed_window.public.clone(),
                image_url: format!("file:///tmp/surface-{observation_suffix}.png"),
                approximate_bytes: 16,
                raster_size: Size {
                    width: 640,
                    height: 480,
                },
                window_bounds: Some(observed_window.geometry.bounds),
                capture_revision: id(
                    &format!("capture-{observation_suffix}"),
                    CaptureRevision::parse,
                ),
                observation_epoch: None,
                transform: SurfaceToWindowTransform {
                    scale_x: 1.0,
                    scale_y: 1.0,
                    offset_x: 0.0,
                    offset_y: 0.0,
                },
                freshness: CaptureFreshness::Fresh,
                owner: SurfaceOwner::Target(observed_window.stamp()),
                menu_id: None,
            }],
            accessibility: Some(NativeAccessibilityUpdate {
                normalized_tree: format!(
                    "window id=root\n  button id=element-1 value={observation_suffix}"
                ),
                elements: vec![NativeAccessibilityElement {
                    id: element_id.clone(),
                    native: NativeElementHandle::new("native-element-1").unwrap(),
                    owner: observed_window.stamp(),
                    role: Some(
                        self.observation_role
                            .lock()
                            .unwrap()
                            .clone()
                            .unwrap_or_else(|| "button".to_owned()),
                    ),
                    subrole: None,
                    label: Some("Fixture button".to_owned()),
                    value: Some(observation_suffix.to_string()),
                    bounds: Some(Rect {
                        x: 10.0,
                        y: 10.0,
                        width: 100.0,
                        height: 30.0,
                    }),
                    actions: vec!["AXPress".to_owned()],
                    menu_id: None,
                }],
                focused_element: Some(element_id),
                selected_text: None,
                selected_elements: Vec::new(),
                document_text: None,
            }),
            menu: NativeMenuObservation::Unchanged,
            captured_at_unix_ms: observation_suffix as u64,
            warnings: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

#[async_trait]
impl SemanticActionProvider<FakeTargetState> for FakePlatform {
    type PreparedAction = FakePreparedSemantic;

    async fn element_click_candidate(
        &self,
        _target: &mut FakeTargetState,
        element: &ResolvedElement,
        spec: &ClickSpec,
    ) -> Result<ElementClickCandidate, NativeError> {
        let usable = !self.semantic_candidate_unusable.load(Ordering::SeqCst)
            && spec.button == MouseButton::Left
            && spec.click_count == 1
            && spec.modifiers.is_empty()
            && element.actions.iter().any(|action| action == "AXPress");
        Ok(if usable {
            ElementClickCandidate::Semantic {
                reason: "semantic_element_click".to_owned(),
            }
        } else {
            ElementClickCandidate::TargetedPointer {
                screen_point: Point { x: 175.0, y: 235.0 },
                reason: "semantic_not_applicable:fake semantic click unavailable".to_owned(),
            }
        })
    }

    async fn element_scroll_candidate(
        &self,
        _target: &mut FakeTargetState,
        _element: &ResolvedElement,
        _spec: &ElementScrollSpec,
    ) -> Result<Candidate<()>, NativeError> {
        Ok(Candidate::not_applicable(
            "fake semantic page scroll unavailable",
        ))
    }

    async fn prepare(
        &self,
        _target: &mut FakeTargetState,
        _scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<Self::PreparedAction, NativeError> {
        self.record("semantic_prepare");
        if self.semantic_prepare_fail.load(Ordering::SeqCst) {
            return Err(NativeError::unsupported(
                "fake semantic shape refused during prepare",
            ));
        }
        match action {
            ResolvedAction::ElementClick { .. } => Ok(FakePreparedSemantic::Click),
            ResolvedAction::SetValue { .. } => Ok(FakePreparedSemantic::SetValue),
            ResolvedAction::Secondary { element, action }
                if action == "AXPress"
                    && element.role.as_deref().is_some_and(|role| {
                        matches!(
                            role,
                            "AXMenu"
                                | "AXMenuItem"
                                | "AXMenuBar"
                                | "AXMenuBarItem"
                                | "AXPopUpButton"
                                | "AXMenuButton"
                        )
                    }) =>
            {
                Err(NativeError::unsupported(
                    "recipe_unproven: fake menu-managed secondary action",
                ))
            }
            ResolvedAction::Secondary { .. } => Ok(FakePreparedSemantic::Secondary),
            _ => Err(NativeError::unsupported("not used by this fixture")),
        }
    }

    async fn dispatch(
        &self,
        target: &mut FakeTargetState,
        _scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: Self::PreparedAction,
    ) -> Result<NativeDispatch, NativeError> {
        boundary.begin()?;
        target.semantic_dispatches += 1;
        self.dispatch_count.fetch_add(1, Ordering::SeqCst);
        self.record("dispatch");
        let effect_readback = matches!(action, FakePreparedSemantic::SetValue);
        if effect_readback {
            target.verification_readback_complete = true;
        }
        if self.semantic_verification_fail.load(Ordering::SeqCst) {
            return Err(NativeError::new(
                ErrorCode::VerificationFailed,
                ErrorPhase::Verify,
                true,
                "fake exact semantic readback mismatch",
            ));
        }
        let mut dispatch = NativeDispatch::dispatch_verified();
        if effect_readback {
            dispatch.verification = VerificationLevel::EffectVerified;
        }
        Ok(dispatch)
    }
}

#[async_trait]
impl PointerActionProvider<FakeTargetState> for FakePlatform {
    type PreparedAction = ResolvedAction;

    async fn prepare(
        &self,
        _target: &mut FakeTargetState,
        _scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<Self::PreparedAction, NativeError> {
        self.record("pointer_prepare");
        if let ResolvedAction::PointClick { point, .. } = action {
            self.pointer_screen_points
                .lock()
                .unwrap()
                .push(point.screen_point);
        }
        Ok(action.clone())
    }

    async fn dispatch(
        &self,
        _target: &mut FakeTargetState,
        _scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        _action: Self::PreparedAction,
    ) -> Result<NativeDispatch, NativeError> {
        if self.block_before_boundary.load(Ordering::SeqCst) {
            self.dispatch_entered.notify_waiters();
            std::future::pending::<()>().await;
        }
        boundary.begin()?;
        boundary.begin()?;
        self.dispatch_count.fetch_add(1, Ordering::SeqCst);
        self.record("dispatch");
        let process = match &_action {
            ResolvedAction::PointClick { point, .. } => Some(point.window.process.as_str()),
            _ => None,
        };
        let blocked_process = self.blocked_dispatch_process.lock().unwrap().clone();
        if self.block_dispatch.load(Ordering::SeqCst)
            && blocked_process
                .as_deref()
                .is_none_or(|blocked| process == Some(blocked))
        {
            self.dispatch_entered.notify_waiters();
            std::future::pending::<()>().await;
        }
        if self.dispatch_fail.load(Ordering::SeqCst) {
            return Err(NativeError::new(
                ErrorCode::DispatchFailed,
                ErrorPhase::Dispatch,
                false,
                "fake dispatch failure",
            ));
        }
        if self.dispatch_poison.load(Ordering::SeqCst) {
            return Err(NativeError::new(
                ErrorCode::DispatchFailed,
                ErrorPhase::Dispatch,
                false,
                "fake native cleanup could not be proved",
            )
            .with_target_invalidated());
        }
        Ok(NativeDispatch::dispatch_verified())
    }
}

#[async_trait]
impl KeyboardActionProvider<FakeTargetState> for FakePlatform {
    type PreparedAction = ResolvedAction;

    async fn prepare(
        &self,
        _target: &mut FakeTargetState,
        _scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<Self::PreparedAction, NativeError> {
        self.record("keyboard_prepare");
        Ok(action.clone())
    }

    async fn dispatch(
        &self,
        _target: &mut FakeTargetState,
        _scope: &mut InteractionScope,
        _boundary: &mut NativeSideEffectBoundary<'_>,
        _action: Self::PreparedAction,
    ) -> Result<NativeDispatch, NativeError> {
        Err(NativeError::unsupported("not used by this fixture"))
    }
}

#[async_trait]
impl InteractionProvider<FakeTargetState, FakeFocus> for FakePlatform {
    type NativeScopePlan = ();

    async fn preflight(
        &self,
        _target: &mut FakeTargetState,
        _focus: &mut FakeFocus,
        action_id: &ActionId,
        window: &ResolvedWindow,
        route: Route,
        _action: &ResolvedAction,
        deadline: MutationDeadline,
        requirements: ScopeRequirements,
    ) -> Result<ScopePlan<Self::NativeScopePlan>, NativeError> {
        self.record("preflight");
        self.preflight_action_ids
            .lock()
            .unwrap()
            .push(action_id.clone());
        Ok(ScopePlan::new(
            action_id.clone(),
            window.clone(),
            route,
            deadline,
            requirements,
            DispatchScopeKind::Process,
            (),
        ))
    }

    async fn acquire_scope(
        &self,
        _target: &mut FakeTargetState,
        _focus: &mut FakeFocus,
        plan: ScopePlan<Self::NativeScopePlan>,
        logical_cursor: TargetCursorHandle,
    ) -> Result<InteractionScope, NativeError> {
        self.record("scope_acquired");
        self.acquired_action_ids
            .lock()
            .unwrap()
            .push(plan.action_id.clone());
        let decision = |required| {
            if required {
                LeaseDecision::Acquired
            } else {
                LeaseDecision::NotApplicable
            }
        };
        let acquisition = ScopeLeaseAcquisition {
            accessibility: decision(plan.requirements.accessibility),
            menu_dismissal: decision(plan.requirements.menu_dismissal),
            target_belief: decision(plan.requirements.target_belief),
        };
        let teardown = |decision| match decision {
            LeaseDecision::Acquired => LeaseTeardownStatus::Released,
            LeaseDecision::NotApplicable => LeaseTeardownStatus::NotApplicable,
        };
        let mut teardown_leases = ScopeLeaseTeardown {
            accessibility: teardown(acquisition.accessibility),
            menu_dismissal: teardown(acquisition.menu_dismissal),
            target_belief: teardown(acquisition.target_belief),
        };
        let failure_count = self.cleanup_failure_count.load(Ordering::SeqCst);
        if failure_count > 0 {
            teardown_leases.target_belief = LeaseTeardownStatus::Failed;
        }
        Ok(plan.into_scope(
            acquisition,
            logical_cursor,
            NativeEvidence::default(),
            Box::new(FakeScopeCleanup {
                ordering: Arc::clone(&self.ordering),
                cleanup_count: Arc::clone(&self.cleanup_count),
                failure_count,
                leases: teardown_leases,
            }),
        ))
    }
}

#[async_trait]
impl PlatformDriver for FakePlatform {
    type TargetState = FakeTargetState;
    type TargetFocusCoordinator = FakeFocus;
    type Lifecycle = Self;
    type Windows = Self;
    type Observation = Self;
    type Semantic = Self;
    type Pointer = Self;
    type Keyboard = Self;
    type Interaction = Self;
    type Invalidations = EmptyInvalidations;

    async fn create_target_state(
        &self,
        _window: &ResolvedWindow,
    ) -> Result<(Self::TargetState, Self::TargetFocusCoordinator), NativeError> {
        self.target_creations.fetch_add(1, Ordering::SeqCst);
        Ok((
            FakeTargetState::default(),
            FakeFocus {
                shutdowns: Arc::clone(&self.shutdowns),
            },
        ))
    }

    fn lifecycle(&self) -> &Self::Lifecycle {
        self
    }

    fn windows(&self) -> &Self::Windows {
        self
    }

    fn observation(&self) -> &Self::Observation {
        self
    }

    fn semantic(&self) -> &Self::Semantic {
        self
    }

    fn pointer(&self) -> &Self::Pointer {
        self
    }

    fn keyboard(&self) -> &Self::Keyboard {
        self
    }

    fn interaction(&self) -> &Self::Interaction {
        self
    }

    fn capability_cells(&self, os_version: &str) -> Vec<CapabilityCell> {
        vec![
            CapabilityCell {
                key: CapabilityKey {
                    platform: PlatformName::Macos,
                    os_version: os_version.to_owned(),
                    action: ActionKind::Click,
                    addressing: AddressingMode::CapturedPoint,
                    framework: Framework::AppKit,
                    window_state: WindowStateKind::Visible,
                },
                decision: RouteDecision::Unsupported {
                    reason: "published evidence intentionally lags live fallback".to_owned(),
                },
            },
            CapabilityCell {
                key: CapabilityKey {
                    platform: PlatformName::Macos,
                    os_version: os_version.to_owned(),
                    action: ActionKind::Click,
                    addressing: AddressingMode::Element,
                    framework: Framework::AppKit,
                    window_state: WindowStateKind::Visible,
                },
                decision: RouteDecision::Supported {
                    route: Route::Semantic,
                },
            },
            CapabilityCell {
                key: CapabilityKey {
                    platform: PlatformName::Macos,
                    os_version: os_version.to_owned(),
                    action: ActionKind::PerformSecondaryAction,
                    addressing: AddressingMode::Element,
                    framework: Framework::AppKit,
                    window_state: WindowStateKind::Visible,
                },
                decision: RouteDecision::Supported {
                    route: Route::Semantic,
                },
            },
            CapabilityCell {
                key: CapabilityKey {
                    platform: PlatformName::Macos,
                    os_version: os_version.to_owned(),
                    action: ActionKind::SetValue,
                    addressing: AddressingMode::Element,
                    framework: Framework::AppKit,
                    window_state: WindowStateKind::Visible,
                },
                decision: RouteDecision::Supported {
                    route: Route::Semantic,
                },
            },
        ]
    }

    fn subscribe_invalidations(&self) -> Self::Invalidations {
        EmptyInvalidations
    }
}

#[tokio::test]
async fn keyboard_actions_reject_unknown_observations_before_provider_prepare() {
    let platform = Arc::new(FakePlatform::default());
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("keyboard-stale-client", ClientId::parse);
    controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: false,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();

    let missing = id("missing-observation", ObservationId::parse);
    let press_error = controller
        .press_key(
            &client,
            PressKeyCommand {
                window: window_ref(),
                request: PressKeyRequest {
                    observation_id: missing.clone(),
                    stroke: KeyStroke {
                        key: "a".to_owned(),
                        modifiers: Vec::new(),
                    },
                },
            },
        )
        .await
        .unwrap_err();
    let type_error = controller
        .type_text(
            &client,
            TypeTextCommand {
                window: window_ref(),
                request: TypeTextRequest {
                    observation_id: missing,
                    text: "content-never-reaches-provider".to_owned(),
                },
            },
        )
        .await
        .unwrap_err();

    for error in [press_error, type_error] {
        assert_eq!(error.code, ErrorCode::ObservationStale);
    }
    assert_eq!(platform.ordering.lock().unwrap().as_slice(), ["observe"]);
}

#[tokio::test]
async fn empty_type_text_is_a_signed_compatible_noop_without_native_dispatch() {
    let platform = Arc::new(FakePlatform::default());
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("empty-type-text-client", ClientId::parse);
    let state = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: false,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();

    let receipt = controller
        .type_text(
            &client,
            TypeTextCommand {
                window: window_ref(),
                request: TypeTextRequest {
                    observation_id: state.observation_id.clone(),
                    text: String::new(),
                },
            },
        )
        .await
        .unwrap();

    assert_eq!(receipt.consumed_observation_id, state.observation_id);
    assert_eq!(receipt.verification, VerificationLevel::EffectVerified);
    assert_eq!(
        receipt.native_evidence.fields.get("route_detail"),
        Some(&serde_json::json!("empty_text_noop"))
    );
    assert_eq!(platform.ordering.lock().unwrap().as_slice(), ["observe"]);
}

#[tokio::test]
async fn controller_preserves_target_state_and_consumes_at_dispatch_boundary() {
    let platform = Arc::new(FakePlatform::default());
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let manifest = controller.get_capabilities().await.unwrap();
    assert_eq!(manifest.cells.len(), 4);
    assert!(manifest
        .cells
        .iter()
        .any(|cell| matches!(cell.decision, RouteDecision::Unsupported { .. })));
    assert!(manifest.cells.iter().any(|cell| matches!(
        cell.decision,
        RouteDecision::Supported {
            route: Route::Semantic
        }
    )));

    let client_one = id("client-one", ClientId::parse);
    let first = controller
        .get_window_state(
            &client_one,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        first.accessibility.as_ref().unwrap().tree_update,
        AxTreeUpdate::Full { .. }
    ));

    let second = controller
        .get_window_state(
            &client_one,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();
    let AxTreeUpdate::Diff {
        base_revision,
        revision,
        ..
    } = &second.accessibility.as_ref().unwrap().tree_update
    else {
        panic!("second observation must be a diff");
    };
    assert_ne!(base_revision, revision);
    assert_eq!(platform.target_creations.load(Ordering::SeqCst), 1);

    let surface_id = second.surfaces[0].id.clone();
    let command = ClickCommand {
        window: window_ref(),
        request: ClickRequest {
            target: ClickTarget::Point {
                observation_id: second.observation_id.clone(),
                surface_id,
                point: Point { x: 20.0, y: 20.0 },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    };
    let receipt = controller
        .click(&client_one, command.clone())
        .await
        .unwrap();
    assert_eq!(receipt.consumed_observation_id, second.observation_id);
    assert_eq!(receipt.route, Route::TargetedPointer);
    assert_eq!(
        receipt.native_evidence.fields["capability_evidence"],
        "published_unsupported_nonblocking"
    );
    assert_eq!(
        receipt.native_evidence.fields["route_detail"],
        "direct_live_provider"
    );
    assert_eq!(receipt.native_evidence.fields["dispatch_scope"], "process");
    assert_eq!(receipt.settlement.state, SettledState::Settled);
    assert_eq!(
        platform.preflight_action_ids.lock().unwrap().as_slice(),
        [receipt.action_id.clone()]
    );
    assert_eq!(
        platform.acquired_action_ids.lock().unwrap().as_slice(),
        [receipt.action_id.clone()]
    );
    let scope_evidence = receipt
        .native_evidence
        .interaction_scope
        .as_ref()
        .expect("receipt carries typed scope evidence");
    assert_eq!(
        scope_evidence.acquisition.target_belief,
        LeaseDecision::Acquired
    );
    assert_eq!(
        scope_evidence
            .teardown
            .as_ref()
            .expect("completed scope carries teardown evidence")
            .target_belief,
        LeaseTeardownStatus::Released
    );
    assert_eq!(
        receipt.native_evidence.fields.get("fake_cleanup"),
        Some(&serde_json::json!("complete"))
    );

    let mut cross_window = command.clone();
    cross_window.window.id = id("window-2", WindowId::parse);
    let cross_error = controller
        .click(&client_one, cross_window)
        .await
        .unwrap_err();
    assert_eq!(cross_error.code, ErrorCode::WindowIdentityChanged);
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 1);
    let stale = controller.click(&client_one, command).await.unwrap_err();
    assert_eq!(stale.code, ErrorCode::ObservationStale);
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        platform.ordering.lock().unwrap().as_slice(),
        [
            "observe",
            "observe",
            "preflight",
            "scope_acquired",
            "pointer_prepare",
            "dispatch",
            "settle",
            "scope_released"
        ]
    );

    let resynchronized = controller
        .get_window_state(
            &client_one,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        resynchronized.accessibility.as_ref().unwrap().tree_update,
        AxTreeUpdate::Full { .. }
    ));
    platform.settle_pending.store(true, Ordering::SeqCst);
    let pending_command = ClickCommand {
        window: window_ref(),
        request: ClickRequest {
            target: ClickTarget::Point {
                observation_id: resynchronized.observation_id.clone(),
                surface_id: resynchronized.surfaces[0].id.clone(),
                point: Point { x: 20.0, y: 20.0 },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    };
    let pending_error = controller
        .click(&client_one, pending_command)
        .await
        .unwrap_err();
    assert_eq!(pending_error.code, ErrorCode::UiNotSettled);
    assert!(pending_error.pending_settlement.is_some());
    assert!(matches!(
        pending_error.partial_evidence.as_deref(),
        Some(PartialEvidence::Action {
            dispatch: Some(PartialNativeDispatch { .. }),
            pending_settlement: Some(_),
            ..
        })
    ));
    platform.settle_pending.store(false, Ordering::SeqCst);
    controller
        .get_window_state(
            &client_one,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();

    let client_two = id("client-two", ClientId::parse);
    controller
        .get_window_state(
            &client_two,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();
    assert_eq!(controller.close_connection(&client_one).await.unwrap(), 1);
    assert_eq!(controller.targets.len().await, 1);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(
        controller
            .targets
            .handle_invalidation(
                &platform,
                TargetInvalidation::ProcessExited {
                    process: resolved_window().process,
                },
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(controller.targets.len().await, 0);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn indexed_element_click_prefers_exact_semantic_delivery() {
    let platform = Arc::new(FakePlatform::default());
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("semantic-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: false,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let element = observed
        .accessibility
        .as_ref()
        .unwrap()
        .elements
        .first()
        .unwrap()
        .element_ref
        .clone();
    let receipt = controller
        .click(
            &client,
            ClickCommand {
                window: window_ref(),
                request: ClickRequest {
                    target: ClickTarget::Element { element },
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Vec::new(),
                },
            },
        )
        .await
        .unwrap();

    assert_eq!(receipt.route, Route::Semantic);
    assert_eq!(
        receipt.native_evidence.fields["route_detail"],
        "semantic_element_click"
    );
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 1);
    let key = TargetKey::from_window(client.clone(), &resolved_window());
    let target = controller.targets.get(&key).await.unwrap();
    assert_eq!(target.state.lock().await.platform.semantic_dispatches, 1);
    assert_eq!(
        receipt
            .native_evidence
            .interaction_scope
            .as_ref()
            .expect("element click carries typed scope evidence")
            .acquisition
            .target_belief,
        LeaseDecision::Acquired
    );
    assert_eq!(
        platform.ordering.lock().unwrap().as_slice(),
        [
            "observe",
            "preflight",
            "scope_acquired",
            "semantic_prepare",
            "dispatch",
            "settle",
            "scope_released"
        ]
    );
}

#[tokio::test]
async fn indexed_element_click_falls_back_before_dispatch_when_semantic_is_not_applicable() {
    let platform = Arc::new(FakePlatform::default());
    platform
        .semantic_candidate_unusable
        .store(true, Ordering::SeqCst);
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("pointer-fallback-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: false,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let element = observed.accessibility.as_ref().unwrap().elements[0]
        .element_ref
        .clone();

    let receipt = controller
        .click(
            &client,
            ClickCommand {
                window: window_ref(),
                request: ClickRequest {
                    target: ClickTarget::Element { element },
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Vec::new(),
                },
            },
        )
        .await
        .unwrap();

    assert_eq!(receipt.route, Route::TargetedPointer);
    assert_eq!(
        receipt.native_evidence.fields["route_detail"],
        "semantic_not_applicable:fake semantic click unavailable"
    );
    assert_eq!(
        receipt.native_evidence.fields["route_selection_detail"],
        "semantic_not_applicable:fake semantic click unavailable"
    );
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        platform.pointer_screen_points.lock().unwrap().as_slice(),
        [Point { x: 175.0, y: 235.0 }]
    );
    let key = TargetKey::from_window(client.clone(), &resolved_window());
    let target = controller.targets.get(&key).await.unwrap();
    assert_eq!(target.state.lock().await.platform.semantic_dispatches, 0);
    assert_eq!(
        platform.ordering.lock().unwrap().as_slice(),
        [
            "observe",
            "preflight",
            "scope_acquired",
            "pointer_prepare",
            "dispatch",
            "settle",
            "scope_released"
        ]
    );
}

#[tokio::test]
async fn element_scroll_without_semantic_page_action_uses_targeted_fractional_page_route() {
    let platform = Arc::new(FakePlatform::default());
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("element-scroll-fallback-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let element = observed.accessibility.as_ref().unwrap().elements[0]
        .element_ref
        .clone();

    let receipt = controller
        .scroll(
            &client,
            ScrollCommand {
                window: window_ref(),
                request: ScrollRequest::Element {
                    element,
                    direction: ScrollDirection::Down,
                    pages: 0.5,
                },
            },
        )
        .await
        .unwrap();

    assert_eq!(receipt.route, Route::TargetedPointer);
    assert_eq!(
        receipt.native_evidence.fields["capability_evidence"],
        "unmeasured"
    );
    assert!(receipt.native_evidence.fields["route_detail"]
        .as_str()
        .is_some_and(|detail| detail.starts_with("semantic_not_applicable:")));
}

#[tokio::test]
async fn semantic_prepare_refusal_preserves_the_observation_and_never_enters_dispatch() {
    let platform = Arc::new(FakePlatform::default());
    platform.semantic_prepare_fail.store(true, Ordering::SeqCst);
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("semantic-prepare-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: false,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let element = observed
        .accessibility
        .as_ref()
        .unwrap()
        .elements
        .first()
        .unwrap()
        .element_ref
        .clone();
    let command = ClickCommand {
        window: window_ref(),
        request: ClickRequest {
            target: ClickTarget::Element { element },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    };

    let refused = controller
        .click(&client, command.clone())
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnsupportedInBackground);
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 0);
    let key = TargetKey::from_window(client.clone(), &resolved_window());
    let target = controller.targets.get(&key).await.unwrap();
    assert_eq!(target.state.lock().await.platform.semantic_dispatches, 0);

    platform
        .semantic_prepare_fail
        .store(false, Ordering::SeqCst);
    controller.click(&client, command).await.unwrap();
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn menu_managed_secondary_axpress_refuses_in_prepare_without_consuming_observation() {
    for role in [
        "AXMenu",
        "AXMenuItem",
        "AXMenuBar",
        "AXMenuBarItem",
        "AXPopUpButton",
        "AXMenuButton",
    ] {
        let platform = Arc::new(FakePlatform::default());
        *platform.observation_role.lock().unwrap() = Some(role.to_owned());
        let controller =
            DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
        let client = ClientId::parse(format!("menu-secondary-{role}")).unwrap();
        let observed = controller
            .get_window_state(
                &client,
                GetWindowStateRequest {
                    window: window_ref(),
                    include_text: true,
                    include_screenshots: false,
                    ax_tree_mode: AxTreeMode::Full,
                },
            )
            .await
            .unwrap();
        let element = observed
            .accessibility
            .as_ref()
            .unwrap()
            .elements
            .first()
            .unwrap()
            .element_ref
            .clone();

        let error = controller
            .perform_secondary_action(
                &client,
                SecondaryActionCommand {
                    window: window_ref(),
                    request: SecondaryActionRequest {
                        element,
                        action: "AXPress".to_owned(),
                    },
                },
            )
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::UnsupportedInBackground, "{role}");
        assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 0, "{role}");
        let key = TargetKey::from_window(client, &resolved_window());
        let target = controller.targets.get(&key).await.unwrap();
        let mut state = target.state.lock().await;
        let current_window = state.window.clone();
        state
            .observations
            .current(&observed.observation_id, &current_window)
            .unwrap_or_else(|error| panic!("{role} observation was consumed: {error}"));
    }
}

#[tokio::test]
async fn semantic_readback_mismatch_settles_and_remains_the_primary_failure() {
    let platform = Arc::new(FakePlatform::default());
    platform
        .semantic_verification_fail
        .store(true, Ordering::SeqCst);
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("semantic-readback-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: false,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let element = observed
        .accessibility
        .as_ref()
        .unwrap()
        .elements
        .first()
        .unwrap()
        .element_ref
        .clone();

    let error = controller
        .set_value(
            &client,
            SetValueCommand {
                window: window_ref(),
                request: SetValueRequest {
                    element,
                    value: "expected exact value".to_owned(),
                },
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::VerificationFailed);
    assert!(!error
        .related_failures
        .iter()
        .any(|failure| failure.code == ErrorCode::UiNotSettled));
    let Some(PartialEvidence::Action {
        pending_settlement, ..
    }) = error.partial_evidence.as_deref()
    else {
        panic!("post-dispatch verification failure must retain partial action evidence");
    };
    assert!(pending_settlement.is_none());
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn coherent_observation_accepts_refreshed_geometry_without_relaxing_identity() {
    let platform = Arc::new(FakePlatform::default());
    platform
        .refresh_geometry_during_observe
        .store(true, Ordering::SeqCst);
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("geometry-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();

    let key = TargetKey::from_window(client, &resolved_window());
    let target = controller.targets.get(&key).await.unwrap();
    let mut state = target.state.lock().await;
    assert_eq!(
        state.window.geometry.revision,
        id("geometry-refreshed", GeometryRevision::parse)
    );
    assert_eq!(state.window.geometry.bounds.x, 110.0);
    let current_window = state.window.clone();
    state
        .observations
        .current(&observed.observation_id, &current_window)
        .unwrap();
}

#[tokio::test]
async fn target_registry_tears_down_idle_and_superseded_generations_exactly() {
    let platform = FakePlatform::default();
    let registry = TargetControllerRegistry::new(Duration::ZERO);
    let client = id("registry-client", ClientId::parse);
    let generation_one = resolved_window();
    registry
        .get_or_create(
            &platform,
            TargetKey::from_window(client.clone(), &generation_one),
            generation_one.clone(),
        )
        .await
        .unwrap();
    assert_eq!(registry.expire_idle(&platform).await.unwrap(), 1);
    assert!(registry.is_empty().await);

    registry
        .get_or_create(
            &platform,
            TargetKey::from_window(client.clone(), &generation_one),
            generation_one.clone(),
        )
        .await
        .unwrap();
    let mut generation_two = generation_one;
    generation_two.public.generation = WindowGeneration(2);
    registry
        .get_or_create(
            &platform,
            TargetKey::from_window(client, &generation_two),
            generation_two,
        )
        .await
        .unwrap();
    assert_eq!(registry.len().await, 1);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn native_event_source_lag_tears_down_every_cached_target() {
    let platform = FakePlatform::default();
    let registry = TargetControllerRegistry::new(Duration::from_secs(60));
    let first = resolved_window();
    registry
        .get_or_create(
            &platform,
            TargetKey::from_window(id("lag-client-one", ClientId::parse), &first),
            first.clone(),
        )
        .await
        .unwrap();
    registry
        .get_or_create(
            &platform,
            TargetKey::from_window(id("lag-client-two", ClientId::parse), &first),
            first,
        )
        .await
        .unwrap();

    assert_eq!(
        registry
            .handle_invalidation(&platform, TargetInvalidation::NativeStateResyncRequired)
            .await
            .unwrap(),
        2
    );
    assert!(registry.is_empty().await);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 2);
}

#[test]
fn interaction_scope_release_is_idempotent_and_accumulates_teardown_evidence() {
    let cleanup_count = Arc::new(AtomicUsize::new(0));
    let ordering = Arc::new(Mutex::new(Vec::new()));
    let acquisition = ScopeLeaseAcquisition {
        accessibility: LeaseDecision::Acquired,
        menu_dismissal: LeaseDecision::NotApplicable,
        target_belief: LeaseDecision::Acquired,
    };
    let mut scope = ScopePlan::new(
        id("idempotent-action", ActionId::parse),
        resolved_window(),
        Route::TargetedPointer,
        test_mutation_deadline(),
        ScopeRequirements::for_route(Route::TargetedPointer),
        DispatchScopeKind::Process,
        (),
    )
    .into_scope(
        acquisition,
        TargetCursorHandle::default(),
        NativeEvidence::default(),
        Box::new(FakeScopeCleanup {
            ordering,
            cleanup_count: Arc::clone(&cleanup_count),
            failure_count: 2,
            leases: ScopeLeaseTeardown {
                accessibility: LeaseTeardownStatus::Released,
                menu_dismissal: LeaseTeardownStatus::NotApplicable,
                target_belief: LeaseTeardownStatus::Failed,
            },
        }),
    );

    let first = scope.release();
    let second = scope.release();

    assert_eq!(first, second);
    assert_eq!(first.failures.len(), 2);
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        scope.native_evidence.fields.get("fake_cleanup"),
        Some(&serde_json::json!("complete"))
    );
    assert_eq!(
        scope
            .native_evidence
            .interaction_scope
            .as_ref()
            .and_then(|evidence| evidence.teardown.as_ref())
            .map(|teardown| teardown.target_belief),
        Some(LeaseTeardownStatus::Failed)
    );
    drop(scope);
    assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dispatch_failure_remains_primary_and_keeps_every_cleanup_failure() {
    let platform = Arc::new(FakePlatform::default());
    platform.dispatch_fail.store(true, Ordering::SeqCst);
    platform.settle_pending.store(true, Ordering::SeqCst);
    platform.cleanup_failure_count.store(2, Ordering::SeqCst);
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("failure-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();
    let error = controller
        .click(
            &client,
            ClickCommand {
                window: window_ref(),
                request: ClickRequest {
                    target: ClickTarget::Point {
                        observation_id: observed.observation_id.clone(),
                        surface_id: observed.surfaces[0].id.clone(),
                        point: Point { x: 20.0, y: 20.0 },
                    },
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Vec::new(),
                },
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::DispatchFailed);
    assert_eq!(error.related_failures.len(), 3);
    for expected in [
        ErrorCode::UiNotSettled,
        ErrorCode::VerificationFailed,
        ErrorCode::Internal,
    ] {
        assert!(error
            .related_failures
            .iter()
            .any(|failure| failure.code == expected));
    }
    let Some(PartialEvidence::Action {
        dispatch: None,
        native_evidence,
        pending_settlement: Some(_),
        ..
    }) = error.partial_evidence.as_deref()
    else {
        panic!("post-dispatch failure must keep scope and settlement evidence");
    };
    assert_eq!(
        native_evidence
            .interaction_scope
            .as_ref()
            .and_then(|evidence| evidence.teardown.as_ref())
            .map(|teardown| teardown.target_belief),
        Some(LeaseTeardownStatus::Failed)
    );
    assert_eq!(platform.cleanup_count.load(Ordering::SeqCst), 1);
    let missing = controller
        .targets
        .get(&TargetKey::from_window(client, &resolved_window()))
        .await
        .err()
        .expect("failed scope teardown removes the target");
    assert_eq!(missing.code, ErrorCode::ObservationStale);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn different_process_mutations_progress_while_one_dispatch_is_blocked() {
    let platform = Arc::new(FakePlatform::default());
    let controller = Arc::new(DriverController::new(
        Arc::clone(&platform),
        PlatformName::Macos,
        "test-os",
    ));
    let client = id("disjoint-process-client", ClientId::parse);
    let first_window = window_ref();
    let second_window = alternate_window("window-2", "app-2", 84, "process-84").public;
    let first_state = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: first_window.clone(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let second_state = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: second_window.clone(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();

    *platform.blocked_dispatch_process.lock().unwrap() = Some("process-42".to_owned());
    platform.block_dispatch.store(true, Ordering::SeqCst);
    let dispatch_entered = platform.dispatch_entered.notified();
    let first = tokio::spawn({
        let controller = Arc::clone(&controller);
        let client = client.clone();
        let command = point_click_command(first_window, &first_state);
        async move { controller.click(&client, command).await }
    });
    dispatch_entered.await;

    tokio::time::timeout(
        Duration::from_secs(1),
        controller.click(&client, point_click_command(second_window, &second_state)),
    )
    .await
    .expect("a different process must not wait behind the blocked dispatch")
    .unwrap();
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 2);

    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn process_guard_remains_held_through_settlement() {
    let platform = Arc::new(FakePlatform::default());
    let controller = Arc::new(DriverController::new(
        Arc::clone(&platform),
        PlatformName::Macos,
        "test-os",
    ));
    let client = id("settlement-guard-client", ClientId::parse);
    let first_window = window_ref();
    let same_process_window = alternate_window("window-3", "app-1", 42, "process-42").public;
    let first_state = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: first_window.clone(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let second_state = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: same_process_window.clone(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();

    platform.block_settle.store(true, Ordering::SeqCst);
    let settle_entered = platform.settle_entered.notified();
    let first = tokio::spawn({
        let controller = Arc::clone(&controller);
        let client = client.clone();
        let command = point_click_command(first_window, &first_state);
        async move { controller.click(&client, command).await }
    });
    settle_entered.await;

    let conflict = controller
        .click(
            &client,
            point_click_command(same_process_window, &second_state),
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::TargetBusy);
    assert_eq!(conflict.details["native_side_effect_started"], false);
    assert_eq!(platform.dispatch_count.load(Ordering::SeqCst), 1);

    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn cancellation_before_first_native_side_effect_preserves_current_settled_observation() {
    let platform = Arc::new(FakePlatform::default());
    let controller = Arc::new(DriverController::new(
        Arc::clone(&platform),
        PlatformName::Macos,
        "test-os",
    ));
    let client = id("cancel-before-boundary-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let command = ClickCommand {
        window: window_ref(),
        request: ClickRequest {
            target: ClickTarget::Point {
                observation_id: observed.observation_id.clone(),
                surface_id: observed.surfaces[0].id.clone(),
                point: Point { x: 20.0, y: 20.0 },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    };

    platform.block_before_boundary.store(true, Ordering::SeqCst);
    let dispatch_entered = platform.dispatch_entered.notified();
    let action = tokio::spawn({
        let controller = Arc::clone(&controller);
        let client = client.clone();
        let command = command.clone();
        async move { controller.click(&client, command).await }
    });
    dispatch_entered.await;
    action.abort();
    assert!(action.await.unwrap_err().is_cancelled());
    assert_eq!(platform.cleanup_count.load(Ordering::SeqCst), 1);

    let target = controller
        .targets
        .get(&TargetKey::from_window(client.clone(), &resolved_window()))
        .await
        .unwrap();
    let mut state = target.state.lock().await;
    assert!(state.settlement.settled_evidence().is_some());
    state
        .observations
        .current(&observed.observation_id, &resolved_window())
        .expect("pre-boundary cancellation must preserve the current observation");
    drop(state);

    platform
        .block_before_boundary
        .store(false, Ordering::SeqCst);
    controller.click(&client, command).await.unwrap();
}

#[tokio::test]
async fn cancellation_inside_provider_leaves_observation_consumed_and_target_dirty() {
    let platform = Arc::new(FakePlatform::default());
    let controller = Arc::new(DriverController::new(
        Arc::clone(&platform),
        PlatformName::Macos,
        "test-os",
    ));
    let client = id("cancel-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();
    let command = ClickCommand {
        window: window_ref(),
        request: ClickRequest {
            target: ClickTarget::Point {
                observation_id: observed.observation_id.clone(),
                surface_id: observed.surfaces[0].id.clone(),
                point: Point { x: 20.0, y: 20.0 },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    };

    platform.block_dispatch.store(true, Ordering::SeqCst);
    let dispatch_entered = platform.dispatch_entered.notified();
    let action = tokio::spawn({
        let controller = Arc::clone(&controller);
        let client = client.clone();
        let command = command.clone();
        async move { controller.click(&client, command).await }
    });
    dispatch_entered.await;
    action.abort();
    assert!(action.await.unwrap_err().is_cancelled());
    assert_eq!(platform.cleanup_count.load(Ordering::SeqCst), 1);
    platform.block_dispatch.store(false, Ordering::SeqCst);

    let target = controller
        .targets
        .get(&TargetKey::from_window(client.clone(), &resolved_window()))
        .await
        .unwrap();
    let mut state = target.state.lock().await;
    assert!(state.settlement.settled_evidence().is_none());
    let stale = state
        .observations
        .current(&observed.observation_id, &resolved_window())
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::ObservationStale);
    drop(state);

    let dirty = controller.click(&client, command).await.unwrap_err();
    assert_eq!(dirty.code, ErrorCode::UiNotSettled);
}

#[tokio::test]
async fn cancellation_cleanup_failure_poison_is_rebuilt_by_the_next_observation() {
    let platform = Arc::new(FakePlatform::default());
    platform.cleanup_failure_count.store(1, Ordering::SeqCst);
    let controller = Arc::new(DriverController::new(
        Arc::clone(&platform),
        PlatformName::Macos,
        "test-os",
    ));
    let client = id("cancel-cleanup-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();
    let command = ClickCommand {
        window: window_ref(),
        request: ClickRequest {
            target: ClickTarget::Point {
                observation_id: observed.observation_id,
                surface_id: observed.surfaces[0].id.clone(),
                point: Point { x: 20.0, y: 20.0 },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    };

    platform.block_dispatch.store(true, Ordering::SeqCst);
    let dispatch_entered = platform.dispatch_entered.notified();
    let action = tokio::spawn({
        let controller = Arc::clone(&controller);
        let client = client.clone();
        async move { controller.click(&client, command).await }
    });
    dispatch_entered.await;
    action.abort();
    assert!(action.await.unwrap_err().is_cancelled());
    assert_eq!(platform.cleanup_count.load(Ordering::SeqCst), 1);

    let poisoned = controller
        .targets
        .get(&TargetKey::from_window(client.clone(), &resolved_window()))
        .await
        .err()
        .expect("cancellation cleanup failure poisons the target");
    assert_eq!(poisoned.code, ErrorCode::WindowIdentityChanged);

    platform.block_dispatch.store(false, Ordering::SeqCst);
    platform.cleanup_failure_count.store(0, Ordering::SeqCst);
    controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    assert_eq!(platform.target_creations.load(Ordering::SeqCst), 2);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn typed_native_cleanup_poison_removes_target_and_forces_rebuild() {
    let platform = Arc::new(FakePlatform::default());
    platform.dispatch_poison.store(true, Ordering::SeqCst);
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os");
    let client = id("native-cleanup-poison-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    let error = controller
        .click(
            &client,
            ClickCommand {
                window: window_ref(),
                request: ClickRequest {
                    target: ClickTarget::Point {
                        observation_id: observed.observation_id,
                        surface_id: observed.surfaces[0].id.clone(),
                        point: Point { x: 20.0, y: 20.0 },
                    },
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Vec::new(),
                },
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::DispatchFailed);
    assert!(controller
        .targets
        .get(&TargetKey::from_window(client.clone(), &resolved_window()))
        .await
        .is_err());

    platform.dispatch_poison.store(false, Ordering::SeqCst);
    controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::Full,
            },
        )
        .await
        .unwrap();
    assert_eq!(platform.target_creations.load(Ordering::SeqCst), 2);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn wedged_dispatch_deadline_releases_scope_without_reusing_observation() {
    let platform = Arc::new(FakePlatform::default());
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os")
        .with_mutation_timeouts(Duration::from_millis(40), Duration::from_millis(100));
    let client = id("deadline-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();
    let command = ClickCommand {
        window: window_ref(),
        request: ClickRequest {
            target: ClickTarget::Point {
                observation_id: observed.observation_id.clone(),
                surface_id: observed.surfaces[0].id.clone(),
                point: Point { x: 20.0, y: 20.0 },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    };

    platform.block_dispatch.store(true, Ordering::SeqCst);
    let error = controller
        .click(&client, command.clone())
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::DispatchFailed);
    assert_eq!(error.phase, ErrorPhase::Dispatch);
    assert!(!error.retryable);
    let Some(PartialEvidence::Action {
        dispatch: None,
        pending_settlement: Some(pending),
        native_evidence,
        ..
    }) = error.partial_evidence.as_deref()
    else {
        panic!("dispatch deadline must keep incomplete-dispatch and teardown evidence");
    };
    assert!(!pending
        .observed_signals
        .contains(&SettlementSignal::DispatchComplete));
    assert_eq!(
        native_evidence.fields.get("fake_cleanup"),
        Some(&serde_json::json!("complete"))
    );
    assert_eq!(platform.cleanup_count.load(Ordering::SeqCst), 1);
    let ordering = platform.ordering.lock().unwrap();
    let dispatch_index = ordering
        .iter()
        .position(|event| *event == "dispatch")
        .unwrap();
    let release_index = ordering
        .iter()
        .position(|event| *event == "scope_released")
        .unwrap();
    assert!(dispatch_index < release_index);
    drop(ordering);

    let target = controller
        .targets
        .get(&TargetKey::from_window(client.clone(), &resolved_window()))
        .await
        .unwrap();
    let mut state = target.state.lock().await;
    assert!(state.settlement.settled_evidence().is_none());
    let stale = state
        .observations
        .current(&observed.observation_id, &resolved_window())
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::ObservationStale);
    drop(state);
    platform.block_dispatch.store(false, Ordering::SeqCst);
    let dirty = controller.click(&client, command).await.unwrap_err();
    assert_eq!(dirty.code, ErrorCode::UiNotSettled);
}

#[tokio::test]
async fn deadline_cleanup_failure_keeps_evidence_and_removes_poisoned_target() {
    let platform = Arc::new(FakePlatform::default());
    platform.cleanup_failure_count.store(1, Ordering::SeqCst);
    let controller = DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os")
        .with_mutation_timeouts(Duration::from_millis(40), Duration::from_millis(100));
    let client = id("deadline-cleanup-client", ClientId::parse);
    let observed = controller
        .get_window_state(
            &client,
            GetWindowStateRequest {
                window: window_ref(),
                include_text: true,
                include_screenshots: true,
                ax_tree_mode: AxTreeMode::DiffIfAvailable,
            },
        )
        .await
        .unwrap();
    platform.block_dispatch.store(true, Ordering::SeqCst);
    let error = controller
        .click(
            &client,
            ClickCommand {
                window: window_ref(),
                request: ClickRequest {
                    target: ClickTarget::Point {
                        observation_id: observed.observation_id,
                        surface_id: observed.surfaces[0].id.clone(),
                        point: Point { x: 20.0, y: 20.0 },
                    },
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Vec::new(),
                },
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::DispatchFailed);
    assert!(error
        .related_failures
        .iter()
        .any(|failure| failure.code == ErrorCode::VerificationFailed));
    let Some(PartialEvidence::Action {
        dispatch: None,
        pending_settlement: Some(pending),
        native_evidence,
        ..
    }) = error.partial_evidence.as_deref()
    else {
        panic!("cleanup failure must retain dispatch, settlement, and scope evidence");
    };
    assert!(!pending
        .observed_signals
        .contains(&SettlementSignal::DispatchComplete));
    assert_eq!(
        native_evidence.fields.get("fake_cleanup"),
        Some(&serde_json::json!("complete"))
    );
    assert_eq!(
        native_evidence
            .interaction_scope
            .as_ref()
            .and_then(|evidence| evidence.teardown.as_ref())
            .map(|teardown| teardown.target_belief),
        Some(LeaseTeardownStatus::Failed)
    );
    assert_eq!(platform.cleanup_count.load(Ordering::SeqCst), 1);
    let missing = controller
        .targets
        .get(&TargetKey::from_window(client, &resolved_window()))
        .await
        .err()
        .expect("deadline cleanup failure removes the poisoned target");
    assert_eq!(missing.code, ErrorCode::ObservationStale);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 1);
}

#[test]
fn pending_settlement_progress_survives_for_the_next_attempt() {
    let action_id = id("pending-action", ActionId::parse);
    let profile = SettlementProfile::requiring(
        "exact_value",
        [
            SettlementSignal::AxValueChanged,
            SettlementSignal::VerificationReadbackComplete,
        ],
    );
    let mut state = SettlementState::default();
    state.mark_dirty(action_id.clone(), profile).unwrap();
    let dirty = state.begin(false).unwrap();
    let pending = PendingSettlementEvidence {
        state: PendingSettlementState::Pending,
        trigger_action_id: action_id,
        profile: "exact_value".to_owned(),
        elapsed_ms: 20,
        observed_signals: vec![
            SettlementSignal::DispatchComplete,
            SettlementSignal::AxValueChanged,
        ],
        missing_signals: vec![SettlementSignal::VerificationReadbackComplete],
    };
    assert!(!dirty
        .observed_signals
        .contains(&SettlementSignal::AxValueChanged));
    state.preserve_pending(&pending).unwrap();
    let resumed = state.begin(true).unwrap();
    assert!(resumed
        .observed_signals
        .contains(&SettlementSignal::AxValueChanged));
    assert!(resumed.resumed_from_prior_call);
}

#[test]
fn menu_state_only_publishes_stable_lifecycle_states() {
    let mut menu = MenuControllerState::default();
    let action = id("action-1", ActionId::parse);
    let owner = resolved_window().stamp();
    let menu_id = menu.begin_open(action.clone(), window_ref(), owner.clone());
    let transitional = menu.observation().unwrap_err();
    assert_eq!(transitional.code, ErrorCode::MenuStateStale);
    menu.record_open(
        &menu_id,
        &menu_id,
        &action,
        &owner,
        NativeMenuIdentity {
            process: NativeProcessHandle::new("menu-process").unwrap(),
            window: NativeWindowHandle::new("menu-window").unwrap(),
            generation: WindowGeneration(1),
        },
        Vec::new(),
        None,
    )
    .unwrap();
    assert!(matches!(
        menu.observation().unwrap(),
        MenuState::Open {
            opened_by_action_id,
            ..
        } if opened_by_action_id == action
    ));
    menu.begin_dismiss(id("action-2", ActionId::parse)).unwrap();
    menu.close();
    assert!(matches!(
        menu.observation().unwrap(),
        MenuState::Closed { .. }
    ));
}

#[tokio::test]
async fn native_v2_stdio_drains_deadline_owned_work_before_eof_cleanup() {
    use cua_driver_core::{
        protocol::{V2HandshakeRequest, V2HandshakeResponse, V2_METHODS},
        server::{run_v2_io, V2ServerMetadata},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn write_json(
        writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        value: &impl serde::Serialize,
    ) {
        writer
            .write_all(serde_json::to_string(value).unwrap().as_bytes())
            .await
            .unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn read_json<T: serde::de::DeserializeOwned>(
        reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    ) -> T {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    let platform = Arc::new(FakePlatform::default());
    let controller = Arc::new(
        DriverController::new(Arc::clone(&platform), PlatformName::Macos, "test-os")
            .with_mutation_timeouts(Duration::from_millis(40), Duration::from_millis(100)),
    );
    let targets = Arc::clone(&controller.targets);
    let (client_stream, server_stream) = tokio::io::duplex(1024 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server = tokio::spawn(run_v2_io(
        server_read,
        server_write,
        controller,
        V2ServerMetadata {
            driver_name: "fake-macos".to_owned(),
            driver_version: "test".to_owned(),
            build: "test-build".to_owned(),
        },
    ));
    let (client_read, mut client_write) = tokio::io::split(client_stream);
    let mut client_read = BufReader::new(client_read);

    write_json(
        &mut client_write,
        &V2HandshakeRequest {
            request_id: "handshake".to_owned(),
            minimum_version: V2_PROTOCOL_VERSION,
            maximum_version: V2_PROTOCOL_VERSION,
        },
    )
    .await;
    let handshake: V2HandshakeResponse = read_json(&mut client_read).await;
    assert_eq!(handshake.request_id, "handshake");
    assert_eq!(handshake.minimum_version, V2_PROTOCOL_VERSION);
    assert_eq!(handshake.maximum_version, V2_PROTOCOL_VERSION);
    assert_eq!(
        handshake.methods,
        V2_METHODS
            .iter()
            .map(|method| (*method).to_owned())
            .collect::<Vec<_>>()
    );

    for (request_id, name_contains) in [
        ("apps-slow", "slow-protocol-read"),
        ("apps-fast", "fast-protocol-read"),
    ] {
        write_json(
            &mut client_write,
            &V2RequestEnvelope {
                request_id: request_id.to_owned(),
                protocol_version: V2_PROTOCOL_VERSION,
                command: V2Command::ListApps(ListAppsRequest {
                    query: Some(AppQuery {
                        name_contains: Some(name_contains.to_owned()),
                        running: None,
                    }),
                }),
            },
        )
        .await;
    }
    let fast: V2ResponseEnvelope<serde_json::Value> = read_json(&mut client_read).await;
    let slow: V2ResponseEnvelope<serde_json::Value> = read_json(&mut client_read).await;
    assert_eq!(fast.request_id, "apps-fast");
    assert_eq!(slow.request_id, "apps-slow");

    let mut latest_state = None;
    for (request_id, mode) in [
        ("state-full", AxTreeMode::Full),
        ("state-diff", AxTreeMode::DiffIfAvailable),
    ] {
        write_json(
            &mut client_write,
            &V2RequestEnvelope {
                request_id: request_id.to_owned(),
                protocol_version: V2_PROTOCOL_VERSION,
                command: V2Command::GetWindowState(GetWindowStateRequest {
                    window: window_ref(),
                    include_text: true,
                    include_screenshots: false,
                    ax_tree_mode: mode,
                }),
            },
        )
        .await;
        let response: V2ResponseEnvelope<WindowState> = read_json(&mut client_read).await;
        let V2ResponseBody::Result(result) = response.body else {
            panic!("fake observation should succeed");
        };
        let state = result.result;
        let accessibility = state.accessibility.as_ref().expect("fake AX state");
        match (mode, &accessibility.tree_update) {
            (AxTreeMode::Full, AxTreeUpdate::Full { .. })
            | (AxTreeMode::DiffIfAvailable, AxTreeUpdate::Diff { .. }) => {}
            other => panic!("unexpected revision mode: {other:?}"),
        }
        latest_state = Some(state);
    }

    assert_eq!(platform.target_creations.load(Ordering::SeqCst), 1);
    assert_eq!(targets.len().await, 1);

    let state = latest_state.expect("latest observation");
    platform.block_dispatch.store(true, Ordering::SeqCst);
    let click_command = ClickCommand {
        window: window_ref(),
        request: ClickRequest {
            target: ClickTarget::Point {
                observation_id: state.observation_id,
                surface_id: state.surfaces[0].id.clone(),
                point: Point { x: 20.0, y: 20.0 },
            },
            button: MouseButton::Left,
            click_count: 1,
            modifiers: Vec::new(),
        },
    };
    write_json(
        &mut client_write,
        &V2RequestEnvelope {
            request_id: "click-drained-after-eof".to_owned(),
            protocol_version: V2_PROTOCOL_VERSION,
            command: V2Command::Click(click_command.clone()),
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while platform.dispatch_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("click dispatch entered before EOF");

    write_json(
        &mut client_write,
        &V2RequestEnvelope {
            request_id: "click-concurrent-refused".to_owned(),
            protocol_version: V2_PROTOCOL_VERSION,
            command: V2Command::Click(click_command),
        },
    )
    .await;
    let concurrent: V2ResponseEnvelope<serde_json::Value> = read_json(&mut client_read).await;
    assert_eq!(concurrent.request_id, "click-concurrent-refused");
    let V2ResponseBody::Error(failure) = concurrent.body else {
        panic!("a second effectful request must not queue outside native deadlines");
    };
    assert_eq!(failure.error.code, ErrorCode::TargetBusy);
    assert!(failure.error.retryable);

    client_write.shutdown().await.unwrap();
    drop(client_write);
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("EOF drain honors the native mutation deadline")
        .unwrap()
        .unwrap();
    let response: V2ResponseEnvelope<serde_json::Value> = read_json(&mut client_read).await;
    assert_eq!(response.request_id, "click-drained-after-eof");
    let V2ResponseBody::Error(failure) = response.body else {
        panic!("wedged dispatch must complete with its typed native deadline failure");
    };
    assert_eq!(failure.error.code, ErrorCode::DispatchFailed);
    assert_eq!(platform.cleanup_count.load(Ordering::SeqCst), 1);
    assert!(targets.is_empty().await);
    assert_eq!(platform.shutdowns.load(Ordering::SeqCst), 1);
}

#[allow(dead_code)]
fn _settlement_type_assertion(_: BTreeSet<SettlementSignal>) {}
