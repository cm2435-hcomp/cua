//! Exact, focus-preserving recovery of a minimized current-Space window.
//!
//! The signed macOS helper writes `AXMinimized=false` on the addressed AX
//! window, then waits for exact AX and WindowServer readback. This module keeps
//! that effect behind the caller's window claim and never activates the target.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use core_foundation::base::{CFRelease, CFTypeRef};
use serde_json::{json, Value};

use super::bindings::{
    ax_get_window_id, copy_ax_windows, copy_bool_attr, copy_string_attr, is_attribute_settable,
    kAXErrorSuccess, set_bool_attr, AXUIElementCreateApplication, AXUIElementRef,
};

const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
const READBACK_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactAxWindowFacts {
    pub minimized: Option<bool>,
    pub subrole: Option<String>,
    pub is_main: Option<bool>,
}

impl ExactAxWindowFacts {
    pub fn is_recoverable_standard(&self) -> bool {
        matches!(self.subrole.as_deref(), Some("AXStandardWindow"))
            || (self.minimized == Some(true) && matches!(self.subrole.as_deref(), Some("AXDialog")))
    }
}

#[derive(Debug, Clone)]
pub struct RecoveryEvidence {
    pub pid: i32,
    pub window_id: u32,
    pub frontmost_pid_before: Option<i32>,
    pub frontmost_pid_after: Option<i32>,
    pub target_activation_observed: bool,
}

impl RecoveryEvidence {
    pub fn structured(&self) -> Value {
        json!({
            "performed": true,
            "pid": self.pid,
            "window_id": self.window_id,
            "minimized_before_restore": true,
            "minimized_after_restore": false,
            "frontmost_pid_before": self.frontmost_pid_before,
            "frontmost_pid_after": self.frontmost_pid_after,
            "target_activation_observed": self.target_activation_observed,
        })
    }

    pub fn decorate_error(&self, mut structured: Value) -> Value {
        let object = structured
            .as_object_mut()
            .expect("recovery error details are always a JSON object");
        object.insert("effect_may_have_occurred".into(), true.into());
        object.insert("native_side_effect_started".into(), true.into());
        object.insert("minimized_recovery".into(), self.structured());
        structured
    }
}

#[derive(Debug)]
pub struct RecoveryFailure {
    pub code: &'static str,
    pub phase: &'static str,
    pub message: String,
    pub retryable: bool,
    pub effect_may_have_occurred: bool,
    pub details: Value,
}

impl RecoveryFailure {
    fn preflight(code: &'static str, message: impl Into<String>, details: Value) -> Self {
        Self {
            code,
            phase: "preflight",
            message: message.into(),
            retryable: true,
            effect_may_have_occurred: false,
            details,
        }
    }

    fn after_write(
        code: &'static str,
        phase: &'static str,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            code,
            phase,
            message: message.into(),
            retryable: false,
            effect_may_have_occurred: true,
            details,
        }
    }

    pub fn structured(&self) -> Value {
        let mut value = json!({
            "code": self.code,
            "phase": self.phase,
            "retryable": self.retryable,
            "effect_may_have_occurred": self.effect_may_have_occurred,
            "native_side_effect_started": self.effect_may_have_occurred,
        });
        let object = value.as_object_mut().expect("failure payload is an object");
        if let Some(details) = self.details.as_object() {
            object.extend(details.clone());
        }
        value
    }
}

/// Read the AX facts for exactly one `(pid, CGWindowID)`.
///
/// `None` means zero or multiple AX windows mapped to the id. Callers must not
/// infer a target from either shape.
pub fn exact_ax_window_facts(pid: i32, window_id: u32) -> Option<ExactAxWindowFacts> {
    ax_window_facts_for_pid(pid).remove(&window_id)
}

/// Read every uniquely mapped top-level AX window for one process in one AX
/// enumeration. Ambiguous duplicate mappings are omitted.
pub fn ax_window_facts_for_pid(pid: i32) -> HashMap<u32, ExactAxWindowFacts> {
    unsafe {
        let application = AXUIElementCreateApplication(pid);
        if application.is_null() {
            return HashMap::new();
        }
        let windows = copy_ax_windows(application);
        CFRelease(application as CFTypeRef);
        let mut facts: HashMap<u32, Option<ExactAxWindowFacts>> = HashMap::new();
        for window in windows {
            if let Some(window_id) = ax_get_window_id(window) {
                let candidate = ExactAxWindowFacts {
                    minimized: copy_bool_attr(window, "AXMinimized"),
                    subrole: copy_string_attr(window, "AXSubrole"),
                    is_main: copy_bool_attr(window, "AXMain"),
                };
                facts
                    .entry(window_id)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(candidate));
            }
            CFRelease(window as CFTypeRef);
        }
        facts
            .into_iter()
            .filter_map(|(window_id, facts)| facts.map(|facts| (window_id, facts)))
            .collect()
    }
}

