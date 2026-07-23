use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use cua_driver_core::{
    api::{
        capabilities::{CapabilityManifest, PlatformName},
        contracts::{
            ActionId, AppId, AppQuery, AppRef, AppSelector, PermissionState, Readiness, WindowId,
            WindowRef,
        },
        errors::{ErrorCode, ErrorPhase, NativeError},
        interaction::PostureResult,
        platform::{LaunchPostureScope, LifecycleProvider, NativeLaunch, WindowProvider},
        settlement::{
            PendingSettlementEvidence, PendingSettlementState, SettledState, SettlementEvidence,
            SettlementSignal,
        },
    },
    protocol::V2_PROTOCOL_VERSION,
};

use crate::{
    apps::{
        self,
        nsworkspace::{self, RunningApplicationInfo, WorkspaceEventHub, WorkspaceEventKind},
    },
    input::{skylight, slps_make_key},
    permissions, windows,
};

use super::{posture::MacLaunchPostureWitness, windows::MacWindowRegistry};

const LAUNCH_DEADLINE: Duration = Duration::from_secs(12);
const WINDOW_QUIET_WINDOW: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct MacLifecycle {
    windows: MacWindowRegistry,
    workspace_events: Arc<WorkspaceEventHub>,
}

impl MacLifecycle {
    pub fn new(windows: MacWindowRegistry) -> Self {
        Self {
            windows,
            workspace_events: WorkspaceEventHub::shared(),
        }
    }
}

#[async_trait]
impl LifecycleProvider for MacLifecycle {
    async fn readiness(&self) -> Result<Readiness, NativeError> {
        tokio::task::spawn_blocking(readiness_snapshot)
            .await
            .map_err(join_error)
    }

    async fn capabilities(&self) -> Result<CapabilityManifest, NativeError> {
        let readiness = self.readiness().await?;
        Ok(CapabilityManifest {
            platform: PlatformName::Macos,
            driver_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: format!(
                "{}.{}",
                V2_PROTOCOL_VERSION.major, V2_PROTOCOL_VERSION.minor
            ),
            permissions: readiness
                .permissions
                .into_iter()
                .map(|(name, state)| (name, state == PermissionState::Granted))
                .collect(),
            cells: Vec::new(),
        })
    }

    async fn list_apps(&self, query: AppQuery) -> Result<Vec<AppRef>, NativeError> {
        tokio::task::spawn_blocking(move || list_apps_native(query))
            .await
            .map_err(join_error)?
    }

