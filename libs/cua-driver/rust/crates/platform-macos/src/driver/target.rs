use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use cua_driver_core::api::{
    contracts::{ActionId, MenuId},
    errors::{ErrorCode, ErrorPhase, NativeError},
    observation::{InvalidationReason, NativeProcessHandle, ResolvedWindowStamp},
    platform::{InvalidationSubscription, TargetFocusCoordinator, TargetInvalidation},
};

use crate::apps::nsworkspace::{WorkspaceEventHub, WorkspaceEventKind};

use super::{
    observation::{MacElementRegistry, MacFrameHistory, RetainedAxElement},
    settlement::{
        MacAxEvent, MacAxObserverRegistration, MacDisplayObserverRegistration, MacSignalJournal,
    },
};

#[derive(Clone)]
pub struct MacInvalidationHub {
    sender: tokio::sync::broadcast::Sender<TargetInvalidation>,
}

impl Default for MacInvalidationHub {
    fn default() -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(256);
        Self { sender }
    }
}

impl MacInvalidationHub {
    pub fn publish(&self, invalidation: TargetInvalidation) {
        let _ = self.sender.send(invalidation);
    }

    pub fn subscribe(&self) -> MacInvalidationSubscription {
        MacInvalidationSubscription {
            receiver: self.sender.subscribe(),
            workspace: WorkspaceEventHub::shared().subscribe(),
            registry_closed: false,
            workspace_closed: false,
        }
    }
}

pub struct MacInvalidationSubscription {
    receiver: tokio::sync::broadcast::Receiver<TargetInvalidation>,
    workspace: tokio::sync::broadcast::Receiver<crate::apps::nsworkspace::WorkspaceEvent>,
    registry_closed: bool,
    workspace_closed: bool,
}

#[async_trait]
impl InvalidationSubscription for MacInvalidationSubscription {
    async fn next(&mut self) -> Option<TargetInvalidation> {
        loop {
            if self.registry_closed && self.workspace_closed {
                return None;
            }
            tokio::select! {
                result = self.receiver.recv(), if !self.registry_closed => match result {
                    Ok(invalidation) => return Some(invalidation),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "macOS window invalidation subscriber lagged");
                        return Some(TargetInvalidation::NativeStateResyncRequired);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        self.registry_closed = true;
                    }
                },
                result = self.workspace.recv(), if !self.workspace_closed => match result {
                    Ok(event) if event.kind == WorkspaceEventKind::Terminated => {
                        let Some(generation) = event.process_generation else {
                            tracing::warn!(
                                pid = event.pid,
                                "terminated app had no launch generation; refusing pid-only invalidation"
                            );
                            continue;
                        };
                        let process = NativeProcessHandle::new(format!(
                            "macos:{}:{generation:016x}",
                            event.pid
                        ))
                        .expect("constructed macOS process handle is nonempty");
                        return Some(TargetInvalidation::ProcessExited { process });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "macOS workspace invalidation subscriber lagged");
                        return Some(TargetInvalidation::NativeStateResyncRequired);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        self.workspace_closed = true;
                    }
                }
            }
        }
    }
}

/// Native resources associated with one core target controller. Later plans
/// add AX, menu, window and frame observer registrations here. The invalidated
/// flag lets native callbacks revoke their own resources immediately even
/// before the core registry finishes removing the controller.
pub struct MacTargetState {
    pub window: ResolvedWindowStamp,
    pub signals: MacSignalJournal,
    pub elements: MacElementRegistry,
    pub frames: MacFrameHistory,
    observer: Option<MacAxObserverRegistration>,
    display_observer: Option<MacDisplayObserverRegistration>,
    invalidated: Arc<AtomicBool>,
    menu_focus_state: Option<Arc<Mutex<MacFocusState>>>,
}