/// Restore an exact minimized window without activating its application.
///
/// `Ok(None)` means the exact AX window was already non-minimized or could not
/// be resolved, so the ordinary observation path remains authoritative.
pub async fn restore_if_minimized(
    pid: i32,
    window_id: u32,
) -> Result<Option<RecoveryEvidence>, RecoveryFailure> {
    let initial = tokio::task::spawn_blocking(move || exact_ax_window_facts(pid, window_id))
        .await
        .map_err(|error| {
            RecoveryFailure::preflight(
                "internal",
                format!("minimized-window AX preflight task failed: {error}"),
                json!({"pid": pid, "window_id": window_id}),
            )
        })?;
    let Some(initial) = initial else {
        return Ok(None);
    };
    if initial.minimized != Some(true) {
        return Ok(None);
    }
    if !initial.is_recoverable_standard() {
        return Err(RecoveryFailure::preflight(
            "unsupported_in_background",
            "minimized-window recovery requires an exact standard AX window",
            json!({
                "pid": pid,
                "window_id": window_id,
                "subrole": initial.subrole,
                "minimized": initial.minimized,
            }),
        ));
    }

    let frontmost_pid_before = crate::apps::frontmost_pid();
    let lease = frontmost_pid_before
        .filter(|prior| *prior != pid)
        .map(|prior| {
            crate::focus_steal::begin_suppression(Some(pid), prior, "get_window_state.deminimize")
        });
    let write_started = Arc::new(AtomicBool::new(false));
    let recovery_write_started = Arc::clone(&write_started);
    let recovery = tokio::task::spawn_blocking(move || {
        recover_blocking(pid, window_id, recovery_write_started.as_ref())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let target_activation_observed = lease
        .as_ref()
        .is_some_and(crate::focus_steal::SuppressionLease::matched_activation);
    let frontmost_pid_after = crate::apps::frontmost_pid();
    drop(lease);

    let wrote = recovery.map_err(|error| {
        let details = json!({"pid": pid, "window_id": window_id});
        if write_started.load(Ordering::Acquire) {
            RecoveryFailure::after_write(
                "internal",
                "dispatch",
                format!("minimized-window recovery task failed after dispatch: {error}"),
                details,
            )
        } else {
            RecoveryFailure::preflight(
                "internal",
                format!("minimized-window recovery task failed: {error}"),
                details,
            )
        }
    })??;
    if !wrote {
        return Ok(None);
    }

    let evidence = RecoveryEvidence {
        pid,
        window_id,
        frontmost_pid_before,
        frontmost_pid_after,
        target_activation_observed,
    };
    if target_activation_observed
        || (frontmost_pid_before != Some(pid) && frontmost_pid_after == Some(pid))
    {
        return Err(RecoveryFailure::after_write(
            "verification_failed",
            "verify",
            "foreground evidence could not prove that minimized-window recovery stayed in the background",
            evidence.decorate_error(json!({
                "pid": pid,
                "window_id": window_id,
            })),
        ));
    }
    Ok(Some(evidence))
}

fn recover_blocking(
    pid: i32,
    window_id: u32,
    write_started: &AtomicBool,
) -> Result<bool, RecoveryFailure> {
    let server = exact_server_window(pid, window_id).ok_or_else(|| {
        RecoveryFailure::preflight(
            "window_not_found",
            "the exact minimized WindowServer target disappeared before recovery",
            json!({"pid": pid, "window_id": window_id}),
        )
    })?;
    if server.on_current_space != Some(true) {
        return Err(RecoveryFailure::preflight(
            "unsupported_in_background",
            "minimized-window recovery requires an exact current-Space target",
            json!({
                "pid": pid,
                "window_id": window_id,
                "on_current_space": server.on_current_space,
            }),
        ));
    }
    if crate::apps::is_hidden(pid) != Some(false) {
        return Err(RecoveryFailure::preflight(
            "unsupported_in_background",
            "minimized-window recovery refuses a hidden or unknown application state",
            json!({"pid": pid, "window_id": window_id}),
        ));
    }

    let window = exact_ax_window(pid, window_id).ok_or_else(|| {
        RecoveryFailure::preflight(
            "observation_stale",
            "the exact AX window disappeared before minimized recovery",
            json!({"pid": pid, "window_id": window_id}),
        )
    })?;
    let minimized = unsafe { copy_bool_attr(window, "AXMinimized") };
    let subrole = unsafe { copy_string_attr(window, "AXSubrole") };
    if minimized == Some(false) {
        unsafe { CFRelease(window as CFTypeRef) };
        return Ok(false);
    }
    let recoverable = minimized == Some(true)
        && matches!(subrole.as_deref(), Some("AXStandardWindow" | "AXDialog"));
    if !recoverable {
        unsafe { CFRelease(window as CFTypeRef) };
        return Err(RecoveryFailure::preflight(
            "unsupported_in_background",
            "the exact AX window no longer has a recoverable minimized state",
            json!({
                "pid": pid,
                "window_id": window_id,
                "minimized": minimized,
                "subrole": subrole,
            }),
        ));
    }
    let settable = unsafe { is_attribute_settable(window, "AXMinimized") };
    if !settable {
        unsafe { CFRelease(window as CFTypeRef) };
        return Err(RecoveryFailure::preflight(
            "unsupported_in_background",
            "the exact window's AXMinimized attribute is not writable",
            json!({"pid": pid, "window_id": window_id, "attribute": "AXMinimized"}),
        ));
    }
    write_started.store(true, Ordering::Release);
    let status = unsafe { set_bool_attr(window, "AXMinimized", false) };
    unsafe { CFRelease(window as CFTypeRef) };
    if status != kAXErrorSuccess {
        return Err(RecoveryFailure::after_write(
            "dispatch_failed",
            "dispatch",
            "AX failed to restore the exact minimized window",
            json!({
                "pid": pid,
                "window_id": window_id,
                "attribute": "AXMinimized",
                "ax_status": status,
            }),
        ));
    }

    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    loop {
        let ax_settled = exact_ax_window_facts(pid, window_id)
            .is_some_and(|facts| facts.minimized == Some(false));
        let server_settled = exact_server_window(pid, window_id)
            .is_some_and(|window| window.is_on_screen && window.on_current_space == Some(true));
        if ax_settled && server_settled && crate::apps::is_hidden(pid) == Some(false) {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Err(RecoveryFailure::after_write(
                "ui_not_settled",
                "settle",
                "the exact minimized window did not become observable before the bounded deadline",
                json!({
                    "pid": pid,
                    "window_id": window_id,
                    "ax_minimized": exact_ax_window_facts(pid, window_id)
                        .and_then(|facts| facts.minimized),
                    "windowserver_on_screen": exact_server_window(pid, window_id)
                        .map(|window| window.is_on_screen),
                    "windowserver_on_current_space": exact_server_window(pid, window_id)
                        .and_then(|window| window.on_current_space),
                }),
            ));
        }
        thread::sleep(READBACK_INTERVAL);
    }
}

fn exact_server_window(pid: i32, window_id: u32) -> Option<crate::windows::WindowInfo> {
    let mut matches = crate::windows::all_windows()
        .into_iter()
        .filter(|window| window.pid == pid && window.window_id == window_id);
    let exact = matches.next()?;
    matches.next().is_none().then_some(exact)
}

fn exact_ax_window(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    unsafe {
        let mut matches = matching_ax_windows(pid, window_id);
        if matches.len() != 1 {
            release_all(matches);
            return None;
        }
        Some(matches.remove(0))
    }
}

unsafe fn matching_ax_windows(pid: i32, window_id: u32) -> Vec<AXUIElementRef> {
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return Vec::new();
    }
    let windows = copy_ax_windows(application);
    CFRelease(application as CFTypeRef);
    let mut matches = Vec::new();
    for window in windows {
        if ax_get_window_id(window) == Some(window_id) {
            matches.push(window);
        } else {
            CFRelease(window as CFTypeRef);
        }
    }
    matches
}

unsafe fn release_all(elements: Vec<AXUIElementRef>) {
    for element in elements {
        CFRelease(element as CFTypeRef);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_standard_or_minimized_dialog_facts_are_recoverable() {
        let standard = ExactAxWindowFacts {
            minimized: Some(true),
            subrole: Some("AXStandardWindow".into()),
            is_main: Some(true),
        };
        let minimized_dialog = ExactAxWindowFacts {
            minimized: Some(true),
            subrole: Some("AXDialog".into()),
            is_main: Some(false),
        };
        let visible_dialog = ExactAxWindowFacts {
            minimized: Some(false),
            subrole: Some("AXDialog".into()),
            is_main: Some(false),
        };
        assert!(standard.is_recoverable_standard());
        assert!(minimized_dialog.is_recoverable_standard());
        assert!(!visible_dialog.is_recoverable_standard());
    }

    #[test]
    fn post_write_failure_is_non_retryable_and_truthful() {
        let failure = RecoveryFailure::after_write(
            "ui_not_settled",
            "settle",
            "fixture",
            json!({"window_id": 7}),
        );
        assert!(!failure.retryable);
        assert!(failure.effect_may_have_occurred);
        assert_eq!(failure.structured()["native_side_effect_started"], true);
    }
}