    async fn launch_background(
        &self,
        selector: AppSelector,
        posture_scope: &mut LaunchPostureScope,
    ) -> Result<NativeLaunch, NativeError> {
        if !permissions::status::accessibility_granted() {
            return Err(NativeError::new(
                ErrorCode::PermissionDenied,
                ErrorPhase::Preflight,
                true,
                "Accessibility permission is required for exact background window identity",
            ));
        }
        let resolved = tokio::task::spawn_blocking(move || resolve_launch(selector))
            .await
            .map_err(join_error)??;
        let preexisting = nsworkspace::running_applications()
            .into_iter()
            .find(|application| resolved.matches(application));

        if reusable_existing_launch(&resolved, preexisting.as_ref()) {
            if let Some(application) = preexisting.as_ref() {
                let started = Instant::now();
                let deadline = started + LAUNCH_DEADLINE;
                let mut events = self.workspace_events.subscribe();
                let mut witness = MacLaunchPostureWitness::begin(deadline)?;
                witness.set_target(application.pid)?;
                let app = app_ref_for_running(application);
                let generation = application
                    .process_generation
                    .expect("reusable_existing_launch requires an exact process generation");
                let settlement_action_id = posture_scope
                    .action_id()
                    .cloned()
                    .unwrap_or_else(ActionId::new);
                posture_scope.record_partial_result(app.clone(), Vec::new());
                posture_scope.pending_settlement = Some(pending_launch_evidence(
                    &settlement_action_id,
                    "macos_launch_reuse",
                    started,
                    Vec::new(),
                    Vec::new(),
                ));
                let result = match self.windows.list_windows(Some(&app)).await {
                    Ok(baseline_windows) => {
                        posture_scope.record_partial_result(app.clone(), baseline_windows.clone());
                        wait_for_stable_windows(
                            &self.windows,
                            LaunchWait {
                                app: &app,
                                pid: application.pid,
                                expected_generation: generation,
                                baseline_windows: &baseline_windows,
                                dispatched: false,
                                settlement_action_id: &settlement_action_id,
                                profile: "macos_launch_reuse",
                                started,
                                deadline,
                            },
                            &mut events,
                            &mut witness,
                            posture_scope,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                let (posture, native_evidence) = witness.finish();
                posture_scope.posture = posture.clone();
                posture_scope.native_evidence = native_evidence;
                if result.is_ok() {
                    posture_scope.pending_settlement = None;
                }
                let posture_failure = posture_failure(&posture, Some(&app), posture_scope);
                return match (result, posture_failure) {
                    (Ok(stable), None) => Ok(NativeLaunch {
                        app,
                        windows: stable.windows,
                        reused_running_app: true,
                        posture,
                        settlement: launch_settlement(
                            "macos_launch_reuse",
                            started,
                            false,
                            stable.window_list_changed,
                            stable.windows_empty,
                        ),
                    }),
                    (Ok(_), Some(posture_error)) => Err(posture_error),
                    (Err(error), None) => Err(error),
                    (Err(error), Some(posture_error)) => {
                        Err(NativeError::primary(vec![error, posture_error])
                            .expect("launch failures are nonempty"))
                    }
                };
            }
        }

        let started = Instant::now();
        let deadline = started + LAUNCH_DEADLINE;
        let settlement_action_id = posture_scope
            .action_id()
            .cloned()
            .unwrap_or_else(ActionId::new);
        let baseline_windows = if let Some(application) = preexisting.as_ref() {
            let app = app_ref_for_running(application);
            self.windows.list_windows(Some(&app)).await?
        } else {
            Vec::new()
        };
        let mut events = self.workspace_events.subscribe();
        // All posture streams, exact baselines and containment must be live
        // before this call records or performs any launch side effect.
        let mut witness =
            begin_launch_after_witness(posture_scope, MacLaunchPostureWitness::begin(deadline))?;
        posture_scope.pending_settlement = Some(pending_launch_evidence(
            &settlement_action_id,
            "macos_background_launch",
            started,
            vec![SettlementSignal::DispatchStarted],
            vec![SettlementSignal::DispatchComplete],
        ));

        let launch_ref = resolved.app_ref.clone();
        let config = nsworkspace::OpenConfig {
            arguments: resolved.arguments.clone(),
            apple_event_bundle_id: resolved.bundle_id.clone(),
            ..Default::default()
        };
        let completion_timeout = deadline.saturating_duration_since(Instant::now());
        let completion_containment = witness.completion_containment();
        let dispatch = tokio::task::spawn_blocking(move || {
            nsworkspace::open_application_pid_with_timeout(
                &launch_ref,
                &config,
                completion_timeout,
                move |pid| {
                    let _ =
                        completion_containment.narrow_to_target(pid, Duration::from_millis(250));
                },
            )
        })
        .await
        .map_err(join_error)
        .and_then(|result| result.map_err(|error| launch_error(error.to_string())));

        let attempt = match dispatch {
            Err(error) => Err(error),
            Ok(pid) => match witness.set_target(pid) {
                Err(error) => Err(error),
                Ok(()) => {
                    posture_scope.pending_settlement = Some(pending_launch_evidence(
                        &settlement_action_id,
                        "macos_background_launch",
                        started,
                        vec![
                            SettlementSignal::DispatchStarted,
                            SettlementSignal::DispatchComplete,
                        ],
                        Vec::new(),
                    ));
                    match nsworkspace::process_generation(pid) {
                        None => Err(launch_error(
                            "NSWorkspace returned a pid without an exact live process generation",
                        )
                        .with_detail("pid", pid)),
                        Some(generation) => match wait_for_registered_application(
                            pid,
                            generation,
                            &resolved,
                            deadline,
                            &mut events,
                            nsworkspace::running_application,
                        )
                        .await
                        {
                            Err(error) => Err(error),
                            Ok(application) => {
                                witness.drain_events();
                                let app = app_ref_for_running(&application);
                                posture_scope.record_partial_result(app.clone(), Vec::new());
                                wait_for_stable_windows(
                                    &self.windows,
                                    LaunchWait {
                                        app: &app,
                                        pid,
                                        expected_generation: generation,
                                        baseline_windows: &baseline_windows,
                                        dispatched: true,
                                        settlement_action_id: &settlement_action_id,
                                        profile: "macos_background_launch",
                                        started,
                                        deadline,
                                    },
                                    &mut events,
                                    &mut witness,
                                    posture_scope,
                                )
                                .await
                                .map(|stable| LaunchAttempt {
                                    reused_running_app: preexisting.as_ref().is_some_and(|prior| {
                                        same_process_identity(prior, &application)
                                    }),
                                    app,
                                    stable,
                                })
                            }
                        },
                    }
                }
            },
        };
        let (posture, native_evidence) = witness.finish();
        posture_scope.posture = posture.clone();
        posture_scope.native_evidence = native_evidence;
        if attempt.is_ok() {
            posture_scope.pending_settlement = None;
        }
        let posture_failure =
            posture_failure(&posture, posture_scope.partial_app.as_ref(), posture_scope);
        match (attempt, posture_failure) {
            (Ok(attempt), None) => Ok(NativeLaunch {
                app: attempt.app,
                windows: attempt.stable.windows,
                reused_running_app: attempt.reused_running_app,
                posture,
                settlement: launch_settlement(
                    "macos_background_launch",
                    started,
                    true,
                    attempt.stable.window_list_changed,
                    attempt.stable.windows_empty,
                ),
            }),
            (Ok(_), Some(posture_error)) => Err(posture_error),
            (Err(error), None) => Err(error),
            (Err(error), Some(posture_error)) => {
                Err(NativeError::primary(vec![error, posture_error])
                    .expect("launch failures are nonempty"))
            }
        }
    }
}

struct LaunchAttempt {
    app: AppRef,
    stable: StableLaunchWindows,
    reused_running_app: bool,
}

fn begin_launch_after_witness<T>(
    posture_scope: &mut LaunchPostureScope,
    witness: Result<T, NativeError>,
) -> Result<T, NativeError> {
    let witness = witness?;
    posture_scope.begin_launch();
    Ok(witness)
}

struct StableLaunchWindows {
    windows: Vec<WindowRef>,
    window_list_changed: bool,
    windows_empty: bool,
}

struct LaunchWait<'a> {
    app: &'a AppRef,
    pid: i32,
    expected_generation: u64,
    baseline_windows: &'a [WindowRef],
    dispatched: bool,
    settlement_action_id: &'a ActionId,
    profile: &'a str,
    started: Instant,
    deadline: Instant,
}

async fn wait_for_registered_application<F>(
    pid: i32,
    expected_generation: u64,
    resolved: &ResolvedLaunch,
    deadline: Instant,
    events: &mut tokio::sync::broadcast::Receiver<nsworkspace::WorkspaceEvent>,
    mut lookup: F,
) -> Result<RunningApplicationInfo, NativeError>
where
    F: FnMut(i32) -> Option<RunningApplicationInfo>,
{
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if let Some(application) = lookup(pid) {
            if let Some(false) = resolved.compares_to(&application) {
                return Err(launch_settle_error(
                    "NSWorkspace launch pid resolved to a different application",
                )
                .with_detail("pid", pid)
                .with_detail(
                    "actual_bundle_id",
                    application.bundle_id.clone().unwrap_or_default(),
                ));
            }
            match application.process_generation {
                Some(generation) if generation != expected_generation => {
                    return Err(NativeError::new(
                        ErrorCode::WindowIdentityChanged,
                        ErrorPhase::Settle,
                        true,
                        "launched process identity changed before registration completed",
                    )
                    .with_detail("pid", pid)
                    .with_detail("expected_process_generation", expected_generation)
                    .with_detail("current_process_generation", generation));
                }
                Some(_)
                    if application.finished_launching
                        && resolved.compares_to(&application) == Some(true) =>
                {
                    return Ok(application)
                }
                Some(_) | None => {}
            }
        }
        if Instant::now() >= deadline {
            return Err(launch_settle_error(
                "launched app did not register and finish launching before its deadline",
            )
            .with_detail("pid", pid));
        }
        tokio::select! {
            _ = poll.tick() => {}
            event = events.recv() => match event {
                Ok(event) => {
                    if let Some(error) = launch_event_failure(pid, &event) {
                        return Err(error);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(launch_settle_error(
                        "workspace lifecycle event stream closed before app registration",
                    )
                    .with_detail("pid", pid));
                }
            },
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err(launch_settle_error("app launch deadline elapsed before registration")
                    .with_detail("pid", pid));
            }
        }
    }
}

async fn wait_for_stable_windows(
    registry: &MacWindowRegistry,
    wait: LaunchWait<'_>,
    events: &mut tokio::sync::broadcast::Receiver<nsworkspace::WorkspaceEvent>,
    witness: &mut MacLaunchPostureWitness,
    posture_scope: &mut LaunchPostureScope,
) -> Result<StableLaunchWindows, NativeError> {
    let mut poll = tokio::time::interval(Duration::from_millis(25));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stability = LaunchWindowStability::new(Instant::now());
    loop {
        if Instant::now() >= wait.deadline {
            return Err(launch_settle_error(
                "app launch did not reach a stable window set before its deadline",
            )
            .with_detail("pid", wait.pid));
        }
        tokio::select! {
            _ = poll.tick() => {}
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if let Some(error) = launch_event_failure(wait.pid, &event) {
                            return Err(error);
                        }
                    }
                    // A lagged lifecycle receiver is not evidence that the
                    // process survived. The exact registry snapshot below is
                    // the conservative resynchronization boundary.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(launch_settle_error("workspace lifecycle event stream closed during launch"));
                    }
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wait.deadline)) => {
                return Err(launch_settle_error("app launch deadline elapsed")
                    .with_detail("pid", wait.pid));
            }
        }
        witness.drain_events();
        let application = nsworkspace::running_application(wait.pid).ok_or_else(|| {
            launch_settle_error("app disappeared before launch settlement completed")
                .with_detail("pid", wait.pid)
        })?;
        if application.process_generation != Some(wait.expected_generation) {
            return Err(launch_settle_error(
                "process identity changed before launch settlement completed",
            )
            .with_detail("pid", wait.pid)
            .with_detail("expected_process_generation", wait.expected_generation)
            .with_detail(
                "current_process_generation",
                application.process_generation.unwrap_or_default(),
            ));
        }
        let windows = registry.list_windows(Some(wait.app)).await?;
        posture_scope.record_partial_result(wait.app.clone(), windows.clone());
        let observed_signals = wait
            .dispatched
            .then_some(SettlementSignal::DispatchComplete)
            .into_iter()
            .collect();
        posture_scope.pending_settlement = Some(pending_launch_evidence(
            wait.settlement_action_id,
            wait.profile,
            wait.started,
            observed_signals,
            Vec::new(),
        ));
        if stability.observe(&windows, Instant::now(), application.finished_launching) {
            let window_list_changed = window_ids(wait.baseline_windows) != window_ids(&windows);
            return Ok(StableLaunchWindows {
                windows_empty: windows.is_empty(),
                windows,
                window_list_changed,
            });
        }
    }
}