impl MacTargetState {
    pub fn new(
        window: ResolvedWindowStamp,
        pid: i32,
        cg_window_id: u32,
        invalidations: MacInvalidationHub,
    ) -> Result<Self, NativeError> {
        let signals = MacSignalJournal::default();
        let invalidation_stamp = window.clone();
        let ax_invalidations = invalidations.clone();
        let on_event = Arc::new(move |event| {
            let reason = match event {
                MacAxEvent::ContentChanged
                | MacAxEvent::FocusChanged
                | MacAxEvent::MenuOpened
                | MacAxEvent::ScrollChanged => InvalidationReason::ContentChanged,
                MacAxEvent::TreeChanged | MacAxEvent::WindowDestroyed => {
                    InvalidationReason::AccessibilityInvalidated
                }
                MacAxEvent::WindowGeometryChanged => InvalidationReason::WindowChanged,
                MacAxEvent::MenuDismissed => InvalidationReason::TransientDismissed,
            };
            ax_invalidations.publish(TargetInvalidation::ObservationChanged {
                app_id: invalidation_stamp.app_id.clone(),
                window_id: invalidation_stamp.window_id.clone(),
                generation: invalidation_stamp.generation,
                reason,
            });
        });
        let observer =
            MacAxObserverRegistration::start(pid, cg_window_id, signals.clone(), on_event)?;
        let display_stamp = window.clone();
        let on_display_change = Arc::new(move || {
            invalidations.publish(TargetInvalidation::ObservationChanged {
                app_id: display_stamp.app_id.clone(),
                window_id: display_stamp.window_id.clone(),
                generation: display_stamp.generation,
                reason: InvalidationReason::DisplayChanged,
            });
        });
        let display_observer =
            MacDisplayObserverRegistration::start(signals.clone(), on_display_change)?;
        Ok(Self {
            window,
            signals,
            elements: MacElementRegistry::default(),
            frames: MacFrameHistory::default(),
            observer: Some(observer),
            display_observer: Some(display_observer),
            invalidated: Arc::new(AtomicBool::new(false)),
            menu_focus_state: None,
        })
    }

    pub fn invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Acquire)
    }

    pub(crate) fn replace_observed_elements(
        &self,
        elements: Vec<RetainedAxElement>,
    ) -> Result<(), NativeError> {
        self.observer
            .as_ref()
            .ok_or_else(|| {
                NativeError::stale(
                    cua_driver_core::api::errors::ErrorCode::WindowIdentityChanged,
                    "macOS AX observer was invalidated before descendant registration",
                )
            })?
            .replace_elements(elements)
    }

    pub fn invalidate(&mut self) {
        self.invalidated.store(true, Ordering::Release);
        self.observer.take();
        self.display_observer.take();
    }

    pub(crate) fn poison_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.invalidated)
    }

    pub(crate) fn bind_menu_focus_state(&mut self, state: Arc<Mutex<MacFocusState>>) {
        self.menu_focus_state = Some(state);
    }

    pub(crate) fn arm_menu_suppression(
        &self,
        action_id: &ActionId,
        menu_id: &MenuId,
    ) -> Result<(), NativeError> {
        let state = self.menu_focus_state.as_ref().ok_or_else(|| {
            NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Dispatch,
                false,
                "menu suppression has no target focus-state binding",
            )
        })?;
        let mut state = state.lock().map_err(|_| {
            NativeError::new(
                ErrorCode::Internal,
                ErrorPhase::Dispatch,
                false,
                "menu suppression focus-state lock was poisoned",
            )
        })?;
        if state.menu_suppression_action_id.as_ref() != Some(action_id)
            || state.menu_suppression_menu_id.as_ref() != Some(menu_id)
        {
            return Err(NativeError::stale(
                ErrorCode::MenuStateStale,
                "menu suppression reservation no longer matches the dispatched action",
            ));
        }
        if state.menu_dismissal_suppression_enabled {
            return Err(NativeError::new(
                ErrorCode::TargetBusy,
                ErrorPhase::Dispatch,
                false,
                "menu suppression was already armed for this action",
            ));
        }
        state.menu_dismissal_suppression_enabled = true;
        state.menu_suppression_was_armed = true;
        Ok(())
    }
}

