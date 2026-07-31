//! Canonical AppKit-to-CGEvent synthesis used by the signed macOS helper.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicIsize, Ordering},
};

use core_graphics::{
    event::{CGEvent as OwnedCGEvent, ScrollEventUnit},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use cua_driver_core::api::{
    contracts::{Modifier, MouseButton, Point},
    errors::{ErrorCode, ErrorPhase, NativeError},
};
use foreign_types::ForeignType;
use objc2::msg_send;
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::NSPoint;

pub(crate) const CPS_NOTIFY_KEY_FOCUS_RETURNED: i16 = i16::MIN;

const APPKIT_DEFINED_EVENT_TYPE: usize = 13;
const APP_ACTIVATED_SUBTYPE: i16 = 1;
const APP_ACTIVATED_MODIFIER_FLAGS: usize = 0xC0000;
const PROCESS_NOTIFICATION_EVENT_TYPE: usize = 21;

// `-[NSEvent CGEvent]` returns a `CGEventRef` whose Objective-C type encoding
// is `^{__CGEvent=}`. objc2 checks that encoding at runtime, so a generic
// `*mut c_void` (`^v`) is not a valid return type for the selector.
#[repr(C)]
struct CGEvent {
    _opaque: [u8; 0],
}

unsafe impl objc2::RefEncode for CGEvent {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("__CGEvent", &[]));
}

type CGEventRef = *mut CGEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseEventKind {
    Down,
    Dragged,
    Up,
}

pub(crate) struct MouseEventSpec<'a> {
    pub pid: i32,
    pub cg_window_id: u32,
    pub screen: Point,
    pub window_local: Point,
    pub button: MouseButton,
    pub click_count: u8,
    pub modifiers: &'a [Modifier],
    pub kind: MouseEventKind,
}

pub(crate) struct PixelScrollEventSpec {
    pub pid: i32,
    pub cg_window_id: u32,
    pub screen: Point,
    pub window_local: Point,
    /// Public logical-pixel convention: positive values reveal content to the
    /// right and down. Conversion to the native wheel convention happens once
    /// in this factory.
    pub delta_x: f64,
    pub delta_y: f64,
}

pub(crate) fn post_mouse_event(spec: &MouseEventSpec<'_>) -> Result<(), NativeError> {
    post_mouse_event_with_number(spec, next_event_number())
}

fn post_mouse_event_with_number(
    spec: &MouseEventSpec<'_>,
    event_number: isize,
) -> Result<(), NativeError> {
    let event_type = match (spec.kind, spec.button) {
        (MouseEventKind::Down, MouseButton::Left) => NSEventType::LeftMouseDown,
        (MouseEventKind::Dragged, MouseButton::Left) => NSEventType::LeftMouseDragged,
        (MouseEventKind::Up, MouseButton::Left) => NSEventType::LeftMouseUp,
        (MouseEventKind::Down, MouseButton::Right) => NSEventType::RightMouseDown,
        (MouseEventKind::Dragged, MouseButton::Right) => NSEventType::RightMouseDragged,
        (MouseEventKind::Up, MouseButton::Right) => NSEventType::RightMouseUp,
        (MouseEventKind::Down, MouseButton::Middle) => NSEventType::OtherMouseDown,
        (MouseEventKind::Dragged, MouseButton::Middle) => NSEventType::OtherMouseDragged,
        (MouseEventKind::Up, MouseButton::Middle) => NSEventType::OtherMouseUp,
    };
    let event = unsafe {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            event_type,
            NSPoint::new(spec.screen.x, spec.screen.y),
            ns_modifier_flags(spec.modifiers),
            0.0,
            isize::try_from(spec.cg_window_id).unwrap_or(isize::MAX),
            None,
            event_number,
            isize::from(spec.click_count),
            1.0,
        )
    }
    .ok_or_else(|| construction_failed("NSEvent.mouseEvent"))?;

    let cg_event = unsafe { cg_event(&event) }?;
    unsafe {
        CGEventSetFlags(cg_event, modifier_bits(spec.modifiers));
        CGEventSetLocation(cg_event.cast(), spec.screen.x, spec.screen.y);
        CGEventSetIntegerValueField(cg_event, 3, button_number(spec.button));
        CGEventSetIntegerValueField(cg_event, 7, 3);
        CGEventSetIntegerValueField(cg_event, 91, i64::from(spec.cg_window_id));
        CGEventSetIntegerValueField(cg_event, 92, i64::from(spec.cg_window_id));
        CGEventSetWindowLocation(cg_event, spec.window_local.x, spec.window_local.y);
        timestamp_and_post(spec.pid, cg_event)?;
    }
    Ok(())
}

