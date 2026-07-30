//! Raw FFI bindings to the macOS Accessibility API (AXUIElement).
//!
//! We call the C-level AX API directly rather than using a crate wrapper,
//! because most available crates are incomplete or unmaintained.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]

use core_foundation::{
    array::CFArrayRef,
    base::{CFRelease, CFRetain, CFTypeID, CFTypeRef},
    runloop::CFRunLoopSourceRef,
    string::CFStringRef,
};
use std::os::raw::{c_int, c_void};

// ── AXUIElement opaque type ──────────────────────────────────────────────────

#[repr(C)]
pub struct __AXUIElement(c_void);
pub type AXUIElementRef = *mut __AXUIElement;

// ── AXObserver opaque type ──────────────────────────────────────────────────

#[repr(C)]
pub struct __AXObserver(c_void);
pub type AXObserverRef = *mut __AXObserver;
pub type AXObserverCallback = unsafe extern "C" fn(
    observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
);

// ── AXError ──────────────────────────────────────────────────────────────────

pub type AXError = c_int;
pub const kAXErrorSuccess: AXError = 0;
pub const kAXErrorFailure: AXError = -25200;
pub const kAXErrorInvalidUIElement: AXError = -25202;
pub const kAXErrorAttributeUnsupported: AXError = -25205;
pub const kAXErrorNoValue: AXError = -25212;
pub const kAXErrorAPIDisabled: AXError = -25211;

// ── AXValue opaque type ──────────────────────────────────────────────────────

#[repr(C)]
pub struct __AXValue(c_void);
pub type AXValueRef = *mut __AXValue;

pub type AXValueType = c_int;
pub const kAXValueCGPointType: AXValueType = 1;
pub const kAXValueCGSizeType: AXValueType = 2;
pub const kAXValueCGRectType: AXValueType = 3;
pub const kAXValueCFRangeType: AXValueType = 4;
pub const kAXValueIllegalType: AXValueType = 1_000;

// ── Link to AXUIElement functions ────────────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    pub fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    pub fn AXUIElementCopyAttributeNames(
        element: AXUIElementRef,
        names: *mut CFArrayRef,
    ) -> AXError;
    pub fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> AXError;
    pub fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    pub fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    pub fn AXUIElementIsAttributeSettable(
        element: AXUIElementRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> AXError;
    pub fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> AXError;
    pub fn AXUIElementGetTypeID() -> CFTypeID;
    pub fn AXObserverCreate(
        application: i32,
        callback: AXObserverCallback,
        observer: *mut AXObserverRef,
    ) -> AXError;
    pub fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        refcon: *mut c_void,
    ) -> AXError;
    pub fn AXObserverRemoveNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
    ) -> AXError;
    pub fn AXObserverGetRunLoopSource(observer: AXObserverRef) -> CFRunLoopSourceRef;
    pub fn AXIsProcessTrusted() -> bool;
    /// `AXIsProcessTrustedWithOptions(options)` — when called with
    /// `{kAXTrustedCheckOptionPrompt: true}` raises the system Accessibility
    /// prompt if the process isn't already trusted.  Returns the post-prompt
    /// trust state (may still be false if the user dismissed the prompt).
    pub fn AXIsProcessTrustedWithOptions(
        options: core_foundation::dictionary::CFDictionaryRef,
    ) -> bool;

    /// Private SPI: maps an AX window element to its CGWindowID.
    /// Stable since macOS 10.9; used by yabai, Hammerspoon, Accessibility Inspector.
    pub fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> AXError;
}

// ── AXValue functions ────────────────────────────────────────────────────────
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXValueCreate(the_type: AXValueType, value_ptr: *const c_void) -> AXValueRef;
    pub fn AXValueGetTypeID() -> CFTypeID;
    pub fn AXValueGetType(value: AXValueRef) -> AXValueType;
    pub fn AXValueGetValue(
        value: AXValueRef,
        the_type: AXValueType,
        value_ptr: *mut c_void,
    ) -> bool;
}

// ── Helper functions ──────────────────────────────────────────────────────────

use core_foundation::{array::CFArray, base::TCFType, string::CFString as CFStr};

/// Copy a string attribute from an AX element. Returns `None` on any error.
pub unsafe fn copy_string_attr(element: AXUIElementRef, attr_name: &str) -> Option<String> {
    copy_string_attr_exact(element, attr_name).ok().flatten()
}

