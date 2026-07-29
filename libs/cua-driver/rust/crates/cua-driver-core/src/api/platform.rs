//! Portable platform-provider seams for the v2 controller.

use std::time::Instant;

use async_trait::async_trait;

use super::{
    capabilities::CapabilityManifest,
    contracts::{
        AppId, AppQuery, AppRef, AppSelector, ElementRef, KeyStroke, Modifier, MouseButton,
        Readiness, ScrollDirection, SelectionType, WindowGeneration, WindowId, WindowRef,
    },
    errors::NativeError,
    interaction::{
        InteractionScope, MutationDeadline, NativeEvidence, NativeSideEffectBoundary, ScopePlan,
        ScopeRequirements, TargetCursorHandle,
    },
    menu::NativeMenuEvidence,
    observation::{
        InvalidationReason, NativeObservationUpdate, NativeProcessHandle, ResolvedDrag,
        ResolvedElement, ResolvedFocus, ResolvedPoint, ResolvedScroll, ResolvedWindow,
    },
    settlement::{DirtyState, SettlementAttempt, SettlementEvidence},
    Route, VerificationLevel,
};

#[derive(Debug, Clone)]
pub struct NativeLaunch {
    pub app: AppRef,
    pub windows: Vec<WindowRef>,
    pub reused_running_app: bool,
    pub settlement: SettlementEvidence,
}

#[derive(Debug, Clone, Default)]
pub struct LaunchScope {
    pub native_evidence: NativeEvidence,
    pub partial_app: Option<AppRef>,
    pub partial_windows: Vec<WindowRef>,
    pub pending_settlement: Option<super::settlement::PendingSettlementEvidence>,
    action_id: Option<super::contracts::ActionId>,
    side_effect_started: bool,
}

impl LaunchScope {
    pub fn for_action(action_id: super::contracts::ActionId) -> Self {
        Self {
            action_id: Some(action_id),
            ..Self::default()
        }
    }

    pub fn action_id(&self) -> Option<&super::contracts::ActionId> {
        self.action_id.as_ref()
    }

    pub fn begin_launch(&mut self) {
        self.side_effect_started = true;
    }

    pub fn side_effect_started(&self) -> bool {
        self.side_effect_started
    }

