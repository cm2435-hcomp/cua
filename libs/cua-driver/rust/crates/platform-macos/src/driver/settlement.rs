//! Native target signals and notification-driven post-action settlement.

use std::{
    collections::{BTreeSet, VecDeque},
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use core_foundation::{
    base::{CFGetTypeID, CFRelease, CFTypeRef, TCFType},
    runloop::{
        kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopAddSource,
        CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRemoveSource, CFRunLoopWakeUp,
    },
    string::{CFString, CFStringRef},
};
use cua_driver_core::api::{
    errors::{ErrorCode, ErrorPhase, NativeError},
    settlement::{
        DirtyState, PendingSettlementEvidence, PendingSettlementState, SettledState,
        SettlementAttempt, SettlementEvidence, SettlementSignal,
    },
};
use tokio::sync::Notify;

use crate::ax::bindings::{
    self, copy_ax_windows, kAXErrorSuccess, AXObserverAddNotification, AXObserverCreate,
    AXObserverGetRunLoopSource, AXObserverRef, AXUIElementCopyAttributeValue,
    AXUIElementCreateApplication, AXUIElementGetTypeID, AXUIElementRef,
};

use super::observation::RetainedAxElement;

const SIGNAL_JOURNAL_CAPACITY: usize = 512;
const AX_OBSERVER_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(750);
const AX_OBSERVER_COMMAND_TIMEOUT: Duration = Duration::from_millis(750);
const AX_OBSERVER_POLL_INTERVAL: Duration = Duration::from_millis(20);

type CGDirectDisplayId = u32;
type CGDisplayChangeSummaryFlags = u32;
type CGError = i32;
type CGDisplayReconfigurationCallback = unsafe extern "C" fn(
    display: CGDirectDisplayId,
    flags: CGDisplayChangeSummaryFlags,
    user_info: *mut c_void,
);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGDisplayRegisterReconfigurationCallback(
        callback: CGDisplayReconfigurationCallback,
        user_info: *mut c_void,
    ) -> CGError;
    fn CGDisplayRemoveReconfigurationCallback(
        callback: CGDisplayReconfigurationCallback,
        user_info: *mut c_void,
    ) -> CGError;
}

#[derive(Debug, Clone, Copy)]
struct RecordedSignal {
    at: Instant,
    signal: SettlementSignal,
}

#[derive(Debug, Default)]
struct JournalState {
    signals: VecDeque<RecordedSignal>,
    epoch: u64,
}

/// Bounded, content-free native event journal for one target controller.
///
/// The journal intentionally stores only signal kinds and monotonic times. AX
/// values, selected text and screenshots belong to observations and artifacts,
/// never logs or the settlement channel.
#[derive(Debug, Clone)]
pub struct MacSignalJournal {
    state: Arc<Mutex<JournalState>>,
    changed: Arc<Notify>,
}

impl Default for MacSignalJournal {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(JournalState::default())),
            changed: Arc::new(Notify::new()),
        }
    }
}

impl MacSignalJournal {
    pub fn record(&self, signal: SettlementSignal) {
        let mut state = self.state.lock().expect("macOS signal journal poisoned");
        state.epoch = state.epoch.wrapping_add(1);
        if state.signals.len() == SIGNAL_JOURNAL_CAPACITY {
            state.signals.pop_front();
        }
        state.signals.push_back(RecordedSignal {
            at: Instant::now(),
            signal,
        });
        drop(state);
        self.changed.notify_waiters();
    }

    /// Monotonic target-mutation bracket for coherent observations.
    pub fn epoch(&self) -> u64 {
        self.state
            .lock()
            .expect("macOS signal journal poisoned")
            .epoch
    }

    pub fn latest_signal(&self) -> Option<SettlementSignal> {
        self.state
            .lock()
            .expect("macOS signal journal poisoned")
            .signals
            .back()
            .map(|record| record.signal)
    }

    /// Run a synchronous publication while native callbacks are excluded from
    /// advancing the epoch. `None` means evidence changed since the caller's
    /// A-side sample and no publication occurred.
    pub fn commit_if_epoch<T>(
        &self,
        expected: u64,
        commit: impl FnOnce() -> Result<T, NativeError>,
    ) -> Result<Option<T>, NativeError> {
        let state = self.state.lock().expect("macOS signal journal poisoned");
        if state.epoch != expected {
            return Ok(None);
        }
        let result = commit()?;
        drop(state);
        Ok(Some(result))
    }