fn same_process_identity(left: &RunningApplicationInfo, right: &RunningApplicationInfo) -> bool {
    left.pid == right.pid
        && left.process_generation.is_some()
        && left.process_generation == right.process_generation
}

fn window_ids(windows: &[WindowRef]) -> Vec<WindowId> {
    let mut ids: Vec<_> = windows.iter().map(|window| window.id.clone()).collect();
    ids.sort();
    ids
}

fn pending_launch_evidence(
    action_id: &ActionId,
    profile: &str,
    started: Instant,
    observed_signals: Vec<SettlementSignal>,
    missing_signals: Vec<SettlementSignal>,
) -> PendingSettlementEvidence {
    PendingSettlementEvidence {
        state: PendingSettlementState::Pending,
        trigger_action_id: action_id.clone(),
        profile: profile.to_owned(),
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        observed_signals,
        missing_signals,
    }
}

fn launch_settlement(
    profile: &str,
    started: Instant,
    dispatched: bool,
    window_list_changed: bool,
    windows_empty: bool,
) -> SettlementEvidence {
    let mut observed_signals = Vec::new();
    if dispatched {
        observed_signals.push(SettlementSignal::DispatchComplete);
    }
    if window_list_changed {
        observed_signals.push(SettlementSignal::WindowListChanged);
    }
    SettlementEvidence {
        state: SettledState::Settled,
        trigger_action_id: None,
        profile: profile.to_owned(),
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        observed_signals,
        terminal_signal: if windows_empty {
            "application_finished_without_windows".to_owned()
        } else {
            "stable_window_set".to_owned()
        },
        quiet_window_ms: WINDOW_QUIET_WINDOW.as_millis() as u64,
        resumed_from_prior_call: false,
    }
}

