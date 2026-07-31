//! Complete per-mutation macOS interaction scope acquisition and teardown.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use async_trait::async_trait;
use cua_driver_core::api::{
    contracts::{ActionId, Route},
    errors::{ErrorCode, NativeError},
    interaction::{
        InteractionScope, LeaseDecision, LeaseTeardownStatus, MutationDeadline, NativeEvidence,
        ScopeCleanup, ScopeLeaseAcquisition, ScopeLeaseTeardown, ScopePlan, ScopeRequirements,
        ScopeTeardownOutcome, TargetCursorHandle,
    },
    menu::MenuMutationIntent,
    observation::ResolvedWindow,
    platform::{InteractionProvider, ResolvedAction},
};
use serde_json::Value;

use crate::{ax::enablement::AxEnablementLease, input::keyboard::normalize_chord};

use super::{
    focus::{
        prepare_target_focus, select_scope_recipe, AccessibilityRecipe, HostRecipeContext,
        MacScopeRecipe, MenuSuppressionRecipe, TargetBeliefRecipe,
    },
    menu::{self, MenuSuppressionPlan},
    settlement::target_is_focused_window,
    target::{MacFocusState, MacTargetFocusCoordinator, MacTargetState},
    windows::{MacWindowFacts, MacWindowRegistry},
};

#[derive(Debug)]
pub struct MacNativeScopePlan {
    facts: MacWindowFacts,
    host: HostRecipeContext,
    recipe: MacScopeRecipe,
    menu: MenuSuppressionPlan,
}

trait LeaseResource: Send {
    fn release(&mut self, deadline: Instant) -> LeaseRelease;
}

#[derive(Default)]
struct LeaseRelease {
    evidence: NativeEvidence,
    failure: Option<NativeError>,
}

