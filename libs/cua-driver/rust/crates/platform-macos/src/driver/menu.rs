//! Exact action-bounded native-menu dismissal suppression.
//!
//! The controller-lifetime target event tap is installed in `focus_steal`.
//! Acquisition only switches its small locked predicate state on for the
//! current menu mutation. The recovered helper predicate permits events whose
//! source Unix PID is the target or native menu process and rejects other
//! mouse down/up/drag events as delivered to the target tap.

use std::sync::{Arc, Mutex};

use cua_driver_core::api::{
    contracts::{ActionId, MenuId},
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::NativeEvidence,
    menu::NativeMenuIdentity,
};

use super::{
    observation::RetainedAxElement,
    target::MacFocusState,
    windows::{MacWindowFacts, MacWindowRegistry},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuSuppressionPlan {
    NotApplicable,
    ExactSourcePidPredicate {
        target_pid: i32,
        menu_pid: Option<i32>,
        menu_id: MenuId,
        action_id: ActionId,
    },
}

pub(crate) trait MenuSuppressionResource: Send {
    fn release(&mut self) -> Result<NativeEvidence, NativeError>;
}

pub(crate) async fn resolve_menu_identity(
    windows: &MacWindowRegistry,
    parent: &MacWindowFacts,
    menu_window_id: u32,
    menu_element: &RetainedAxElement,
) -> Result<NativeMenuIdentity, NativeError> {
    if menu_window_id == parent.cg_window_id {
        return Ok(NativeMenuIdentity {
            process: parent.stamp.process.clone(),
            window: parent.stamp.native_window.clone(),
            generation: parent.stamp.generation,
        });
    }
    let mut related = windows
        .register_related_windows(
            parent,
            vec![(menu_window_id, menu_element.as_ptr() as usize)],
        )
        .await?;
    if related.len() != 1 {
        return Err(NativeError::stale(
            ErrorCode::MenuStateStale,
            "native menu discovery did not register exactly one related window",
        ));
    }
    let related = related.remove(0);
    Ok(NativeMenuIdentity {
        process: related.stamp.process,
        window: related.stamp.native_window,
        generation: related.stamp.generation,
    })
}

pub(crate) fn acquire_production(
    plan: &MenuSuppressionPlan,
    focus_state: Arc<Mutex<MacFocusState>>,
) -> Result<Option<Box<dyn MenuSuppressionResource>>, NativeError> {
    let MenuSuppressionPlan::ExactSourcePidPredicate {
        target_pid,
        menu_pid,
        menu_id,
        action_id,
    } = plan
    else {
        return Ok(None);
    };
    {
        let mut state = focus_state
            .lock()
            .map_err(|_| suppression_state_error("menu suppression state lock was poisoned"))?;
        if state.pid != *target_pid {
            return Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "menu suppression target PID no longer matches the focus coordinator",
            ));
        }
        if state.menu_suppression_action_id.is_some() || state.menu_suppression_menu_id.is_some() {
            return Err(NativeError::new(
                ErrorCode::TargetBusy,
                ErrorPhase::Preflight,
                true,
                "a menu dismissal suppression scope is already active for this target",
            ));
        }
        state.menu_pid = *menu_pid;
        state.menu_suppression_action_id = Some(action_id.clone());
        state.menu_suppression_menu_id = Some(menu_id.clone());
        state.menu_suppression_was_armed = false;
        state.menu_suppressed_event_count = 0;
        state.menu_last_suppressed_source_pid = None;
        state.menu_dismissal_suppression_enabled = false;
    }
    Ok(Some(Box::new(SystemMenuSuppressionResource {
        focus_state,
        target_pid: *target_pid,
        menu_pid: *menu_pid,
        menu_id: menu_id.clone(),
        action_id: action_id.clone(),
        released: false,
    })))
}

struct SystemMenuSuppressionResource {
    focus_state: Arc<Mutex<MacFocusState>>,
    target_pid: i32,
    menu_pid: Option<i32>,
    menu_id: MenuId,
    action_id: ActionId,
    released: bool,
}

