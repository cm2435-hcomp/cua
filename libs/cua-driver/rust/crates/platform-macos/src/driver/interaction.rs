//! Complete per-mutation macOS interaction scope acquisition and teardown.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use cua_driver_core::api::{
    contracts::{ActionId, Route},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{
        InteractionScope, LeaseDecision, LeaseTeardownStatus, MutationDeadline, NativeEvidence,
        PostureResult, ScopeCleanup, ScopeLeaseAcquisition, ScopeLeaseTeardown, ScopePlan,
        ScopeRequirements, ScopeTeardownOutcome, TargetCursorHandle,
    },
    observation::ResolvedWindow,
    platform::{InteractionProvider, ResolvedAction},
};

use crate::{
    apps::nsworkspace::WorkspaceEventHub,
    ax::enablement::AxEnablementLease,
    focus_steal::{SuppressionLease, SuppressionOutcome},
    input::slps_make_key,
};

use super::{
    focus::{
        select_scope_recipe, AccessibilityRecipe, HostRecipeContext, MacScopeRecipe,
        MenuSuppressionRecipe, TargetBeliefLease, TargetBeliefRecipe,
    },
    menu::{self, MenuSuppressionPlan},
    posture::MacInteractionPostureWitness,
    target::{MacFocusState, MacTargetFocusCoordinator, MacTargetState},
    windows::{MacWindowFacts, MacWindowRegistry},
};

const CONTAINMENT_BARRIER_TIMEOUT: Duration = Duration::from_millis(250);

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

trait PostureResource: Send {
    fn finish(&mut self, deadline: Instant)
        -> Result<(PostureResult, NativeEvidence), NativeError>;
}

struct PostureAcquisition {
    prior_frontmost_pid: i32,
    resource: Box<dyn PostureResource>,
}

trait MacInteractionHooks: Send + Sync {
    fn acquire_posture(&self) -> Result<PostureAcquisition, NativeError>;

