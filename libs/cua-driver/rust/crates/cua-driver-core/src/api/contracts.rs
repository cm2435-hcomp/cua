//! Stable serde contracts for the background-only v2 API.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{interaction::NativeEvidence, menu::MenuState, settlement::SettlementEvidence};

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            pub fn parse(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(concat!(stringify!($name), " cannot be empty").to_owned());
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(ClientId);
opaque_id!(AppId);
opaque_id!(WindowId);
opaque_id!(ObservationId);
opaque_id!(SurfaceId);
opaque_id!(ElementId);
opaque_id!(ActionId);
opaque_id!(AxRevision);
opaque_id!(MenuId);
opaque_id!(MenuRevision);
opaque_id!(GeometryRevision);
opaque_id!(CaptureRevision);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowGeneration(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppSelector {
    Name {
        name: String,
    },
    BundleId {
        bundle_id: String,
    },
    Executable {
        path: String,
        #[serde(default)]
        arguments: Vec<String>,
    },
}

impl AppSelector {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Name { name } => validate_non_empty("app name", name),
            Self::BundleId { bundle_id } => validate_non_empty("bundle_id", bundle_id),
            Self::Executable { path, .. } => validate_non_empty("executable path", path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppRef {
    pub id: AppId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowRef {
    pub id: WindowId,
    pub app: AppRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn validate(&self) -> Result<(), String> {
        if self.x.is_finite() && self.y.is_finite() {
            Ok(())
        } else {
            Err("point coordinates must be finite".to_owned())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub fn validate(&self) -> Result<(), String> {
        if self.width == 0 || self.height == 0 {
            Err("size width and height must be nonzero".to_owned())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn validate(&self) -> Result<(), String> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
        {
            return Err("rectangle geometry must be finite".to_owned());
        }
        if self.width <= 0.0 || self.height <= 0.0 {
            return Err("rectangle width and height must be positive".to_owned());
        }
        Ok(())
    }

    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + self.width
            && point.y < self.y + self.height
    }

    pub fn center(&self) -> Point {
        Point {
            x: self.x + self.width / 2.0,
            y: self.y + self.height / 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    Window,
    Menu,
    Popover,
    Sheet,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSurface {
    pub id: SurfaceId,
    pub owner_window: WindowRef,
    pub kind: SurfaceKind,
    pub image_url: String,
    pub size: Size,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_bounds: Option<Rect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElementRef {
    pub observation_id: ObservationId,
    pub id: ElementId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessibilityElement {
    #[serde(rename = "ref")]
    pub element_ref: ElementRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
    #[serde(default)]
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceAxLines {
    pub start_line: usize,
    pub delete_count: usize,
    #[serde(default)]
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AxTreeUpdate {
    Full {
        revision: AxRevision,
        tree: String,
    },
    Diff {
        base_revision: AxRevision,
        revision: AxRevision,
        operations: Vec<ReplaceAxLines>,
    },
}

impl AxTreeUpdate {
    pub fn revision(&self) -> &AxRevision {
        match self {
            Self::Full { revision, .. } | Self::Diff { revision, .. } => revision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessibilityState {
    pub tree_update: AxTreeUpdate,
    pub elements: Vec<AccessibilityElement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focused_element: Option<ElementRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_text: Option<String>,
    #[serde(default)]
    pub selected_elements: Vec<ElementRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowState {
    pub observation_id: ObservationId,
    pub window: WindowRef,
    pub surfaces: Vec<CapturedSurface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility: Option<AccessibilityState>,
    pub menu: MenuState,
    pub settlement: SettlementEvidence,
    pub captured_at_unix_ms: u64,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Shift,
    Control,
    Alt,
    Meta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClickTarget {
    Point {
        observation_id: ObservationId,
        surface_id: SurfaceId,
        point: Point,
    },
    Element {
        element: ElementRef,
    },
}

impl ClickTarget {
    pub fn observation_id(&self) -> &ObservationId {
        match self {
            Self::Point { observation_id, .. } => observation_id,
            Self::Element { element } => &element.observation_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClickRequest {
    pub target: ClickTarget,
    #[serde(default)]
    pub button: MouseButton,
    #[serde(default = "default_click_count")]
    pub click_count: u8,
    #[serde(default)]
    pub modifiers: Vec<Modifier>,
}

fn default_click_count() -> u8 {
    1
}

impl ClickRequest {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=3).contains(&self.click_count) {
            return Err("click_count must be between 1 and 3".to_owned());
        }
        if let ClickTarget::Point { point, .. } = &self.target {
            point.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DragRequest {
    pub observation_id: ObservationId,
    pub surface_id: SurfaceId,
    pub start: Point,
    pub end: Point,
    #[serde(default)]
    pub button: MouseButton,
    #[serde(default)]
    pub modifiers: Vec<Modifier>,
    #[serde(default = "default_drag_duration_ms")]
    pub duration_ms: u32,
}

fn default_drag_duration_ms() -> u32 {
    300
}

impl DragRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.duration_ms > 10_000 {
            return Err("duration_ms must be at most 10000".to_owned());
        }
        self.start.validate()?;
        self.end.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScrollRequest {
    Delta {
        observation_id: ObservationId,
        surface_id: SurfaceId,
        point: Point,
        #[serde(default)]
        delta_x: f64,
        #[serde(default)]
        delta_y: f64,
    },
    Element {
        element: ElementRef,
        direction: ScrollDirection,
        #[serde(default = "default_pages")]
        pages: f64,
    },
}

fn default_pages() -> f64 {
    1.0
}

impl ScrollRequest {
    pub fn observation_id(&self) -> &ObservationId {
        match self {
            Self::Delta { observation_id, .. } => observation_id,
            Self::Element { element, .. } => &element.observation_id,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Delta {
                point,
                delta_x,
                delta_y,
                ..
            } => {
                point.validate()?;
                if !delta_x.is_finite() || !delta_y.is_finite() {
                    return Err("delta_x and delta_y must be finite".to_owned());
                }
                if *delta_x == 0.0 && *delta_y == 0.0 {
                    return Err("delta_x and delta_y cannot both be zero".to_owned());
                }
                Ok(())
            }
            Self::Element { pages, .. } if !pages.is_finite() || *pages <= 0.0 => {
                Err("pages must be positive and finite".to_owned())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyStroke {
    pub key: String,
    #[serde(default)]
    pub modifiers: Vec<Modifier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PressKeyRequest {
    pub observation_id: ObservationId,
    pub stroke: KeyStroke,
}

impl PressKeyRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_non_empty("key", &self.stroke.key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeTextRequest {
    pub observation_id: ObservationId,
    pub text: String,
}

impl TypeTextRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.text.is_empty() {
            Err("text cannot be empty".to_owned())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetValueRequest {
    pub element: ElementRef,
    pub value: String,
}

impl SetValueRequest {
    /// Empty values are intentionally valid because they clear a control.
    pub fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionType {
    #[default]
    Text,
    CursorBefore,
    CursorAfter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectTextRequest {
    pub element: ElementRef,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    #[serde(default)]
    pub selection_type: SelectionType,
}

impl SelectTextRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.text.is_empty() {
            return Err("selection text/anchor cannot be empty".to_owned());
        }
        if self.prefix.as_ref().is_some_and(String::is_empty)
            || self.suffix.as_ref().is_some_and(String::is_empty)
        {
            return Err("selection prefix/suffix cannot be an empty string".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecondaryActionRequest {
    pub element: ElementRef,
    pub action: String,
}

impl SecondaryActionRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_non_empty("secondary action", &self.action)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Semantic,
    TargetedPointer,
    TargetedKeyboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationLevel {
    EffectVerified,
    DispatchVerified,
    DispatchUnverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectVerification {
    EffectVerified,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionReceipt {
    pub action_id: ActionId,
    pub window: WindowRef,
    pub consumed_observation_id: ObservationId,
    pub route: Route,
    pub verification: VerificationLevel,
    pub settlement: SettlementEvidence,
    #[serde(default)]
    pub native_evidence: NativeEvidence,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchResult {
    pub action_id: ActionId,
    pub app: AppRef,
    pub windows: Vec<WindowRef>,
    pub reused_running_app: bool,
    pub verification: EffectVerification,
    pub settlement: SettlementEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Granted,
    Denied,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    pub ready: bool,
    #[serde(default)]
    pub permissions: BTreeMap<String, PermissionState>,
    #[serde(default)]
    pub diagnostics: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AxTreeMode {
    #[default]
    DiffIfAvailable,
    Full,
}

macro_rules! empty_request {
    ($name:ident) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {}
    };
}

empty_request!(CheckReadinessRequest);
empty_request!(GetCapabilitiesRequest);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListAppsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<AppQuery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchAppRequest {
    pub app: AppSelector,
}

impl LaunchAppRequest {
    pub fn validate(&self) -> Result<(), String> {
        self.app.validate()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListWindowsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetWindowRequest {
    pub window_id: WindowId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetWindowStateRequest {
    pub window: WindowRef,
    #[serde(default = "default_true")]
    pub include_text: bool,
    #[serde(default = "default_true")]
    pub include_screenshots: bool,
    #[serde(default)]
    pub ax_tree_mode: AxTreeMode,
}

fn default_true() -> bool {
    true
}

fn validate_non_empty(label: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{label} cannot be empty"))
    } else {
        Ok(())
    }
}

macro_rules! action_command {
    ($name:ident, $request:ty) => {
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            pub window: WindowRef,
            pub request: $request,
        }
    };
}

action_command!(ClickCommand, ClickRequest);
action_command!(DragCommand, DragRequest);
action_command!(ScrollCommand, ScrollRequest);
action_command!(PressKeyCommand, PressKeyRequest);
action_command!(TypeTextCommand, TypeTextRequest);
action_command!(SetValueCommand, SetValueRequest);
action_command!(SelectTextCommand, SelectTextRequest);
action_command!(SecondaryActionCommand, SecondaryActionRequest);