trait MacInteractionHooks: Send + Sync {
    fn acquire_accessibility(
        &self,
        recipe: AccessibilityRecipe,
        pid: i32,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError>;

    fn acquire_menu(
        &self,
        recipe: MenuSuppressionRecipe,
        plan: &MenuSuppressionPlan,
        focus_state: Arc<Mutex<MacFocusState>>,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError>;

    fn prepare_target_belief(
        &self,
        recipe: TargetBeliefRecipe,
        facts: &MacWindowFacts,
        focus_state: Arc<Mutex<MacFocusState>>,
        deadline: Instant,
    ) -> Result<Option<NativeEvidence>, NativeError>;
}

#[derive(Default)]
struct SystemInteractionHooks;

impl MacInteractionHooks for SystemInteractionHooks {
    fn acquire_accessibility(
        &self,
        recipe: AccessibilityRecipe,
        pid: i32,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
        match recipe {
            AccessibilityRecipe::NotApplicable => Ok(None),
            AccessibilityRecipe::ChromiumPriorStatePreserving => {
                let lease = AxEnablementLease::acquire(pid)?;
                Ok(Some(Box::new(AxLeaseResource(lease))))
            }
            AccessibilityRecipe::PriorStatePreservingIfSupported => {
                Ok(AxEnablementLease::acquire_optional(pid)?
                    .map(|lease| Box::new(AxLeaseResource(lease)) as Box<dyn LeaseResource>))
            }
        }
    }

    fn acquire_menu(
        &self,
        _recipe: MenuSuppressionRecipe,
        plan: &MenuSuppressionPlan,
        focus_state: Arc<Mutex<MacFocusState>>,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
        menu::acquire_production(plan, focus_state).map(|resource| {
            resource
                .map(|resource| Box::new(MenuResourceAdapter(resource)) as Box<dyn LeaseResource>)
        })
    }

    fn prepare_target_belief(
        &self,
        recipe: TargetBeliefRecipe,
        facts: &MacWindowFacts,
        focus_state: Arc<Mutex<MacFocusState>>,
        deadline: Instant,
    ) -> Result<Option<NativeEvidence>, NativeError> {
        prepare_target_focus(recipe, facts, focus_state, deadline)
    }
}

struct AxLeaseResource(AxEnablementLease);

impl LeaseResource for AxLeaseResource {
    fn release(&mut self, _deadline: Instant) -> LeaseRelease {
        let mut evidence = NativeEvidence::default();
        evidence
            .fields
            .insert("ax_attribute".to_owned(), self.0.attribute().into());
        evidence
            .fields
            .insert("ax_prior_value".to_owned(), self.0.prior().into());
        evidence
            .fields
            .insert("ax_state_changed".to_owned(), self.0.changed().into());
        let failure = self.0.release().err();
        evidence
            .fields
            .insert("ax_release_succeeded".to_owned(), failure.is_none().into());
        LeaseRelease { evidence, failure }
    }
}

struct MenuResourceAdapter(Box<dyn menu::MenuSuppressionResource>);

impl LeaseResource for MenuResourceAdapter {
    fn release(&mut self, _deadline: Instant) -> LeaseRelease {
        match self.0.release() {
            Ok(evidence) => LeaseRelease {
                evidence,
                failure: None,
            },
            Err(error) => LeaseRelease {
                evidence: NativeEvidence::default(),
                failure: Some(error),
            },
        }
    }
}

#[derive(Clone)]
pub struct MacInteractionProvider {
    windows: MacWindowRegistry,
    host: HostRecipeContext,
    hooks: Arc<dyn MacInteractionHooks>,
}

impl MacInteractionProvider {
    pub fn new(windows: MacWindowRegistry) -> Self {
        Self {
            windows,
            host: HostRecipeContext::current(),
            hooks: Arc::new(SystemInteractionHooks),
        }
    }
}

#[async_trait]
impl InteractionProvider<MacTargetState, MacTargetFocusCoordinator> for MacInteractionProvider {
    type NativeScopePlan = MacNativeScopePlan;

    async fn preflight(
        &self,
        target: &mut MacTargetState,
        focus: &mut MacTargetFocusCoordinator,
        action_id: &ActionId,
        window: &ResolvedWindow,
        route: Route,
        action: &ResolvedAction,
        deadline: MutationDeadline,
        requirements: ScopeRequirements,
    ) -> Result<ScopePlan<Self::NativeScopePlan>, NativeError> {
        ensure_target_live(target, focus, window)?;
        let facts = self.windows.facts_for_stamp(&window.stamp()).await?;
        ensure_native_facts_match(&facts, target, window)?;
        ensure_actionable_window_state(&facts)?;
        if let ResolvedAction::PressKey { stroke, .. } = action {
            // Platform key vocabulary is validated before any accessibility
            // lease or target-focus preparation is acquired.
            normalize_chord(stroke)?;
        }
        let recipe =
            select_scope_recipe(&self.host, &window.framework, route, action, &requirements)?;
        let menu = MenuSuppressionPlan::NotApplicable;
        Ok(ScopePlan::new(
            action_id.clone(),
            window.clone(),
            route,
            deadline,
            requirements,
            MacNativeScopePlan {
                facts,
                host: self.host.clone(),
                recipe,
                menu,
            },
        ))
    }

    async fn acquire_scope(
        &self,
        target: &mut MacTargetState,
        focus: &mut MacTargetFocusCoordinator,
        mut plan: ScopePlan<Self::NativeScopePlan>,
        logical_cursor: TargetCursorHandle,
    ) -> Result<InteractionScope, NativeError> {
        ensure_target_live(target, focus, &plan.window)?;
        let live_facts = self.windows.facts_for_stamp(&plan.window.stamp()).await?;
        if live_facts != plan.native.facts {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "native window facts changed between interaction preflight and lease acquisition",
            ));
        }
        ensure_native_facts_match(&live_facts, target, &plan.window)?;
        plan.native.menu = match &plan.menu_intent {
            Some(MenuMutationIntent::Dismissing { .. }) => {
                // An explicit outside click or Escape is the dismissal event;
                // arming the menu-dismissal tap here would suppress the exact
                // effect the caller requested.
                MenuSuppressionPlan::NotApplicable
            }
            Some(intent) => {
                let menu_pid = match intent {
                    MenuMutationIntent::Opening { .. } => None,
                    MenuMutationIntent::Targeting { identity, .. } => {
                        if identity.process != live_facts.stamp.process {
                            return Err(NativeError::unsupported(
                                "cross-process native menu suppression requires a second exact per-menu PID tap",
                            )
                            .with_detail("recipe_status", "cross_process_menu_tap_unavailable"));
                        }
                        Some(live_facts.pid)
                    }
                    MenuMutationIntent::Dismissing { .. } => unreachable!(),
                };
                MenuSuppressionPlan::ExactSourcePidPredicate {
                    target_pid: live_facts.pid,
                    menu_pid,
                    menu_id: intent.menu_id().clone(),
                    action_id: plan.action_id.clone(),
                }
            }
            None => MenuSuppressionPlan::NotApplicable,
        };
        if plan.requirements.menu_dismissal != plan.menu_intent.is_some() {
            return Err(NativeError::new(
                ErrorCode::Internal,
                cua_driver_core::api::errors::ErrorPhase::Preflight,
                false,
                "menu suppression requirement and bound menu intent disagree",
            ));
        }

        let focus_state = focus.state_handle();
        target.bind_menu_focus_state(Arc::clone(&focus_state));
        let acquired = acquire_resources(
            self.hooks.as_ref(),
            &plan,
            target.poison_handle(),
            focus_state,
        )?;
        Ok(plan.into_scope(
            acquired.acquisition,
            logical_cursor,
            acquired.evidence,
            Box::new(acquired.cleanup),
        ))
    }
}