impl SystemMenuSuppressionResource {
    fn release_inner(&mut self) -> Result<NativeEvidence, NativeError> {
        let mut state = self
            .focus_state
            .lock()
            .map_err(|_| suppression_state_error("menu suppression state lock was poisoned"))?;
        state.menu_dismissal_suppression_enabled = false;
        state.menu_pid = None;
        state.menu_suppression_action_id = None;
        state.menu_suppression_menu_id = None;
        if state.menu_dismissal_suppression_enabled
            || state.menu_pid.is_some()
            || state.menu_suppression_action_id.is_some()
            || state.menu_suppression_menu_id.is_some()
        {
            return Err(suppression_state_error(
                "menu suppression remained active after synchronous release",
            ));
        }
        self.released = true;
        let mut evidence = NativeEvidence::default();
        evidence.fields.insert(
            "menu_suppression_target_pid".to_owned(),
            self.target_pid.into(),
        );
        evidence.fields.insert(
            "menu_suppression_menu_pid".to_owned(),
            self.menu_pid
                .map_or(serde_json::Value::Null, serde_json::Value::from),
        );
        evidence.fields.insert(
            "menu_suppression_menu_id".to_owned(),
            self.menu_id.to_string().into(),
        );
        evidence.fields.insert(
            "menu_suppression_action_id".to_owned(),
            self.action_id.to_string().into(),
        );
        evidence.fields.insert(
            "menu_suppression_predicate".to_owned(),
            "source_pid_is_target_or_menu".into(),
        );
        evidence.fields.insert(
            "menu_suppression_event_mask".to_owned(),
            "left_right_other_down_up_dragged".into(),
        );
        evidence.fields.insert(
            "menu_suppression_armed_after_dispatch".to_owned(),
            state.menu_suppression_was_armed.into(),
        );
        evidence.fields.insert(
            "menu_suppression_suppressed_event_count".to_owned(),
            state.menu_suppressed_event_count.into(),
        );
        evidence.fields.insert(
            "menu_suppression_last_suppressed_source_pid".to_owned(),
            state
                .menu_last_suppressed_source_pid
                .map_or(serde_json::Value::Null, serde_json::Value::from),
        );
        evidence.fields.insert(
            "menu_suppression_active_after_release".to_owned(),
            false.into(),
        );
        Ok(evidence)
    }
}

impl MenuSuppressionResource for SystemMenuSuppressionResource {
    fn release(&mut self) -> Result<NativeEvidence, NativeError> {
        if self.released {
            let mut evidence = NativeEvidence::default();
            evidence.fields.insert(
                "menu_suppression_release_idempotent".to_owned(),
                true.into(),
            );
            return Ok(evidence);
        }
        self.release_inner()
    }
}

impl Drop for SystemMenuSuppressionResource {
    fn drop(&mut self) {
        if !self.released {
            if let Err(error) = self.release_inner() {
                tracing::error!(%error, "menu suppression failed during final drop");
            }
        }
    }
}

fn suppression_state_error(message: &'static str) -> NativeError {
    NativeError::new(ErrorCode::Internal, ErrorPhase::Verify, false, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focus_state() -> Arc<Mutex<MacFocusState>> {
        Arc::new(Mutex::new(MacFocusState::new(44, 9)))
    }

    #[test]
    fn exact_suppression_acquires_and_releases_the_shared_predicate_state() {
        let state = focus_state();
        let plan = MenuSuppressionPlan::ExactSourcePidPredicate {
            target_pid: 44,
            menu_pid: Some(55),
            menu_id: MenuId::parse("menu").unwrap(),
            action_id: ActionId::parse("action").unwrap(),
        };
        let mut resource = acquire_production(&plan, Arc::clone(&state))
            .unwrap()
            .unwrap();
        {
            let state = state.lock().unwrap();
            assert!(!state.menu_dismissal_suppression_enabled);
            assert_eq!(state.menu_pid, Some(55));
            assert_eq!(
                state.menu_suppression_action_id.as_ref(),
                Some(&ActionId::parse("action").unwrap())
            );
            assert_eq!(
                state.menu_suppression_menu_id.as_ref(),
                Some(&MenuId::parse("menu").unwrap())
            );
        }
        let evidence = resource.release().unwrap();
        assert_eq!(
            evidence.fields["menu_suppression_active_after_release"],
            false
        );
        let state = state.lock().unwrap();
        assert!(!state.menu_dismissal_suppression_enabled);
        assert_eq!(state.menu_pid, None);
    }
}