    fn snapshot_since(&self, since: Instant) -> Vec<RecordedSignal> {
        self.state
            .lock()
            .expect("macOS signal journal poisoned")
            .signals
            .iter()
            .copied()
            .filter(|record| record.at >= since)
            .collect()
    }

    /// Wait until terminal evidence exists and target-relevant native signals
    /// have been quiet for the profile window. The timer is not a fixed sleep:
    /// every relevant notification interrupts and resets it.
    pub async fn settle(
        &self,
        dirty: &DirtyState,
        relevant_signals: &BTreeSet<SettlementSignal>,
        deadline: Instant,
    ) -> SettlementAttempt {
        let quiet_window = Duration::from_millis(dirty.profile.quiet_window_ms);
        let mut eligible_since: Option<Instant> = None;

        loop {
            // Register the waiter before reading state so a signal cannot land
            // between the snapshot and the await and be lost.
            let changed = self.changed.notified();
            let records = self.snapshot_since(dirty.since);
            let mut observed = dirty.observed_signals.clone();
            observed.extend(records.iter().map(|record| record.signal));

            let terminal = observed.contains(&SettlementSignal::DispatchComplete)
                && dirty.profile.required_terminal_signals.is_subset(&observed);
            let latest_relevant = latest_relevant_signal(&records, relevant_signals);

            if terminal {
                let now = Instant::now();
                let quiet_since = match eligible_since {
                    Some(prior) => latest_relevant.map_or(prior, |latest| prior.max(latest)),
                    None => latest_relevant.map_or(now, |latest| latest.max(now)),
                };
                eligible_since = Some(quiet_since);
                if now.saturating_duration_since(quiet_since) >= quiet_window {
                    return SettlementAttempt::Settled(SettlementEvidence {
                        state: SettledState::Settled,
                        trigger_action_id: Some(dirty.action_id.clone()),
                        profile: dirty.profile.name.clone(),
                        elapsed_ms: dirty.since.elapsed().as_millis().min(u128::from(u64::MAX))
                            as u64,
                        observed_signals: observed.iter().copied().collect(),
                        terminal_signal: "target_notifications_quiet".to_owned(),
                        quiet_window_ms: dirty.profile.quiet_window_ms,
                        resumed_from_prior_call: dirty.resumed_from_prior_call,
                    });
                }
            } else {
                eligible_since = None;
            }

            let now = Instant::now();
            if now >= deadline {
                return SettlementAttempt::Pending(pending_evidence(dirty, observed));
            }

            let wake_at = eligible_since
                .map(|quiet_since| quiet_since + quiet_window)
                .unwrap_or(deadline)
                .min(deadline);
            tokio::select! {
                _ = changed => {}
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)) => {}
            }
        }
    }
}

fn latest_relevant_signal(
    records: &[RecordedSignal],
    relevant_signals: &BTreeSet<SettlementSignal>,
) -> Option<Instant> {
    records
        .iter()
        .filter(|record| relevant_signals.contains(&record.signal))
        .map(|record| record.at)
        .max()
}

fn pending_evidence(
    dirty: &DirtyState,
    observed: BTreeSet<SettlementSignal>,
) -> PendingSettlementEvidence {
    let mut missing_signals: Vec<_> = dirty
        .profile
        .required_terminal_signals
        .difference(&observed)
        .copied()
        .collect();
    if !observed.contains(&SettlementSignal::DispatchComplete) {
        missing_signals.push(SettlementSignal::DispatchComplete);
    }
    PendingSettlementEvidence {
        state: PendingSettlementState::Pending,
        trigger_action_id: dirty.action_id.clone(),
        profile: dirty.profile.name.clone(),
        elapsed_ms: dirty.since.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        observed_signals: observed.iter().copied().collect(),
        missing_signals,
    }
}