/// Copy a string attribute without collapsing query/type failures into a
/// truthful missing value.
pub unsafe fn copy_string_attr_exact(
    element: AXUIElementRef,
    attr_name: &str,
) -> Result<Option<String>, AXError> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err == kAXErrorNoValue || err == kAXErrorAttributeUnsupported {
        return Ok(None);
    }
    if err != kAXErrorSuccess {
        return Err(err);
    }
    if value.is_null() {
        return Ok(None);
    }
    let cf_string_type_id = CFStr::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_string_type_id {
        CFRelease(value);
        return Err(kAXErrorFailure);
    }
    let s = CFStr::wrap_under_create_rule(value as _);
    Ok(Some(s.to_string()))
}

/// Copy a numeric attribute from an AX element as an `f64`. Returns `None` on
/// any error or if the attribute is not a `CFNumber`. SwiftUI sliders expose a
/// readable numeric `AXValue` even when that value is not settable — this lets
/// the stepping fallback read the control's current position for feedback.
pub unsafe fn copy_number_attr(element: AXUIElementRef, attr_name: &str) -> Option<f64> {
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let cf_number_type_id = CFNumber::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_number_type_id {
        CFRelease(value);
        return None;
    }
    let n = CFNumber::wrap_under_create_rule(value as _);
    n.to_f64()
}

/// Copy a boolean attribute from an AX element. Returns `None` when the
/// attribute is missing, unsupported, or not a CFBoolean.
pub unsafe fn copy_bool_attr(element: AXUIElementRef, attr_name: &str) -> Option<bool> {
    copy_bool_attr_exact(element, attr_name).ok().flatten()
}

/// Copy a boolean attribute without collapsing query/type failures into a
/// truthful missing value.
///
/// # Safety
///
/// `element` must be a live AX element reference for the duration of the call.
pub unsafe fn copy_bool_attr_exact(
    element: AXUIElementRef,
    attr_name: &str,
) -> Result<Option<bool>, AXError> {
    use core_foundation::boolean::CFBoolean;

    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err == kAXErrorNoValue || err == kAXErrorAttributeUnsupported {
        return Ok(None);
    }
    if err != kAXErrorSuccess {
        return Err(err);
    }
    if value.is_null() {
        return Ok(None);
    }
    if core_foundation::base::CFGetTypeID(value) != CFBoolean::type_id() {
        CFRelease(value);
        return Err(kAXErrorFailure);
    }
    let boolean = CFBoolean::wrap_under_create_rule(value as _);
    Ok(Some(bool::from(boolean)))
}

/// Read the process which owns an AX element without inferring ownership from
/// the top-level application used to discover it.
///
/// # Safety
///
/// `element` must be a live AX element reference for the duration of the call.
pub unsafe fn element_pid(element: AXUIElementRef) -> Result<i32, AXError> {
    let mut pid = 0_i32;
    let error = AXUIElementGetPid(element, &mut pid);
    if error != kAXErrorSuccess {
        return Err(error);
    }
    if pid <= 0 {
        return Err(kAXErrorFailure);
    }
    Ok(pid)
}

/// Get the action names for an AX element.
pub unsafe fn copy_action_names(element: AXUIElementRef) -> Vec<String> {
    copy_action_names_exact(element).unwrap_or_default()
}

/// Get the exact current action-name list without collapsing an AX query
/// failure into a truthful empty list.
pub unsafe fn copy_action_names_exact(element: AXUIElementRef) -> Result<Vec<String>, AXError> {
    let mut names: CFArrayRef = std::ptr::null_mut();
    let err = AXUIElementCopyActionNames(element, &mut names);
    if err != kAXErrorSuccess {
        return Err(err);
    }
    if names.is_null() {
        return Err(kAXErrorFailure);
    }
    // Use CFArray<CFStr> (the typed wrapper) to satisfy FromVoid bound.
    let arr = CFArray::<CFStr>::wrap_under_create_rule(names);
    Ok((0..arr.len())
        .filter_map(|i| {
            let cf = arr.get(i)?;
            Some(cf.to_string())
        })
        .collect())
}

/// Copy an arbitrary AX attribute value under the create rule. The caller
/// owns any non-null result and must release it.
pub unsafe fn copy_attr_value(
    element: AXUIElementRef,
    attr_name: &str,
) -> Result<Option<CFTypeRef>, AXError> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err == kAXErrorNoValue || err == kAXErrorAttributeUnsupported {
        return Ok(None);
    }
    if err != kAXErrorSuccess {
        return Err(err);
    }
    if value.is_null() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

/// Core Foundation's `CFRange` layout, used by `AXSelectedTextRange`.
/// Locations and lengths are UTF-16 code-unit offsets for AX text elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct AxCfRange {
    pub location: isize,
    pub length: isize,
}

