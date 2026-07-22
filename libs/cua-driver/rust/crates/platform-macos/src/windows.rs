//! macOS window enumeration via CGWindowList APIs.
//!
//! Uses the C-level CGWindowListCopyWindowInfo API which returns a CFArray
//! of CFDictionary objects describing each window.

use serde::{Deserialize, Serialize};
use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub window_id: u32,
    pub pid: i32,
    pub app_name: String,
    pub title: String,
    pub bounds: WindowBounds,
    pub layer: i32,
    pub z_index: usize,
    pub is_on_screen: bool,
    pub on_current_space: Option<bool>,
    pub space_ids: Option<Vec<u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceFacts {
    pub space_ids: Vec<u64>,
    pub current_space_ids: Vec<u64>,
    pub on_current_space: bool,
}

// ── CGWindow option flags ─────────────────────────────────────────────────────
// Apple-canonical kCG* naming preserved to match the public Apple headers — the
// upper-case-globals lint would rename them to KCG_..., which would silently
// shadow the Apple-namespaced constant references in any future code that
// re-introduces them. Mirrors platform-windows::uia/windows_enum.rs which uses
// the same allow for UIA_* constants.
#[allow(non_upper_case_globals)]
const kCGWindowListExcludeDesktopElements: u32 = 16;
#[allow(non_upper_case_globals)]
const kCGWindowListOptionOnScreenOnly: u32 = 1;
#[allow(non_upper_case_globals)]
const kCGNullWindowID: u32 = 0;

// ── Internal CGWindowInfo parsing ─────────────────────────────────────────────
//
// We use `system_profiler` workaround via `CGWindowListCopyWindowInfo` which
// returns a plist-like structure. The simplest cross-compile-safe approach
// is to dump via `osascript` or use the Objective-C runtime.
//
// For the initial version we use the `core-foundation` crate + direct C linkage.

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(
        option: u32,
        relativeToWindow: u32,
    ) -> core_foundation::array::CFArrayRef;
}

/// Enumerate all windows (including off-screen).
pub fn all_windows() -> Vec<WindowInfo> {
    enumerate_windows(kCGWindowListExcludeDesktopElements, false)
}

/// Enumerate non-desktop WindowServer surfaces on every layer.
///
/// This is intentionally not used for public window discovery. Observation
/// joins it against AX-owned menu/popover/sheet window ids so transient
/// surfaces are included only with structural ownership evidence.
pub fn all_windows_including_transients() -> Vec<WindowInfo> {
    enumerate_windows(kCGWindowListExcludeDesktopElements, true)
}

/// Enumerate only on-screen windows.
pub fn visible_windows() -> Vec<WindowInfo> {
    enumerate_windows(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        false,
    )
}

fn enumerate_windows(options: u32, include_nonzero_layers: bool) -> Vec<WindowInfo> {
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, TCFType, CFTypeRef},
        dictionary::CFDictionary,
        string::CFString,
        number::CFNumber,
        boolean::CFBoolean,
    };
    use std::os::raw::c_void;

    let raw_ref = unsafe {
        CGWindowListCopyWindowInfo(options, kCGNullWindowID)
    };
    if raw_ref.is_null() {
        return vec![];
    }

    let raw: CFArray<CFTypeRef> = unsafe { CFArray::wrap_under_create_rule(raw_ref as _) };
    let total = raw.len() as usize;
    let mut result = Vec::new();

    for (idx, item) in raw.iter().enumerate() {
        let item = *item;
        // Each item should be a CFDictionary.
        let dict_type = CFDictionary::<*const c_void, *const c_void>::type_id();
        if unsafe { CFGetTypeID(item) } != dict_type {
            continue;
        }

        let dict: CFDictionary<*const c_void, *const c_void> = unsafe {
            CFDictionary::wrap_under_get_rule(item as _)
        };

        // Helper: get number from dict by key string.
        let get_num = |key: &str| -> i64 {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFNumber::type_id() {
                        CFNumber::wrap_under_get_rule(v as _).to_i64()
                    } else { None }
                })
                .unwrap_or(0)
        };

        let get_str = |key: &str| -> String {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFString::type_id() {
                        Some(CFString::wrap_under_get_rule(v as _).to_string())
                    } else { None }
                })
                .unwrap_or_default()
        };

        let get_bool = |key: &str| -> bool {
            let k = CFString::new(key);
            dict.find(k.as_concrete_TypeRef() as *const c_void)
                .map(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFBoolean::type_id() {
                        bool::from(CFBoolean::wrap_under_get_rule(v as _))
                    } else { false }
                })
                .unwrap_or(false)
        };

        let window_id = get_num("kCGWindowNumber") as u32;
        let pid = get_num("kCGWindowOwnerPID") as i32;
        let app_name = get_str("kCGWindowOwnerName");
        let title = get_str("kCGWindowName");
        let layer = get_num("kCGWindowLayer") as i32;
        let is_on_screen = get_bool("kCGWindowIsOnscreen");

        // Only include layer-0 windows.
        if !include_nonzero_layers && layer != 0 { continue; }

        // Parse bounds dict.
        let bounds = {
            let bk = CFString::new("kCGWindowBounds");
            dict.find(bk.as_concrete_TypeRef() as *const c_void)
                .and_then(|v| unsafe {
                    let v = *v;
                    if CFGetTypeID(v) == CFDictionary::<*const c_void, *const c_void>::type_id() {
                        let bd: CFDictionary<*const c_void, *const c_void> =
                            CFDictionary::wrap_under_get_rule(v as _);
                        let x = get_bounds_num(&bd, "X");
                        let y = get_bounds_num(&bd, "Y");
                        let w = get_bounds_num(&bd, "Width");
                        let h = get_bounds_num(&bd, "Height");
                        Some(WindowBounds { x, y, width: w, height: h })
                    } else { None }
                })
                .unwrap_or(WindowBounds { x: 0., y: 0., width: 0., height: 0. })
        };

        // z_index: CGWindowList front-to-back → assign reverse index.
        let z_index = total - idx;

        result.push(WindowInfo {
            window_id,
            pid,
            app_name,
            title,
            bounds,
            layer,
            z_index,
            is_on_screen,
            on_current_space: None,
            space_ids: None,
        });
    }

    result
}

