//! Prior-state-preserving scoped AX enablement for Chromium accessibility.

use std::sync::Arc;

use core_foundation::{
    base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
    boolean::CFBoolean,
    string::CFString,
};
use cua_driver_core::api::errors::{ErrorCode, ErrorPhase, NativeError};

use super::bindings::{
    kAXErrorAttributeUnsupported, kAXErrorNoValue, kAXErrorSuccess, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementSetAttributeValue,
};

const ATTRIBUTES: [&str; 2] = ["AXManualAccessibility", "AXEnhancedUserInterface"];

pub(crate) trait AxBooleanAccess: Send + Sync {
    fn read(&self, pid: i32, attribute: &'static str) -> Result<Option<bool>, NativeError>;
    fn write(
        &self,
        pid: i32,
        attribute: &'static str,
        value: bool,
        phase: ErrorPhase,
    ) -> Result<(), NativeError>;
}

#[derive(Default)]
struct SystemAxBooleanAccess;

impl AxBooleanAccess for SystemAxBooleanAccess {
    fn read(&self, pid: i32, attribute: &'static str) -> Result<Option<bool>, NativeError> {
        unsafe {
            let application = AXUIElementCreateApplication(pid);
            if application.is_null() {
                return Err(ax_error(
                    ErrorPhase::Preflight,
                    true,
                    "AX application element is unavailable during enablement preflight",
                    pid,
                    attribute,
                    None,
                ));
            }
            let name = CFString::new(attribute);
            let mut value: CFTypeRef = std::ptr::null();
            let status =
                AXUIElementCopyAttributeValue(application, name.as_concrete_TypeRef(), &mut value);
            CFRelease(application as CFTypeRef);
            if status == kAXErrorAttributeUnsupported || status == kAXErrorNoValue {
                return Ok(None);
            }
            if status != kAXErrorSuccess || value.is_null() {
                return Err(ax_error(
                    ErrorPhase::Preflight,
                    true,
                    "AX enablement state could not be read exactly",
                    pid,
                    attribute,
                    Some(status),
                ));
            }
            if CFGetTypeID(value) != CFBoolean::type_id() {
                CFRelease(value);
                return Err(ax_error(
                    ErrorPhase::Preflight,
                    false,
                    "AX enablement attribute was not a boolean",
                    pid,
                    attribute,
                    None,
                ));
            }
            let value = CFBoolean::wrap_under_create_rule(value.cast_mut().cast());
            Ok(Some(bool::from(value)))
        }
    }

    fn write(
        &self,
        pid: i32,
        attribute: &'static str,
        value: bool,
        phase: ErrorPhase,
    ) -> Result<(), NativeError> {
        unsafe {
            let application = AXUIElementCreateApplication(pid);
            if application.is_null() {
                return Err(ax_error(
                    phase,
                    true,
                    "AX application element is unavailable during enablement write",
                    pid,
                    attribute,
                    None,
                ));
            }
            let name = CFString::new(attribute);
            let value = if value {
                CFBoolean::true_value()
            } else {
                CFBoolean::false_value()
            };
            let status = AXUIElementSetAttributeValue(
                application,
                name.as_concrete_TypeRef(),
                value.as_CFTypeRef(),
            );
            CFRelease(application as CFTypeRef);
            if status == kAXErrorSuccess {
                Ok(())
            } else {
                Err(ax_error(
                    phase,
                    true,
                    "AX enablement state write failed",
                    pid,
                    attribute,
                    Some(status),
                ))
            }
        }
    }
}

pub struct AxEnablementLease {
    pid: i32,
    attribute: &'static str,
    prior: bool,
    changed: bool,
    access: Arc<dyn AxBooleanAccess>,
    released: bool,
}

impl AxEnablementLease {
    pub fn acquire(pid: i32) -> Result<Self, NativeError> {
        Self::acquire_with(pid, Arc::new(SystemAxBooleanAccess))
    }

    fn acquire_with(pid: i32, access: Arc<dyn AxBooleanAccess>) -> Result<Self, NativeError> {
        for attribute in ATTRIBUTES {
            let Some(prior) = access.read(pid, attribute)? else {
                continue;
            };
            let changed = !prior;
            if changed {
                access.write(pid, attribute, true, ErrorPhase::Preflight)?;
            }
            return Ok(Self {
                pid,
                attribute,
                prior,
                changed,
                access,
                released: false,
            });
        }
        Err(NativeError::unsupported(
            "accessibility_enablement_unavailable: neither reversible Chromium AX enablement attribute is readable",
        )
        .with_detail("pid", pid))
    }

    pub fn attribute(&self) -> &'static str {
        self.attribute
    }

    pub fn prior(&self) -> bool {
        self.prior
    }

    pub fn changed(&self) -> bool {
        self.changed
    }

    pub fn release(&mut self) -> Result<(), NativeError> {
        if self.released {
            return Ok(());
        }
        if self.changed {
            self.access
                .write(self.pid, self.attribute, self.prior, ErrorPhase::Verify)?;
        }
        self.released = true;
        Ok(())
    }
}

impl Drop for AxEnablementLease {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracing::error!(error = %error, "failed to restore prior AX enablement state");
        }
    }
}

fn ax_error(
    phase: ErrorPhase,
    retryable: bool,
    message: &'static str,
    pid: i32,
    attribute: &'static str,
    status: Option<i32>,
) -> NativeError {
    let mut error = NativeError::new(ErrorCode::Internal, phase, retryable, message)
        .with_detail("pid", pid)
        .with_detail("attribute", attribute);
    if let Some(status) = status {
        error = error.with_detail("ax_status", status);
    }
    error
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use super::*;

    #[derive(Default)]
    struct FakeAccess {
        values: Mutex<HashMap<&'static str, Option<bool>>>,
        writes: Mutex<Vec<(&'static str, bool)>>,
    }

    impl AxBooleanAccess for FakeAccess {
        fn read(&self, _pid: i32, attribute: &'static str) -> Result<Option<bool>, NativeError> {
            Ok(self
                .values
                .lock()
                .unwrap()
                .get(attribute)
                .copied()
                .flatten())
        }

        fn write(
            &self,
            _pid: i32,
            attribute: &'static str,
            value: bool,
            _phase: ErrorPhase,
        ) -> Result<(), NativeError> {
            self.writes.lock().unwrap().push((attribute, value));
            Ok(())
        }
    }

    #[test]
    fn enablement_restores_the_exact_prior_attribute_value() {
        let access = Arc::new(FakeAccess::default());
        access
            .values
            .lock()
            .unwrap()
            .insert("AXManualAccessibility", Some(false));
        let mut lease = AxEnablementLease::acquire_with(42, access.clone()).unwrap();
        assert!(lease.changed());
        assert!(!lease.prior());
        lease.release().unwrap();
        assert_eq!(
            *access.writes.lock().unwrap(),
            vec![
                ("AXManualAccessibility", true),
                ("AXManualAccessibility", false)
            ]
        );
    }

    #[test]
    fn already_enabled_state_is_observed_without_a_write_or_clear() {
        let access = Arc::new(FakeAccess::default());
        access
            .values
            .lock()
            .unwrap()
            .insert("AXManualAccessibility", Some(true));
        let mut lease = AxEnablementLease::acquire_with(42, access.clone()).unwrap();
        assert!(!lease.changed());
        lease.release().unwrap();
        assert!(access.writes.lock().unwrap().is_empty());
    }
}