fn posture_failure(
    posture: &PostureResult,
    app: Option<&AppRef>,
    posture_scope: &LaunchPostureScope,
) -> Option<NativeError> {
    (!posture.held).then(|| {
        let observed_excursion = posture.frontmost_changed
            || posture.key_window_changed
            || posture.physical_cursor_moved
            || posture.restored_after_violation;
        let (code, retryable, message) = if observed_excursion {
            (
                ErrorCode::PostureViolated,
                false,
                "app launched but foreground, exact focused-window, or cursor posture changed",
            )
        } else {
            (
                ErrorCode::PostureUnverifiable,
                true,
                "app launch posture witness was incomplete, unreadable, or lagged",
            )
        };
        NativeError::new(code, ErrorPhase::Verify, retryable, message)
            .with_detail("app", serde_json::to_value(app).unwrap_or_default())
            .with_detail(
                "windows",
                serde_json::to_value(&posture_scope.partial_windows).unwrap_or_default(),
            )
            .with_detail("posture", serde_json::to_value(posture).unwrap_or_default())
    })
}

#[derive(Debug)]
struct LaunchWindowStability {
    prior_ids: Option<Vec<cua_driver_core::api::contracts::WindowId>>,
    stable_since: Instant,
}

impl LaunchWindowStability {
    fn new(now: Instant) -> Self {
        Self {
            prior_ids: None,
            stable_since: now,
        }
    }

    fn observe(&mut self, windows: &[WindowRef], now: Instant, finished: bool) -> bool {
        let ids: Vec<_> = windows.iter().map(|window| window.id.clone()).collect();
        if self.prior_ids.as_ref() != Some(&ids) {
            self.prior_ids = Some(ids);
            self.stable_since = now;
        }
        finished && now.saturating_duration_since(self.stable_since) >= WINDOW_QUIET_WINDOW
    }
}

fn launch_event_failure(pid: i32, event: &nsworkspace::WorkspaceEvent) -> Option<NativeError> {
    (event.pid == pid && event.kind == WorkspaceEventKind::Terminated).then(|| {
        launch_settle_error("app terminated before launch settlement completed")
            .with_detail("pid", pid)
    })
}

#[derive(Debug, Clone)]
struct ResolvedLaunch {
    app_ref: String,
    bundle_id: Option<String>,
    executable_path: Option<PathBuf>,
    name: Option<String>,
    arguments: Vec<String>,
}

impl ResolvedLaunch {
    /// Compare only identity fields that NSRunningApplication has published.
    /// `None` means registry publication is still incomplete, not a mismatch.
    fn compares_to(&self, application: &RunningApplicationInfo) -> Option<bool> {
        if let Some(bundle_id) = &self.bundle_id {
            return application
                .bundle_id
                .as_ref()
                .map(|candidate| candidate == bundle_id);
        }
        if let Some(executable) = &self.executable_path {
            return application
                .executable_path
                .as_ref()
                .map(|path| Path::new(path) == executable);
        }
        self.name.as_ref().and_then(|name| {
            application
                .name
                .as_ref()
                .map(|candidate| candidate.eq_ignore_ascii_case(name))
        })
    }

    fn matches(&self, application: &RunningApplicationInfo) -> bool {
        self.compares_to(application) == Some(true)
    }
}