pub(crate) fn post_pixel_scroll_event(spec: &PixelScrollEventSpec) -> Result<(), NativeError> {
    let delta_x = native_wheel_delta(spec.delta_x, "delta_x")?;
    let delta_y = native_wheel_delta(spec.delta_y, "delta_y")?;
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| construction_failed("CGEventSourceCreate"))?;
    let event =
        OwnedCGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, 1, delta_y, delta_x, 0)
            .map_err(|_| construction_failed("CGEventCreateScrollWheelEvent"))?;
    let event_ref = event.as_ptr().cast::<CGEvent>();
    unsafe {
        CGEventSetLocation(event_ref.cast(), spec.screen.x, spec.screen.y);
        CGEventSetIntegerValueField(event_ref, 51, i64::from(spec.cg_window_id));
        CGEventSetIntegerValueField(event_ref, 91, i64::from(spec.cg_window_id));
        CGEventSetIntegerValueField(event_ref, 92, i64::from(spec.cg_window_id));
        CGEventSetWindowLocation(event_ref, spec.window_local.x, spec.window_local.y);
        timestamp_and_post(spec.pid, event_ref)?;
    }
    Ok(())
}

fn native_wheel_delta(value: f64, name: &'static str) -> Result<i32, NativeError> {
    if !value.is_finite()
        || value.fract() != 0.0
        || value <= f64::from(i32::MIN)
        || value > f64::from(i32::MAX)
    {
        return Err(NativeError::new(
            ErrorCode::UnsupportedInBackground,
            ErrorPhase::Preflight,
            false,
            "the recovered macOS pixel-scroll route requires an integral 32-bit logical delta",
        )
        .with_detail("recipe_status", "native_integer_delta_required")
        .with_detail("field", name)
        .with_detail("value", value));
    }
    Ok(-(value as i32))
}

pub(crate) fn post_app_activated(
    pid: i32,
    cg_window_id: u32,
    window_bounds: cua_driver_core::api::contracts::Rect,
    activation_point: Option<Point>,
) -> Result<usize, NativeError> {
    let event = unsafe {
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType(APPKIT_DEFINED_EVENT_TYPE),
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags(if cg_window_id == 0 {
                0
            } else {
                APP_ACTIVATED_MODIFIER_FLAGS
            }),
            0.0,
            isize::try_from(cg_window_id).unwrap_or(isize::MAX),
            None,
            APP_ACTIVATED_SUBTYPE,
            0,
            0,
        )
    }
    .ok_or_else(|| construction_failed("NSEvent.appActivated"))?;
    let cg_event = unsafe { cg_event(&event) }?;
    unsafe {
        timestamp_and_post(pid, cg_event)?;
    }

    let Some(screen) = activation_point else {
        return Ok(1);
    };
    let window_local = Point {
        x: screen.x - window_bounds.x,
        y: screen.y - window_bounds.y,
    };
    for (event_number, kind) in [(1, MouseEventKind::Down), (2, MouseEventKind::Up)] {
        post_mouse_event_with_number(
            &MouseEventSpec {
                pid,
                cg_window_id,
                screen,
                window_local,
                button: MouseButton::Left,
                click_count: 1,
                modifiers: &[],
                kind,
            },
            event_number,
        )?;
    }
    Ok(3)
}