struct ObserverContext {
    target_window_id: u32,
    focused_target: AtomicBool,
    journal: MacSignalJournal,
    on_event: Arc<dyn Fn(MacAxEvent) + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacAxEvent {
    ContentChanged,
    FocusChanged,
    TreeChanged,
    WindowGeometryChanged,
    WindowDestroyed,
    MenuOpened,
    MenuDismissed,
    ScrollChanged,
}

impl MacAxEvent {
    fn settlement_signal(self) -> SettlementSignal {
        match self {
            Self::ContentChanged => SettlementSignal::AxValueChanged,
            Self::FocusChanged => SettlementSignal::FocusChanged,
            Self::TreeChanged => SettlementSignal::AxAction,
            Self::WindowGeometryChanged => SettlementSignal::WindowGeometryChanged,
            Self::WindowDestroyed => SettlementSignal::WindowListChanged,
            Self::MenuOpened => SettlementSignal::MenuOpened,
            Self::MenuDismissed => SettlementSignal::MenuDismissed,
            Self::ScrollChanged => SettlementSignal::ScrollChanged,
        }
    }
}

unsafe extern "C" fn observer_callback(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
) {
    if refcon.is_null() || element.is_null() || notification.is_null() {
        return;
    }
    let notification = CFString::wrap_under_get_rule(notification).to_string();
    let context = &*(refcon as *const ObserverContext);
    if matches!(
        notification.as_str(),
        "AXFocusedUIElementChanged" | "AXFocusedWindowChanged"
    ) {
        // Application-level focus callbacks carry the application object.
        // Accept only a transition entering, staying within, or leaving this
        // exact target window.
        let focused_target = focused_window_id(element) == Some(context.target_window_id);
        let was_focused = context
            .focused_target
            .swap(focused_target, Ordering::AcqRel);
        if !was_focused && !focused_target {
            return;
        }
    }
    // Other callbacks can only originate from elements explicitly registered
    // as this target or a retained descendant. Never query the callback
    // element after AXUIElementDestroyed.
    if let Some(event) = event_for_notification(&notification) {
        context.journal.record(event.settlement_signal());
        (context.on_event)(event);
    }
}

unsafe fn focused_window_id(application: AXUIElementRef) -> Option<u32> {
    let attribute = CFString::new("AXFocusedWindow");
    let mut value: CFTypeRef = std::ptr::null();
    if AXUIElementCopyAttributeValue(application, attribute.as_concrete_TypeRef(), &mut value)
        != kAXErrorSuccess
        || value.is_null()
    {
        return None;
    }
    if CFGetTypeID(value) != AXUIElementGetTypeID() {
        CFRelease(value);
        return None;
    }
    let window_id = bindings::ax_get_window_id(value.cast_mut().cast());
    CFRelease(value);
    window_id
}

pub(crate) fn target_is_focused_window(pid: i32, target_window_id: u32) -> Option<bool> {
    unsafe {
        let application = AXUIElementCreateApplication(pid);
        if application.is_null() {
            return None;
        }
        let focused = focused_window_id(application).map(|window_id| window_id == target_window_id);
        CFRelease(application.cast());
        focused
    }
}

fn event_for_notification(notification: &str) -> Option<MacAxEvent> {
    match notification {
        "AXFocusedUIElementChanged" | "AXFocusedWindowChanged" => Some(MacAxEvent::FocusChanged),
        "AXValueChanged" | "AXSelectedTextChanged" | "AXSelectedChildrenChanged" => {
            Some(MacAxEvent::ContentChanged)
        }
        "AXUIElementDestroyed" => Some(MacAxEvent::WindowDestroyed),
        "AXMoved" | "AXResized" => Some(MacAxEvent::WindowGeometryChanged),
        "AXMenuOpened" => Some(MacAxEvent::MenuOpened),
        "AXMenuClosed" | "AXMenuItemSelected" => Some(MacAxEvent::MenuDismissed),
        "AXSelectedRowsChanged" | "AXSelectedColumnsChanged" => Some(MacAxEvent::ScrollChanged),
        "AXLayoutChanged" => Some(MacAxEvent::TreeChanged),
        "AXTitleChanged" => Some(MacAxEvent::ContentChanged),
        _ => None,
    }
}

struct DisplayObserverContext {
    journal: MacSignalJournal,
    on_change: Arc<dyn Fn() + Send + Sync>,
}

unsafe extern "C" fn display_reconfiguration_callback(
    _display: CGDirectDisplayId,
    _flags: CGDisplayChangeSummaryFlags,
    user_info: *mut c_void,
) {
    if user_info.is_null() {
        return;
    }
    let context = &*(user_info as *const DisplayObserverContext);
    context
        .journal
        .record(SettlementSignal::WindowGeometryChanged);
    (context.on_change)();
}

/// Exact-target invalidation bridge for global display topology/scale changes.
pub struct MacDisplayObserverRegistration {
    context: *mut DisplayObserverContext,
}

unsafe impl Send for MacDisplayObserverRegistration {}
unsafe impl Sync for MacDisplayObserverRegistration {}

impl MacDisplayObserverRegistration {
    pub fn start(
        journal: MacSignalJournal,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, NativeError> {
        let context = Box::into_raw(Box::new(DisplayObserverContext { journal, on_change }));
        let error = unsafe {
            CGDisplayRegisterReconfigurationCallback(
                display_reconfiguration_callback,
                context.cast(),
            )
        };
        if error != 0 {
            unsafe { drop(Box::from_raw(context)) };
            return Err(observer_error(format!(
                "CGDisplayRegisterReconfigurationCallback failed with {error}"
            )));
        }
        Ok(Self { context })
    }
}

impl Drop for MacDisplayObserverRegistration {
    fn drop(&mut self) {
        unsafe {
            let _ = CGDisplayRemoveReconfigurationCallback(
                display_reconfiguration_callback,
                self.context.cast(),
            );
            drop(Box::from_raw(self.context));
        }
    }
}

/// Dedicated native AX observer bound to one exact process generation/window
/// identity. The caller must recreate it after any generation change.
pub struct MacAxObserverRegistration {
    stopping: Arc<AtomicBool>,
    run_loop: Arc<AtomicUsize>,
    commands: mpsc::Sender<ObserverCommand>,
    finished: mpsc::Receiver<()>,
    thread: Option<thread::JoinHandle<()>>,
}

enum ObserverCommand {
    ReplaceElements {
        elements: Vec<RetainedAxElement>,
        reply: mpsc::SyncSender<Result<(), NativeError>>,
    },
}

#[derive(Clone, Copy)]
struct AxObserverTarget {
    pid: i32,
    window_id: u32,
}

impl MacAxObserverRegistration {
    pub fn start(
        pid: i32,
        target_window_id: u32,
        journal: MacSignalJournal,
        on_event: Arc<dyn Fn(MacAxEvent) + Send + Sync>,
    ) -> Result<Self, NativeError> {
        let stopping = Arc::new(AtomicBool::new(false));
        let run_loop = Arc::new(AtomicUsize::new(0));
        let thread_stopping = stopping.clone();
        let thread_run_loop = run_loop.clone();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let (command_tx, command_rx) = mpsc::channel();

        let thread = thread::Builder::new()
            .name(format!("cua-ax-observer-{pid}-{target_window_id}"))
            .spawn(move || {
                let result = unsafe {
                    run_ax_observer(
                        AxObserverTarget {
                            pid,
                            window_id: target_window_id,
                        },
                        journal,
                        on_event,
                        &thread_stopping,
                        &thread_run_loop,
                        &started_tx,
                        &command_rx,
                    )
                };
                if let Err(error) = result {
                    let _ = started_tx.send(Err(error));
                }
                let _ = finished_tx.send(());
            })
            .map_err(|error| observer_error(format!("failed to spawn AX observer: {error}")))?;

        match started_rx.recv_timeout(AX_OBSERVER_HANDSHAKE_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                stopping,
                run_loop,
                commands: command_tx,
                finished: finished_rx,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = finished_rx.recv_timeout(AX_OBSERVER_HANDSHAKE_TIMEOUT);
                // Never join a native AX run-loop thread. The bounded finish
                // acknowledgement is the teardown proof; dropping the handle
                // detaches a wedged thread whose callback context it still owns.
                drop(thread);
                Err(error)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop(thread);
                Err(observer_error("AX observer exited before registration"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stopping.store(true, Ordering::Release);
                let active_run_loop = run_loop.load(Ordering::Acquire) as CFRunLoopRef;
                if !active_run_loop.is_null() {
                    unsafe { CFRunLoopWakeUp(active_run_loop) };
                }
                Err(observer_error(
                    "AX observer registration exceeded its bounded deadline",
                ))
            }
        }
    }

    pub fn replace_elements(&self, elements: Vec<RetainedAxElement>) -> Result<(), NativeError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.commands
            .send(ObserverCommand::ReplaceElements {
                elements,
                reply: reply_tx,
            })
            .map_err(|_| observer_error("AX observer stopped before descendant registration"))?;
        let run_loop = self.run_loop.load(Ordering::Acquire) as CFRunLoopRef;
        if !run_loop.is_null() {
            unsafe { CFRunLoopWakeUp(run_loop) };
        }
        reply_rx
            .recv_timeout(AX_OBSERVER_COMMAND_TIMEOUT)
            .map_err(|_| {
                observer_error("AX descendant registration exceeded its bounded deadline")
            })?
    }
}

impl Drop for MacAxObserverRegistration {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let run_loop = self.run_loop.load(Ordering::Acquire) as CFRunLoopRef;
        if !run_loop.is_null() {
            unsafe { CFRunLoopWakeUp(run_loop) };
        }
        if let Some(thread) = self.thread.take() {
            let _ = self.finished.recv_timeout(AX_OBSERVER_HANDSHAKE_TIMEOUT);
            // The observer thread owns every retained AX object and callback
            // context. Detach after the bounded acknowledgement instead of
            // risking an unbounded join if CoreFoundation stalls on teardown.
            drop(thread);
        }
    }
}

unsafe fn run_ax_observer(
    target_info: AxObserverTarget,
    journal: MacSignalJournal,
    on_event: Arc<dyn Fn(MacAxEvent) + Send + Sync>,
    stopping: &AtomicBool,
    run_loop_slot: &AtomicUsize,
    started: &mpsc::SyncSender<Result<(), NativeError>>,
    commands: &mpsc::Receiver<ObserverCommand>,
) -> Result<(), NativeError> {
    let pid = target_info.pid;
    let target_window_id = target_info.window_id;
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return Err(observer_error("AX application element is unavailable"));
    }
    let windows = copy_ax_windows(application);
    let target = windows
        .iter()
        .copied()
        .find(|element| bindings::ax_get_window_id(*element) == Some(target_window_id));
    for window in windows
        .iter()
        .copied()
        .filter(|window| Some(*window) != target)
    {
        CFRelease(window as CFTypeRef);
    }
    let Some(target) = target else {
        CFRelease(application as CFTypeRef);
        return Err(observer_error("exact target AX window is unavailable"));
    };

