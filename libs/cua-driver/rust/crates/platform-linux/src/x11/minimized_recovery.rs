//! Exact, focus-preserving recovery of a minimized GNOME X11 window.
//!
//! Mutter keeps minimized client drawables mapped and cannot be reliably
//! deminiaturized by a normal X11 client without activation. The bundled,
//! desktop-owned Shell extension performs that compositor mutation after this
//! module proves the exact `(pid, XID, title)` identity. Every failure after
//! dispatch retains possible-effect evidence.

use std::{fs, os::unix::fs::MetadataExt, process::Command, time::Duration};

use serde_json::{json, Value};

use super::WindowInfo;

const DEST: &str = "org.cua.WinRestore";
const PATH: &str = "/org/cua/WinRestore";
const IFACE: &str = "org.cua.WinRestore";
const DBUS_DEST: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct RecoveryEvidence {
    pub pid: u32,
    pub window_id: u64,
    pub active_window_before: Option<u64>,
    pub active_window_after: Option<u64>,
}

impl RecoveryEvidence {
    pub fn structured(&self) -> Value {
        json!({
            "performed": true,
            "pid": self.pid,
            "window_id": self.window_id,
            "minimized_before_restore": true,
            "minimized_after_restore": false,
            "active_window_before": self.active_window_before,
            "active_window_after": self.active_window_after,
            "target_activation_observed": self.active_window_after == Some(self.window_id),
            "route": "gnome_shell_exact_window",
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
    pub message: String,
    pub structured: Value,
}

fn preflight(
    message: impl Into<String>,
    code: &'static str,
    retryable: bool,
    details: Value,
) -> RecoveryFailure {
    let mut structured = json!({
        "code": code,
        "phase": "preflight",
        "retryable": retryable,
        "effect_may_have_occurred": false,
        "native_side_effect_started": false,
    });
    structured
        .as_object_mut()
        .unwrap()
        .extend(details.as_object().cloned().unwrap_or_default());
    RecoveryFailure {
        message: message.into(),
        structured,
    }
}

fn post_dispatch(
    evidence: &RecoveryEvidence,
    message: impl Into<String>,
    code: &'static str,
) -> RecoveryFailure {
    RecoveryFailure {
        message: message.into(),
        structured: evidence.decorate_error(json!({
            "code": code,
            "phase": "verify",
            "retryable": false,
        })),
    }
}

pub fn restore_if_minimized(
    target: &WindowInfo,
) -> Result<Option<RecoveryEvidence>, RecoveryFailure> {
    if target.is_on_screen {
        return Ok(None);
    }
    let pid = target.pid.ok_or_else(|| {
        preflight(
            "minimized-window recovery requires a verified X11 owner pid",
            "observation_raced",
            true,
            json!({"window_id": target.xid}),
        )
    })?;
    let current = super::list_windows(Some(pid))
        .into_iter()
        .filter(|window| window.xid == target.xid && window.title == target.title)
        .collect::<Vec<_>>();
    if current.len() != 1 {
        return Err(preflight(
            "minimized-window recovery could not revalidate one exact pid/XID/title target",
            "observation_raced",
            true,
            json!({"pid": pid, "window_id": target.xid}),
        ));
    }
    if current[0].is_on_screen {
        return Ok(None);
    }
    let owner = trusted_shell_owner().ok_or_else(|| {
        preflight(
            "background minimized-window recovery requires the trusted GNOME Shell helper",
            "unsupported_in_background",
            false,
            json!({"pid": pid, "window_id": target.xid}),
        )
    })?;
    let xid = u32::try_from(target.xid).map_err(|_| {
        preflight(
            "minimized-window recovery requires a 32-bit X11 window id",
            "observation_raced",
            false,
            json!({"pid": pid, "window_id": target.xid}),
        )
    })?;
    let active_window_before = super::active_window_id();
    let output = gdbus_call(
        &owner,
        PATH,
        &format!("{IFACE}.Restore"),
        &[pid.to_string(), xid.to_string(), target.title.clone()],
        Duration::from_secs(1),
    );
    let mut evidence = RecoveryEvidence {
        pid,
        window_id: target.xid,
        active_window_before,
        active_window_after: super::active_window_id(),
    };
    if !output.is_some_and(|value| value.trim_start().starts_with("(true,")) {
        return Err(post_dispatch(
            &evidence,
            "GNOME Shell did not confirm exact minimized-window recovery",
            "verification_failed",
        ));
    }
    let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
    loop {
        evidence.active_window_after = super::active_window_id();
        let observed = super::list_windows(Some(pid))
            .into_iter()
            .find(|window| window.xid == target.xid && window.title == target.title);
        if observed.as_ref().is_some_and(|window| window.is_on_screen)
            && evidence.active_window_after == active_window_before
        {
            return Ok(Some(evidence));
        }
        if std::time::Instant::now() >= deadline {
            return Err(post_dispatch(
                &evidence,
                "background minimized-window recovery did not settle on the exact X11 window without activation",
                "ui_not_settled",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn trusted_shell_owner() -> Option<String> {
    let owner = quoted(&gdbus_call(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.GetNameOwner"),
        &[DEST.to_owned()],
        Duration::from_millis(800),
    )?)?;
    let pid = first_u32(&gdbus_call(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.GetConnectionUnixProcessID"),
        &[owner.clone()],
        Duration::from_millis(800),
    )?)?;
    let uid = first_u32(&gdbus_call(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.GetConnectionUnixUser"),
        &[owner.clone()],
        Duration::from_millis(800),
    )?)?;
    let executable = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let metadata = fs::metadata(&executable).ok()?;
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let version = first_u32(&gdbus_call(
        &owner,
        PATH,
        &format!("{IFACE}.GetVersion"),
        &[],
        Duration::from_millis(800),
    )?)?;
    (uid == unsafe { libc::geteuid() }
        && comm.trim() == "gnome-shell"
        && executable.file_name().and_then(|name| name.to_str()) == Some("gnome-shell")
        && metadata.uid() == 0
        && version >= 2)
        .then_some(owner)
}

fn gdbus_call(
    destination: &str,
    object_path: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> Option<String> {
    let mut child = Command::new("gdbus");
    child
        .arg("call")
        .arg("--session")
        .arg("--dest")
        .arg(destination)
        .arg("--object-path")
        .arg(object_path)
        .arg("--method")
        .arg(method)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    wait_timeout(child.spawn().ok()?, timeout)
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn wait_timeout(mut child: std::process::Child, timeout: Duration) -> Option<std::process::Output> {
    let started = std::time::Instant::now();
    loop {
        if child.try_wait().ok()?.is_some() {
            return child.wait_with_output().ok();
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn quoted(raw: &str) -> Option<String> {
    let start = raw.find('\'')? + 1;
    let end = raw[start..].find('\'')? + start;
    (end > start).then(|| raw[start..end].to_owned())
}

fn first_u32(raw: &str) -> Option<u32> {
    let payload = raw.split_once("uint32").map_or(raw, |(_, payload)| payload);
    payload
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELPER_SOURCE: &str =
        include_str!("../../../../../x11-helper/winrestore@cua/extension.js");
    const HELPER_METADATA: &str =
        include_str!("../../../../../x11-helper/winrestore@cua/metadata.json");

    #[test]
    fn dbus_scalar_parsers_are_exact() {
        assert_eq!(quoted("(':1.42',)"), Some(":1.42".to_owned()));
        assert_eq!(first_u32("(uint32 157360,)"), Some(157360));
        assert_eq!(quoted("(nothing,)"), None);
    }

    #[test]
    fn bundled_helper_requires_exact_pid_xid_and_title() {
        assert!(HELPER_SOURCE.contains("RestoreAsync([pid, xid, title], invocation)"));
        assert!(HELPER_SOURCE.contains("xWindowId(window) === xid"));
        assert!(HELPER_SOURCE.contains("global.display.focus_window === focusedBefore"));
        assert!(!HELPER_SOURCE.contains("GetWindows"));
        assert!(HELPER_METADATA.contains("\"version\": 2"));
    }

    #[test]
    fn missing_helper_refusal_is_non_retryable_and_pre_dispatch() {
        let failure = preflight(
            "helper missing",
            "unsupported_in_background",
            false,
            json!({"window_id": 42}),
        );
        assert_eq!(failure.structured["phase"], "preflight");
        assert_eq!(failure.structured["retryable"], false);
        assert_eq!(failure.structured["effect_may_have_occurred"], false);
        assert_eq!(failure.structured["native_side_effect_started"], false);
    }
}