    pub fn record_partial_result(&mut self, app: AppRef, windows: Vec<WindowRef>) {
        self.partial_app = Some(app);
        self.partial_windows = windows;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ObserveRequest {
    pub include_text: bool,
    pub include_screenshots: bool,
}

#[derive(Debug, Clone)]
pub struct ClickSpec {
    pub button: MouseButton,
    pub click_count: u8,
    pub modifiers: Vec<Modifier>,
}

#[derive(Debug, Clone)]
pub struct ElementScrollSpec {
    pub direction: ScrollDirection,
    pub pages: u16,
}

#[derive(Debug, Clone)]
pub struct SelectionSpec {
    pub text: String,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub selection_type: SelectionType,
}

#[derive(Debug, Clone)]
pub struct NativeDispatch {
    pub verification: VerificationLevel,
    pub evidence: NativeEvidence,
    pub warnings: Vec<String>,
    pub menu: Option<NativeMenuEvidence>,
}

#[derive(Debug, Clone)]
pub enum ResolvedAction {
    ElementClick {
        source: ElementRef,
        element: ResolvedElement,
        spec: ClickSpec,
    },
    PointClick {
        point: ResolvedPoint,
        spec: ClickSpec,
    },
    Drag(Box<ResolvedDrag>),
    ElementScroll {
        element: ResolvedElement,
        spec: ElementScrollSpec,
    },
    DeltaScroll(ResolvedScroll),
    PressKey {
        focus: ResolvedFocus,
        stroke: KeyStroke,
    },
    TypeText {
        focus: ResolvedFocus,
        text: String,
    },
    SetValue {
        element: ResolvedElement,
        value: String,
    },
    SelectText {
        element: ResolvedElement,
        selection: SelectionSpec,
    },
    Secondary {
        element: ResolvedElement,
        action: String,
    },
}

impl NativeDispatch {
    pub fn dispatch_verified() -> Self {
        Self {
            verification: VerificationLevel::DispatchVerified,
            evidence: NativeEvidence::default(),
            warnings: Vec::new(),
            menu: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetInvalidation {
    /// The native event source overran, so core cannot prove which cached
    /// target identities remain valid. Every target must be torn down and
    /// explicitly re-observed from an exact platform snapshot.
    NativeStateResyncRequired,
    ProcessExited {
        process: NativeProcessHandle,
    },
    WindowGenerationChanged {
        app_id: AppId,
        window_id: WindowId,
        previous: WindowGeneration,
        current: WindowGeneration,
    },
    /// Native state changed without destroying the target controller. Core
    /// invalidates perception state under the target lock; the platform
    /// remains the producer of the signal, not a second state-machine owner.
    ObservationChanged {
        app_id: AppId,
        window_id: WindowId,
        generation: WindowGeneration,
        reason: InvalidationReason,
    },
}

#[async_trait]
pub trait InvalidationSubscription: Send {
    async fn next(&mut self) -> Option<TargetInvalidation>;
}

#[async_trait]
pub trait TargetFocusCoordinator: Send {
    async fn shutdown(&mut self) -> Result<(), NativeError>;
}

#[async_trait]
pub trait LifecycleProvider: Send + Sync {
    async fn readiness(&self) -> Result<Readiness, NativeError>;
    async fn capabilities(&self) -> Result<CapabilityManifest, NativeError>;
    async fn list_apps(&self, query: AppQuery) -> Result<Vec<AppRef>, NativeError>;
    async fn launch_background(
        &self,
        app: AppSelector,
        scope: &mut LaunchScope,
    ) -> Result<NativeLaunch, NativeError>;
}

#[async_trait]
pub trait WindowProvider: Send + Sync {
    async fn list_windows(&self, app: Option<&AppRef>) -> Result<Vec<WindowRef>, NativeError>;
    async fn rehydrate(
        &self,
        id: &WindowId,
        app: Option<&AppRef>,
    ) -> Result<WindowRef, NativeError>;
    async fn resolve(&self, window: &WindowRef) -> Result<ResolvedWindow, NativeError>;
}

#[async_trait]
pub trait ObservationProvider<TargetState>: Send + Sync {
    async fn settle(
        &self,
        target: &mut TargetState,
        dirty: &DirtyState,
        deadline: Instant,
    ) -> Result<SettlementAttempt, NativeError>;
    async fn observe(
        &self,
        target: &mut TargetState,
        window: &ResolvedWindow,
        request: ObserveRequest,
    ) -> Result<NativeObservationUpdate, NativeError>;
}

/// Two-phase semantic operations under the locked native target state.
///
/// `prepare` is side-effect free and runs after scope acquisition but before
/// observation consumption or dirty marking. It owns every fallible shape,
/// identity, exposure, settable, selection, and route proof and returns a
/// retained typed plan. `dispatch` is the controller-owned dispatch boundary:
/// method entry consumes the observation and permits only the selected native
/// operation and its post-dispatch verification readback.
#[async_trait]
pub trait SemanticActionProvider<TargetState>: Send + Sync {
    type PreparedAction: Send;

    /// Determines whether an element click has an exact, currently usable
    /// semantic primitive. A false result lets core resolve the element's
    /// current observation-owned point and query the captured-point route.
    /// Stale identity failures must be returned, never converted to fallback.
    async fn element_click_candidate(
        &self,
        target: &mut TargetState,
        element: &ResolvedElement,
        spec: &ClickSpec,
    ) -> Result<bool, NativeError>;

    async fn prepare(
        &self,
        target: &mut TargetState,
        scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<Self::PreparedAction, NativeError>;

    async fn dispatch(
        &self,
        target: &mut TargetState,
        scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: Self::PreparedAction,
    ) -> Result<NativeDispatch, NativeError>;
}

/// Two-phase targeted pointer operations under the locked native target state.
///
/// `prepare` is side-effect free and retains the exact surface/capture/native
/// route proof before core consumes the observation. `dispatch` is the
/// controller-owned dispatch boundary and may only execute that retained plan.
#[async_trait]
pub trait PointerActionProvider<TargetState>: Send + Sync {
    type PreparedAction: Send;

    async fn prepare(
        &self,
        target: &mut TargetState,
        scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<Self::PreparedAction, NativeError>;

    async fn dispatch(
        &self,
        target: &mut TargetState,
        scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: Self::PreparedAction,
    ) -> Result<NativeDispatch, NativeError>;
}

/// Two-phase keyboard operations against a core-resolved focus token.
///
/// Normalization and exact native focus/route proof complete in `prepare`
/// before observation consumption. `dispatch` may only execute the retained
/// plan and must release any partially posted modifier sequence on failure.
#[async_trait]
pub trait KeyboardActionProvider<TargetState>: Send + Sync {
    type PreparedAction: Send;

    async fn prepare(
        &self,
        target: &mut TargetState,
        scope: &mut InteractionScope,
        action: &ResolvedAction,
    ) -> Result<Self::PreparedAction, NativeError>;

    async fn dispatch(
        &self,
        target: &mut TargetState,
        scope: &mut InteractionScope,
        boundary: &mut NativeSideEffectBoundary<'_>,
        action: Self::PreparedAction,
    ) -> Result<NativeDispatch, NativeError>;
}

#[async_trait]
pub trait InteractionProvider<TargetState, Focus>: Send + Sync {
    type NativeScopePlan: Send;

    /// Route-specific native preflight. This is the final fallible preparation
    /// seam and runs before scope acquisition, observation consumption, or
    /// dirty marking. The returned native plan is action-aware and is carried
    /// by value into acquisition; providers must not use mutable pending
    /// recipe state to connect these calls. Preflight must be side-effect free:
    /// it may not acquire native resources or dispatch, and cancellation at
    /// its controller-owned deadline must leave the target reusable.
    #[allow(clippy::too_many_arguments)] // Each action/target/route invariant is explicit.
    async fn preflight(
        &self,
        target: &mut TargetState,
        focus: &mut Focus,
        action_id: &super::contracts::ActionId,
        window: &ResolvedWindow,
        route: Route,
        action: &ResolvedAction,
        deadline: MutationDeadline,
        requirements: ScopeRequirements,
    ) -> Result<ScopePlan<Self::NativeScopePlan>, NativeError>;

    /// Acquires the leases in `plan`. Every partially acquired resource must
    /// be owned by a cancellation-safe RAII guard until the complete scope is
    /// returned; a cancelled acquisition causes core to poison the target
    /// because complete teardown evidence is unavailable.
    async fn acquire_scope(
        &self,
        target: &mut TargetState,
        focus: &mut Focus,
        plan: ScopePlan<Self::NativeScopePlan>,
        logical_cursor: TargetCursorHandle,
    ) -> Result<InteractionScope, NativeError>;
}

#[async_trait]
pub trait PlatformDriver: Send + Sync + 'static {
    type TargetState: Send;
    type TargetFocusCoordinator: TargetFocusCoordinator;
    type Lifecycle: LifecycleProvider;
    type Windows: WindowProvider;
    type Observation: ObservationProvider<Self::TargetState>;
    type Semantic: SemanticActionProvider<Self::TargetState>;
    type Pointer: PointerActionProvider<Self::TargetState>;
    type Keyboard: KeyboardActionProvider<Self::TargetState>;
    type Interaction: InteractionProvider<Self::TargetState, Self::TargetFocusCoordinator>;
    type Invalidations: InvalidationSubscription;

    async fn create_target_state(
        &self,
        window: &ResolvedWindow,
    ) -> Result<(Self::TargetState, Self::TargetFocusCoordinator), NativeError>;

    async fn destroy_target_state(
        &self,
        target: &mut Self::TargetState,
        focus: &mut Self::TargetFocusCoordinator,
    ) -> Result<(), NativeError> {
        let _ = target;
        focus.shutdown().await
    }

    fn lifecycle(&self) -> &Self::Lifecycle;
    fn windows(&self) -> &Self::Windows;
    fn observation(&self) -> &Self::Observation;
    fn semantic(&self) -> &Self::Semantic;
    fn pointer(&self) -> &Self::Pointer;
    fn keyboard(&self) -> &Self::Keyboard;
    fn interaction(&self) -> &Self::Interaction;
    /// Complete, conservative capability matrix for this platform/version.
    /// Core seeds its immutable routing registry exclusively from these cells.
    fn capability_cells(&self, os_version: &str) -> Vec<super::capabilities::CapabilityCell>;
    fn subscribe_invalidations(&self) -> Self::Invalidations;
}
