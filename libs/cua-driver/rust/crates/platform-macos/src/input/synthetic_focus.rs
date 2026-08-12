//! Signed-helper-compatible synthetic target focus belief.
//!
//! The real foreground application must keep its keyboard focus. We therefore
//! post process-notification events only to the target pid; unlike the older
//! SkyLight approximation, this path never sends a defocus record to the
//! user's current application.

use std::{
    collections::HashMap,
    ffi::c_void,
    fmt,
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use core_foundation::base::{CFRelease, CFTypeRef};
use objc2::msg_send;
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::NSPoint;

use crate::{
    apps,
    ax::bindings::{
        ax_get_window_id, copy_ax_windows, copy_bool_attr, copy_point_attr, element_screen_rect,
        AXUIElementCreateApplication, AXUIElementRef,
    },
};

const APPKIT_DEFINED_EVENT_TYPE: usize = 13;
const APP_ACTIVATED_SUBTYPE: i16 = 1;
const APP_ACTIVATED_MODIFIER_FLAGS: usize = 0xC0000;
const PROCESS_NOTIFICATION_EVENT_TYPE: usize = 21;
const CPS_NOTIFY_KEY_FOCUS_RETURNED: i16 = i16::MIN;
const BELIEF_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const BELIEF_ACK_POLL: Duration = Duration::from_millis(10);

#[repr(C)]
struct CGEvent {
    _opaque: [u8; 0],
}

unsafe impl objc2::RefEncode for CGEvent {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("__CGEvent", &[]));
}

type CGEventRef = *mut CGEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FocusBeliefState {
    bundle_id: Option<String>,
    application_is_active: bool,
    application_believes_it_is_active: bool,
    application_believes_it_has_focus: bool,
}

impl FocusBeliefState {
    fn new(pid: i32, bundle_id: Option<String>) -> Self {
        let application_is_active = apps::frontmost_pid() == Some(pid);
        Self {
            bundle_id,
            application_is_active,
            application_believes_it_is_active: application_is_active,
            application_believes_it_has_focus: application_is_active,
        }
    }