    let context = Box::new(ObserverContext {
        target_window_id,
        focused_target: AtomicBool::new(focused_window_id(application) == Some(target_window_id)),
        journal,
        on_event,
    });
    let context_ptr = Box::into_raw(context);
    let mut observer = std::ptr::null_mut();
    let create_error = AXObserverCreate(pid, observer_callback, &mut observer);
    if create_error != kAXErrorSuccess || observer.is_null() {
        drop(Box::from_raw(context_ptr));
        CFRelease(target as CFTypeRef);
        CFRelease(application as CFTypeRef);
        return Err(observer_error(format!(
            "AXObserverCreate failed with {create_error}"
        )));
    }

    let target_notifications = ["AXUIElementDestroyed", "AXMoved", "AXResized"];
    let application_notifications = ["AXFocusedUIElementChanged", "AXFocusedWindowChanged"];
    let descendant_notifications = [
        "AXValueChanged",
        "AXSelectedTextChanged",
        "AXSelectedChildrenChanged",
        "AXMenuOpened",
        "AXMenuClosed",
        "AXMenuItemSelected",
        "AXSelectedRowsChanged",
        "AXSelectedColumnsChanged",
        "AXLayoutChanged",
        "AXTitleChanged",
    ];
    let mut target_registrations = 0usize;
    for notification in target_notifications {
        let notification = CFString::new(notification);
        if AXObserverAddNotification(
            observer,
            target,
            notification.as_concrete_TypeRef(),
            context_ptr.cast(),
        ) == kAXErrorSuccess
        {
            target_registrations += 1;
        }
    }
    let mut focus_registrations = 0usize;
    for notification in application_notifications {
        let notification = CFString::new(notification);
        if AXObserverAddNotification(
            observer,
            application,
            notification.as_concrete_TypeRef(),
            context_ptr.cast(),
        ) == kAXErrorSuccess
        {
            focus_registrations += 1;
        }
    }
    if target_registrations == 0 || focus_registrations == 0 {
        CFRelease(observer as CFTypeRef);
        drop(Box::from_raw(context_ptr));
        CFRelease(target as CFTypeRef);
        CFRelease(application as CFTypeRef);
        return Err(observer_error(
            "target AX window lacks required exact window or application-focus notifications",
        ));
    }

