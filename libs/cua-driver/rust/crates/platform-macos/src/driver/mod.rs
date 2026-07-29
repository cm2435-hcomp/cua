//! Composition root for the background-only macOS v2 platform driver.

use cua_driver_core::api::{
    capabilities::CapabilityCell,
    errors::NativeError,
    observation::ResolvedWindow,
    platform::{PlatformDriver, TargetFocusCoordinator},
};

pub mod actions;
pub mod focus;
pub mod interaction;
pub mod lifecycle;
pub mod menu;
pub mod observation;
pub mod settlement;
pub mod target;
pub mod windows;

use actions::{
    keyboard_capability_cells, pointer_capability_cells, semantic_capability_cells,
    MacKeyboardActions, MacPointerActions, MacSemanticActions,
};
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
    pointer: MacPointerActions,
    keyboard: MacKeyboardActions,
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
        let pointer = MacPointerActions::new(windows.clone());
        let keyboard = MacKeyboardActions::new(windows.clone());
        Self {
            lifecycle,
            windows,
            observation,
            interaction,
            semantic,
            pointer,
            keyboard,
            invalidations,
        }
    }
}

impl Default for MacDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl PlatformDriver for MacDriver {
    type TargetState = MacTargetState;
    type TargetFocusCoordinator = MacTargetFocusCoordinator;
    type Lifecycle = MacLifecycle;
    type Windows = MacWindowRegistry;
    type Observation = MacObservationProvider;
    type Semantic = MacSemanticActions;
    type Pointer = MacPointerActions;
    type Keyboard = MacKeyboardActions;
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
            MacTargetFocusCoordinator::new(facts.pid, facts.cg_window_id)?,
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
        &self.pointer
    }

    fn keyboard(&self) -> &Self::Keyboard {
        &self.keyboard
    }

    fn interaction(&self) -> &Self::Interaction {
        &self.interaction
    }

    fn capability_cells(&self, os_version: &str) -> Vec<CapabilityCell> {
        let mut cells = semantic_capability_cells(os_version);
        cells.extend(pointer_capability_cells(os_version));
        cells.extend(keyboard_capability_cells(os_version));
        cells
    }

    fn subscribe_invalidations(&self) -> Self::Invalidations {
        self.invalidations.subscribe()
    }
}