pub(crate) fn post_key_focus_returned(pid: i32) -> Result<(), NativeError> {
    post_other_event(
        pid,
        NSEventType(PROCESS_NOTIFICATION_EVENT_TYPE),
        CPS_NOTIFY_KEY_FOCUS_RETURNED,
        "NSEvent.processNotification",
    )
}

fn post_other_event(
    pid: i32,
    event_type: NSEventType,
    subtype: i16,
    event_name: &'static str,
) -> Result<(), NativeError> {
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
    .ok_or_else(|| construction_failed(event_name))?;
    let cg_event = unsafe { cg_event(&event) }?;
    unsafe {
        timestamp_and_post(pid, cg_event)?;
    }
    Ok(())
}

unsafe fn cg_event(event: &NSEvent) -> Result<CGEventRef, NativeError> {
    let cg_event: CGEventRef = msg_send![event, CGEvent];
    if cg_event.is_null() {
        Err(construction_failed("NSEvent.CGEvent"))
    } else {
        Ok(cg_event)
    }
}

unsafe fn timestamp_and_post(pid: i32, event: CGEventRef) -> Result<(), NativeError> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) != 0 {
        return Err(NativeError::new(
            ErrorCode::DispatchFailed,
            ErrorPhase::Dispatch,
            false,
            "failed to read the macOS uptime clock before targeted event posting",
        ));
    }
    let seconds = u64::try_from(time.tv_sec).unwrap_or_default();
    let nanoseconds = u64::try_from(time.tv_nsec).unwrap_or_default();
    let timestamp = seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds))
        .ok_or_else(|| {
            NativeError::new(
                ErrorCode::DispatchFailed,
                ErrorPhase::Dispatch,
                false,
                "macOS event timestamp overflowed",
            )
        })?;
    CGEventSetTimestamp(event as *mut c_void, timestamp);
    CGEventPostToPid(pid, event);
    Ok(())
}

pub(crate) fn fresh_event_timestamp() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) } != 0 {
        return 0;
    }
    u64::try_from(time.tv_sec)
        .unwrap_or_default()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(time.tv_nsec).unwrap_or_default())
}

pub(crate) unsafe fn set_event_timestamp(event: *mut c_void, timestamp: u64) {
    CGEventSetTimestamp(event, timestamp);
}

fn next_event_number() -> isize {
    static NEXT_EVENT_NUMBER: AtomicIsize = AtomicIsize::new(1);
    NEXT_EVENT_NUMBER.fetch_add(1, Ordering::Relaxed)
}

fn ns_modifier_flags(modifiers: &[Modifier]) -> NSEventModifierFlags {
    NSEventModifierFlags(modifier_bits(modifiers) as usize)
}

fn modifier_bits(modifiers: &[Modifier]) -> u64 {
    modifiers.iter().fold(0, |flags, modifier| {
        flags
            | match modifier {
                Modifier::Shift => 1 << 17,
                Modifier::Control => 1 << 18,
                Modifier::Alt => 1 << 19,
                Modifier::Meta => 1 << 20,
            }
    })
}

fn button_number(button: MouseButton) -> i64 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

fn construction_failed(native_constructor: &'static str) -> NativeError {
    NativeError::new(
        ErrorCode::DispatchFailed,
        ErrorPhase::Dispatch,
        false,
        "AppKit could not construct the synthesized native event",
    )
    .with_detail("native_constructor", native_constructor)
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
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
    fn appkit_cg_event_selector_uses_the_real_cg_event_ref_abi() {
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
        .expect("AppKit should construct the process-notification event");

        let cg_event =
            unsafe { cg_event(&event) }.expect("NSEvent.CGEvent should return a CGEventRef");

        assert!(!cg_event.is_null());
    }

    #[test]
    fn public_scroll_delta_is_converted_once_at_the_native_wheel_boundary() {
        assert_eq!(native_wheel_delta(42.0, "delta_y").unwrap(), -42);
        assert_eq!(native_wheel_delta(-7.0, "delta_x").unwrap(), 7);
        assert!(native_wheel_delta(0.5, "delta_x").is_err());
        assert!(native_wheel_delta(f64::from(i32::MIN), "delta_y").is_err());
    }
}
