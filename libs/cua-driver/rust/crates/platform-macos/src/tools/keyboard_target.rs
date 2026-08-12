//! Exact AX target resolution shared by background keyboard routes.

use core_foundation::base::{CFEqual, CFRelease, CFRetain, CFTypeRef};

use crate::ax::bindings::{
    copy_bool_attr, copy_element_attr, element_pid, is_attribute_settable, AXUIElementRef,
};

const MAX_PROCESS_ANCESTORS: usize = 64;

/// A retained effective keyboard element and the process that must receive its
/// targeted events. The application PID remains distinct because it owns the
/// window and synthetic-focus belief.
pub(super) struct KeyboardTarget {
    application_pid: i32,
    dispatch_pid: i32,
    element: Option<RetainedAxElement>,
}

impl KeyboardTarget {
    pub(super) fn resolve(
        application_pid: i32,
        element_ptr: Option<usize>,
    ) -> anyhow::Result<Self> {
        let Some(element_ptr) = element_ptr else {
            return Ok(Self {
                application_pid,
                dispatch_pid: application_pid,
                element: None,
            });
        };
        let focused = unsafe { RetainedAxElement::from_borrowed(element_ptr as AXUIElementRef) };
        let focused_pid = unsafe { element_pid(focused.as_ptr()) }
            .map_err(|error| anyhow::anyhow!("AXUIElementGetPid failed with status {error}"))?;
        if focused_pid != application_pid {
            return Ok(Self {
                application_pid,
                dispatch_pid: focused_pid,
                element: Some(focused),
            });
        }

        // Match the signed helper's out-of-process containing-element lookup.
        // A view-bridge/web-content ancestor may own the effective dispatch PID
        // even when the leaf reports the top-level application PID.
        let mut current = focused.clone();
        let mut seen = vec![current.clone()];
        for _ in 0..MAX_PROCESS_ANCESTORS {
            let Some(parent) = (unsafe { copy_element_attr(current.as_ptr(), "AXParent") })
                .map(|element| unsafe { RetainedAxElement::from_owned(element) })
            else {
                return Ok(Self {
                    application_pid,
                    dispatch_pid: focused_pid,
                    element: Some(focused),
                });
            };
            if seen.iter().any(|prior| prior.same_identity(&parent)) {
                anyhow::bail!("effective keyboard target ancestry contains a cycle");
            }
            let parent_pid = unsafe { element_pid(parent.as_ptr()) }.map_err(|error| {
                anyhow::anyhow!("AXUIElementGetPid failed during parent walk with status {error}")
            })?;
            if parent_pid != application_pid {
                return Ok(Self {
                    application_pid,
                    dispatch_pid: parent_pid,
                    element: Some(parent),
                });
            }
            seen.push(parent.clone());
            current = parent;
        }
        anyhow::bail!("effective keyboard target ancestry exceeded {MAX_PROCESS_ANCESTORS} levels")
    }

    pub(super) fn application_pid(&self) -> i32 {
        self.application_pid
    }

    pub(super) fn dispatch_pid(&self) -> i32 {
        self.dispatch_pid
    }

    pub(super) fn element_ptr(&self) -> Option<usize> {
        self.element
            .as_ref()
            .map(|element| element.as_ptr() as usize)
    }

    pub(super) fn is_out_of_process(&self) -> bool {
        self.application_pid != self.dispatch_pid
    }

    /// Restore only the already-resolved effective element, and only when AX
    /// explicitly says it is not focused. This is the helper's
    /// `focusFieldIfNeeded` posture; it never chooses a replacement control.
    pub(super) fn focus_field_if_needed(&self) -> anyhow::Result<bool> {
        let Some(element) = &self.element else {
            return Ok(false);
        };
        let focused = unsafe { copy_bool_attr(element.as_ptr(), "AXFocused") };
        if focused != Some(false) {
            return Ok(false);
        }
        if !unsafe { is_attribute_settable(element.as_ptr(), "AXFocused") } {
            anyhow::bail!("exact keyboard target is not focused and AXFocused is not writable");
        }
        crate::input::ax_actions::focus_element(element.as_ptr() as usize)?;
        if unsafe { copy_bool_attr(element.as_ptr(), "AXFocused") } != Some(true) {
            anyhow::bail!("exact keyboard target did not acknowledge AXFocused restoration");
        }
        Ok(true)
    }
}

pub(super) struct RetainedAxElement(AXUIElementRef);

impl RetainedAxElement {
    pub(super) unsafe fn from_owned(element: AXUIElementRef) -> Self {
        Self(element)
    }

    unsafe fn from_borrowed(element: AXUIElementRef) -> Self {
        CFRetain(element as CFTypeRef);
        Self(element)
    }

    pub(super) fn as_ptr(&self) -> AXUIElementRef {
        self.0
    }

    fn same_identity(&self, other: &Self) -> bool {
        unsafe { CFEqual(self.0 as CFTypeRef, other.0 as CFTypeRef) != 0 }
    }
}

impl Clone for RetainedAxElement {
    fn clone(&self) -> Self {
        unsafe { Self::from_borrowed(self.0) }
    }
}

unsafe impl Send for RetainedAxElement {}

impl Drop for RetainedAxElement {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}