    let source = AXObserverGetRunLoopSource(observer);
    if source.is_null() {
        CFRelease(observer as CFTypeRef);
        drop(Box::from_raw(context_ptr));
        CFRelease(target as CFTypeRef);
        CFRelease(application as CFTypeRef);
        return Err(observer_error("AX observer has no run-loop source"));
    }
    let run_loop = CFRunLoopGetCurrent();
    run_loop_slot.store(run_loop as usize, Ordering::Release);
    CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
    if started.send(Ok(())).is_err() {
        remove_static_notifications(
            observer,
            target,
            application,
            &target_notifications,
            &application_notifications,
        );
        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
        run_loop_slot.store(0, Ordering::Release);
        CFRelease(observer as CFTypeRef);
        drop(Box::from_raw(context_ptr));
        CFRelease(target as CFTypeRef);
        CFRelease(application as CFTypeRef);
        return Err(observer_error(
            "AX observer owner disappeared during registration",
        ));
    }

    let mut observed_elements = Vec::new();
    while !stopping.load(Ordering::Acquire) {
        while let Ok(command) = commands.try_recv() {
            match command {
                ObserverCommand::ReplaceElements { elements, reply } => {
                    if same_observed_elements(&observed_elements, &elements) {
                        let _ = reply.send(Ok(()));
                        continue;
                    }
                    remove_descendant_notifications(
                        observer,
                        &observed_elements,
                        &descendant_notifications,
                    );
                    observed_elements = elements;
                    let registered = add_descendant_notifications(
                        observer,
                        context_ptr.cast(),
                        &observed_elements,
                        &descendant_notifications,
                    );
                    let result = if observed_elements.is_empty() || registered > 0 {
                        Ok(())
                    } else {
                        Err(observer_error(
                            "target descendants support no AX mutation notifications",
                        ))
                    };
                    let _ = reply.send(result);
                }
            }
        }
        CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, AX_OBSERVER_POLL_INTERVAL, true);
    }
    remove_descendant_notifications(observer, &observed_elements, &descendant_notifications);
    remove_static_notifications(
        observer,
        target,
        application,
        &target_notifications,
        &application_notifications,
    );
    CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
    run_loop_slot.store(0, Ordering::Release);
    CFRelease(observer as CFTypeRef);
    drop(Box::from_raw(context_ptr));
    CFRelease(target as CFTypeRef);
    CFRelease(application as CFTypeRef);
    Ok(())
}

