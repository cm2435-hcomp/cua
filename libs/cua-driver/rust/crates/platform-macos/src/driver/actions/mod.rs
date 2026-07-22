//! Exact, background-safe macOS semantic action routes.

mod scroll;
mod semantic;

pub use semantic::MacSemanticActions;

use cua_driver_core::api::{
    capabilities::{
        ActionKind, AddressingMode, CapabilityCell, CapabilityKey, Framework, PlatformName,
        RouteDecision, WindowStateKind,
    },
    contracts::Route,
};

pub(crate) fn semantic_capability_cells(os_version: &str) -> Vec<CapabilityCell> {
    let frameworks = [
        Framework::Unknown,
        Framework::AppKit,
        Framework::Chromium,
        Framework::WebKit,
    ];
    let actions = [
        ActionKind::Click,
        ActionKind::Scroll,
        ActionKind::SetValue,
        ActionKind::SelectText,
        ActionKind::PerformSecondaryAction,
    ];
    let states = [
        WindowStateKind::Visible,
        WindowStateKind::Occluded,
        WindowStateKind::Minimized,
        WindowStateKind::OffSpace,
        WindowStateKind::Unknown,
    ];
    let mut cells = Vec::with_capacity(frameworks.len() * actions.len() * states.len());
    for framework in frameworks {
        for action in &actions {
            for state in &states {
                let decision = match state {
                    WindowStateKind::Visible | WindowStateKind::Occluded => {
                        RouteDecision::Supported {
                            route: Route::Semantic,
                        }
                    }
                    WindowStateKind::Minimized => RouteDecision::Unsupported {
                        reason: "macOS semantic background actions refuse minimized windows"
                            .to_owned(),
                    },
                    WindowStateKind::OffSpace => RouteDecision::Unsupported {
                        reason: "macOS semantic background actions refuse off-Space windows"
                            .to_owned(),
                    },
                    WindowStateKind::Unknown => RouteDecision::Unsupported {
                        reason:
                            "macOS semantic background actions require an exact visibility state"
                                .to_owned(),
                    },
                };
                cells.push(CapabilityCell {
                    key: CapabilityKey {
                        platform: PlatformName::Macos,
                        os_version: os_version.to_owned(),
                        action: action.clone(),
                        addressing: AddressingMode::Element,
                        framework: framework.clone(),
                        window_state: state.clone(),
                    },
                    decision,
                });
            }
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_cells_are_semantic_element_only_and_state_explicit() {
        let cells = semantic_capability_cells("fixture");
        assert_eq!(cells.len(), 100);
        assert!(cells.iter().all(|cell| {
            cell.key.addressing == AddressingMode::Element
                && !matches!(
                    cell.key.action,
                    ActionKind::Drag | ActionKind::PressKey | ActionKind::TypeText
                )
        }));
        assert_eq!(
            cells
                .iter()
                .filter(|cell| matches!(cell.decision, RouteDecision::Supported { .. }))
                .count(),
            40
        );
        assert!(cells
            .iter()
            .filter(|cell| matches!(
                cell.key.window_state,
                WindowStateKind::Minimized | WindowStateKind::OffSpace | WindowStateKind::Unknown
            ))
            .all(|cell| matches!(cell.decision, RouteDecision::Unsupported { .. })));
        assert!(!cells.iter().any(|cell| matches!(
            cell.key.framework,
            Framework::Electron | Framework::Catalyst
        )));
    }
}
