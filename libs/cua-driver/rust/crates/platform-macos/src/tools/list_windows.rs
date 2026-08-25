use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;

pub struct ListWindowsTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "list_windows".into(),
        description: "List all layer-0 top-level windows currently known to WindowServer. \
            Includes off-screen windows (minimized, on another Space, hidden-launched). \
            Use this to find a window_id before calling get_window_state.\n\n\
            Per-record fields: window_id, pid, app_name, title, bounds \
            (x/y/width/height, top-left origin), z_index (integer or null; higher values are \
            closer to the front; null means stacking order is unavailable and callers must not \
            infer one), is_on_screen, space_ids, current_space_id (the active Space on that \
            window's display), on_current_space, plus exact AX minimized, \
            is_standard, is_main, and subrole facts when the CGWindowID maps \
            uniquely to one AX window. A minimized AXDialog is standard only \
            while minimized, matching macOS's Calculator representation. The top-level current_space_id is \
            WindowServer's main/global active Space and can differ from a record's \
            current_space_id when displays use independent Spaces. To select a frontmost candidate, take the \
            maximum integer z_index; if every value is null, use an explicit fallback instead of \
            relying on array order.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "pid": {
                    "type": "integer",
                    "description": "Optional pid filter. When set, only this pid's windows are returned."
                },
                "on_screen_only": {
                    "type": "boolean",
                    "description": "When true, drop windows not on the current Space. Default false."
                }
            },
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for ListWindowsTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid_filter: Option<i32> = args.opt_i64("pid").map(|v| v as i32);
        let on_screen_only = args.bool_or("on_screen_only", false);

        let enumeration = if on_screen_only {
            crate::windows::visible_windows_with_space_snapshot()
        } else {
            crate::windows::all_windows_with_space_snapshot()
        };
        let current_space_id = enumeration.current_space_id;
        let mut windows = enumeration.windows;

        if let Some(pid) = pid_filter {
            windows.retain(|w| w.pid == pid);
        }

        let pids: std::collections::HashSet<i32> =
            windows.iter().map(|window| window.pid).collect();
        let ax_facts: std::collections::HashMap<
            (i32, u32),
            crate::ax::minimized_recovery::ExactAxWindowFacts,
        > = pids
            .into_iter()
            .flat_map(|pid| {
                crate::ax::minimized_recovery::ax_window_facts_for_pid(pid)
                    .into_iter()
                    .map(move |(window_id, facts)| ((pid, window_id), facts))
            })
            .collect();
        let windows_json: Vec<Value> = windows
            .iter()
            .map(|window| {
                window_record_json_with_ax_facts(
                    window,
                    ax_facts.get(&(window.pid, window.window_id)),
                )
            })
            .collect();

        ToolResult::text(format!("Found {} window(s).", windows_json.len())).with_structured(
            serde_json::json!({
                "windows": windows_json,
                "current_space_id": current_space_id
            }),
        )
    }
}

pub(super) fn window_record_json(w: &crate::windows::WindowInfo) -> Value {
    window_record_json_with_ax_facts(w, None)
}

fn window_record_json_with_ax_facts(
    w: &crate::windows::WindowInfo,
    facts: Option<&crate::ax::minimized_recovery::ExactAxWindowFacts>,
) -> Value {
    let minimized = facts.and_then(|facts| facts.minimized);
    let subrole = facts.and_then(|facts| facts.subrole.as_deref());
    let is_main = facts.and_then(|facts| facts.is_main);
    let is_standard = facts.map(|facts| facts.is_recoverable_standard());
    serde_json::json!({
        "window_id": w.window_id,
        "pid": w.pid,
        "app_name": w.app_name,
        "title": w.title,
        "bounds": {
            "x": w.bounds.x,
            "y": w.bounds.y,
            "width": w.bounds.width,
            "height": w.bounds.height
        },
        "layer": w.layer,
        "z_index": w.z_index,
        "is_on_screen": w.is_on_screen,
        "current_space_id": w.current_space_id,
        "on_current_space": w.on_current_space,
        "space_ids": w.space_ids,
        "minimized": minimized,
        "subrole": subrole,
        "is_standard": is_standard,
        "is_main": is_main,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_record_includes_observed_z_index() {
        let window = crate::windows::WindowInfo {
            window_id: 42,
            pid: 123,
            app_name: "Example".into(),
            title: "Document".into(),
            bounds: crate::windows::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            layer: 0,
            z_index: 7,
            is_on_screen: true,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        };

        assert_eq!(window_record_json(&window)["z_index"], serde_json::json!(7));
        assert_eq!(
            window_record_json(&window)["current_space_id"],
            serde_json::json!(1)
        );
        assert_eq!(
            window_record_json(&window)["on_current_space"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn minimized_dialog_is_exposed_as_recoverable_standard_window() {
        let window = crate::windows::WindowInfo {
            window_id: 42,
            pid: 123,
            app_name: "Calculator".into(),
            title: "Calculator".into(),
            bounds: crate::windows::WindowBounds {
                x: 1.0,
                y: 2.0,
                width: 300.0,
                height: 200.0,
            },
            layer: 0,
            z_index: 7,
            is_on_screen: false,
            current_space_id: Some(1),
            on_current_space: Some(true),
            space_ids: Some(vec![1]),
        };
        let facts = crate::ax::minimized_recovery::ExactAxWindowFacts {
            minimized: Some(true),
            subrole: Some("AXDialog".into()),
            is_main: Some(false),
        };

        let record = window_record_json_with_ax_facts(&window, Some(&facts));
        assert_eq!(record["minimized"], true);
        assert_eq!(record["is_standard"], true);
        assert_eq!(record["subrole"], "AXDialog");
    }
}