fn get_bounds_num(
    dict: &core_foundation::dictionary::CFDictionary<*const std::os::raw::c_void, *const std::os::raw::c_void>,
    key: &str,
) -> f64 {
    use core_foundation::{
        base::{CFGetTypeID, TCFType},
        number::CFNumber,
        string::CFString,
    };
    use std::os::raw::c_void;

    let k = CFString::new(key);
    dict.find(k.as_concrete_TypeRef() as *const c_void)
        .and_then(|v| unsafe {
            let v = *v;
            if CFGetTypeID(v) == CFNumber::type_id() {
                CFNumber::wrap_under_get_rule(v as _).to_f64()
            } else { None }
        })
        .unwrap_or(0.0)
}

/// Look up a window's bounds by its CGWindowID.
///
/// Returns `None` if the window is not currently known to WindowServer
/// (e.g. it was closed or the window_id is stale).
pub fn window_bounds_by_id(window_id: u32) -> Option<WindowBounds> {
    all_windows()
        .into_iter()
        .find(|w| w.window_id == window_id)
        .map(|w| w.bounds)
}

/// Select the best window_id for a pid.
pub fn resolve_main_window_id(pid: i32) -> anyhow::Result<u32> {
    let windows = all_windows();
    let pid_windows: Vec<&WindowInfo> = windows.iter().filter(|w| w.pid == pid).collect();
    if pid_windows.is_empty() {
        anyhow::bail!("pid {pid} has no windows");
    }
    let mut on_screen: Vec<&&WindowInfo> = pid_windows.iter().filter(|w| w.is_on_screen).collect();
    if !on_screen.is_empty() {
        on_screen.sort_by(|a, b| b.z_index.cmp(&a.z_index));
        return Ok(on_screen[0].window_id);
    }
    let largest = pid_windows.iter().max_by(|a, b| {
        let area_a = a.bounds.width * a.bounds.height;
        let area_b = b.bounds.width * b.bounds.height;
        area_a.partial_cmp(&area_b).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(largest.unwrap().window_id)
}

/// Backing scale for the display containing the largest area of `bounds`.
/// Returns `None` when no active display intersects the window.
pub fn display_scale_for_bounds(bounds: &WindowBounds) -> Option<f64> {
    use core_graphics::display::CGDisplay;

    let mut best: Option<(f64, f64)> = None;
    for display_id in CGDisplay::active_displays().ok()? {
        let display = CGDisplay::new(display_id);
        let frame = display.bounds();
        let left = bounds.x.max(frame.origin.x);
        let top = bounds.y.max(frame.origin.y);
        let right = (bounds.x + bounds.width).min(frame.origin.x + frame.size.width);
        let bottom = (bounds.y + bounds.height).min(frame.origin.y + frame.size.height);
        let area = (right - left).max(0.0) * (bottom - top).max(0.0);
        if area <= 0.0 || frame.size.width <= 0.0 {
            continue;
        }
        let scale = display.pixels_wide() as f64 / frame.size.width;
        if best.map_or(true, |(best_area, _)| area > best_area) {
            best = Some((area, scale));
        }
    }
    best.map(|(_, scale)| scale)
}

type CopySpacesForWindowsFn = unsafe extern "C" fn(
    u32,
    u32,
    core_foundation::array::CFArrayRef,
) -> core_foundation::array::CFArrayRef;
type CopyManagedDisplaySpacesFn = unsafe extern "C" fn(u32) -> core_foundation::array::CFArrayRef;

fn load_skylight_symbol(names: &[&[u8]]) -> Option<*mut c_void> {
    static LOADED: OnceLock<()> = OnceLock::new();
    LOADED.get_or_init(|| unsafe {
        let path = b"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight\0";
        libc::dlopen(
            path.as_ptr() as *const c_char,
            libc::RTLD_LAZY | libc::RTLD_GLOBAL,
        );
    });
    names.iter().find_map(|name| {
        let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char) };
        (!ptr.is_null()).then_some(ptr)
    })
}

