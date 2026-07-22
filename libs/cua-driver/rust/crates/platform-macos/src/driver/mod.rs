//! Composition root for the background-only macOS v2 platform driver.

use async_trait::async_trait;
use cua_driver_core::api::{
    capabilities::CapabilityCell,
    contracts::KeyStroke,
    errors::NativeError,
    interaction::InteractionScope,
    observation::{ResolvedDrag, ResolvedFocus, ResolvedPoint, ResolvedScroll, ResolvedWindow},
    platform::{
        ClickSpec, KeyboardActionProvider, NativeDispatch, PlatformDriver, PointerActionProvider,
        TargetFocusCoordinator,
    },
};

pub mod actions;
pub mod focus;
pub mod interaction;
pub mod lifecycle;
pub mod menu;
pub mod observation;
pub mod posture;
pub mod settlement;
pub mod target;
pub mod windows;

use actions::{semantic_capability_cells, MacSemanticActions};
use interaction::MacInteractionProvider;
use lifecycle::MacLifecycle;
use observation::MacObservationProvider;
use target::{
    MacInvalidationHub, MacInvalidationSubscription, MacTargetFocusCoordinator, MacTargetState,
};
use windows::MacWindowRegistry;

/// Native v2 provider composition. Plans 003 through 007 replace the explicit
/// unsupported action providers while preserving this lifecycle and identity
/// foundation.
#[derive(Clone)]
pub struct MacDriver {
    lifecycle: MacLifecycle,
    windows: MacWindowRegistry,
    observation: MacObservationProvider,
    interaction: MacInteractionProvider,
    semantic: MacSemanticActions,
    unavailable: UnavailableProvider,
    invalidations: MacInvalidationHub,
}

impl MacDriver {
    pub fn new() -> Self {
        let invalidations = MacInvalidationHub::default();
        let windows = MacWindowRegistry::new(invalidations.clone());
        let lifecycle = MacLifecycle::new(windows.clone());
        let observation = MacObservationProvider::new(windows.clone());
        let interaction = MacInteractionProvider::new(windows.clone());
        let semantic = MacSemanticActions::new(windows.clone());
        Self {
            lifecycle,
            windows,
            observation,
            interaction,
            semantic,
            unavailable: UnavailableProvider,
            invalidations,
        }
    }
}

impl Default for MacDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableProvider;

fn later_plan(capability: &str) -> NativeError {
    NativeError::unsupported(format!(
        "macOS v2 {capability} is not implemented by the lifecycle/window provider"
    ))
}

#[async_trait]
impl PointerActionProvider for UnavailableProvider {
    async fn click(
        &self,
        _scope: &mut InteractionScope,
        _point: ResolvedPoint,
        _click: ClickSpec,
    ) -> Result<NativeDispatch, NativeError> {
        Err(later_plan("pointer click"))
    }

    async fn drag(
        &self,
        _scope: &mut InteractionScope,
        _drag: ResolvedDrag,
    ) -> Result<NativeDispatch, NativeError> {
        Err(later_plan("pointer drag"))
    }

    async fn scroll(
        &self,
        _scope: &mut InteractionScope,
        _scroll: ResolvedScroll,
    ) -> Result<NativeDispatch, NativeError> {
        Err(later_plan("pointer scroll"))
    }
}

#[async_trait]
impl KeyboardActionProvider for UnavailableProvider {
    async fn press_key(
        &self,
        _scope: &mut InteractionScope,
        _focus: &ResolvedFocus,
        _stroke: KeyStroke,
    ) -> Result<NativeDispatch, NativeError> {
        Err(later_plan("keyboard input"))
    }

    async fn type_text(
        &self,
        _scope: &mut InteractionScope,
        _focus: &ResolvedFocus,
        _text: &str,
    ) -> Result<NativeDispatch, NativeError> {
        Err(later_plan("text input"))
    }
}

#[async_trait]
impl PlatformDriver for MacDriver {
    type TargetState = MacTargetState;
    type TargetFocusCoordinator = MacTargetFocusCoordinator;
    type Lifecycle = MacLifecycle;
    type Windows = MacWindowRegistry;
    type Observation = MacObservationProvider;
    type Semantic = MacSemanticActions;
    type Pointer = UnavailableProvider;
    type Keyboard = UnavailableProvider;
    type Interaction = MacInteractionProvider;
    type Invalidations = MacInvalidationSubscription;

    async fn create_target_state(
        &self,
        window: &ResolvedWindow,
    ) -> Result<(Self::TargetState, Self::TargetFocusCoordinator), NativeError> {
        let facts = self.windows.facts_for_stamp(&window.stamp()).await?;
        Ok((
            MacTargetState::new(
                window.stamp(),
                facts.pid,
                facts.cg_window_id,
                self.invalidations.clone(),
            )?,
            MacTargetFocusCoordinator::default(),
        ))
    }

    async fn destroy_target_state(
        &self,
        target: &mut Self::TargetState,
        focus: &mut Self::TargetFocusCoordinator,
    ) -> Result<(), NativeError> {
        target.invalidate();
        focus.shutdown().await
    }

    fn lifecycle(&self) -> &Self::Lifecycle {
        &self.lifecycle
    }

    fn windows(&self) -> &Self::Windows {
        &self.windows
    }

    fn observation(&self) -> &Self::Observation {
        &self.observation
    }

    fn semantic(&self) -> &Self::Semantic {
        &self.semantic
    }

    fn pointer(&self) -> &Self::Pointer {
        &self.unavailable
    }

    fn keyboard(&self) -> &Self::Keyboard {
        &self.unavailable
    }

    fn interaction(&self) -> &Self::Interaction {
        &self.interaction
    }

    fn capability_cells(&self, os_version: &str) -> Vec<CapabilityCell> {
        semantic_capability_cells(os_version)
    }

    fn subscribe_invalidations(&self) -> Self::Invalidations {
        self.invalidations.subscribe()
    }
}
