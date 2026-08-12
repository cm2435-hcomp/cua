//! Exact background text selection through `AXSelectedTextRange`.
//!
//! The public request names text plus optional left/right context rather than
//! exposing UTF-16 offsets. The tool resolves one exact occurrence from the
//! retained element's live `AXValue`, writes the corresponding CFRange, and
//! reads both the range and selected text back before publishing confirmation.

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::Arc;

use crate::ax::bindings::{
    copy_cf_range_attr, copy_string_attr, is_attribute_settable, kAXErrorSuccess,
    set_cf_range_attr, AXUIElementRef, AxCfRange,
};

use super::ToolState;

pub struct SelectTextTool {
    state: Arc<ToolState>,
}

impl SelectTextTool {
    pub fn new(state: Arc<ToolState>) -> Self {
        Self { state }
    }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "select_text".into(),
        description: "Select one exact occurrence of text in a macOS accessibility text element, or place the cursor immediately before or after it. Optional prefix and suffix context disambiguate repeated text. The operation is window-targeted, background-only, UTF-16 correct, and confirmed only by exact AX range/text read-back."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "text"],
            "properties": {
                "session": { "type": "string", "description": "Optional session id used for driver-owned per-session state." },
                "pid": { "type": "integer" },
                "window_id": {
                    "type": "integer",
                    "description": "CGWindowID for the window whose snapshot produced the element. Required with element_index; optional with element_token."
                },
                "element_index": cua_driver_core::tool_schema::element_index_schema(),
                "element_token": cua_driver_core::tool_schema::element_token_schema(),
                "snapshot_id": cua_driver_core::tool_schema::snapshot_id_schema(),
                "text": { "type": "string", "minLength": 1 },
                "prefix": { "type": "string", "minLength": 1 },
                "suffix": { "type": "string", "minLength": 1 },
                "selection_type": {
                    "type": "string",
                    "enum": ["text", "cursor_before", "cursor_after"],
                    "default": "text"
                }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: true,
    })
}

#[derive(Clone, Copy)]
enum SelectionType {
    Text,
    CursorBefore,
    CursorAfter,
}

#[derive(Clone)]
struct SelectionRequest {
    text: String,
    prefix: Option<String>,
    suffix: Option<String>,
    selection_type: SelectionType,
}

#[async_trait]
impl Tool for SelectTextTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;

        let pid = match args.require_i32("pid") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let text = match args.require_str("text") {
            Ok(value) if !value.is_empty() => value,
            Ok(_) => return ToolResult::error("select_text requires non-empty text"),
            Err(error) => return error,
        };
        let prefix = args.opt_str("prefix");
        let suffix = args.opt_str("suffix");
        if prefix.as_ref().is_some_and(String::is_empty)
            || suffix.as_ref().is_some_and(String::is_empty)
        {
            return ToolResult::error("select_text prefix/suffix cannot be empty strings");
        }
        let selection_type = match args.opt_str("selection_type").as_deref() {
            None | Some("text") => SelectionType::Text,
            Some("cursor_before") => SelectionType::CursorBefore,
            Some("cursor_after") => SelectionType::CursorAfter,
            Some(other) => {
                return ToolResult::error(format!(
                    "select_text selection_type must be text, cursor_before, or cursor_after; got {other:?}"
                ))
            }
        };
        let request = SelectionRequest {
            text,
            prefix,
            suffix,
            selection_type,
        };

        let element_token = args.opt_str("element_token");
        let window_id_arg = args.opt_u64("window_id").map(|value| value as u32);
        let element_index_arg = args.opt_u64("element_index").map(|value| value as usize);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
            pid,
            element_index_arg,
            element_token.as_deref(),
            args.opt_str("snapshot_id").as_deref(),
            window_id_arg,
            "select_text",
        ) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let (element_index, window_id) = match resolved {
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: Some(window_id),
                element_index,
                ..
            } => (element_index, window_id),
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: None, ..
            } => {
                return ToolResult::error(
                    "select_text requires window_id when element_index is used",
                )
            }
            cua_driver_core::element_token::ResolvedElement::None => {
                return ToolResult::error(
                    "select_text requires element_index (+ window_id) or element_token",
                )
            }
        };

        let element_guard =
            match self
                .state
                .element_cache
                .get_element_retained(pid, window_id, element_index)
            {
                Some(element) => element,
                None => {
                    return ToolResult::error(format!(
                        "Element index {element_index} not found. Call get_window_state first."
                    ))
                }
            };
        let element_ptr = element_guard.as_ptr();

        let _mutation_lease =
            match super::gate_target_local_ax_action(pid, window_id, element_ptr).await {
                Ok(lease) => lease,
                Err(refusal) => return refusal,
            };

        let pointer = element_ptr;
        let outcome = tokio::task::spawn_blocking(move || {
            select_text_blocking(pointer as AXUIElementRef, &request)
        })
        .await;

        match outcome {
            Ok(Ok(SelectionOutcome::Confirmed { range })) => ToolResult::text(format!(
                "Selected text on [{element_index}] at UTF-16 range ({}, {}).",
                range.location, range.length
            ))
            .with_structured(serde_json::json!({
                "path": "ax",
                "verified": true,
                "effect": "confirmed",
                "dispatch_scope": "target",
                "utf16_range": {"location": range.location, "length": range.length},
            })),
            Ok(Ok(SelectionOutcome::Unverifiable { range, detail })) => ToolResult::text(detail)
                .with_structured(serde_json::json!({
                    "path": "ax",
                    "verified": false,
                    "effect": "unverifiable",
                    "dispatch_scope": "target",
                    "utf16_range": {"location": range.location, "length": range.length},
                })),
            Ok(Err(SelectionFailure::Refused(detail))) => ToolResult::error(detail)
                .with_structured(
                    serde_json::json!({"effect": "refused", "dispatch_scope": "target"}),
                ),
            Ok(Err(SelectionFailure::AfterDispatch(detail))) => ToolResult::error(detail)
                .with_structured(serde_json::json!({
                    "path": "ax",
                    "effect": "unverifiable",
                    "dispatch_scope": "target",
                })),
            Err(error) => ToolResult::error(format!(
                "select_text worker failed after native dispatch became possible: {error}"
            ))
            .with_structured(serde_json::json!({
                "path": "ax",
                "effect": "unverifiable",
                "dispatch_scope": "target",
            })),
        }
    }
}