fn reusable_existing_launch(
    resolved: &ResolvedLaunch,
    application: Option<&RunningApplicationInfo>,
) -> bool {
    resolved.arguments.is_empty()
        && application.is_some_and(|app| app.process_generation.is_some() && resolved.matches(app))
}

trait LaunchCatalog {
    fn bundle_exists(&self, bundle_id: &str) -> bool;
    fn locate_name(&self, name: &str) -> Option<(String, Option<String>)>;
    fn bundle_id_for_path(&self, path: &str) -> Option<String>;
}

struct SystemLaunchCatalog;

impl LaunchCatalog for SystemLaunchCatalog {
    fn bundle_exists(&self, bundle_id: &str) -> bool {
        apps::resolve_bundle_id_to_locator(bundle_id).is_some()
    }

    fn locate_name(&self, name: &str) -> Option<(String, Option<String>)> {
        apps::locate_by_name(name).map(|locator| locator.app_ref_and_bundle_id())
    }

    fn bundle_id_for_path(&self, path: &str) -> Option<String> {
        apps::bundle_id_for_app_path(path)
    }
}

fn resolve_launch(selector: AppSelector) -> Result<ResolvedLaunch, NativeError> {
    if let AppSelector::Name { name } = &selector {
        if name.trim().is_empty() {
            return Err(NativeError::invalid("app name cannot be empty"));
        }
        if let Some(resolved) = resolve_running_name(name, &nsworkspace::running_applications())? {
            return Ok(resolved);
        }
    }
    resolve_launch_with_catalog(selector, &SystemLaunchCatalog)
}

fn resolve_running_name(
    name: &str,
    applications: &[RunningApplicationInfo],
) -> Result<Option<ResolvedLaunch>, NativeError> {
    let matches: Vec<_> = applications
        .iter()
        .filter(|application| {
            application
                .name
                .as_ref()
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        })
        .collect();
    if matches.len() > 1 {
        return Err(NativeError::invalid(
            "app name matched multiple running macOS processes; use a bundle id or executable",
        )
        .with_detail("name", name.to_owned())
        .with_detail(
            "pids",
            matches
                .iter()
                .map(|application| application.pid)
                .collect::<Vec<_>>(),
        ));
    }
    let Some(application) = matches.first() else {
        return Ok(None);
    };
    let app_ref = application
        .bundle_id
        .clone()
        .or_else(|| application.executable_path.clone())
        .ok_or_else(|| {
            NativeError::new(
                ErrorCode::AppNotFound,
                ErrorPhase::Preflight,
                false,
                "running macOS app has neither a bundle id nor executable path",
            )
            .with_detail("name", name.to_owned())
            .with_detail("pid", application.pid)
        })?;
    Ok(Some(ResolvedLaunch {
        app_ref,
        bundle_id: application.bundle_id.clone(),
        executable_path: application.executable_path.as_deref().map(PathBuf::from),
        name: Some(name.to_owned()),
        arguments: Vec::new(),
    }))
}

fn resolve_launch_with_catalog(
    selector: AppSelector,
    catalog: &dyn LaunchCatalog,
) -> Result<ResolvedLaunch, NativeError> {
    match selector {
        AppSelector::BundleId { bundle_id } => {
            if bundle_id.trim().is_empty() {
                return Err(NativeError::invalid("bundle_id cannot be empty"));
            }
            if !catalog.bundle_exists(&bundle_id) {
                return Err(app_not_found("bundle_id", &bundle_id));
            }
            Ok(ResolvedLaunch {
                app_ref: bundle_id.clone(),
                bundle_id: Some(bundle_id),
                executable_path: None,
                name: None,
                arguments: Vec::new(),
            })
        }
        AppSelector::Name { name } => {
            if name.trim().is_empty() {
                return Err(NativeError::invalid("app name cannot be empty"));
            }
            let (app_ref, bundle_id) = catalog
                .locate_name(&name)
                .ok_or_else(|| app_not_found("name", &name))?;
            Ok(ResolvedLaunch {
                app_ref,
                bundle_id,
                executable_path: None,
                name: Some(name),
                arguments: Vec::new(),
            })
        }
        AppSelector::Executable { path, arguments } => {
            if path.trim().is_empty() {
                return Err(NativeError::invalid("executable path cannot be empty"));
            }
            let executable =
                std::fs::canonicalize(&path).map_err(|_| app_not_found("executable", &path))?;
            let bundle = enclosing_app_bundle(&executable).ok_or_else(|| {
                NativeError::invalid(
                    "macOS executable selectors must name a .app bundle or an executable inside one",
                )
                .with_detail("path", path.clone())
            })?;
            let bundle_string = bundle.to_string_lossy().to_string();
            Ok(ResolvedLaunch {
                app_ref: bundle_string.clone(),
                bundle_id: catalog.bundle_id_for_path(&bundle_string),
                executable_path: Some(executable),
                name: None,
                arguments,
            })
        }
    }
}

fn enclosing_app_bundle(path: &Path) -> Option<PathBuf> {
    if path.extension().and_then(|extension| extension.to_str()) == Some("app") && path.is_dir() {
        return Some(path.to_path_buf());
    }
    path.ancestors()
        .find(|ancestor| {
            ancestor
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("app")
        })
        .map(Path::to_path_buf)
}

