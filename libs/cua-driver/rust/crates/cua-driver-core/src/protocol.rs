//! MCP JSON-RPC 2.0 protocol types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{
    CheckReadinessRequest, ClickCommand, DragCommand, GetCapabilitiesRequest, GetWindowRequest,
    GetWindowStateRequest, LaunchAppRequest, ListAppsRequest, ListWindowsRequest, NativeError,
    PressKeyCommand, ScrollCommand, SecondaryActionCommand, SelectTextCommand, SetValueCommand,
    TypeTextCommand,
};

// ── Request ──────────────────────────────────────────────────────────────────

/// An incoming JSON-RPC 2.0 request or notification.
#[derive(Debug, Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// Absent on notifications.
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

impl Request {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    pub fn tool_call(&self) -> anyhow::Result<ToolCall> {
        let params = self
            .params
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing params"))?;
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing tool name"))?
            .to_owned();
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        Ok(ToolCall { name, args })
    }
}

pub struct ToolCall {
    pub name: String,
    pub args: Value,
}

// ── Background driver v2 ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V2ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

pub const V2_PROTOCOL_VERSION: V2ProtocolVersion = V2ProtocolVersion { major: 2, minor: 0 };

/// Canonical native method inventory. Keep this list as the single schema
/// comparison boundary for the Rust daemon and typed clients.
pub const V2_METHODS: &[&str] = &[
    "driver.v2.check_readiness",
    "driver.v2.get_capabilities",
    "driver.v2.list_apps",
    "driver.v2.launch_app",
    "driver.v2.list_windows",
    "driver.v2.get_window",
    "driver.v2.get_window_state",
    "driver.v2.click",
    "driver.v2.drag",
    "driver.v2.scroll",
    "driver.v2.press_key",
    "driver.v2.type_text",
    "driver.v2.set_value",
    "driver.v2.select_text",
    "driver.v2.perform_secondary_action",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", deny_unknown_fields)]
pub enum V2Command {
    #[serde(rename = "driver.v2.check_readiness")]
    CheckReadiness(CheckReadinessRequest),
    #[serde(rename = "driver.v2.get_capabilities")]
    GetCapabilities(GetCapabilitiesRequest),
    #[serde(rename = "driver.v2.list_apps")]
    ListApps(ListAppsRequest),
    #[serde(rename = "driver.v2.launch_app")]
    LaunchApp(LaunchAppRequest),
    #[serde(rename = "driver.v2.list_windows")]
    ListWindows(ListWindowsRequest),
    #[serde(rename = "driver.v2.get_window")]
    GetWindow(GetWindowRequest),
    #[serde(rename = "driver.v2.get_window_state")]
    GetWindowState(GetWindowStateRequest),
    #[serde(rename = "driver.v2.click")]
    Click(ClickCommand),
    #[serde(rename = "driver.v2.drag")]
    Drag(DragCommand),
    #[serde(rename = "driver.v2.scroll")]
    Scroll(ScrollCommand),
    #[serde(rename = "driver.v2.press_key")]
    PressKey(PressKeyCommand),
    #[serde(rename = "driver.v2.type_text")]
    TypeText(TypeTextCommand),
    #[serde(rename = "driver.v2.set_value")]
    SetValue(SetValueCommand),
    #[serde(rename = "driver.v2.select_text")]
    SelectText(SelectTextCommand),
    #[serde(rename = "driver.v2.perform_secondary_action")]
    PerformSecondaryAction(SecondaryActionCommand),
}

impl V2Command {
    pub fn method(&self) -> &'static str {
        match self {
            Self::CheckReadiness(_) => "driver.v2.check_readiness",
            Self::GetCapabilities(_) => "driver.v2.get_capabilities",
            Self::ListApps(_) => "driver.v2.list_apps",
            Self::LaunchApp(_) => "driver.v2.launch_app",
            Self::ListWindows(_) => "driver.v2.list_windows",
            Self::GetWindow(_) => "driver.v2.get_window",
            Self::GetWindowState(_) => "driver.v2.get_window_state",
            Self::Click(_) => "driver.v2.click",
            Self::Drag(_) => "driver.v2.drag",
            Self::Scroll(_) => "driver.v2.scroll",
            Self::PressKey(_) => "driver.v2.press_key",
            Self::TypeText(_) => "driver.v2.type_text",
            Self::SetValue(_) => "driver.v2.set_value",
            Self::SelectText(_) => "driver.v2.select_text",
            Self::PerformSecondaryAction(_) => "driver.v2.perform_secondary_action",
        }
    }

    /// Returns whether dispatch may change externally visible application
    /// state. These requests share one connection-wide lane so launch and
    /// action ordering stays deterministic while discovery and observation
    /// requests remain free to overlap.
    pub fn requires_serial_dispatch(&self) -> bool {
        matches!(
            self,
            Self::LaunchApp(_)
                | Self::Click(_)
                | Self::Drag(_)
                | Self::Scroll(_)
                | Self::PressKey(_)
                | Self::TypeText(_)
                | Self::SetValue(_)
                | Self::SelectText(_)
                | Self::PerformSecondaryAction(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V2RequestEnvelope {
    pub request_id: String,
    pub protocol_version: V2ProtocolVersion,
    #[serde(flatten)]
    pub command: V2Command,
}

impl V2RequestEnvelope {
    pub fn validate_version(&self) -> Result<(), NativeError> {
        if self.protocol_version != V2_PROTOCOL_VERSION {
            return Err(NativeError::new(
                crate::api::ErrorCode::ProtocolMismatch,
                crate::api::ErrorPhase::Validate,
                false,
                format!(
                    "unsupported v2 protocol {}.{}; driver requires {}.{}",
                    self.protocol_version.major,
                    self.protocol_version.minor,
                    V2_PROTOCOL_VERSION.major,
                    V2_PROTOCOL_VERSION.minor
                ),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V2HandshakeRequest {
    pub request_id: String,
    pub minimum_version: V2ProtocolVersion,
    pub maximum_version: V2ProtocolVersion,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V2HandshakeResponse {
    pub request_id: String,
    pub minimum_version: V2ProtocolVersion,
    pub maximum_version: V2ProtocolVersion,
    pub driver_name: String,
    pub driver_version: String,
    pub build: String,
    pub methods: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct V2ResponseEnvelope<T> {
    pub request_id: String,
    pub protocol_version: V2ProtocolVersion,
    #[serde(flatten)]
    pub body: V2ResponseBody<T>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum V2ResponseBody<T> {
    Result(V2Success<T>),
    Error(V2Failure),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V2Success<T> {
    pub result: T,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V2Failure {
    pub error: NativeError,
}

// ── Response ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Result { result: Value },
    Error { error: RpcError },
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            body: ResponseBody::Result { result },
        }
    }

    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            body: ResponseBody::Error {
                error: RpcError {
                    code,
                    message: message.into(),
                },
            },
        }
    }

    pub fn parse_error() -> Self {
        Self::error(Value::Null, -32700, "Parse error")
    }

    pub fn method_not_found(id: Value, method: &str) -> Self {
        Self::error(id, -32601, format!("Unknown method: {method}"))
    }
}

// ── Tool result content ───────────────────────────────────────────────────────

/// A single item in the `content` array of a `tools/call` result.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Content {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
    Image {
        data: String, // base64-encoded PNG
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text {
            text: text.into(),
            annotations: None,
        }
    }

    pub fn image_png(data_base64: String) -> Self {
        Content::Image {
            data: data_base64,
            mime_type: "image/png".into(),
            annotations: None,
        }
    }

    pub fn image_jpeg(data_base64: String) -> Self {
        Content::Image {
            data: data_base64,
            mime_type: "image/jpeg".into(),
            annotations: None,
        }
    }
}

/// The value placed in `result` for a `tools/call` response.
#[derive(Debug, Serialize, Default)]
pub struct ToolResult {
    pub content: Vec<Content>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl ToolResult {
    pub fn text(msg: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(msg)],
            ..Default::default()
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(msg)],
            is_error: Some(true),
            ..Default::default()
        }
    }

    pub fn with_structured(mut self, v: Value) -> Self {
        self.structured_content = Some(v);
        self
    }
}

// ── Initialize result ─────────────────────────────────────────────────────────

pub fn initialize_result() -> Value {
    serde_json::json!({
        "protocolVersion": "2025-06-18",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "cua-driver", "version": env!("CARGO_PKG_VERSION") },
        "instructions": agent_instructions()
    })
}

/// MCP `instructions` (`InitializeResult.instructions`) sent to every
/// connecting client. The spec frames this as a "hint... MAY be added
/// to the system prompt" — eager, every-turn cost. We keep it under
/// the community-recommended ~200-word ceiling and host the long-form
/// workflow in `Skills/cua-driver/SKILL.md`.
///
/// Templated per-host: the accessibility-tree provider name (AX on
/// macOS, UIA on Windows, AT-SPI on Linux) is injected so a connecting
/// agent only sees the path that applies, not all three. Same pattern
/// Goose uses in its `ComputerController` extension and Open
/// Interpreter uses for its system message.
fn agent_instructions() -> String {
    let (tree_kind, platform_skill_pointer) = if cfg!(target_os = "macos") {
        (
            "AX (Accessibility)",
            "MACOS.md (no-foreground contract, AXMenuBar navigation, SkyLight click dispatch)",
        )
    } else if cfg!(target_os = "windows") {
        (
            "UIA (UI Automation)",
            "WINDOWS.md (UIA tree, UWP / ApplicationFrameHost hosting, Session 0 isolation)",
        )
    } else {
        (
            "AT-SPI",
            "LINUX.md (X11/Wayland status, AT-SPI bus, BETA-level support)",
        )
    };

    format!(
        r#"cua-driver: cross-platform background computer-use automation.

Tools let you interact with any app without stealing keyboard focus or moving the visible cursor. Prefer element_index ({tree_kind}) paths over pixel coordinates — they work on backgrounded/hidden windows.

Workflow per turn:
0. start_session(session) once at the start of a run → declares THIS run's identity (a stable id you choose, e.g. "research-1"). Pass that same `session` on every action below. It owns your agent cursor (a distinct color per id) and follows the run across apps/windows. End with end_session(session) when done. Concurrent runs/subagents each use their OWN `session`. (Omitting `session` still works, just with no cursor.)
1. launch_app  → idempotent, returns pid + windows array in one call. Pass creates_new_application_instance:true if another run may touch the same app, so you get your own window.
2. (skip list_windows when launch_app already returned a single window)
3. get_window_state(pid, window_id) → refresh the {tree_kind} snapshot, get element indices
4. click/type_text/press_key using element_index from step 3 (+ your `session`)
5. get_window_state(pid, window_id) again → verify the action landed

Agent cursor: a per-SESSION overlay cursor visualises where a run is acting without moving the real pointer. It is shown only for a DECLARED session (pass `session`), is color-coded by the session id, and is removed by end_session or the idle-TTL. The same id over MCP, the CLI, or the raw socket drives the same cursor. set_agent_cursor_* tools hide/show/customise it. Note: a pure accessibility-action (element_index) click snaps the cursor with a brief pulse on its first action rather than a long glide, so it can be easy to miss — issue a pixel click or move_cursor first for a visibly gliding demo/recording.

If a `cua-driver` skill is loaded in your harness (Claude Code / Codex / OpenClaw / OpenCode dirs), prefer its detailed workflow — SKILL.md plus {platform_skill_pointer}. Install with `cua-driver skills install` if not yet present."#
    )
}

#[cfg(test)]
mod image_mime_type_tests {
    use super::Content;

    /// Surface 7 contract: image parts serialize an explicit `mimeType` field
    /// (not `mime_type`, not `format`) so MCP consumers don't have to sniff
    /// magic bytes off the base64 PNG/JPEG to know what they're holding.
    /// This locks in the JSON shape — any rename or drop breaks here first.
    #[test]
    fn image_png_serializes_with_explicit_mime_type() {
        let c = Content::image_png("ZmFrZQ==".into());
        let v = serde_json::to_value(&c).expect("serialize");
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("image"));
        assert_eq!(
            v.get("mimeType").and_then(|t| t.as_str()),
            Some("image/png")
        );
        assert_eq!(v.get("data").and_then(|t| t.as_str()), Some("ZmFrZQ=="));
    }

    #[test]
    fn image_jpeg_serializes_with_explicit_mime_type() {
        let c = Content::image_jpeg("ZmFrZQ==".into());
        let v = serde_json::to_value(&c).expect("serialize");
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("image"));
        assert_eq!(
            v.get("mimeType").and_then(|t| t.as_str()),
            Some("image/jpeg")
        );
    }

    /// Text parts must not grow a mimeType field — the field belongs to
    /// image parts only. Guards against an accidental serde_json renamer
    /// that emits a shared shape across the enum.
    #[test]
    fn text_does_not_carry_mime_type() {
        let c = Content::text("hi");
        let v = serde_json::to_value(&c).expect("serialize");
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("text"));
        assert!(
            v.get("mimeType").is_none(),
            "text content must not carry mimeType"
        );
    }
}