fn same_observed_elements(left: &[RetainedAxElement], right: &[RetainedAxElement]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.same_identity(right))
}

unsafe fn remove_static_notifications(
    observer: AXObserverRef,
    target: AXUIElementRef,
    application: AXUIElementRef,
    target_notifications: &[&str],
    application_notifications: &[&str],
) {
    for notification in target_notifications {
        let notification = CFString::new(notification);
        let _ = bindings::AXObserverRemoveNotification(
            observer,
            target,
            notification.as_concrete_TypeRef(),
        );
    }
    for notification in application_notifications {
        let notification = CFString::new(notification);
        let _ = bindings::AXObserverRemoveNotification(
            observer,
            application,
            notification.as_concrete_TypeRef(),
        );
    }
}

unsafe fn add_descendant_notifications(
    observer: AXObserverRef,
    context: *mut c_void,
    elements: &[RetainedAxElement],
    notifications: &[&str],
) -> usize {
    let mut registrations = 0;
    for element in elements {
        for notification in notifications {
            let notification = CFString::new(notification);
            if AXObserverAddNotification(
                observer,
                element.as_ptr(),
                notification.as_concrete_TypeRef(),
                context,
            ) == kAXErrorSuccess
            {
                registrations += 1;
            }
        }
    }
    registrations
}