fn list_apps_native(query: AppQuery) -> Result<Vec<AppRef>, NativeError> {
    let running = nsworkspace::running_applications();
    let running_by_bundle: HashMap<_, _> = running
        .iter()
        .filter_map(|application| {
            application
                .bundle_id
                .as_ref()
                .map(|bundle| (bundle.clone(), application))
        })
        .collect();
    let mut apps: Vec<AppRef> = running.iter().map(app_ref_for_running).collect();
    for installed in apps::list_installed_apps() {
        if installed
            .bundle_id
            .as_ref()
            .is_some_and(|bundle| running_by_bundle.contains_key(bundle))
        {
            continue;
        }
        let identity = app_id(
            installed.bundle_id.as_deref(),
            installed.launch_path.as_deref(),
            None,
        );
        apps.push(AppRef {
            id: identity,
            name: Some(installed.name),
            pid: None,
            running: false,
        });
    }
    let needle = query.name_contains.map(|value| value.to_lowercase());
    apps.retain(|app| {
        query.running.is_none_or(|running| app.running == running)
            && needle.as_ref().is_none_or(|needle| {
                app.name
                    .as_ref()
                    .is_some_and(|name| name.to_lowercase().contains(needle))
            })
    });
    apps.sort_by(|left, right| left.name.cmp(&right.name).then(left.pid.cmp(&right.pid)));
    Ok(apps)
}

fn app_ref_for_running(application: &RunningApplicationInfo) -> AppRef {
    AppRef {
        id: app_id(
            application.bundle_id.as_deref(),
            application.executable_path.as_deref(),
            application
                .process_generation
                .map(|generation| (application.pid, generation)),
        ),
        name: application.name.clone(),
        pid: u32::try_from(application.pid).ok(),
        running: true,
    }
}

fn app_id(
    bundle_id: Option<&str>,
    executable_path: Option<&str>,
    process: Option<(i32, u64)>,
) -> AppId {
    let value = process
        .map(|(pid, generation)| format!("macos:process:{pid}:{generation:016x}"))
        .or_else(|| bundle_id.map(|bundle| format!("macos:bundle:{bundle}")))
        .or_else(|| executable_path.map(|path| format!("macos:executable:{path}")))
        .unwrap_or_else(|| "macos:unknown".to_owned());
    AppId::parse(value).expect("constructed macOS app id is nonempty")
}

fn readiness_snapshot() -> Readiness {
    let accessibility = permissions::status::accessibility_granted();
    let screen_preflight = permissions::status::screen_recording_granted();
    let screen_live = permissions::status::screen_recording_capturable();
    let targeted_events = skylight::is_available();
    let slps_make_key = slps_make_key::available();
    let spaces = windows::space_query_available();
    let mut permissions = BTreeMap::new();
    permissions.insert(
        "accessibility".to_owned(),
        if accessibility {
            PermissionState::Granted
        } else {
            PermissionState::Denied
        },
    );
    permissions.insert(
        "screen_recording".to_owned(),
        if screen_live {
            PermissionState::Granted
        } else {
            PermissionState::Denied
        },
    );
    let mut diagnostics = BTreeMap::new();
    diagnostics.insert(
        "screen_recording_preflight".to_owned(),
        screen_preflight.into(),
    );
    diagnostics.insert(
        "screen_recording_live_capture".to_owned(),
        screen_live.into(),
    );
    diagnostics.insert("targeted_event_symbols".to_owned(), targeted_events.into());
    diagnostics.insert("slps_make_key_symbols".to_owned(), slps_make_key.into());
    diagnostics.insert("space_query_symbols".to_owned(), spaces.into());
    diagnostics.insert("os_version".to_owned(), macos_version().into());
    Readiness {
        ready: accessibility && screen_live && targeted_events && slps_make_key && spaces,
        permissions,
        diagnostics,
    }
}