impl AxCfRange {
    pub fn from_utf16(location: usize, length: usize) -> Option<Self> {
        Some(Self {
            location: isize::try_from(location).ok()?,
            length: isize::try_from(length).ok()?,
        })
    }
}

/// Return whether AX says an attribute is writable. Errors and indeterminate
/// answers are not promoted to `false`: callers need the distinction to avoid
/// guessing a route from an incomplete preflight.
pub unsafe fn is_attribute_settable(
    element: AXUIElementRef,
    attr_name: &str,
) -> Result<bool, AXError> {
    let attr = CFStr::new(attr_name);
    let mut settable = 0_u8;
    let err = AXUIElementIsAttributeSettable(element, attr.as_concrete_TypeRef(), &mut settable);
    if err == kAXErrorSuccess {
        Ok(settable != 0)
    } else {
        Err(err)
    }
}

/// Copy an AX element-valued attribute. The returned reference follows the
/// create rule and must be released by the caller.
pub unsafe fn copy_element_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Result<Option<AXUIElementRef>, AXError> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err == kAXErrorNoValue || err == kAXErrorAttributeUnsupported {
        return Ok(None);
    }
    if err != kAXErrorSuccess {
        return Err(err);
    }
    if value.is_null() {
        return Ok(None);
    }
    if core_foundation::base::CFGetTypeID(value) != AXUIElementGetTypeID() {
        CFRelease(value);
        return Err(kAXErrorFailure);
    }
    Ok(Some(value as AXUIElementRef))
}

/// Copy a `kAXValueCFRangeType` attribute.
pub unsafe fn copy_cf_range_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Result<Option<AxCfRange>, AXError> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err == kAXErrorNoValue || err == kAXErrorAttributeUnsupported {
        return Ok(None);
    }
    if err != kAXErrorSuccess {
        return Err(err);
    }
    if value.is_null() {
        return Ok(None);
    }
    if core_foundation::base::CFGetTypeID(value) != AXValueGetTypeID() {
        CFRelease(value);
        return Err(kAXErrorFailure);
    }
    let value = value as AXValueRef;
    if AXValueGetType(value) != kAXValueCFRangeType {
        CFRelease(value as CFTypeRef);
        return Err(kAXErrorFailure);
    }
    let mut range = AxCfRange {
        location: 0,
        length: 0,
    };
    let copied = AXValueGetValue(
        value,
        kAXValueCFRangeType,
        &mut range as *mut _ as *mut c_void,
    );
    CFRelease(value as CFTypeRef);
    if copied {
        Ok(Some(range))
    } else {
        Err(kAXErrorFailure)
    }
}

/// Copy a `kAXValueCGPointType` attribute. Missing and unsupported attributes
/// are optional; query and type failures remain distinguishable to callers
/// that need an exact read.
pub unsafe fn copy_point_attr(
    element: AXUIElementRef,
    attr_name: &str,
) -> Result<Option<(f64, f64)>, AXError> {
    let attr = CFStr::new(attr_name);
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err == kAXErrorNoValue || err == kAXErrorAttributeUnsupported {
        return Ok(None);
    }
    if err != kAXErrorSuccess {
        return Err(err);
    }
    if value.is_null() {
        return Ok(None);
    }
    if core_foundation::base::CFGetTypeID(value) != AXValueGetTypeID() {
        CFRelease(value);
        return Err(kAXErrorFailure);
    }
    let value = value as AXValueRef;
    if AXValueGetType(value) != kAXValueCGPointType {
        CFRelease(value as CFTypeRef);
        return Err(kAXErrorFailure);
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut point = CGPoint { x: 0.0, y: 0.0 };
    let copied = AXValueGetValue(
        value,
        kAXValueCGPointType,
        &mut point as *mut _ as *mut c_void,
    );
    CFRelease(value as CFTypeRef);
    if copied {
        Ok(Some((point.x, point.y)))
    } else {
        Err(kAXErrorFailure)
    }
}

/// Set a `kAXValueCFRangeType` attribute.
pub unsafe fn set_cf_range_attr(
    element: AXUIElementRef,
    attr_name: &str,
    range: AxCfRange,
) -> AXError {
    let value = AXValueCreate(kAXValueCFRangeType, &range as *const _ as *const c_void);
    if value.is_null() {
        return kAXErrorFailure;
    }
    let attr = CFStr::new(attr_name);
    let err = AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), value as CFTypeRef);
    CFRelease(value as CFTypeRef);
    err
}