unsafe fn remove_descendant_notifications(
    observer: AXObserverRef,
    elements: &[RetainedAxElement],
    notifications: &[&str],
) {
    for element in elements {
        for notification in notifications {
            let notification = CFString::new(notification);
            let _ = bindings::AXObserverRemoveNotification(
                observer,
                element.as_ptr(),
                notification.as_concrete_TypeRef(),
            );
        }
    }
}

fn observer_error(message: impl Into<String>) -> NativeError {
    NativeError::new(ErrorCode::Internal, ErrorPhase::Preflight, true, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::api::{contracts::ActionId, settlement::SettlementProfile};

    fn dirty(profile: SettlementProfile) -> DirtyState {
        let mut observed_signals = BTreeSet::new();
        observed_signals.insert(SettlementSignal::DispatchStarted);
        observed_signals.insert(SettlementSignal::DispatchComplete);
        DirtyState {
            action_id: ActionId::new(),
            profile,
            since: Instant::now(),
            observed_signals,
            resumed_from_prior_call: false,
        }
    }

    #[test]
    fn notification_mapping_contains_no_ax_content() {
        assert_eq!(
            event_for_notification("AXSelectedTextChanged"),
            Some(MacAxEvent::ContentChanged)
        );
        assert_eq!(event_for_notification("secret document text"), None);
    }

    #[test]
    fn epoch_gate_refuses_publication_after_a_native_signal_race() {
        let journal = MacSignalJournal::default();
        let epoch = journal.epoch();
        journal.record(SettlementSignal::WindowGeometryChanged);
        let published = AtomicBool::new(false);

        let result = journal
            .commit_if_epoch(epoch, || {
                published.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap();

        assert!(result.is_none());
        assert!(!published.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn relevant_signal_resets_quiet_window() {
        let journal = MacSignalJournal::default();
        let mut profile = SettlementProfile::dispatch_only("click");
        profile.quiet_window_ms = 25;
        let dirty = dirty(profile);
        let relevant = BTreeSet::from([SettlementSignal::AxValueChanged]);
        let started = Instant::now();
        let settling = {
            let journal = journal.clone();
            tokio::spawn(async move {
                journal
                    .settle(&dirty, &relevant, started + Duration::from_millis(200))
                    .await
            })
        };
        tokio::task::yield_now().await;
        journal.record(SettlementSignal::FreshFrame);
        tokio::time::sleep(Duration::from_millis(10)).await;
        journal.record(SettlementSignal::AxValueChanged);
        let result = settling.await.unwrap();
        let SettlementAttempt::Settled(evidence) = result else {
            panic!("expected settled evidence");
        };
        assert!(evidence
            .observed_signals
            .contains(&SettlementSignal::AxValueChanged));
        assert!(started.elapsed() >= Duration::from_millis(35));
    }

    #[test]
    fn unrelated_signals_are_excluded_from_the_quiet_window_clock() {
        let started = Instant::now();
        let records = [
            RecordedSignal {
                at: started + Duration::from_millis(10),
                signal: SettlementSignal::AxValueChanged,
            },
            RecordedSignal {
                at: started + Duration::from_millis(20),
                signal: SettlementSignal::FreshFrame,
            },
        ];
        assert_eq!(
            latest_relevant_signal(
                &records,
                &BTreeSet::from([SettlementSignal::AxValueChanged])
            ),
            Some(started + Duration::from_millis(10))
        );
    }

    #[tokio::test]
    async fn timeout_returns_accumulated_progress() {
        let journal = MacSignalJournal::default();
        let profile = SettlementProfile::requiring("menu", [SettlementSignal::MenuDismissed]);
        let dirty = dirty(profile);
        journal.record(SettlementSignal::AxAction);
        let result = journal
            .settle(
                &dirty,
                &BTreeSet::from([SettlementSignal::MenuDismissed]),
                Instant::now() + Duration::from_millis(5),
            )
            .await;
        let SettlementAttempt::Pending(pending) = result else {
            panic!("expected pending settlement");
        };
        assert!(pending
            .observed_signals
            .contains(&SettlementSignal::AxAction));
        assert!(pending
            .missing_signals
            .contains(&SettlementSignal::MenuDismissed));
    }
}