impl Drop for MacTargetState {
    fn drop(&mut self) {
        self.invalidate();
    }
}

#[derive(Debug)]
pub(crate) struct MacFocusState {
    pub pid: i32,
    pub cg_window_id: u32,
    pub application_is_active: bool,
    pub application_believes_it_is_active: bool,
    pub application_believes_it_has_focus: bool,
    pub menu_dismissal_suppression_enabled: bool,
    pub menu_pid: Option<i32>,
    pub menu_suppression_action_id: Option<ActionId>,
    pub menu_suppression_menu_id: Option<MenuId>,
    pub menu_suppression_was_armed: bool,
    pub menu_suppressed_event_count: u64,
    pub menu_last_suppressed_source_pid: Option<i32>,
}

impl MacFocusState {
    pub(crate) fn new(pid: i32, cg_window_id: u32) -> Self {
        let application_is_active = crate::apps::frontmost_pid() == Some(pid);
        Self {
            pid,
            cg_window_id,
            application_is_active,
            application_believes_it_is_active: application_is_active,
            application_believes_it_has_focus: application_is_active,
            menu_dismissal_suppression_enabled: false,
            menu_pid: None,
            menu_suppression_action_id: None,
            menu_suppression_menu_id: None,
            menu_suppression_was_armed: false,
            menu_suppressed_event_count: 0,
            menu_last_suppressed_source_pid: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct MacFocusStateRegistry {
    states: Arc<Mutex<HashMap<NativeProcessHandle, Arc<Mutex<MacFocusState>>>>>,
}

impl MacFocusStateRegistry {
    pub(crate) fn state_for(
        &self,
        process: &NativeProcessHandle,
        pid: i32,
        cg_window_id: u32,
    ) -> Arc<Mutex<MacFocusState>> {
        let mut states = self.states.lock().expect("macOS focus registry poisoned");
        Arc::clone(
            states
                .entry(process.clone())
                .or_insert_with(|| Arc::new(Mutex::new(MacFocusState::new(pid, cg_window_id)))),
        )
    }
}

pub struct MacTargetFocusCoordinator {
    state: Arc<Mutex<MacFocusState>>,
    focus_taps: Option<crate::focus_steal::TargetFocusTapRegistration>,
    shutdown: bool,
}

impl MacTargetFocusCoordinator {
    pub(crate) fn new(pid: i32, state: Arc<Mutex<MacFocusState>>) -> Result<Self, NativeError> {
        let focus_taps =
            crate::focus_steal::TargetFocusTapRegistration::start(pid, Arc::downgrade(&state));
        Ok(Self {
            state,
            focus_taps: Some(focus_taps),
            shutdown: false,
        })
    }

    pub(crate) fn state_handle(&self) -> Arc<Mutex<MacFocusState>> {
        Arc::clone(&self.state)
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown
    }
}

#[async_trait]
impl TargetFocusCoordinator for MacTargetFocusCoordinator {
    async fn shutdown(&mut self) -> Result<(), NativeError> {
        if let Some(mut focus_taps) = self.focus_taps.take() {
            focus_taps.close();
        }
        self.shutdown = true;
        Ok(())
    }
}

impl Drop for MacTargetFocusCoordinator {
    fn drop(&mut self) {
        if let Some(mut focus_taps) = self.focus_taps.take() {
            focus_taps.close();
        }
        self.shutdown = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_registry_reuses_state_per_process_across_exact_windows() {
        let registry = MacFocusStateRegistry::default();
        let first_process = NativeProcessHandle::new("process-1").unwrap();
        let other_process = NativeProcessHandle::new("process-2").unwrap();

        let first_window = registry.state_for(&first_process, 44, 99);
        let replacement_window = registry.state_for(&first_process, 44, 100);
        let other = registry.state_for(&other_process, 45, 101);

        assert!(Arc::ptr_eq(&first_window, &replacement_window));
        assert!(!Arc::ptr_eq(&first_window, &other));
    }
}
