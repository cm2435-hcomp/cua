//! Conservative, explicit route capability reporting and preflight.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::contracts::Route;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformName {
    Macos,
    Windows,
    Linux,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Click,
    Drag,
    Scroll,
    PressKey,
    TypeText,
    SetValue,
    SelectText,
    PerformSecondaryAction,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressingMode {
    Window,
    Element,
    CapturedPoint,
    ObservedFocus,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Framework {
    AppKit,
    Chromium,
    Electron,
    WebKit,
    Catalyst,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowStateKind {
    Visible,
    Occluded,
    Minimized,
    OffSpace,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityKey {
    pub platform: PlatformName,
    pub os_version: String,
    pub action: ActionKind,
    pub addressing: AddressingMode,
    pub framework: Framework,
    pub window_state: WindowStateKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteDecision {
    Supported { route: Route },
    Unsupported { reason: String },
}

impl RouteDecision {
    pub fn route(&self) -> Option<Route> {
        match self {
            Self::Supported { route } => Some(*route),
            Self::Unsupported { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityCell {
    pub key: CapabilityKey,
    pub decision: RouteDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityManifest {
    pub platform: PlatformName,
    pub driver_version: String,
    pub protocol_version: String,
    #[serde(default)]
    pub permissions: BTreeMap<String, bool>,
    #[serde(default)]
    pub cells: Vec<CapabilityCell>,
}

#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    cells: BTreeMap<CapabilityKey, RouteDecision>,
}

impl CapabilityRegistry {
    pub fn from_cells(cells: impl IntoIterator<Item = CapabilityCell>) -> Self {
        let mut registry = Self::default();
        for cell in cells {
            assert!(
                registry.cells.insert(cell.key, cell.decision).is_none(),
                "platform capability source emitted a duplicate cell"
            );
        }
        registry
    }

    pub fn decision(&self, key: &CapabilityKey) -> RouteDecision {
        self.cells
            .get(key)
            .cloned()
            .unwrap_or_else(|| RouteDecision::Unsupported {
                reason: "capability cell has not been proven for background execution".to_owned(),
            })
    }

    pub fn cells(&self) -> impl Iterator<Item = CapabilityCell> + '_ {
        self.cells.iter().map(|(key, decision)| CapabilityCell {
            key: key.clone(),
            decision: decision.clone(),
        })
    }
}
