//! Portable per-mutation interaction scope and evidence.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    contracts::{ActionId, MenuId, ObservationId, Point, Route},
    errors::NativeError,
    observation::{InvalidationReason, ObservationStore, ResolvedWindow, ResolvedWindowStamp},
    settlement::{SettlementProfile, SettlementState},
    target::TargetValidityHandle,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostureResult {
    pub held: bool,
    pub frontmost_changed: bool,
    pub key_window_changed: bool,
    pub physical_cursor_moved: bool,
    pub restored_after_violation: bool,
}

impl Default for PostureResult {
    fn default() -> Self {
        Self {
            held: true,
            frontmost_changed: false,
            key_window_changed: false,
            physical_cursor_moved: false,
            restored_after_violation: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidence {
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_scope: Option<InteractionScopeEvidence>,
}

impl NativeEvidence {
    pub fn merge(&mut self, other: Self) {
        self.fields.extend(other.fields);
        if other.interaction_scope.is_some() {
            self.interaction_scope = other.interaction_scope;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseDecision {
    Acquired,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseTeardownStatus {
    Released,
    NotApplicable,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeLeaseAcquisition {
    pub posture_witness: LeaseDecision,
    pub accessibility: LeaseDecision,
    pub containment: LeaseDecision,
    pub menu_dismissal: LeaseDecision,
    pub target_belief: LeaseDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeLeaseTeardown {
    pub posture_witness: LeaseTeardownStatus,
    pub accessibility: LeaseTeardownStatus,
    pub containment: LeaseTeardownStatus,
    pub menu_dismissal: LeaseTeardownStatus,
    pub target_belief: LeaseTeardownStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionScopeEvidence {
    pub acquisition: ScopeLeaseAcquisition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown: Option<ScopeLeaseTeardown>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRequirements {
    pub target_belief: bool,
    pub containment: bool,
    pub accessibility: bool,
    pub menu_dismissal: bool,
}

/// Controller-owned time boundaries for one mutation.
///
/// Native work must stop by `work`; the later `teardown` boundary is reserved
/// for releasing every acquired lease while containment is still active.
/// Providers must carry this exact value through preflight and acquisition --
/// they do not get to create an independent native timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationDeadline {
    pub work: Instant,
    pub teardown: Instant,
}

/// The single controller-owned transition into possibly mutating native UI.
///
/// Platform providers keep every fallible no-side-effect validation and native
/// event construction before `begin`, then call it immediately before the
/// first AX write/action or targeted event post. The transition is idempotent
/// so cleanup posts can safely pass through the same boundary. While the
/// target lock is held, no other core path can change these stores between
/// action resolution and this transition.
pub struct NativeSideEffectBoundary<'a> {
    observations: &'a mut ObservationStore,
    settlement: &'a mut SettlementState,
    observation_id: ObservationId,
    action_id: ActionId,
    profile: SettlementProfile,
    started: bool,
}

impl<'a> NativeSideEffectBoundary<'a> {
    pub fn new(
        observations: &'a mut ObservationStore,
        settlement: &'a mut SettlementState,
        observation_id: ObservationId,
        action_id: ActionId,
        profile: SettlementProfile,
    ) -> Self {
        debug_assert!(settlement.settled_evidence().is_some());
        Self {
            observations,
            settlement,
            observation_id,
            action_id,
            profile,
            started: false,
        }
    }

    /// Consume perception and mark the target dirty exactly once.
    ///
    /// A provider must call this only after its last no-side-effect check and
    /// immediately before its first native mutation primitive.
    pub fn begin(&mut self) -> Result<(), NativeError> {
        if self.started {
            return Ok(());
        }
        self.observations
            .consume(&self.observation_id, self.action_id.clone())?;
        self.observations
            .invalidate_all(InvalidationReason::MutationDispatched);
        self.settlement
            .mark_dirty(self.action_id.clone(), self.profile.clone())?;
        self.started = true;
        Ok(())
    }

    pub fn started(&self) -> bool {
        self.started
    }
}

impl MutationDeadline {
    pub fn new(work: Instant, teardown: Instant) -> Result<Self, NativeError> {
        if teardown < work {
            return Err(NativeError::invalid(
                "mutation teardown deadline must not precede its work deadline",
            ));
        }
        Ok(Self { work, teardown })
    }
}

impl ScopeRequirements {
    pub fn for_route(route: Route) -> Self {
        Self {
            target_belief: !matches!(route, Route::Semantic),
            containment: true,
            accessibility: true,
            menu_dismissal: false,
        }
    }
}

/// Action-aware output from the final fallible interaction preflight.
///
/// `NativePlan` keeps the selected platform recipe statically typed. Core
/// carries this exact value into acquisition instead of asking a provider to
/// recover recipe state from a mutable side channel.
#[derive(Debug)]
pub struct ScopePlan<NativePlan> {
    pub action_id: ActionId,
    pub window: ResolvedWindow,
    pub route: Route,
    pub deadline: MutationDeadline,
    pub requirements: ScopeRequirements,
    pub opening_menu_id: Option<MenuId>,
    pub native: NativePlan,
}

impl<NativePlan> ScopePlan<NativePlan> {
    pub fn new(
        action_id: ActionId,
        window: ResolvedWindow,
        route: Route,
        deadline: MutationDeadline,
        requirements: ScopeRequirements,
        native: NativePlan,
    ) -> Self {
        Self {
            action_id,
            window,
            route,
            deadline,
            requirements,
            opening_menu_id: None,
            native,
        }
    }

    pub fn bind_opening_menu(&mut self, menu_id: MenuId) {
        self.opening_menu_id = Some(menu_id);
    }

    pub fn into_scope(
        self,
        acquisition: ScopeLeaseAcquisition,
        logical_cursor: TargetCursorHandle,
        native_evidence: NativeEvidence,
        cleanup: Box<dyn ScopeCleanup>,
    ) -> InteractionScope {
        InteractionScope::new(
            self.window,
            self.route,
            self.action_id,
            self.deadline,
            self.opening_menu_id,
            acquisition,
            logical_cursor,
            native_evidence,
            cleanup,
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LogicalCursor {
    pub point: Option<Point>,
}

#[derive(Debug, Clone, Default)]
pub struct TargetCursorHandle(Arc<Mutex<LogicalCursor>>);

impl TargetCursorHandle {
    pub fn position(&self) -> Option<Point> {
        self.0.lock().expect("logical cursor lock poisoned").point
    }

    pub fn update(&self, point: Point) {
        self.0.lock().expect("logical cursor lock poisoned").point = Some(point);
    }
}

pub struct InteractionScope {
    pub window: ResolvedWindow,
    pub route: Route,
    pub deadline: MutationDeadline,
    pub leases: ScopeLeaseAcquisition,
    pub action_id: ActionId,
    pub owner: ResolvedWindowStamp,
    pub opening_menu_id: Option<MenuId>,
    pub logical_cursor: TargetCursorHandle,
    pub posture: PostureResult,
    pub native_evidence: NativeEvidence,
    cleanup: Option<Box<dyn ScopeCleanup>>,
    teardown: Option<ScopeTeardownOutcome>,
    target_validity: Option<TargetValidityHandle>,
}

impl std::fmt::Debug for InteractionScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InteractionScope")
            .field("window", &self.window)
            .field("route", &self.route)
            .field("deadline", &self.deadline)
            .field("leases", &self.leases)
            .field("action_id", &self.action_id)
            .field("owner", &self.owner)
            .field("opening_menu_id", &self.opening_menu_id)
            .field("posture", &self.posture)
            .field("native_evidence", &self.native_evidence)
            .field("teardown", &self.teardown)
            .field("target_validity_bound", &self.target_validity.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScopeTeardownOutcome {
    pub posture: PostureResult,
    pub native_evidence: NativeEvidence,
    pub leases: ScopeLeaseTeardown,
    pub failures: Vec<NativeError>,
}

pub trait ScopeCleanup: Send {
    /// Attempts every bounded native release and returns the complete result.
    /// Implementations must not stop after the first failure. Core invokes the
    /// native cleanup at most once and caches this outcome for repeat release.
    /// `deadline` is the controller's absolute teardown cutoff: callback
    /// barriers and joins must be bounded by it, and already-expired cleanup
    /// must still perform every immediate best-effort release without waiting.
    fn cleanup(&mut self, deadline: Instant) -> ScopeTeardownOutcome;
}

impl InteractionScope {
    #[allow(clippy::too_many_arguments)]
    fn new(
        window: ResolvedWindow,
        route: Route,
        action_id: ActionId,
        deadline: MutationDeadline,
        opening_menu_id: Option<MenuId>,
        leases: ScopeLeaseAcquisition,
        logical_cursor: TargetCursorHandle,
        mut native_evidence: NativeEvidence,
        cleanup: Box<dyn ScopeCleanup>,
    ) -> Self {
        native_evidence.interaction_scope = Some(InteractionScopeEvidence {
            acquisition: leases.clone(),
            teardown: None,
        });
        let owner = window.stamp();
        Self {
            window,
            route,
            deadline,
            leases,
            action_id,
            owner,
            opening_menu_id,
            logical_cursor,
            posture: PostureResult::default(),
            native_evidence,
            cleanup: Some(cleanup),
            teardown: None,
            target_validity: None,
        }
    }

    pub(crate) fn bind_target_validity(&mut self, target_validity: TargetValidityHandle) {
        self.target_validity = Some(target_validity);
    }

    /// Fail closed when a native provider cannot prove that partially posted
    /// state was released. Core removes the poisoned controller after scope
    /// teardown so the next observation constructs fresh native state.
    pub fn invalidate_target(&self) {
        if let Some(target_validity) = &self.target_validity {
            target_validity.invalidate();
        }
    }

    pub fn release(&mut self) -> ScopeTeardownOutcome {
        if let Some(outcome) = &self.teardown {
            return outcome.clone();
        }
        let mut cleanup = self
            .cleanup
            .take()
            .expect("active interaction scope must retain its cleanup");
        let mut outcome = cleanup.cleanup(self.deadline.teardown);
        if Instant::now() > self.deadline.teardown {
            outcome.failures.push(
                NativeError::new(
                    super::errors::ErrorCode::VerificationFailed,
                    super::errors::ErrorPhase::Verify,
                    false,
                    "interaction scope teardown exceeded the controller-owned deadline",
                )
                .with_detail("deadline_stage", "teardown"),
            );
        }
        if !outcome.failures.is_empty() {
            if let Some(target_validity) = &self.target_validity {
                target_validity.invalidate();
            }
        }
        self.posture = outcome.posture.clone();
        self.native_evidence.merge(outcome.native_evidence.clone());
        self.native_evidence.interaction_scope = Some(InteractionScopeEvidence {
            acquisition: self.leases.clone(),
            teardown: Some(outcome.leases.clone()),
        });
        self.teardown = Some(outcome.clone());
        outcome
    }
}

impl Drop for InteractionScope {
    fn drop(&mut self) {
        if self.cleanup.is_none() {
            return;
        }
        let outcome = self.release();
        for error in outcome.failures {
            tracing::error!(error = %error, "failed to release v2 interaction scope during drop");
        }
    }
}