fn ensure_actionable_window_state(facts: &MacWindowFacts) -> Result<(), NativeError> {
    use cua_driver_core::api::capabilities::WindowStateKind;

    match &facts.state {
        WindowStateKind::Visible | WindowStateKind::Occluded => Ok(()),
        WindowStateKind::Minimized => Err(NativeError::unsupported(
            "macOS background actions refuse an exact minimized target window",
        )
        .with_detail("window_state", "minimized")),
        WindowStateKind::OffSpace => Err(NativeError::unsupported(
            "macOS background actions refuse an exact off-Space target window",
        )
        .with_detail("window_state", "off_space")),
        WindowStateKind::Unknown => Err(NativeError::unsupported(
            "macOS background actions require an exact current window-state classification",
        )
        .with_detail("window_state", "unknown")),
    }
}

fn ensure_target_live(
    target: &MacTargetState,
    focus: &MacTargetFocusCoordinator,
    window: &ResolvedWindow,
) -> Result<(), NativeError> {
    if target.invalidated() || focus.is_shutdown() || target.window != window.stamp() {
        return Err(NativeError::stale(
            ErrorCode::WindowIdentityChanged,
            "interaction target no longer matches the live target controller",
        ));
    }
    Ok(())
}

fn ensure_native_facts_match(
    facts: &MacWindowFacts,
    target: &MacTargetState,
    window: &ResolvedWindow,
) -> Result<(), NativeError> {
    if facts.stamp != target.window || facts.stamp != window.stamp() {
        return Err(NativeError::stale(
            ErrorCode::WindowIdentityChanged,
            "revalidated native facts do not match the exact target owner",
        ));
    }
    Ok(())
}

struct AcquiredResources {
    acquisition: ScopeLeaseAcquisition,
    evidence: NativeEvidence,
    cleanup: MacScopeCleanup,
}

fn acquire_resources(
    hooks: &dyn MacInteractionHooks,
    plan: &ScopePlan<MacNativeScopePlan>,
    poison: Arc<AtomicBool>,
    focus_state: Arc<Mutex<MacFocusState>>,
) -> Result<AcquiredResources, NativeError> {
    let frontmost_pid_before = crate::apps::frontmost_pid();
    let key_window_before =
        target_is_focused_window(plan.native.facts.pid, plan.native.facts.cg_window_id);
    let mut cleanup = MacScopeCleanup::new(
        poison,
        plan.deadline.teardown,
        plan.native.facts.pid,
        plan.native.facts.cg_window_id,
        frontmost_pid_before,
        key_window_before,
    );
    let mut target_belief_evidence = None;

    let result = (|| {
        cleanup.accessibility =
            hooks.acquire_accessibility(plan.native.recipe.accessibility, plan.native.facts.pid)?;
        cleanup.menu = hooks.acquire_menu(
            plan.native.recipe.menu,
            &plan.native.menu,
            Arc::clone(&focus_state),
        )?;
        target_belief_evidence = hooks.prepare_target_belief(
            plan.native.recipe.target_belief,
            &plan.native.facts,
            Arc::clone(&focus_state),
            plan.deadline.work,
        )?;
        Ok::<(), NativeError>(())
    })();

    if let Err(primary) = result {
        let outcome = cleanup.cleanup(plan.deadline.teardown);
        let mut failures = Vec::with_capacity(1 + outcome.failures.len());
        failures.push(primary);
        failures.extend(outcome.failures);
        return Err(NativeError::primary(failures).expect("acquisition failure is nonempty"));
    }

    let acquisition = ScopeLeaseAcquisition {
        accessibility: decision(&cleanup.accessibility),
        menu_dismissal: decision(&cleanup.menu),
        target_belief: if target_belief_evidence.is_some() {
            LeaseDecision::Acquired
        } else {
            LeaseDecision::NotApplicable
        },
    };
    let mut evidence = NativeEvidence::default();
    evidence.fields.insert(
        "interaction_recipe".to_owned(),
        plan.native.recipe.evidence_name().into(),
    );
    evidence.fields.insert(
        "os_version".to_owned(),
        plan.native.host.os_version.clone().into(),
    );
    evidence.fields.insert(
        "architecture".to_owned(),
        plan.native.host.architecture.clone().into(),
    );
    evidence
        .fields
        .insert("pid".to_owned(), plan.native.facts.pid.into());
    evidence.fields.insert(
        "cg_window_id".to_owned(),
        plan.native.facts.cg_window_id.into(),
    );
    evidence.fields.insert(
        "frontmost_pid_before".to_owned(),
        frontmost_pid_before.map_or(Value::Null, Value::from),
    );
    evidence.fields.insert(
        "key_window_before".to_owned(),
        key_window_before.map_or(Value::Null, Value::from),
    );
    evidence
        .fields
        .insert("activation_requested".to_owned(), false.into());
    evidence
        .fields
        .insert("hardware_cursor_warp_attempted".to_owned(), false.into());
    evidence
        .fields
        .insert("user_intervention_signal".to_owned(), Value::Null);
    if let Some(target_belief_evidence) = target_belief_evidence {
        evidence.merge(target_belief_evidence);
    }
    Ok(AcquiredResources {
        acquisition,
        evidence,
        cleanup,
    })
}