enum SelectionOutcome {
    Confirmed { range: AxCfRange },
    Unverifiable { range: AxCfRange, detail: String },
}

enum SelectionFailure {
    Refused(String),
    AfterDispatch(String),
}

fn select_text_blocking(
    element: AXUIElementRef,
    request: &SelectionRequest,
) -> Result<SelectionOutcome, SelectionFailure> {
    if !unsafe { is_attribute_settable(element, "AXSelectedTextRange") } {
        return Err(SelectionFailure::Refused(
            "select_text target has no writable AXSelectedTextRange".into(),
        ));
    }
    let document = unsafe { copy_string_attr(element, "AXValue") }.ok_or_else(|| {
        SelectionFailure::Refused(
            "select_text requires an exact string AXValue on the target element".into(),
        )
    })?;
    let range = resolve_selection_range(&document, request).map_err(SelectionFailure::Refused)?;
    let expected_text = match request.selection_type {
        SelectionType::Text => request.text.as_str(),
        SelectionType::CursorBefore | SelectionType::CursorAfter => "",
    };

    let error = unsafe { set_cf_range_attr(element, "AXSelectedTextRange", range) };
    if error != kAXErrorSuccess {
        return Err(SelectionFailure::AfterDispatch(format!(
            "AXSelectedTextRange write returned error {error}"
        )));
    }

    let range_readback = unsafe { copy_cf_range_attr(element, "AXSelectedTextRange") };
    let text_readback = unsafe { copy_string_attr(element, "AXSelectedText") };
    match range_readback {
        Ok(Some(observed)) if observed == range && text_readback.as_deref() == Some(expected_text) => {
            Ok(SelectionOutcome::Confirmed { range })
        }
        Ok(observed) => Ok(SelectionOutcome::Unverifiable {
            range,
            detail: format!(
                "AXSelectedTextRange write completed but exact read-back did not match: range={observed:?}, text={text_readback:?}"
            ),
        }),
        Err(error) => Ok(SelectionOutcome::Unverifiable {
            range,
            detail: format!(
                "AXSelectedTextRange write completed but range read-back failed with error {error}"
            ),
        }),
    }
}

fn resolve_selection_range(
    document: &str,
    request: &SelectionRequest,
) -> Result<AxCfRange, String> {
    let matches: Vec<_> = document
        .char_indices()
        .filter_map(|(start, _)| {
            let tail = &document[start..];
            if !tail.starts_with(&request.text) {
                return None;
            }
            let end = start + request.text.len();
            let prefix_matches = request
                .prefix
                .as_ref()
                .is_none_or(|prefix| document[..start].ends_with(prefix));
            let suffix_matches = request
                .suffix
                .as_ref()
                .is_none_or(|suffix| document[end..].starts_with(suffix));
            (prefix_matches && suffix_matches).then_some((start, end))
        })
        .collect();
    let (start, end) = match matches.as_slice() {
        [] => return Err("requested text/context is absent from the exact AXValue".into()),
        [only] => *only,
        _ => return Err("requested text/context is ambiguous in the exact AXValue".into()),
    };
    let utf16_start = document[..start].encode_utf16().count();
    let utf16_end = document[..end].encode_utf16().count();
    let (location, length) = match request.selection_type {
        SelectionType::Text => (utf16_start, utf16_end - utf16_start),
        SelectionType::CursorBefore => (utf16_start, 0),
        SelectionType::CursorAfter => (utf16_end, 0),
    };
    AxCfRange::from_utf16(location, length)
        .ok_or_else(|| "requested UTF-16 selection exceeds macOS CFRange limits".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_utf16_exact_and_context_disambiguates() {
        let request = SelectionRequest {
            text: "TARGET".into(),
            prefix: Some("🙂 ".into()),
            suffix: Some(" end".into()),
            selection_type: SelectionType::Text,
        };
        let range = resolve_selection_range("TARGET first; 🙂 TARGET end", &request).unwrap();
        assert_eq!(range.location, 17);
        assert_eq!(range.length, 6);
    }

    #[test]
    fn cursor_after_uses_the_end_of_the_matched_utf16_range() {
        let request = SelectionRequest {
            text: "🙂".into(),
            prefix: None,
            suffix: None,
            selection_type: SelectionType::CursorAfter,
        };
        assert_eq!(
            resolve_selection_range("a🙂b", &request).unwrap(),
            AxCfRange {
                location: 3,
                length: 0,
            }
        );
    }
}