    fn acquire_accessibility(
        &self,
        recipe: AccessibilityRecipe,
        pid: i32,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError>;

    fn acquire_containment(
        &self,
        required: bool,
        target_pid: i32,
        prior_frontmost_pid: i32,
        deadline: Instant,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError>;

    fn acquire_menu(
        &self,
        recipe: MenuSuppressionRecipe,
        plan: &MenuSuppressionPlan,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError>;

    fn acquire_target_belief(
        &self,
        recipe: TargetBeliefRecipe,
        action_id: &ActionId,
        pid: i32,
        cg_window_id: u32,
        focus_state: Arc<Mutex<MacFocusState>>,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError>;
}

#[derive(Default)]
struct SystemInteractionHooks;

impl MacInteractionHooks for SystemInteractionHooks {
    fn acquire_posture(&self) -> Result<PostureAcquisition, NativeError> {
        let witness = MacInteractionPostureWitness::begin()?;
        let prior_frontmost_pid = witness.prior_frontmost_pid();
        Ok(PostureAcquisition {
            prior_frontmost_pid,
            resource: Box::new(SystemPostureResource(witness)),
        })
    }

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
        }
    }

    fn acquire_containment(
        &self,
        required: bool,
        target_pid: i32,
        prior_frontmost_pid: i32,
        deadline: Instant,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
        if !required || target_pid == prior_frontmost_pid {
            return Ok(None);
        }
        Ok(Some(Box::new(ContainmentLeaseResource(Some(
            crate::focus_steal::begin_suppression_until(
                Some(target_pid),
                prior_frontmost_pid,
                "driver.v2.interaction",
                deadline,
            ),
        )))))
    }

    fn acquire_menu(
        &self,
        _recipe: MenuSuppressionRecipe,
        plan: &MenuSuppressionPlan,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
        menu::acquire_production(plan).map(|resource| {
            resource
                .map(|resource| Box::new(MenuResourceAdapter(resource)) as Box<dyn LeaseResource>)
        })
    }

    fn acquire_target_belief(
        &self,
        recipe: TargetBeliefRecipe,
        action_id: &ActionId,
        pid: i32,
        cg_window_id: u32,
        focus_state: Arc<Mutex<MacFocusState>>,
    ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
        match recipe {
            TargetBeliefRecipe::NotApplicable => Ok(None),
            TargetBeliefRecipe::ChromiumPointClickSlpsMakeKeyHost26_5_1Arm64 => {
                Ok(Some(Box::new(BeliefLeaseResource(
                    TargetBeliefLease::acquire(action_id.clone(), pid, cg_window_id, focus_state)?,
                ))))
            }
        }
    }
}

struct SystemPostureResource(MacInteractionPostureWitness);

impl PostureResource for SystemPostureResource {
    fn finish(
        &mut self,
        deadline: Instant,
    ) -> Result<(PostureResult, NativeEvidence), NativeError> {
        Ok(self.0.finish(deadline))
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

struct ContainmentLeaseResource(Option<SuppressionLease>);

impl LeaseResource for ContainmentLeaseResource {
    fn release(&mut self, deadline: Instant) -> LeaseRelease {
        let close = self
            .0
            .take()
            .map(|lease| lease.close_with_evidence(remaining(deadline)))
            .unwrap_or_default();
        let workspace_drained = WorkspaceEventHub::shared().barrier(remaining(deadline));
        containment_evidence(
            close.evidence,
            close.callback_queue_drained,
            workspace_drained,
        )
    }
}

fn containment_evidence(
    outcome: SuppressionOutcome,
    containment_drained: bool,
    workspace_drained: bool,
) -> LeaseRelease {
    let mut evidence = NativeEvidence::default();
    evidence.fields.insert(
        "containment_activations".to_owned(),
        outcome.activations.into(),
    );
    evidence.fields.insert(
        "containment_restore_attempts".to_owned(),
        outcome.restore_attempts.into(),
    );
    evidence.fields.insert(
        "containment_restore_failures".to_owned(),
        outcome.restore_failures.into(),
    );
    evidence.fields.insert(
        "containment_callback_queue_drained".to_owned(),
        containment_drained.into(),
    );
    evidence.fields.insert(
        "workspace_callback_queue_drained".to_owned(),
        workspace_drained.into(),
    );
    let mut failures = Vec::new();
    if !workspace_drained || !containment_drained {
        failures.push(
            NativeError::new(
                ErrorCode::PostureUnverifiable,
                ErrorPhase::Verify,
                true,
                "focus-containment callback queues did not drain before release",
            )
            .with_detail("workspace_queue_drained", workspace_drained)
            .with_detail("containment_queue_drained", containment_drained),
        );
    }
    if outcome.activations > 0 || outcome.restore_failures > 0 {
        failures.push(
            NativeError::new(
                ErrorCode::PostureViolated,
                ErrorPhase::Verify,
                false,
                "focus containment observed a foreground activation excursion",
            )
            .with_detail("activations", outcome.activations)
            .with_detail("restore_failures", outcome.restore_failures),
        );
    }
    LeaseRelease {
        evidence,
        failure: NativeError::primary(failures),
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

struct BeliefLeaseResource(TargetBeliefLease);

impl LeaseResource for BeliefLeaseResource {
    fn release(&mut self, _deadline: Instant) -> LeaseRelease {
        let mut evidence = NativeEvidence::default();
        let failure = self.0.release().err();
        evidence
            .fields
            .insert("target_belief_release_attempted".to_owned(), true.into());
        evidence
            .fields
            .insert("target_belief_revoked".to_owned(), failure.is_none().into());
        LeaseRelease { evidence, failure }
    }
}

fn remaining(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .min(CONTAINMENT_BARRIER_TIMEOUT)
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
        let recipe =
            select_scope_recipe(&self.host, &window.framework, route, action, &requirements)?;
        if recipe.target_belief != TargetBeliefRecipe::NotApplicable && !slps_make_key::available()
        {
            return Err(NativeError::unsupported(
                "recipe_unproven: target-only SLPS make-key symbols are unavailable on this host",
            )
            .with_detail("recipe_status", "recipe_unproven")
            .with_detail("os_version", self.host.os_version.clone())
            .with_detail("architecture", self.host.architecture.clone()));
        }
        let menu = if requirements.menu_dismissal {
            return Err(NativeError::unsupported(
                "recipe_unproven: required menu suppression has no exact predicate",
            )
            .with_detail("recipe_status", "recipe_unproven"));
        } else {
            MenuSuppressionPlan::NotApplicable
        };
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
        plan: ScopePlan<Self::NativeScopePlan>,
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

        let acquired = acquire_resources(
            self.hooks.as_ref(),
            &plan,
            target.poison_handle(),
            focus.state_handle(),
        )?;
        Ok(plan.into_scope(
            acquired.acquisition,
            logical_cursor,
            acquired.evidence,
            Box::new(acquired.cleanup),
        ))
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
    let posture = hooks.acquire_posture()?;
    let mut cleanup = MacScopeCleanup::new(poison, posture.resource, plan.deadline.teardown);

    let result = (|| {
        cleanup.accessibility =
            hooks.acquire_accessibility(plan.native.recipe.accessibility, plan.native.facts.pid)?;
        cleanup.containment = hooks.acquire_containment(
            plan.requirements.containment,
            plan.native.facts.pid,
            posture.prior_frontmost_pid,
            plan.deadline.teardown,
        )?;
        cleanup.menu = hooks.acquire_menu(plan.native.recipe.menu, &plan.native.menu)?;
        cleanup.target_belief = hooks.acquire_target_belief(
            plan.native.recipe.target_belief,
            &plan.action_id,
            plan.native.facts.pid,
            plan.native.facts.cg_window_id,
            Arc::clone(&focus_state),
        )?;
        Ok::<(), NativeError>(())
    })();

    if let Err(primary) = result {
        let outcome = cleanup.cleanup(plan.deadline.teardown);
        let belief_still_active = focus_state
            .lock()
            .expect("macOS focus coordinator poisoned")
            .active_belief
            .is_some();
        if belief_still_active {
            cleanup.poison.store(true, Ordering::Release);
        }
        let mut failures = Vec::with_capacity(1 + outcome.failures.len());
        failures.push(primary);
        failures.extend(outcome.failures);
        let mut combined = NativeError::primary(failures).expect("acquisition failure is nonempty");
        if belief_still_active {
            combined = combined.with_detail("target_poisoned", true);
        }
        return Err(combined);
    }

    let acquisition = ScopeLeaseAcquisition {
        posture_witness: LeaseDecision::Acquired,
        accessibility: decision(&cleanup.accessibility),
        containment: decision(&cleanup.containment),
        menu_dismissal: decision(&cleanup.menu),
        target_belief: decision(&cleanup.target_belief),
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
    posture: Option<Box<dyn PostureResource>>,
    accessibility: Option<Box<dyn LeaseResource>>,
    containment: Option<Box<dyn LeaseResource>>,
    menu: Option<Box<dyn LeaseResource>>,
    target_belief: Option<Box<dyn LeaseResource>>,
    outcome: Option<ScopeTeardownOutcome>,
}

impl MacScopeCleanup {
    fn new(
        poison: Arc<AtomicBool>,
        posture: Box<dyn PostureResource>,
        teardown_deadline: Instant,
    ) -> Self {
        Self {
            poison,
            teardown_deadline,
            posture: Some(posture),
            accessibility: None,
            containment: None,
            menu: None,
            target_belief: None,
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
        let target_belief = release_lease(
            &mut self.target_belief,
            deadline,
            &mut evidence,
            &mut failures,
        );
        let menu_dismissal = release_lease(&mut self.menu, deadline, &mut evidence, &mut failures);
        let containment = release_lease(
            &mut self.containment,
            deadline,
            &mut evidence,
            &mut failures,
        );
        let accessibility = release_lease(
            &mut self.accessibility,
            deadline,
            &mut evidence,
            &mut failures,
        );
        let (posture_witness, posture) = match self.posture.as_mut() {
            None => (LeaseTeardownStatus::NotApplicable, PostureResult::default()),
            Some(resource) => match resource.finish(deadline) {
                Ok((posture, posture_evidence)) => {
                    evidence.merge(posture_evidence);
                    let status = LeaseTeardownStatus::Released;
                    if let Some(error) = posture_failure(&posture) {
                        failures.push(error);
                    }
                    (status, posture)
                }
                Err(error) => {
                    failures.push(error);
                    (
                        LeaseTeardownStatus::Failed,
                        PostureResult {
                            held: false,
                            ..PostureResult::default()
                        },
                    )
                }
            },
        };
        self.posture = None;

        if !failures.is_empty() {
            self.poison.store(true, Ordering::Release);
            evidence
                .fields
                .insert("target_poisoned".to_owned(), true.into());
        }
        let outcome = ScopeTeardownOutcome {
            posture,
            native_evidence: evidence,
            leases: ScopeLeaseTeardown {
                posture_witness,
                accessibility,
                containment,
                menu_dismissal,
                target_belief,
            },
            failures,
        };
        self.outcome = Some(outcome.clone());
        outcome
    }
}

fn posture_failure(posture: &PostureResult) -> Option<NativeError> {
    if posture.held {
        return None;
    }
    let observed_excursion = posture.frontmost_changed
        || posture.key_window_changed
        || posture.physical_cursor_moved
        || posture.restored_after_violation;
    let (code, retryable, message) = if observed_excursion {
        (
            ErrorCode::PostureViolated,
            false,
            "interaction changed foreground, focused window, or physical cursor posture",
        )
    } else {
        (
            ErrorCode::PostureUnverifiable,
            true,
            "interaction posture witness was incomplete or lagged",
        )
    };
    Some(
        NativeError::new(code, ErrorPhase::Verify, retryable, message)
            .with_detail("posture", serde_json::to_value(posture).unwrap_or_default()),
    )
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
    use std::sync::Mutex;

    use cua_driver_core::api::{
        capabilities::{Framework, WindowStateKind},
        contracts::{AppId, AppRef, GeometryRevision, Rect, WindowGeneration, WindowId, WindowRef},
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

    struct LoggedPosture {
        log: Arc<Mutex<Vec<&'static str>>>,
        posture: PostureResult,
    }

    impl PostureResource for LoggedPosture {
        fn finish(
            &mut self,
            _deadline: Instant,
        ) -> Result<(PostureResult, NativeEvidence), NativeError> {
            self.log.lock().unwrap().push("posture-");
            Ok((self.posture.clone(), NativeEvidence::default()))
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
        containment_deadlines: Arc<Mutex<Vec<Instant>>>,
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
        fn acquire_posture(&self) -> Result<PostureAcquisition, NativeError> {
            self.log.lock().unwrap().push("posture+");
            Ok(PostureAcquisition {
                prior_frontmost_pid: 7,
                resource: Box::new(LoggedPosture {
                    log: Arc::clone(&self.log),
                    posture: PostureResult::default(),
                }),
            })
        }

        fn acquire_accessibility(
            &self,
            _recipe: AccessibilityRecipe,
            _pid: i32,
        ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
            self.acquire("ax+", "ax-")
        }

        fn acquire_containment(
            &self,
            _required: bool,
            _target_pid: i32,
            _prior_frontmost_pid: i32,
            deadline: Instant,
        ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
            self.containment_deadlines.lock().unwrap().push(deadline);
            self.acquire("containment+", "containment-")
        }

        fn acquire_menu(
            &self,
            _recipe: MenuSuppressionRecipe,
            _plan: &MenuSuppressionPlan,
        ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
            self.acquire("menu+", "menu-")
        }

        fn acquire_target_belief(
            &self,
            _recipe: TargetBeliefRecipe,
            _action_id: &ActionId,
            _pid: i32,
            _cg_window_id: u32,
            _focus_state: Arc<Mutex<MacFocusState>>,
        ) -> Result<Option<Box<dyn LeaseResource>>, NativeError> {
            self.acquire("belief+", "belief-")
        }
    }

    fn scope_plan() -> ScopePlan<MacNativeScopePlan> {
        let app = AppRef {
            id: AppId::parse("app").unwrap(),
            name: Some("Fixture".to_owned()),
            pid: Some(44),
            running: true,
        };
        let public = WindowRef {
            id: WindowId::parse("window").unwrap(),
            app,
            title: None,
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
                    target_belief: TargetBeliefRecipe::ChromiumPointClickSlpsMakeKeyHost26_5_1Arm64,
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
            containment_deadlines: Arc::new(Mutex::new(Vec::new())),
            fail_at: None,
        };
        let poison = Arc::new(AtomicBool::new(false));
        let plan = scope_plan();
        let mut acquired = acquire_resources(
            &hooks,
            &plan,
            poison,
            Arc::new(Mutex::new(MacFocusState::default())),
        )
        .unwrap();
        assert_eq!(
            hooks.containment_deadlines.lock().unwrap().as_slice(),
            [plan.deadline.teardown]
        );
        let outcome = acquired.cleanup.cleanup(plan.deadline.teardown);
        assert!(outcome.failures.is_empty());
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "posture+",
                "ax+",
                "containment+",
                "menu+",
                "belief+",
                "belief-",
                "menu-",
                "containment-",
                "ax-",
                "posture-",
            ]
        );
    }

    #[test]
    fn acquisition_failure_unwinds_every_earlier_lease_in_reverse_order() {
        for (fail_at, expected) in [
            ("ax+", vec!["posture+", "ax+", "posture-"]),
            (
                "containment+",
                vec!["posture+", "ax+", "containment+", "ax-", "posture-"],
            ),
            (
                "menu+",
                vec![
                    "posture+",
                    "ax+",
                    "containment+",
                    "menu+",
                    "containment-",
                    "ax-",
                    "posture-",
                ],
            ),
            (
                "belief+",
                vec![
                    "posture+",
                    "ax+",
                    "containment+",
                    "menu+",
                    "belief+",
                    "menu-",
                    "containment-",
                    "ax-",
                    "posture-",
                ],
            ),
        ] {
            let log = Arc::new(Mutex::new(Vec::new()));
            let hooks = LoggingHooks {
                log: Arc::clone(&log),
                containment_deadlines: Arc::new(Mutex::new(Vec::new())),
                fail_at: Some(fail_at),
            };
            let error = acquire_resources(
                &hooks,
                &scope_plan(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(MacFocusState::default())),
            )
            .err()
            .unwrap();
            assert_eq!(error.phase, ErrorPhase::Preflight);
            assert_eq!(*log.lock().unwrap(), expected, "failure at {fail_at}");
        }
    }

    #[test]
    fn cleanup_is_reverse_order_and_restored_violation_poison_is_sticky() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let poison = Arc::new(AtomicBool::new(false));
        let mut cleanup = MacScopeCleanup::new(
            Arc::clone(&poison),
            Box::new(LoggedPosture {
                log: Arc::clone(&log),
                posture: PostureResult {
                    held: false,
                    frontmost_changed: true,
                    key_window_changed: false,
                    physical_cursor_moved: false,
                    restored_after_violation: true,
                },
            }),
            test_deadline().teardown,
        );
        cleanup.accessibility = Some(logged(&log, "ax-"));
        cleanup.containment = Some(logged(&log, "containment-"));
        cleanup.menu = Some(logged(&log, "menu-"));
        cleanup.target_belief = Some(logged(&log, "belief-"));

        let outcome = cleanup.cleanup(test_deadline().teardown);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["belief-", "menu-", "containment-", "ax-", "posture-"]
        );
        assert!(outcome.posture.restored_after_violation);
        assert_eq!(outcome.failures[0].code, ErrorCode::PostureViolated);
        assert!(poison.load(Ordering::Acquire));
    }

    #[test]
    fn every_release_is_attempted_after_an_earlier_cleanup_failure() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let poison = Arc::new(AtomicBool::new(false));
        let mut cleanup = MacScopeCleanup::new(
            Arc::clone(&poison),
            Box::new(LoggedPosture {
                log: Arc::clone(&log),
                posture: PostureResult::default(),
            }),
            test_deadline().teardown,
        );
        cleanup.target_belief = Some(Box::new(LoggedLease {
            log: Arc::clone(&log),
            release: "belief-",
            fail: true,
        }));
        cleanup.containment = Some(logged(&log, "containment-"));
        cleanup.accessibility = Some(logged(&log, "ax-"));

        let outcome = cleanup.cleanup(test_deadline().teardown);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["belief-", "containment-", "ax-", "posture-"]
        );
        assert_eq!(outcome.leases.target_belief, LeaseTeardownStatus::Failed);
        assert_eq!(outcome.leases.containment, LeaseTeardownStatus::Released);
        assert_eq!(outcome.native_evidence.fields["belief-_attempted"], true);
        assert!(poison.load(Ordering::Acquire));
    }

    #[test]
    fn containment_barrier_timeout_preserves_evidence_and_is_unverifiable() {
        let release = containment_evidence(
            SuppressionOutcome {
                activations: 0,
                restore_attempts: 2,
                restore_failures: 0,
            },
            false,
            true,
        );
        assert_eq!(release.evidence.fields["containment_restore_attempts"], 2);
        assert_eq!(
            release.failure.as_ref().unwrap().code,
            ErrorCode::PostureUnverifiable
        );
    }

    #[test]
    fn containment_excursion_preserves_evidence_and_is_a_violation() {
        let release = containment_evidence(
            SuppressionOutcome {
                activations: 1,
                restore_attempts: 1,
                restore_failures: 0,
            },
            true,
            true,
        );
        assert_eq!(release.evidence.fields["containment_activations"], 1);
        assert_eq!(
            release.failure.as_ref().unwrap().code,
            ErrorCode::PostureViolated
        );
    }
}