/// Read the on-screen center of an AX element (AXPosition + AXSize → center).
/// Returns `(cx, cy)` in screen coordinates, or `None` if either attribute
/// is unavailable or the element has zero size.
pub unsafe fn element_screen_center(element: AXUIElementRef) -> Option<(f64, f64)> {
    // AXPosition → CGPoint
    let pos_attr = CFStr::new("AXPosition");
    let mut pos_ref: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, pos_attr.as_concrete_TypeRef(), &mut pos_ref);
    if err != kAXErrorSuccess || pos_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut pos = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        pos_ref as AXValueRef,
        kAXValueCGPointType,
        &mut pos as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(pos_ref);
    if !ok {
        return None;
    }

    // AXSize → CGSize
    let sz_attr = CFStr::new("AXSize");
    let mut sz_ref: CFTypeRef = std::ptr::null();
    let err2 = AXUIElementCopyAttributeValue(element, sz_attr.as_concrete_TypeRef(), &mut sz_ref);
    if err2 != kAXErrorSuccess || sz_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let mut sz = CGSize { w: 0.0, h: 0.0 };
    let ok2 = AXValueGetValue(
        sz_ref as AXValueRef,
        kAXValueCGSizeType,
        &mut sz as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(sz_ref);
    if !ok2 || sz.w < 1.0 || sz.h < 1.0 {
        return None;
    }

    Some((pos.x + sz.w / 2.0, pos.y + sz.h / 2.0))
}

/// Read the on-screen bounding rect of an AX element.
/// Returns `[x, y, width, height]` in screen coordinates (top-left origin), or `None`.
pub unsafe fn element_screen_rect(element: AXUIElementRef) -> Option<[f64; 4]> {
    // AXPosition → CGPoint
    let pos_attr = CFStr::new("AXPosition");
    let mut pos_ref: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, pos_attr.as_concrete_TypeRef(), &mut pos_ref);
    if err != kAXErrorSuccess || pos_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    let mut pos = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        pos_ref as AXValueRef,
        kAXValueCGPointType,
        &mut pos as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(pos_ref);
    if !ok {
        return None;
    }

    // AXSize → CGSize
    let sz_attr = CFStr::new("AXSize");
    let mut sz_ref: CFTypeRef = std::ptr::null();
    let err2 = AXUIElementCopyAttributeValue(element, sz_attr.as_concrete_TypeRef(), &mut sz_ref);
    if err2 != kAXErrorSuccess || sz_ref.is_null() {
        return None;
    }
    #[repr(C)]
    struct CGSize {
        w: f64,
        h: f64,
    }
    let mut sz = CGSize { w: 0.0, h: 0.0 };
    let ok2 = AXValueGetValue(
        sz_ref as AXValueRef,
        kAXValueCGSizeType,
        &mut sz as *mut _ as *mut std::ffi::c_void,
    );
    CFRelease(sz_ref);
    if !ok2 || sz.w < 1.0 || sz.h < 1.0 {
        return None;
    }

    Some([pos.x, pos.y, sz.w, sz.h])
}

/// Get the focused UI element of a running application by pid.
/// Returns a retained `AXUIElementRef` that the caller must release, or `None`.
pub unsafe fn focused_element_of_pid(pid: i32) -> Option<AXUIElementRef> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let attr = CFStr::new("AXFocusedUIElement");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(app, attr.as_concrete_TypeRef(), &mut value);
    CFRelease(app as CFTypeRef);
    if err != kAXErrorSuccess || value.is_null() {
        return None;
    }
    let ax_type_id = AXUIElementGetTypeID();
    if core_foundation::base::CFGetTypeID(value) != ax_type_id {
        CFRelease(value);
        return None;
    }
    // Already retained by CopyAttributeValue — hand the raw pointer to the caller.
    Some(value as AXUIElementRef)
}

/// Get the children of an AX element.
pub unsafe fn copy_children(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    let attr = CFStr::new("AXChildren");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return vec![];
    }
    let cf_array_type_id = CFArray::<CFTypeRef>::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_array_type_id {
        CFRelease(value);
        return vec![];
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    (0..arr.len())
        .filter_map(|i| {
            let item = *arr.get(i)?;
            if core_foundation::base::CFGetTypeID(item) == ax_type_id {
                // Retain so we own it — caller is responsible for releasing.
                CFRetain(item);
                Some(item as AXUIElementRef)
            } else {
                None
            }
        })
        .collect()
}