fn macos_version() -> String {
    std::process::Command::new("sw_vers")
        .args(["-productVersion"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn app_not_found(selector: &str, value: &str) -> NativeError {
    NativeError::new(
        ErrorCode::AppNotFound,
        ErrorPhase::Preflight,
        false,
        format!("no installed macOS app matched {selector} '{value}'"),
    )
    .with_detail(selector, value.to_owned())
}

fn launch_error(message: impl Into<String>) -> NativeError {
    NativeError::new(
        ErrorCode::AppLaunchFailed,
        ErrorPhase::Dispatch,
        true,
        message,
    )
}

fn launch_settle_error(message: impl Into<String>) -> NativeError {
    NativeError::new(
        ErrorCode::AppLaunchFailed,
        ErrorPhase::Settle,
        true,
        message,
    )
}

fn join_error(error: tokio::task::JoinError) -> NativeError {
    NativeError::new(
        ErrorCode::Internal,
        ErrorPhase::Preflight,
        true,
        format!("macOS lifecycle task failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use cua_driver_core::api::contracts::{AppId, WindowId};

    use super::*;

    struct FakeCatalog;

    impl LaunchCatalog for FakeCatalog {
        fn bundle_exists(&self, bundle_id: &str) -> bool {
            bundle_id == "com.example.fixture"
        }

        fn locate_name(&self, name: &str) -> Option<(String, Option<String>)> {
            (name == "Fixture").then(|| {
                (
                    "/Applications/Fixture.app".to_owned(),
                    Some("com.example.fixture".to_owned()),
                )
            })
        }

        fn bundle_id_for_path(&self, _path: &str) -> Option<String> {
            Some("com.example.executable".to_owned())
        }
    }

    fn running_fixture() -> RunningApplicationInfo {
        RunningApplicationInfo {
            pid: 120,
            name: Some("Fixture".to_owned()),
            bundle_id: Some("com.example.fixture".to_owned()),
            executable_path: Some("/Applications/Fixture.app/Contents/MacOS/fixture".to_owned()),
            process_generation: Some(7),
            active: false,
            hidden: false,
            finished_launching: true,
            regular: true,
        }
    }

    fn window(id: &str) -> WindowRef {
        WindowRef {
            id: WindowId::parse(id).unwrap(),
            app: AppRef {
                id: AppId::parse("macos:bundle:com.example.fixture").unwrap(),
                name: Some("Fixture".to_owned()),
                pid: Some(120),
                running: true,
            },
            title: None,
        }
    }

    #[test]
    fn all_launch_selector_shapes_preserve_native_dispatch_inputs() {
        let bundle = resolve_launch_with_catalog(
            AppSelector::BundleId {
                bundle_id: "com.example.fixture".to_owned(),
            },
            &FakeCatalog,
        )
        .unwrap();
        assert_eq!(bundle.app_ref, "com.example.fixture");
        assert_eq!(bundle.bundle_id.as_deref(), Some("com.example.fixture"));

        let named = resolve_launch_with_catalog(
            AppSelector::Name {
                name: "Fixture".to_owned(),
            },
            &FakeCatalog,
        )
        .unwrap();
        assert_eq!(named.app_ref, "/Applications/Fixture.app");
        assert_eq!(named.name.as_deref(), Some("Fixture"));

        let root = std::env::temp_dir().join(format!(
            "cua-driver-plan002-selector-{}",
            uuid::Uuid::new_v4()
        ));
        let bundle_path = root.join("Executable.app");
        fs::create_dir_all(&bundle_path).unwrap();
        let executable = resolve_launch_with_catalog(
            AppSelector::Executable {
                path: bundle_path.to_string_lossy().into_owned(),
                arguments: vec!["--fixture".to_owned()],
            },
            &FakeCatalog,
        )
        .unwrap();
        assert_eq!(executable.arguments, vec!["--fixture"]);
        assert_eq!(
            executable.bundle_id.as_deref(),
            Some("com.example.executable")
        );
        let canonical_bundle = fs::canonicalize(&bundle_path).unwrap();
        assert_eq!(
            executable.executable_path.as_deref(),
            Some(canonical_bundle.as_path())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_and_unresolvable_selectors_fail_before_launch() {
        for selector in [
            AppSelector::BundleId {
                bundle_id: " ".to_owned(),
            },
            AppSelector::Name {
                name: "".to_owned(),
            },
            AppSelector::Executable {
                path: " ".to_owned(),
                arguments: Vec::new(),
            },
        ] {
            let error = resolve_launch_with_catalog(selector, &FakeCatalog).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest);
            assert_eq!(error.phase, ErrorPhase::Validate);
        }

        let error = resolve_launch_with_catalog(
            AppSelector::BundleId {
                bundle_id: "com.example.missing".to_owned(),
            },
            &FakeCatalog,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::AppNotFound);
    }

    #[test]
    fn reuse_is_exact_and_executable_arguments_force_a_launch_dispatch() {
        let application = running_fixture();
        let mut resolved = ResolvedLaunch {
            app_ref: "com.example.fixture".to_owned(),
            bundle_id: Some("com.example.fixture".to_owned()),
            executable_path: None,
            name: None,
            arguments: Vec::new(),
        };
        assert!(reusable_existing_launch(&resolved, Some(&application)));

        resolved.arguments.push("--new-document".to_owned());
        assert!(!reusable_existing_launch(&resolved, Some(&application)));
        resolved.arguments.clear();
        resolved.bundle_id = Some("com.example.other".to_owned());
        assert!(!reusable_existing_launch(&resolved, Some(&application)));

        let mut generation_unknown = running_fixture();
        generation_unknown.process_generation = None;
        resolved.bundle_id = Some("com.example.fixture".to_owned());
        assert!(!reusable_existing_launch(
            &resolved,
            Some(&generation_unknown)
        ));
    }

    #[test]
    fn reused_running_app_requires_the_returned_exact_process_identity() {
        let prior = running_fixture();
        let mut same = prior.clone();
        assert!(same_process_identity(&prior, &same));

        same.process_generation = Some(8);
        assert!(!same_process_identity(&prior, &same));
        same.process_generation = Some(7);
        same.pid = 121;
        assert!(!same_process_identity(&prior, &same));
    }

    #[test]
    fn running_name_resolution_handles_core_services_and_rejects_ambiguity() {
        let mut finder = running_fixture();
        finder.name = Some("Finder".to_owned());
        finder.bundle_id = Some("com.apple.finder".to_owned());
        finder.executable_path =
            Some("/System/Library/CoreServices/Finder.app/Contents/MacOS/Finder".to_owned());
        let resolved = resolve_running_name("finder", &[finder.clone()])
            .unwrap()
            .unwrap();
        assert_eq!(resolved.app_ref, "com.apple.finder");
        assert_eq!(resolved.bundle_id.as_deref(), Some("com.apple.finder"));

        let mut duplicate = finder.clone();
        duplicate.pid += 1;
        duplicate.process_generation = Some(8);
        let error = resolve_running_name("Finder", &[finder, duplicate]).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.phase, ErrorPhase::Validate);
    }

    #[tokio::test]
    async fn cold_launch_waits_for_registry_publication_and_finished_state() {
        let resolved = ResolvedLaunch {
            app_ref: "com.example.fixture".to_owned(),
            bundle_id: Some("com.example.fixture".to_owned()),
            executable_path: None,
            name: None,
            arguments: Vec::new(),
        };
        let (_sender, mut events) = tokio::sync::broadcast::channel(4);
        let mut lookups = 0;
        let application = wait_for_registered_application(
            120,
            7,
            &resolved,
            Instant::now() + Duration::from_millis(250),
            &mut events,
            move |_| {
                lookups += 1;
                match lookups {
                    1 => None,
                    2 => {
                        let mut application = running_fixture();
                        application.bundle_id = None;
                        application.finished_launching = false;
                        Some(application)
                    }
                    _ => Some(running_fixture()),
                }
            },
        )
        .await
        .unwrap();
        assert!(application.finished_launching);
        assert_eq!(application.process_generation, Some(7));
    }

    #[test]
    fn settlement_reports_only_signals_that_were_observed() {
        let started = Instant::now();
        let empty_dispatch =
            launch_settlement("macos_background_launch", started, true, false, true);
        assert_eq!(
            empty_dispatch.observed_signals,
            vec![SettlementSignal::DispatchComplete]
        );

        let reused = launch_settlement("macos_launch_reuse", started, false, false, false);
        assert!(reused.observed_signals.is_empty());
        assert_eq!(
            window_ids(&[window("one"), window("two")]),
            window_ids(&[window("two"), window("one")])
        );
    }

    #[test]
    fn settlement_quiet_window_restarts_for_each_window_set_change() {
        let start = Instant::now();
        let mut tracker = LaunchWindowStability::new(start);
        assert!(!tracker.observe(&[], start, true));
        assert!(!tracker.observe(&[window("one")], start + Duration::from_millis(80), true,));
        assert!(!tracker.observe(
            &[window("one"), window("two")],
            start + Duration::from_millis(160),
            true,
        ));
        assert!(!tracker.observe(
            &[window("one"), window("two")],
            start + Duration::from_millis(259),
            true,
        ));
        assert!(tracker.observe(
            &[window("one"), window("two")],
            start + Duration::from_millis(260),
            true,
        ));
    }

    #[test]
    fn finished_app_with_no_windows_is_a_stable_success_shape() {
        let start = Instant::now();
        let mut tracker = LaunchWindowStability::new(start);
        assert!(!tracker.observe(&[], start, true));
        assert!(tracker.observe(&[], start + WINDOW_QUIET_WINDOW, true));
    }

    #[test]
    fn only_target_termination_aborts_launch_settlement() {
        let unrelated = nsworkspace::WorkspaceEvent {
            kind: WorkspaceEventKind::Terminated,
            pid: 121,
            bundle_id: Some("com.example.other".to_owned()),
            process_generation: Some(8),
        };
        assert!(launch_event_failure(120, &unrelated).is_none());

        let target = nsworkspace::WorkspaceEvent {
            pid: 120,
            ..unrelated
        };
        let error = launch_event_failure(120, &target).unwrap();
        assert_eq!(error.code, ErrorCode::AppLaunchFailed);
        assert_eq!(error.phase, ErrorPhase::Settle);
    }

    #[test]
    fn launch_posture_distinguishes_unverifiable_witness_from_observed_excursion() {
        let scope = LaunchPostureScope::default();
        let unverifiable = posture_failure(
            &PostureResult {
                held: false,
                ..PostureResult::default()
            },
            None,
            &scope,
        )
        .unwrap();
        assert_eq!(unverifiable.code, ErrorCode::PostureUnverifiable);

        let violated = posture_failure(
            &PostureResult {
                held: false,
                frontmost_changed: true,
                restored_after_violation: true,
                ..PostureResult::default()
            },
            None,
            &scope,
        )
        .unwrap();
        assert_eq!(violated.code, ErrorCode::PostureViolated);
    }

    #[test]
    fn failed_witness_acquisition_refuses_before_launch_side_effect_is_marked() {
        let mut scope = LaunchPostureScope::for_action(ActionId::new());
        let error = begin_launch_after_witness::<()>(
            &mut scope,
            Err(NativeError::new(
                ErrorCode::PostureUnverifiable,
                ErrorPhase::Preflight,
                true,
                "injected witness acquisition failure",
            )),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::PostureUnverifiable);
        assert!(!scope.side_effect_started());
    }
}