unsafe fn symbol_as<T: Copy>(pointer: *mut c_void) -> T {
    std::mem::transmute_copy::<*mut c_void, T>(&pointer)
}

fn copy_spaces_for_windows_fn() -> Option<CopySpacesForWindowsFn> {
    static SYMBOL: OnceLock<Option<CopySpacesForWindowsFn>> = OnceLock::new();
    *SYMBOL.get_or_init(|| {
        load_skylight_symbol(&[b"SLSCopySpacesForWindows\0", b"CGSCopySpacesForWindows\0"])
            .map(|pointer| unsafe { symbol_as(pointer) })
    })
}

fn copy_managed_display_spaces_fn() -> Option<CopyManagedDisplaySpacesFn> {
    static SYMBOL: OnceLock<Option<CopyManagedDisplaySpacesFn>> = OnceLock::new();
    *SYMBOL.get_or_init(|| {
        load_skylight_symbol(&[
            b"SLSCopyManagedDisplaySpaces\0",
            b"CGSCopyManagedDisplaySpaces\0",
        ])
        .map(|pointer| unsafe { symbol_as(pointer) })
    })
}

pub fn space_query_available() -> bool {
    crate::input::skylight::main_connection_id().is_some()
        && copy_spaces_for_windows_fn().is_some()
        && copy_managed_display_spaces_fn().is_some()
}

/// Read a window's managed Space ids and the active Space on every display.
/// All symbols are private SkyLight reads and are resolved dynamically, so an
/// unavailable API is reported as `None` rather than guessed from visibility.
pub fn space_facts(window_id: u32) -> Option<SpaceFacts> {
    let connection = crate::input::skylight::main_connection_id()?;
    let window_spaces = copy_window_space_ids(connection, window_id)?;
    let current_spaces = copy_current_space_ids(connection)?;
    let on_current_space = window_spaces
        .iter()
        .any(|space| current_spaces.contains(space));
    Some(SpaceFacts {
        space_ids: window_spaces,
        current_space_ids: current_spaces,
        on_current_space,
    })
}

fn copy_window_space_ids(connection: u32, window_id: u32) -> Option<Vec<u64>> {
    use core_foundation::{array::CFArray, base::TCFType, number::CFNumber};

    let window = CFNumber::from(window_id as i64);
    let windows = CFArray::from_CFTypes(&[window]);
    let raw =
        unsafe { copy_spaces_for_windows_fn()?(connection, 0x7, windows.as_concrete_TypeRef()) };
    if raw.is_null() {
        return None;
    }
    let spaces = unsafe { CFArray::<CFNumber>::wrap_under_create_rule(raw) };
    Some(
        spaces
            .iter()
            .filter_map(|space| space.to_i64())
            .filter_map(|space| u64::try_from(space).ok())
            .collect(),
    )
}

fn copy_current_space_ids(connection: u32) -> Option<Vec<u64>> {
    use core_foundation::{
        array::CFArray,
        base::{CFGetTypeID, CFTypeRef, TCFType},
        dictionary::CFDictionary,
        number::CFNumber,
        string::CFString,
    };

    let raw = unsafe { copy_managed_display_spaces_fn()?(connection) };
    if raw.is_null() {
        return None;
    }
    let displays = unsafe { CFArray::<CFTypeRef>::wrap_under_create_rule(raw) };
    let mut result = Vec::new();
    for display in displays.iter() {
        let display = *display;
        if unsafe { CFGetTypeID(display) }
            != CFDictionary::<*const c_void, *const c_void>::type_id()
        {
            continue;
        }
        let display = unsafe {
            CFDictionary::<*const c_void, *const c_void>::wrap_under_get_rule(display as _)
        };
        let current_key = CFString::new("Current Space");
        let Some(current) = display.find(current_key.as_concrete_TypeRef() as *const c_void) else {
            continue;
        };
        let current = *current;
        if unsafe { CFGetTypeID(current) }
            != CFDictionary::<*const c_void, *const c_void>::type_id()
        {
            continue;
        }
        let current = unsafe {
            CFDictionary::<*const c_void, *const c_void>::wrap_under_get_rule(current as _)
        };
        let id = ["ManagedSpaceID", "id64"].iter().find_map(|key| {
            let key = CFString::new(key);
            let value = *current.find(key.as_concrete_TypeRef() as *const c_void)?;
            if unsafe { CFGetTypeID(value) } != CFNumber::type_id() {
                return None;
            }
            unsafe { CFNumber::wrap_under_get_rule(value as _) }
                .to_i64()
                .and_then(|value| u64::try_from(value).ok())
        });
        if let Some(id) = id {
            result.push(id);
        }
    }
    Some(result)
}
