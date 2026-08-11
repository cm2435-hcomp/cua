use super::*;
use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

#[tokio::test]
async fn same_pid_lease_refuses_immediately_through_the_first_action_lifecycle() {
    const PID: i32 = -92_101;
    let events = Arc::new(Mutex::new(Vec::new()));
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
    let (finish_first_tx, finish_first_rx) = tokio::sync::oneshot::channel();

    let first_events = events.clone();
    let first = tokio::spawn(async move {
        let _lease = acquire_background_mutation(PID).unwrap();
        first_events
            .lock()
            .unwrap()
            .extend(["first:proof", "first:dispatch"]);
        first_started_tx.send(()).unwrap();
        finish_first_rx.await.unwrap();
        first_events
            .lock()
            .unwrap()
            .extend(["first:restore", "first:verify"]);
    });
    first_started_rx.await.unwrap();

    let refusal = acquire_background_mutation(PID).unwrap_err();
    assert_eq!(refusal.is_error, Some(true));
    let structured = refusal.structured_content.unwrap();
    assert_eq!(structured["code"], "target_busy");
    assert_eq!(structured["effect"], "refused");
    assert_eq!(structured["native_side_effect_started"], false);
    finish_first_tx.send(()).unwrap();
    first.await.unwrap();
    acquire_background_mutation(PID).unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "first:proof",
            "first:dispatch",
            "first:restore",
            "first:verify"
        ]
    );
}

#[tokio::test]
async fn different_pid_lease_enters_while_first_mutation_is_in_flight() {
    const FIRST_PID: i32 = -92_102;
    const SECOND_PID: i32 = -92_103;
    let first = acquire_background_mutation(FIRST_PID).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();

    let second = tokio::spawn(async move {
        let _lease = acquire_background_mutation(SECOND_PID).unwrap();
        entered_tx.send(()).unwrap();
    });

    tokio::time::timeout(Duration::from_secs(1), entered_rx)
        .await
        .expect("a different pid must not wait for the in-flight mutation")
        .unwrap();
    drop(first);
    second.await.unwrap();
}

struct KeyboardProbe {
    calls: Arc<AtomicUsize>,
}

static KEYBOARD_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for KeyboardProbe {
    fn def(&self) -> &ToolDef {
        KEYBOARD_DEF.get_or_init(|| ToolDef {
            name: "press_key".into(),
            description: "keyboard ambiguity regression probe".into(),
            input_schema: serde_json::json!({"type": "object"}),
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: false,
        })
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ToolResult::text("keyboard actuator invoked")
    }
}

fn candidate(window_id: u64) -> WindowTargetCandidate {
    WindowTargetCandidate {
        window_id,
        title: format!("Document {window_id}"),
        app_name: Some("Editor".into()),
        is_on_screen: true,
    }
}

#[tokio::test]
async fn pid_only_keyboard_refuses_same_pid_multi_window_ambiguity_before_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let candidates: WindowTargetCandidates = Arc::new(|pid| {
        (pid == 42)
            .then(|| vec![candidate(7), candidate(8)])
            .unwrap_or_default()
    });
    let keyboard = pid_window_guarded(
        KeyboardProbe {
            calls: calls.clone(),
        },
        &candidates,
    );

    let result = keyboard
        .invoke(serde_json::json!({"pid": 42, "key": "return"}))
        .await;

    assert_eq!(calls.load(Ordering::SeqCst), 0, "no key may be posted");
    assert_eq!(result.is_error, Some(true));
    let refusal = result.structured_content.unwrap();
    assert_eq!(refusal["code"], "ambiguous_window_target");
    assert_eq!(refusal["effect"], "refused");
    assert_eq!(refusal["candidates"][0]["window_id"], 7);
    assert_eq!(refusal["candidates"][1]["window_id"], 8);
}