    fn plan(
        &mut self,
        application_is_active: bool,
        application_believes_frontmost: Option<bool>,
        refresh_key_focus: bool,
    ) -> FocusPlan {
        if self.application_is_active != application_is_active {
            self.application_is_active = application_is_active;
            self.application_believes_it_is_active = application_is_active;
            self.application_believes_it_has_focus = application_is_active;
        } else if !application_is_active
            && application_believes_frontmost == Some(false)
            && (self.application_believes_it_is_active || self.application_believes_it_has_focus)
        {
            // A real activate/deactivate cycle can invalidate the target-only
            // belief between driver actions. The signed helper receives that
            // transition through its controller-lifetime observers. Upstream's
            // tool runtime has no equivalent per-target controller, so the
            // target's own AXFrontmost reset is the exact action-time signal
            // available here.
            self.application_believes_it_is_active = false;
            self.application_believes_it_has_focus = false;
        }
        FocusPlan {
            post_key_focus_returned: !self.application_is_active
                && (refresh_key_focus || !self.application_believes_it_has_focus),
            post_app_activated: !self.application_is_active
                && !self.application_believes_it_is_active,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FocusPlan {
    post_key_focus_returned: bool,
    post_app_activated: bool,
}

fn states() -> &'static Mutex<HashMap<i32, FocusBeliefState>> {
    static STATES: OnceLock<Mutex<HashMap<i32, FocusBeliefState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Failure from the synthetic-focus prelude, including whether any native
/// target-only event was already posted. Callers must preserve this boundary:
/// a failure before the first post is safely retryable, while a failure after a
/// post has an uncertain native effect and must not be replayed blindly.
#[derive(Debug)]
pub struct FocusPreparationError {
    source: anyhow::Error,
    native_side_effect_started: bool,
}

impl FocusPreparationError {
    fn before_post(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
            native_side_effect_started: false,
        }
    }

    fn from_progress(source: impl Into<anyhow::Error>, native_side_effect_started: bool) -> Self {
        Self {
            source: source.into(),
            native_side_effect_started,
        }
    }

    pub fn native_side_effect_started(&self) -> bool {
        self.native_side_effect_started
    }
}

impl fmt::Display for FocusPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for FocusPreparationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

/// Establish the target's AppKit focus belief without changing or defocusing
/// the WindowServer foreground application.
///
/// Returns whether this call posted any belief event. A false return means a
/// durable per-process belief was already acknowledged and reused.
pub fn enforce(pid: i32, window_id: u32) -> Result<bool, FocusPreparationError> {
    enforce_with_key_focus_refresh(pid, window_id, false)
}

/// Prepare the signed helper's keyboard recipe. Keyboard actions refresh the
/// CPS key-focus-returned notification on every background dispatch while
/// reusing the durable application-activation belief.
pub fn enforce_keyboard(pid: i32, window_id: u32) -> Result<bool, FocusPreparationError> {
    enforce_with_key_focus_refresh(pid, window_id, true)
}

fn enforce_with_key_focus_refresh(
    pid: i32,
    window_id: u32,
    refresh_key_focus: bool,
) -> Result<bool, FocusPreparationError> {
    let bundle_id = apps::bundle_id_for_pid(pid);
    let application_is_active = apps::frontmost_pid() == Some(pid);
    let believed_frontmost_before = application_believes_frontmost(pid);
    let plan = {
        let mut registry = states().lock().map_err(|_| {
            FocusPreparationError::before_post(anyhow::anyhow!(
                "synthetic focus registry was poisoned"
            ))
        })?;
        let state = registry
            .entry(pid)
            .or_insert_with(|| FocusBeliefState::new(pid, bundle_id.clone()));
        if state.bundle_id != bundle_id {
            *state = FocusBeliefState::new(pid, bundle_id);
        }
        state.plan(
            application_is_active,
            believed_frontmost_before,
            refresh_key_focus,
        )
    };

    let mut native_side_effect_started = false;
    if plan.post_key_focus_returned {
        post_key_focus_returned(pid).map_err(FocusPreparationError::before_post)?;
        native_side_effect_started = true;
        states()
            .lock()
            .map_err(|_| {
                FocusPreparationError::from_progress(
                    anyhow::anyhow!("synthetic focus registry was poisoned"),
                    native_side_effect_started,
                )
            })?
            .get_mut(&pid)
            .expect("focus state exists after planning")
            .application_believes_it_has_focus = true;
    }
    if plan.post_app_activated {
        let window = resolve_exact_ax_window(pid, window_id).map_err(|error| {
            FocusPreparationError::from_progress(error, native_side_effect_started)
        })?;
        let result = post_app_activated(pid, window_id, window, &mut native_side_effect_started);
        unsafe { CFRelease(window as CFTypeRef) };
        result.map_err(|error| {
            FocusPreparationError::from_progress(error, native_side_effect_started)
        })?;
        states()
            .lock()
            .map_err(|_| {
                FocusPreparationError::from_progress(
                    anyhow::anyhow!("synthetic focus registry was poisoned"),
                    native_side_effect_started,
                )
            })?
            .get_mut(&pid)
            .expect("focus state exists after planning")
            .application_believes_it_is_active = true;
    }

    let deadline = Instant::now() + BELIEF_ACK_TIMEOUT;
    while !application_is_active && application_believes_frontmost(pid) != Some(true) {
        if Instant::now() >= deadline {
            if let Ok(mut registry) = states().lock() {
                registry.remove(&pid);
            }
            return Err(FocusPreparationError::from_progress(
                anyhow::anyhow!(
                    "target pid {pid} did not acknowledge synthetic focus through AXFrontmost"
                ),
                native_side_effect_started,
            ));
        }
        thread::sleep(BELIEF_ACK_POLL);
    }

    Ok(plan.post_key_focus_returned || plan.post_app_activated)
}

fn application_believes_frontmost(pid: i32) -> Option<bool> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return None;
        }
        let value = copy_bool_attr(app, "AXFrontmost");
        CFRelease(app as CFTypeRef);
        value
    }
}

fn resolve_exact_ax_window(pid: i32, window_id: u32) -> anyhow::Result<AXUIElementRef> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            anyhow::bail!("could not create AX application for target pid {pid}");
        }
        let windows = copy_ax_windows(app);
        CFRelease(app as CFTypeRef);
        let mut selected = None;
        for window in windows {
            if selected.is_none() && ax_get_window_id(window) == Some(window_id) {
                selected = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
        selected.ok_or_else(|| {
            anyhow::anyhow!("target window {window_id} is absent from pid {pid}'s AXWindows")
        })
    }
}

fn post_key_focus_returned(pid: i32) -> anyhow::Result<()> {
    post_other_event(
        pid,
        NSEventType(PROCESS_NOTIFICATION_EVENT_TYPE),
        CPS_NOTIFY_KEY_FOCUS_RETURNED,
        "NSEvent.processNotification",
    )
}