/// Perform an AX action using a string attribute name.
pub unsafe fn perform_action(element: AXUIElementRef, action_name: &str) -> AXError {
    let action = CFStr::new(action_name);
    AXUIElementPerformAction(element, action.as_concrete_TypeRef())
}

/// Set an AX attribute to a CFString value.
pub unsafe fn set_string_attr(element: AXUIElementRef, attr_name: &str, value: &str) -> AXError {
    let attr = CFStr::new(attr_name);
    let cf_value = CFStr::new(value);
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_value.as_CFTypeRef())
}

/// Set an AX attribute to a CFNumber (double) value. Numeric controls — most
/// notably `AXSlider` (NSSlider) and `AXStepper` — expose a numeric `AXValue`
/// reject a `CFString` write — `-25200` (kAXErrorFailure, observed live on a
/// SwiftUI `AXSlider`) or `-25201` (kAXErrorIllegalArgument); only a `CFNumber`
/// is accepted. Text fields, by contrast, take a `CFString`.
pub unsafe fn set_number_attr(element: AXUIElementRef, attr_name: &str, value: f64) -> AXError {
    use core_foundation::number::CFNumber;
    let attr = CFStr::new(attr_name);
    let cf_value = CFNumber::from(value);
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_value.as_CFTypeRef())
}

/// Set an AX attribute to a CFBoolean true value.
pub unsafe fn set_bool_attr_true(element: AXUIElementRef, attr_name: &str) -> AXError {
    use core_foundation::boolean::CFBoolean;
    let attr = CFStr::new(attr_name);
    let cf_true = CFBoolean::true_value();
    AXUIElementSetAttributeValue(element, attr.as_concrete_TypeRef(), cf_true.as_CFTypeRef())
}

/// Signal to a Chromium/Electron application root that a real assistive client
/// is present so it materializes its full web-content accessibility tree.
///
/// Returns `true` when an attribute write was accepted — meaning the app was
/// flipped from "tree off" to "tree building" and the caller should let the
/// tree settle before walking. Returns `false` when the app does not support
/// either attribute (native Cocoa apps such as Finder / Calculator / TextEdit),
/// in which case no settle delay is warranted.
///
/// `AXManualAccessibility` is the modern opt-in with no screen-reader side
/// effects; `AXEnhancedUserInterface` is the legacy fallback some Electron
/// builds expose instead (the modern attribute returns
/// `kAXErrorAttributeUnsupported` on those builds).
pub unsafe fn enable_chromium_accessibility(app_element: AXUIElementRef) -> bool {
    let manual = set_bool_attr_true(app_element, "AXManualAccessibility");
    if manual == kAXErrorSuccess {
        return true;
    }
    if manual != kAXErrorAttributeUnsupported {
        // A transient error (e.g. timeout / app busy) rather than a hard
        // "this app has no such attribute" — don't bother with the legacy
        // fallback, and don't claim enablement happened.
        return false;
    }
    set_bool_attr_true(app_element, "AXEnhancedUserInterface") == kAXErrorSuccess
}

/// Get the CGWindowID of an AX window element via the private `_AXUIElementGetWindow` SPI.
/// Returns `None` if the element is not a composited window.
pub unsafe fn ax_get_window_id(element: AXUIElementRef) -> Option<u32> {
    let mut wid: u32 = 0;
    let err = _AXUIElementGetWindow(element, &mut wid);
    if err == kAXErrorSuccess && wid != 0 {
        Some(wid)
    } else {
        None
    }
}

/// Read the `AXWindows` attribute of an application element.
/// Unlike `AXChildren`, this returns the window list regardless of whether
/// the app is frontmost. Returns a Vec of retained AXUIElementRefs.
pub unsafe fn copy_ax_windows(element: AXUIElementRef) -> Vec<AXUIElementRef> {
    let attr = CFStr::new("AXWindows");
    let mut value: CFTypeRef = std::ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value);
    if err != kAXErrorSuccess || value.is_null() {
        return vec![];
    }
    let cf_array_type_id = CFArray::<CFTypeRef>::type_id();
    if core_foundation::base::CFGetTypeID(value) != cf_array_type_id {
        CFRelease(value);
        return vec![];
    }
    let arr = CFArray::<CFTypeRef>::wrap_under_create_rule(value as _);
    let ax_type_id = AXUIElementGetTypeID();
    (0..arr.len())
        .filter_map(|i| {
            let item = *arr.get(i)?;
            if core_foundation::base::CFGetTypeID(item) == ax_type_id {
                CFRetain(item);
                Some(item as AXUIElementRef)
            } else {
                None
            }
        })
        .collect()
}