fn decision(resource: &Option<Box<dyn LeaseResource>>) -> LeaseDecision {
    if resource.is_some() {
        LeaseDecision::Acquired
    } else {
        LeaseDecision::NotApplicable
    }
}

struct MacScopeCleanup {
    poison: Arc<AtomicBool>,
    teardown_deadline: Instant,
    target_pid: i32,
    target_window_id: u32,
    frontmost_pid_before: Option<i32>,
    key_window_before: Option<bool>,
    accessibility: Option<Box<dyn LeaseResource>>,
    menu: Option<Box<dyn LeaseResource>>,
    outcome: Option<ScopeTeardownOutcome>,
}

impl MacScopeCleanup {
    fn new(
        poison: Arc<AtomicBool>,
        teardown_deadline: Instant,
        target_pid: i32,
        target_window_id: u32,
        frontmost_pid_before: Option<i32>,
        key_window_before: Option<bool>,
    ) -> Self {
        Self {
            poison,
            teardown_deadline,
            target_pid,
            target_window_id,
            frontmost_pid_before,
            key_window_before,
            accessibility: None,
            menu: None,
            outcome: None,
        }
    }
}

impl ScopeCleanup for MacScopeCleanup {
    fn cleanup(&mut self, deadline: Instant) -> ScopeTeardownOutcome {
        if let Some(outcome) = &self.outcome {
            return outcome.clone();
        }

        let mut evidence = NativeEvidence::default();
        let mut failures = Vec::new();
        let menu_dismissal = release_lease(&mut self.menu, deadline, &mut evidence, &mut failures);
        let accessibility = release_lease(
            &mut self.accessibility,
            deadline,
            &mut evidence,
            &mut failures,
        );
        let frontmost_pid_after = crate::apps::frontmost_pid();
        let key_window_after = target_is_focused_window(self.target_pid, self.target_window_id);
        evidence.fields.insert(
            "frontmost_pid_before".to_owned(),
            self.frontmost_pid_before.map_or(Value::Null, Value::from),
        );
        evidence.fields.insert(
            "frontmost_pid_after".to_owned(),
            frontmost_pid_after.map_or(Value::Null, Value::from),
        );
        evidence.fields.insert(
            "target_became_frontmost".to_owned(),
            (self.frontmost_pid_before != Some(self.target_pid)
                && frontmost_pid_after == Some(self.target_pid))
            .into(),
        );
        evidence.fields.insert(
            "key_window_before".to_owned(),
            self.key_window_before.map_or(Value::Null, Value::from),
        );
        evidence.fields.insert(
            "key_window_after".to_owned(),
            key_window_after.map_or(Value::Null, Value::from),
        );

        if !failures.is_empty() {
            self.poison.store(true, Ordering::Release);
            evidence
                .fields
                .insert("target_poisoned".to_owned(), true.into());
        }
        let outcome = ScopeTeardownOutcome {
            native_evidence: evidence,
            leases: ScopeLeaseTeardown {
                accessibility,
                menu_dismissal,
                target_belief: LeaseTeardownStatus::NotApplicable,
            },
            failures,
        };
        self.outcome = Some(outcome.clone());
        outcome
    }
}