fn post_app_activated(
    pid: i32,
    window_id: u32,
    window: AXUIElementRef,
    native_side_effect_started: &mut bool,
) -> anyhow::Result<()> {
    let event = unsafe {
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType(APPKIT_DEFINED_EVENT_TYPE),
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags(APP_ACTIVATED_MODIFIER_FLAGS),
            0.0,
            isize::try_from(window_id).unwrap_or(isize::MAX),
            None,
            APP_ACTIVATED_SUBTYPE,
            0,
            0,
        )
    }
    .ok_or_else(|| anyhow::anyhow!("AppKit could not construct app-activation event"))?;
    unsafe { timestamp_and_post(pid, cg_event(&event)?)? };
    *native_side_effect_started = true;

    let activation_point = unsafe { copy_point_attr(window, "AXActivationPoint") }
        .map_err(|status| anyhow::anyhow!("AXActivationPoint query failed with status {status}"))?;
    let Some((screen_x, screen_y)) = activation_point else {
        return Ok(());
    };
    let [window_x, window_y, _, _] = unsafe { element_screen_rect(window) }
        .ok_or_else(|| anyhow::anyhow!("target AX window has no exact live frame"))?;
    for (event_number, event_type) in [
        (1, NSEventType::LeftMouseDown),
        (2, NSEventType::LeftMouseUp),
    ] {
        let event = unsafe {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                event_type,
                NSPoint::new(screen_x, screen_y),
                NSEventModifierFlags(0),
                0.0,
                isize::try_from(window_id).unwrap_or(isize::MAX),
                None,
                event_number,
                1,
                1.0,
            )
        }
        .ok_or_else(|| anyhow::anyhow!("AppKit could not construct activation pointer event"))?;
        let cg_event = unsafe { cg_event(&event)? };
        unsafe {
            CGEventSetLocation(cg_event.cast(), screen_x, screen_y);
            CGEventSetIntegerValueField(cg_event, 3, 0);
            CGEventSetIntegerValueField(cg_event, 7, 3);
            CGEventSetIntegerValueField(cg_event, 91, i64::from(window_id));
            CGEventSetIntegerValueField(cg_event, 92, i64::from(window_id));
            CGEventSetWindowLocation(cg_event, screen_x - window_x, screen_y - window_y);
            timestamp_and_post(pid, cg_event)?;
        }
        *native_side_effect_started = true;
    }
    Ok(())
}

fn post_other_event(
    pid: i32,
    event_type: NSEventType,
    subtype: i16,
    name: &'static str,
) -> anyhow::Result<()> {
    let event = unsafe {
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            event_type,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags(0),
            0.0,
            0,
            None,
            subtype,
            0,
            0,
        )
    }
    .ok_or_else(|| anyhow::anyhow!("AppKit could not construct {name}"))?;
    unsafe { timestamp_and_post(pid, cg_event(&event)?) }
}

unsafe fn cg_event(event: &NSEvent) -> anyhow::Result<CGEventRef> {
    let event: CGEventRef = msg_send![event, CGEvent];
    if event.is_null() {
        anyhow::bail!("NSEvent.CGEvent returned null");
    }
    Ok(event)
}

unsafe fn timestamp_and_post(pid: i32, event: CGEventRef) -> anyhow::Result<()> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) != 0 {
        anyhow::bail!("failed to read macOS uptime before targeted event posting");
    }
    let timestamp = u64::try_from(time.tv_sec)
        .unwrap_or_default()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(time.tv_nsec).unwrap_or_default());
    CGEventSetTimestamp(event.cast(), timestamp);
    CGEventPostToPid(pid, event);
    Ok(())
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSetLocation(event: *mut c_void, x: f64, y: f64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventSetWindowLocation(event: CGEventRef, x: f64, y: f64);
    fn CGEventSetTimestamp(event: *mut c_void, timestamp: u64);
    fn CGEventPostToPid(pid: i32, event: CGEventRef);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_plan_is_durable_and_rearms_after_real_activation_transition() {
        let mut state = FocusBeliefState {
            bundle_id: Some("example.app".to_owned()),
            application_is_active: false,
            application_believes_it_is_active: false,
            application_believes_it_has_focus: false,
        };
        assert_eq!(
            state.plan(false, Some(false), false),
            FocusPlan {
                post_key_focus_returned: true,
                post_app_activated: true,
            }
        );
        state.application_believes_it_is_active = true;
        state.application_believes_it_has_focus = true;
        assert_eq!(
            state.plan(false, Some(true), false),
            FocusPlan {
                post_key_focus_returned: false,
                post_app_activated: false,
            }
        );
        assert_eq!(
            state.plan(true, Some(true), false),
            FocusPlan {
                post_key_focus_returned: false,
                post_app_activated: false,
            }
        );
        assert_eq!(
            state.plan(false, Some(false), false),
            FocusPlan {
                post_key_focus_returned: true,
                post_app_activated: true,
            }
        );
    }

    #[test]
    fn keyboard_plan_refreshes_key_focus_but_reuses_app_activation_belief() {
        let mut state = FocusBeliefState {
            bundle_id: Some("example.app".to_owned()),
            application_is_active: false,
            application_believes_it_is_active: true,
            application_believes_it_has_focus: true,
        };
        assert_eq!(
            state.plan(false, Some(true), true),
            FocusPlan {
                post_key_focus_returned: true,
                post_app_activated: false,
            }
        );
    }

    #[test]
    fn process_notification_uses_the_real_cg_event_ref_abi() {
        let event = unsafe {
            NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType(PROCESS_NOTIFICATION_EVENT_TYPE),
                NSPoint::new(0.0, 0.0),
                NSEventModifierFlags(0),
                0.0,
                0,
                None,
                CPS_NOTIFY_KEY_FOCUS_RETURNED,
                0,
                0,
            )
        }
        .expect("AppKit should construct a process-notification event");
        assert!(!unsafe { cg_event(&event) }
            .expect("NSEvent.CGEvent should resolve")
            .is_null());
    }
}
