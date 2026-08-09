//! Exact, background-safe macOS semantic action routes.

mod click;
mod keyboard;
mod pointer;
mod scroll;
mod semantic;

pub(crate) use keyboard::keyboard_capability_cells;
pub use keyboard::MacKeyboardActions;
pub use pointer::MacPointerActions;
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

pub(crate) fn pointer_capability_cells(os_version: &str) -> Vec<CapabilityCell> {
    let frameworks = [
        Framework::Unknown,
        Framework::AppKit,
        Framework::Chromium,
        Framework::Electron,
        Framework::WebKit,
        Framework::Catalyst,
    ];
    let actions = [ActionKind::Click, ActionKind::Drag, ActionKind::Scroll];
    let states = [
        WindowStateKind::Visible,
        WindowStateKind::Occluded,
        WindowStateKind::Minimized,
        WindowStateKind::OffSpace,
        WindowStateKind::Unknown,
    ];
    let architecture = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        value => value,
    };
    let mut cells = Vec::with_capacity(frameworks.len() * actions.len() * states.len());
    for framework in frameworks {
        for action in &actions {
            for state in &states {
                let candidate_pointer_family = os_version == "26.5.1"
                    && architecture == "arm64"
                    && *state == WindowStateKind::Visible;
                let decision = if candidate_pointer_family {
                    RouteDecision::Supported {
                        route: Route::TargetedPointer,
                    }
                } else {
                    RouteDecision::Unsupported {
                        reason: format!(
                            "recipe_unproven: targeted pointer {action:?} is not manually proved for macOS {os_version} {architecture} {framework:?} {state:?}"
                        ),
                    }
                };
                cells.push(CapabilityCell {
                    key: CapabilityKey {
                        platform: PlatformName::Macos,
                        os_version: os_version.to_owned(),
                        action: action.clone(),
                        addressing: AddressingMode::CapturedPoint,
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

    #[test]
    fn targeted_pointer_cells_publish_one_generic_visible_pointer_family() {
        let cells = pointer_capability_cells("26.5.1");
        assert_eq!(cells.len(), 90);
        let supported: Vec<_> = cells
            .iter()
            .filter(|cell| matches!(cell.decision, RouteDecision::Supported { .. }))
            .collect();
        if std::env::consts::ARCH == "aarch64" {
            assert_eq!(supported.len(), 18);
            assert!(supported.iter().all(|cell| {
                cell.key.addressing == AddressingMode::CapturedPoint
                    && cell.key.window_state == WindowStateKind::Visible
                    && matches!(
                        cell.decision,
                        RouteDecision::Supported {
                            route: Route::TargetedPointer
                        }
                    )
            }));
            assert!(supported
                .iter()
                .any(|cell| cell.key.framework == Framework::Unknown));
            assert!(supported
                .iter()
                .any(|cell| cell.key.framework == Framework::Catalyst));
            assert!(cells.iter().any(|cell| {
                cell.key.action == ActionKind::Click
                    && cell.key.framework == Framework::Chromium
                    && cell.key.window_state == WindowStateKind::Occluded
                    && matches!(cell.decision, RouteDecision::Unsupported { .. })
            }));
        } else {
            assert!(supported.is_empty());
        }
        assert!(cells
            .iter()
            .filter(|cell| {
                matches!(cell.key.action, ActionKind::Drag | ActionKind::Scroll)
                    && cell.key.window_state != WindowStateKind::Visible
            })
            .all(|cell| matches!(cell.decision, RouteDecision::Unsupported { .. })));
    }
}