impl Drop for MacScopeCleanup {
    fn drop(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        let outcome = self.cleanup(self.teardown_deadline);
        for error in outcome.failures {
            tracing::error!(error = %error, "macOS interaction cleanup failed during drop");
        }
    }
}

fn release_lease(
    resource: &mut Option<Box<dyn LeaseResource>>,
    deadline: Instant,
    evidence: &mut NativeEvidence,
    failures: &mut Vec<NativeError>,
) -> LeaseTeardownStatus {
    let Some(mut resource) = resource.take() else {
        return LeaseTeardownStatus::NotApplicable;
    };
    let release = resource.release(deadline);
    evidence.merge(release.evidence);
    if let Some(error) = release.failure {
        failures.push(error);
        LeaseTeardownStatus::Failed
    } else {
        LeaseTeardownStatus::Released
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Duration};

    use cua_driver_core::api::{
        capabilities::{Framework, WindowStateKind},
        contracts::{AppId, AppRef, GeometryRevision, Rect, WindowGeneration, WindowId, WindowRef},
        errors::ErrorPhase,
        observation::{NativeProcessHandle, NativeWindowHandle, WindowGeometry},
    };

    use super::*;

    struct LoggedLease {
        log: Arc<Mutex<Vec<&'static str>>>,
        release: &'static str,
        fail: bool,
    }

    impl LeaseResource for LoggedLease {
        fn release(&mut self, _deadline: Instant) -> LeaseRelease {
            self.log.lock().unwrap().push(self.release);
            let mut evidence = NativeEvidence::default();
            evidence
                .fields
                .insert(format!("{}_attempted", self.release), true.into());
            let failure = self.fail.then(|| {
                NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Verify,
                    false,
                    "injected release failure",
                )
            });
            LeaseRelease { evidence, failure }
        }
    }

    fn logged(
        log: &Arc<Mutex<Vec<&'static str>>>,
        release: &'static str,
    ) -> Box<dyn LeaseResource> {
        Box::new(LoggedLease {
            log: Arc::clone(log),
            release,
            fail: false,
        })
    }

    struct LoggingHooks {
        log: Arc<Mutex<Vec<&'static str>>>,
        fail_at: Option<&'static str>,
    }

    impl LoggingHooks {
        fn acquire(
            &self,
            acquire: &'static str,
            release: &'static str,
        ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
            self.log.lock().unwrap().push(acquire);
            if self.fail_at == Some(acquire) {
                Err(NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Preflight,
                    false,
                    "injected acquisition failure",
                ))
            } else {
                Ok(Some(logged(&self.log, release)))
            }
        }
    }

    impl MacInteractionHooks for LoggingHooks {
        fn acquire_accessibility(
            &self,
            _recipe: AccessibilityRecipe,
            _pid: i32,
        ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
            self.acquire("ax+", "ax-")
        }

        fn acquire_menu(
            &self,
            _recipe: MenuSuppressionRecipe,
            _plan: &MenuSuppressionPlan,
            _focus_state: Arc<Mutex<MacFocusState>>,
        ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
            self.acquire("menu+", "menu-")
        }

        fn prepare_target_belief(
            &self,
            _recipe: TargetBeliefRecipe,
            _facts: &MacWindowFacts,
            _focus_state: Arc<Mutex<MacFocusState>>,
            _deadline: Instant,
        ) -> Result<Option<NativeEvidence>, NativeError> {
            self.log.lock().unwrap().push("belief+");
            if self.fail_at == Some("belief+") {
                return Err(NativeError::new(
                    ErrorCode::Internal,
                    ErrorPhase::Preflight,
                    false,
                    "injected belief preparation failure",
                ));
            }
            Ok(Some(NativeEvidence::default()))
        }
    }

    fn scope_plan() -> ScopePlan<MacNativeScopePlan> {
        let app = AppRef {
            id: AppId::parse("app").unwrap(),
            canonical_id: None,
            name: Some("Fixture".to_owned()),
            pid: Some(44),
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
        let window = ResolvedWindow {
            public,
            native: NativeWindowHandle::new("native-window").unwrap(),
            process: NativeProcessHandle::new("native-process").unwrap(),
            framework: Framework::Chromium,
            geometry: WindowGeometry {
                bounds: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 100.0,
                },
                scale_factor: 2.0,
                revision: GeometryRevision::parse("geometry").unwrap(),
            },
            generation: WindowGeneration(1),
            state: WindowStateKind::Visible,
        };
        let facts = MacWindowFacts {
            stamp: window.stamp(),
            pid: 44,
            process_generation: 1,
            cg_window_id: 99,
            owner_name: "Fixture".to_owned(),
            layer: 0,
            bounds: window.geometry.bounds,
            activation_point: Some(cua_driver_core::api::contracts::Point { x: 10.0, y: 16.0 }),
            scale_factor: Some(2.0),
            state: WindowStateKind::Visible,
            is_on_screen: true,
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
            minimized: Some(false),
        };
        ScopePlan::new(
            ActionId::parse("action").unwrap(),
            window,
            Route::TargetedPointer,
            test_deadline(),
            ScopeRequirements::for_route(Route::TargetedPointer),
            MacNativeScopePlan {
                facts,
                host: HostRecipeContext {
                    os_version: "26.5.1".to_owned(),
                    architecture: "arm64".to_owned(),
                },
                recipe: MacScopeRecipe {
                    accessibility: AccessibilityRecipe::ChromiumPriorStatePreserving,
                    menu: MenuSuppressionRecipe::NotApplicable,
                    target_belief: TargetBeliefRecipe::SwiftCoordinateClick,
                },
                menu: MenuSuppressionPlan::NotApplicable,
            },
        )
    }

    fn test_deadline() -> MutationDeadline {
        let work = Instant::now() + Duration::from_secs(30);
        MutationDeadline::new(work, work + Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn fake_hooks_prove_exact_acquisition_and_reverse_release_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let hooks = LoggingHooks {
            log: Arc::clone(&log),
            fail_at: None,
        };
        let poison = Arc::new(AtomicBool::new(false));
        let plan = scope_plan();
        let mut acquired = acquire_resources(
            &hooks,
            &plan,
            poison,
            Arc::new(Mutex::new(MacFocusState::new(44, 99))),
        )
        .unwrap();
        let outcome = acquired.cleanup.cleanup(plan.deadline.teardown);
        assert!(outcome.failures.is_empty());
        assert_eq!(
            *log.lock().unwrap(),
            vec!["ax+", "menu+", "belief+", "menu-", "ax-"]
        );
    }

    #[test]
    fn acquisition_failure_unwinds_every_earlier_lease_in_reverse_order() {
        for (fail_at, expected) in [
            ("ax+", vec!["ax+"]),
            ("menu+", vec!["ax+", "menu+", "ax-"]),
            ("belief+", vec!["ax+", "menu+", "belief+", "menu-", "ax-"]),
        ] {
            let log = Arc::new(Mutex::new(Vec::new()));
            let hooks = LoggingHooks {
                log: Arc::clone(&log),
                fail_at: Some(fail_at),
            };
            let error = acquire_resources(
                &hooks,
                &scope_plan(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(MacFocusState::new(44, 99))),
            )
            .err()
            .unwrap();
            assert_eq!(error.phase, ErrorPhase::Preflight);
            assert_eq!(*log.lock().unwrap(), expected, "failure at {fail_at}");
        }
    }

    #[test]
    fn every_release_is_attempted_after_an_earlier_cleanup_failure() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let poison = Arc::new(AtomicBool::new(false));
        let mut cleanup = MacScopeCleanup::new(
            Arc::clone(&poison),
            test_deadline().teardown,
            44,
            99,
            Some(7),
            Some(false),
        );
        cleanup.menu = Some(Box::new(LoggedLease {
            log: Arc::clone(&log),
            release: "menu-",
            fail: true,
        }));
        cleanup.accessibility = Some(logged(&log, "ax-"));

        let outcome = cleanup.cleanup(test_deadline().teardown);
        assert_eq!(*log.lock().unwrap(), vec!["menu-", "ax-"]);
        assert_eq!(outcome.leases.menu_dismissal, LeaseTeardownStatus::Failed);
        assert_eq!(
            outcome.leases.target_belief,
            LeaseTeardownStatus::NotApplicable
        );
        assert_eq!(outcome.native_evidence.fields["menu-_attempted"], true);
        assert!(poison.load(Ordering::Acquire));
    }
}
