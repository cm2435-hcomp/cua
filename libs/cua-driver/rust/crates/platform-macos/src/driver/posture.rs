use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicUsize, Ordering},
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
use core_graphics::event::{
    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};
use cua_driver_core::api::{
    errors::{ErrorCode, ErrorPhase, NativeError},
    interaction::{NativeEvidence, PostureResult},
};

use crate::{
    apps::{
        self,
        nsworkspace::{WorkspaceEvent, WorkspaceEventHub, WorkspaceEventKind},
    },
    ax::bindings::{
        self, kAXErrorNoValue, kAXErrorSuccess, AXObserverAddNotification, AXObserverCreate,
        AXObserverGetRunLoopSource, AXObserverRef, AXUIElementCopyAttributeValue,
        AXUIElementCreateApplication, AXUIElementGetTypeID, AXUIElementRef,
    },
    focus_steal::{DeadlineSuppression, SuppressionCloseOutcome, SuppressionOutcome},
};

const NATIVE_BARRIER_TIMEOUT: Duration = Duration::from_millis(250);
const OBSERVER_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(750);
const OBSERVER_CLOSE_TIMEOUT: Duration = Duration::from_millis(750);
const OBSERVER_RUN_LOOP_INTERVAL: Duration = Duration::from_millis(10);

/// Ordered, content-free posture evidence. The before/after samples are a
/// diagnostic and final-state cross-check; transient proof comes from the
/// native workspace, AX and HID streams.
#[derive(Debug, Default)]
struct PostureProofState {
    frontmost_changed: bool,
    key_window_changed: bool,
    physical_cursor_moved: bool,
    workspace_stream_complete: bool,
    ax_stream_complete: bool,
    hid_stream_complete: bool,
    containment_stream_complete: bool,
    activation_pids: Vec<i32>,
    workspace_lagged_events: u64,
    ax_focus_events: u64,
    hid_cursor_events: u64,
}

impl PostureProofState {
    fn new() -> Self {
        Self {
            workspace_stream_complete: true,
            ax_stream_complete: true,
            hid_stream_complete: true,
            containment_stream_complete: true,
            ..Self::default()
        }
    }

    fn observe_workspace(
        &mut self,
        receiver: &mut tokio::sync::broadcast::Receiver<WorkspaceEvent>,
        baseline_frontmost: i32,
    ) {
        loop {
            match receiver.try_recv() {
                Ok(event) if event.kind == WorkspaceEventKind::Activated => {
                    if event.pid != baseline_frontmost {
                        self.frontmost_changed = true;
                        self.activation_pids.push(event.pid);
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    self.workspace_stream_complete = false;
                    self.workspace_lagged_events =
                        self.workspace_lagged_events.saturating_add(skipped);
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    self.workspace_stream_complete = false;
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            }
        }
    }

    fn observe_final_sample(&mut self, baseline: PostureSample, final_sample: PostureSample) {
        if final_sample.frontmost.is_none()
            || final_sample.key_window.is_err()
            || final_sample.cursor.is_err()
        {
            self.workspace_stream_complete = false;
            self.ax_stream_complete = false;
            self.hid_stream_complete = false;
        }
        self.frontmost_changed |= matches!(
            (baseline.frontmost, final_sample.frontmost),
            (Some(before), Some(after)) if before != after
        );
        self.key_window_changed |= matches!(
            (baseline.key_window, final_sample.key_window),
            (Ok(before), Ok(after)) if before != after
        );
        self.physical_cursor_moved |= matches!(
            (baseline.cursor, final_sample.cursor),
            (Ok(before), Ok(after)) if before != after
        );
    }

    fn complete(&self) -> bool {
        self.workspace_stream_complete
            && self.ax_stream_complete
            && self.hid_stream_complete
            && self.containment_stream_complete
    }

    fn posture(&self, baseline: PostureSample, final_sample: PostureSample) -> PostureResult {
        let excursion =
            self.frontmost_changed || self.key_window_changed || self.physical_cursor_moved;
        let restored = excursion
            && baseline.frontmost == final_sample.frontmost
            && baseline.key_window == final_sample.key_window
            && baseline.cursor == final_sample.cursor;
        PostureResult {
            held: self.complete() && !excursion,
            frontmost_changed: self.frontmost_changed,
            key_window_changed: self.key_window_changed,
            physical_cursor_moved: self.physical_cursor_moved,
            restored_after_violation: restored,
        }
    }
}

enum ObserverCommand {
    Close {
        deadline: Instant,
        reply: mpsc::SyncSender<bool>,
    },
}

struct ObserverControl {
    commands: mpsc::Sender<ObserverCommand>,
    run_loop: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
    closed: bool,
}

impl ObserverControl {
    fn close(&mut self, timeout: Duration) -> bool {
        if self.closed {
            return true;
        }
        self.closed = true;
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let native_deadline = Instant::now() + timeout.saturating_sub(OBSERVER_RUN_LOOP_INTERVAL);
        let sent = self
            .commands
            .send(ObserverCommand::Close {
                deadline: native_deadline,
                reply: reply_tx,
            })
            .is_ok();
        let run_loop = self.run_loop.load(Ordering::Acquire) as CFRunLoopRef;
        if !run_loop.is_null() {
            unsafe { CFRunLoopWakeUp(run_loop) };
        }
        let closed = sent && reply_rx.recv_timeout(timeout).unwrap_or(false);
        // Never join a possibly wedged AX or HID native thread. Dropping a
        // JoinHandle detaches it; the thread owns its callback context and will
        // free it if/when the native run loop returns.
        self.thread.take();
        closed
    }
}

impl Drop for ObserverControl {
    fn drop(&mut self) {
        let _ = self.close(OBSERVER_CLOSE_TIMEOUT);
    }
}

struct AxFocusContext {
    state: Arc<Mutex<PostureProofState>>,
}

unsafe extern "C" fn ax_focus_callback(
    _observer: AXObserverRef,
    _element: AXUIElementRef,
    notification: CFStringRef,
    refcon: *mut c_void,
) {
    if notification.is_null() || refcon.is_null() {
        return;
    }
    let notification = CFString::wrap_under_get_rule(notification).to_string();
    if !matches!(
        notification.as_str(),
        "AXFocusedWindowChanged" | "AXFocusedUIElementChanged"
    ) {
        return;
    }
    let context = &*(refcon as *const AxFocusContext);
    let mut state = context
        .state
        .lock()
        .expect("posture AX proof state poisoned");
    state.key_window_changed = true;
    state.ax_focus_events = state.ax_focus_events.saturating_add(1);
}

fn start_ax_focus_stream(
    pid: i32,
    state: Arc<Mutex<PostureProofState>>,
) -> Result<ObserverControl, NativeError> {
    let run_loop = Arc::new(AtomicUsize::new(0));
    let thread_run_loop = Arc::clone(&run_loop);
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (command_tx, command_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name(format!("cua-posture-ax-{pid}"))
        .spawn(move || unsafe {
            run_ax_focus_stream(pid, state, &thread_run_loop, &started_tx, &command_rx)
        })
        .map_err(|error| {
            posture_preflight(format!("failed to spawn AX posture stream: {error}"))
        })?;
    let mut control = ObserverControl {
        commands: command_tx,
        run_loop,
        thread: Some(thread),
        closed: false,
    };
    match started_rx.recv_timeout(OBSERVER_HANDSHAKE_TIMEOUT) {
        Ok(Ok(())) => Ok(control),
        Ok(Err(error)) => {
            let _ = control.close(OBSERVER_CLOSE_TIMEOUT);
            Err(error)
        }
        Err(_) => {
            let _ = control.close(OBSERVER_CLOSE_TIMEOUT);
            Err(posture_preflight(
                "AX posture stream registration exceeded its bounded deadline",
            ))
        }
    }
}

unsafe fn run_ax_focus_stream(
    pid: i32,
    state: Arc<Mutex<PostureProofState>>,
    run_loop_slot: &AtomicUsize,
    started: &mpsc::SyncSender<Result<(), NativeError>>,
    commands: &mpsc::Receiver<ObserverCommand>,
) {
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        let _ = started.send(Err(posture_preflight(
            "foreground AX application element is unavailable",
        )));
        return;
    }
    let context = Box::into_raw(Box::new(AxFocusContext { state }));
    let mut observer = std::ptr::null_mut();
    let create_error = AXObserverCreate(pid, ax_focus_callback, &mut observer);
    if create_error != kAXErrorSuccess || observer.is_null() {
        drop(Box::from_raw(context));
        CFRelease(application as CFTypeRef);
        let _ = started.send(Err(posture_preflight(format!(
            "foreground AX posture observer creation failed with {create_error}"
        ))));
        return;
    }
    let notifications = ["AXFocusedWindowChanged", "AXFocusedUIElementChanged"];
    let mut registered = 0usize;
    for notification in notifications {
        let notification = CFString::new(notification);
        if AXObserverAddNotification(
            observer,
            application,
            notification.as_concrete_TypeRef(),
            context.cast(),
        ) == kAXErrorSuccess
        {
            registered += 1;
        }
    }
    let source = AXObserverGetRunLoopSource(observer);
    if registered != notifications.len() || source.is_null() {
        remove_ax_focus_notifications(observer, application, &notifications);
        CFRelease(observer as CFTypeRef);
        drop(Box::from_raw(context));
        CFRelease(application as CFTypeRef);
        let _ = started.send(Err(posture_preflight(
            "foreground AX focus/key event stream is incomplete",
        )));
        return;
    }
    let run_loop = CFRunLoopGetCurrent();
    run_loop_slot.store(run_loop as usize, Ordering::Release);
    CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
    if started.send(Ok(())).is_err() {
        remove_ax_focus_notifications(observer, application, &notifications);
        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
        run_loop_slot.store(0, Ordering::Release);
        CFRelease(observer as CFTypeRef);
        drop(Box::from_raw(context));
        CFRelease(application as CFTypeRef);
        return;
    }

    let close_reply = loop {
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            OBSERVER_RUN_LOOP_INTERVAL,
            true,
        );
        match commands.try_recv() {
            Ok(ObserverCommand::Close { deadline, reply }) => {
                break Some((reply, drain_native_run_loop(deadline)))
            }
            Err(mpsc::TryRecvError::Disconnected) => break None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    };
    remove_ax_focus_notifications(observer, application, &notifications);
    CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
    run_loop_slot.store(0, Ordering::Release);
    CFRelease(observer as CFTypeRef);
    drop(Box::from_raw(context));
    CFRelease(application as CFTypeRef);
    if let Some((reply, drained)) = close_reply {
        let _ = reply.send(drained);
    }
}

unsafe fn remove_ax_focus_notifications(
    observer: AXObserverRef,
    application: AXUIElementRef,
    notifications: &[&str],
) {
    for notification in notifications {
        let notification = CFString::new(notification);
        let _ = bindings::AXObserverRemoveNotification(
            observer,
            application,
            notification.as_concrete_TypeRef(),
        );
    }
}

fn start_hid_cursor_stream(
    state: Arc<Mutex<PostureProofState>>,
) -> Result<ObserverControl, NativeError> {
    let run_loop = Arc::new(AtomicUsize::new(0));
    let thread_run_loop = Arc::clone(&run_loop);
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (command_tx, command_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("cua-posture-hid-cursor".to_owned())
        .spawn(move || run_hid_cursor_stream(state, &thread_run_loop, &started_tx, &command_rx))
        .map_err(|error| {
            posture_preflight(format!("failed to spawn HID posture stream: {error}"))
        })?;
    let mut control = ObserverControl {
        commands: command_tx,
        run_loop,
        thread: Some(thread),
        closed: false,
    };
    match started_rx.recv_timeout(OBSERVER_HANDSHAKE_TIMEOUT) {
        Ok(Ok(())) => Ok(control),
        Ok(Err(error)) => {
            let _ = control.close(OBSERVER_CLOSE_TIMEOUT);
            Err(error)
        }
        Err(_) => {
            let _ = control.close(OBSERVER_CLOSE_TIMEOUT);
            Err(posture_preflight(
                "HID cursor stream registration exceeded its bounded deadline",
            ))
        }
    }
}

fn run_hid_cursor_stream(
    state: Arc<Mutex<PostureProofState>>,
    run_loop_slot: &AtomicUsize,
    started: &mpsc::SyncSender<Result<(), NativeError>>,
    commands: &mpsc::Receiver<ObserverCommand>,
) {
    let callback_state = Arc::clone(&state);
    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
        ],
        move |_proxy, event_type, _event| {
            let mut state = callback_state
                .lock()
                .expect("posture HID proof state poisoned");
            match event_type {
                CGEventType::MouseMoved
                | CGEventType::LeftMouseDragged
                | CGEventType::RightMouseDragged
                | CGEventType::OtherMouseDragged => {
                    state.physical_cursor_moved = true;
                    state.hid_cursor_events = state.hid_cursor_events.saturating_add(1);
                }
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    state.hid_stream_complete = false;
                }
                _ => {}
            }
            None
        },
    ) {
        Ok(tap) => tap,
        Err(()) => {
            let _ = started.send(Err(posture_preflight(
                "listen-only HID cursor event tap is unavailable",
            )));
            return;
        }
    };
    let source = match tap.mach_port.create_runloop_source(0) {
        Ok(source) => source,
        Err(()) => {
            let _ = started.send(Err(posture_preflight(
                "listen-only HID cursor event tap has no run-loop source",
            )));
            return;
        }
    };
    let run_loop = CFRunLoop::get_current();
    run_loop_slot.store(run_loop.as_concrete_TypeRef() as usize, Ordering::Release);
    run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    if started.send(Ok(())).is_err() {
        run_loop.remove_source(&source, unsafe { kCFRunLoopCommonModes });
        run_loop_slot.store(0, Ordering::Release);
        return;
    }

    let close_reply = loop {
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            OBSERVER_RUN_LOOP_INTERVAL,
            true,
        );
        match commands.try_recv() {
            Ok(ObserverCommand::Close { deadline, reply }) => {
                break Some((reply, drain_native_run_loop(deadline)))
            }
            Err(mpsc::TryRecvError::Disconnected) => break None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    };
    run_loop.remove_source(&source, unsafe { kCFRunLoopCommonModes });
    run_loop_slot.store(0, Ordering::Release);
    if let Some((reply, drained)) = close_reply {
        let _ = reply.send(drained);
    }
}

fn drain_native_run_loop(deadline: Instant) -> bool {
    while Instant::now() < deadline {
        let result = CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(1),
            true,
        );
        if result != core_foundation::runloop::CFRunLoopRunResult::HandledSource {
            return true;
        }
    }
    false
}

pub struct MacLaunchPostureWitness {
    baseline: PostureSample,
    target_pid: Option<i32>,
    events: tokio::sync::broadcast::Receiver<WorkspaceEvent>,
    state: Arc<Mutex<PostureProofState>>,
    ax_stream: ObserverControl,
    hid_stream: ObserverControl,
    containment: DeadlineSuppression,
    finished: bool,
}

impl MacLaunchPostureWitness {
    pub fn begin(deadline: Instant) -> Result<Self, NativeError> {
        let workspace = WorkspaceEventHub::shared();
        let mut events = workspace.subscribe();
        let initial_frontmost = apps::frontmost_pid().ok_or_else(|| {
            posture_preflight("exact foreground baseline pid is unavailable before launch")
        })?;
        let state = Arc::new(Mutex::new(PostureProofState::new()));
        let ax_stream = start_ax_focus_stream(initial_frontmost, Arc::clone(&state))?;
        let hid_stream = start_hid_cursor_stream(Arc::clone(&state))?;
        if !workspace.barrier(NATIVE_BARRIER_TIMEOUT)
            || !crate::focus_steal::FocusStealPreventer::barrier(NATIVE_BARRIER_TIMEOUT)
        {
            return Err(posture_preflight(
                "native posture callback queues were unavailable before launch dispatch",
            ));
        }
        let baseline = exact_posture_sample();
        if !baseline.complete() || baseline.frontmost != Some(initial_frontmost) {
            return Err(posture_preflight(
                "foreground, AX focus/key, or cursor baseline changed during witness acquisition",
            ));
        }
        {
            let mut proof = state.lock().expect("launch posture proof state poisoned");
            proof.observe_workspace(&mut events, initial_frontmost);
            if proof.frontmost_changed
                || proof.key_window_changed
                || proof.physical_cursor_moved
                || !proof.complete()
            {
                return Err(posture_preflight(
                    "foreground, AX focus/key, or HID cursor changed during witness acquisition",
                ));
            }
        }
        let containment = crate::focus_steal::begin_deadline_owned_suppression_until(
            None,
            initial_frontmost,
            "driver.v2.launch.deadline_owned",
            deadline,
        );
        Ok(Self {
            baseline,
            target_pid: None,
            events,
            state,
            ax_stream,
            hid_stream,
            containment,
            finished: false,
        })
    }

    /// Clone for the LaunchServices completion block. If the awaiting task is
    /// cancelled, this handle still narrows the detached wildcard and the
    /// dispatcher retains it through the native deadline.
    pub fn completion_containment(&self) -> DeadlineSuppression {
        self.containment.clone()
    }

    pub fn set_target(&mut self, pid: i32) -> Result<(), NativeError> {
        self.target_pid = Some(pid);
        if self
            .containment
            .narrow_to_target(pid, NATIVE_BARRIER_TIMEOUT)
        {
            Ok(())
        } else {
            self.state
                .lock()
                .expect("launch posture proof state poisoned")
                .containment_stream_complete = false;
            Err(posture_preflight(
                "launch containment wildcard could not be narrowed on its serial callback queue",
            ))
        }
    }

    pub fn drain_events(&mut self) {
        let baseline_frontmost = self
            .baseline
            .frontmost
            .expect("launch posture baseline was validated");
        self.state
            .lock()
            .expect("launch posture proof state poisoned")
            .observe_workspace(&mut self.events, baseline_frontmost);
    }

    pub fn finish(mut self) -> (PostureResult, NativeEvidence) {
        let containment = self.containment.close_with_evidence(NATIVE_BARRIER_TIMEOUT);
        let workspace_barrier = WorkspaceEventHub::shared().barrier(NATIVE_BARRIER_TIMEOUT);
        self.drain_events();
        let ax_barrier = self.ax_stream.close(OBSERVER_CLOSE_TIMEOUT);
        let hid_barrier = self.hid_stream.close(OBSERVER_CLOSE_TIMEOUT);
        let final_sample = exact_posture_sample();

        let mut state = self
            .state
            .lock()
            .expect("launch posture proof state poisoned");
        state.workspace_stream_complete &= workspace_barrier;
        state.ax_stream_complete &= ax_barrier;
        state.hid_stream_complete &= hid_barrier;
        state.containment_stream_complete &= containment.callback_queue_drained;
        if containment.evidence.activations > 0 {
            state.frontmost_changed = true;
        }
        state.observe_final_sample(self.baseline, final_sample);
        let posture = state.posture(self.baseline, final_sample);
        let evidence = posture_evidence(
            &state,
            self.baseline,
            final_sample,
            self.target_pid,
            containment,
        );
        drop(state);
        self.finished = true;
        (posture, evidence)
    }
}

impl Drop for MacLaunchPostureWitness {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Deliberately do not close deadline-owned containment. Cancellation
        // can drop this witness while LaunchServices still owns the completion
        // block; the dispatcher entry must survive until its native deadline.
        let _ = WorkspaceEventHub::shared().barrier(NATIVE_BARRIER_TIMEOUT);
        let _ = self.ax_stream.close(OBSERVER_CLOSE_TIMEOUT);
        let _ = self.hid_stream.close(OBSERVER_CLOSE_TIMEOUT);
    }
}

pub struct MacInteractionPostureWitness {
    baseline: PostureSample,
    events: tokio::sync::broadcast::Receiver<WorkspaceEvent>,
    state: Arc<Mutex<PostureProofState>>,
    ax_stream: ObserverControl,
    hid_stream: ObserverControl,
    finished: Option<(PostureResult, NativeEvidence)>,
}

impl MacInteractionPostureWitness {
    pub fn begin() -> Result<Self, NativeError> {
        let workspace = WorkspaceEventHub::shared();
        let mut events = workspace.subscribe();
        let initial_frontmost = apps::frontmost_pid().ok_or_else(|| {
            posture_preflight("exact foreground baseline pid is unavailable before interaction")
        })?;
        let state = Arc::new(Mutex::new(PostureProofState::new()));
        let ax_stream = start_ax_focus_stream(initial_frontmost, Arc::clone(&state))?;
        let hid_stream = start_hid_cursor_stream(Arc::clone(&state))?;
        if !workspace.barrier(NATIVE_BARRIER_TIMEOUT) {
            return Err(posture_preflight(
                "workspace posture callback queue was unavailable before interaction",
            ));
        }
        let baseline = exact_posture_sample();
        if !baseline.complete() || baseline.frontmost != Some(initial_frontmost) {
            return Err(posture_preflight(
                "foreground, AX focus/key, or cursor baseline changed during witness acquisition",
            ));
        }
        {
            let mut proof = state
                .lock()
                .expect("interaction posture proof state poisoned");
            proof.observe_workspace(&mut events, initial_frontmost);
            if proof.frontmost_changed
                || proof.key_window_changed
                || proof.physical_cursor_moved
                || !proof.complete()
            {
                return Err(posture_preflight(
                    "foreground, AX focus/key, or HID cursor changed during witness acquisition",
                ));
            }
        }
        Ok(Self {
            baseline,
            events,
            state,
            ax_stream,
            hid_stream,
            finished: None,
        })
    }

    pub fn finish(&mut self, deadline: Instant) -> (PostureResult, NativeEvidence) {
        if let Some(finished) = &self.finished {
            return finished.clone();
        }
        let workspace_barrier =
            WorkspaceEventHub::shared().barrier(remaining_until(deadline, NATIVE_BARRIER_TIMEOUT));
        let baseline_frontmost = self
            .baseline
            .frontmost
            .expect("interaction posture baseline was validated");
        let mut state = self
            .state
            .lock()
            .expect("interaction posture proof state poisoned");
        state.observe_workspace(&mut self.events, baseline_frontmost);
        drop(state);
        let ax_barrier = self
            .ax_stream
            .close(remaining_until(deadline, OBSERVER_CLOSE_TIMEOUT));
        let hid_barrier = self
            .hid_stream
            .close(remaining_until(deadline, OBSERVER_CLOSE_TIMEOUT));
        let final_sample = exact_posture_sample();
        let mut state = self
            .state
            .lock()
            .expect("interaction posture proof state poisoned");
        state.workspace_stream_complete &= workspace_barrier;
        state.ax_stream_complete &= ax_barrier;
        state.hid_stream_complete &= hid_barrier;
        state.observe_final_sample(self.baseline, final_sample);
        let posture = state.posture(self.baseline, final_sample);
        let evidence = posture_evidence(
            &state,
            self.baseline,
            final_sample,
            None,
            SuppressionCloseOutcome {
                evidence: SuppressionOutcome::default(),
                callback_queue_drained: true,
            },
        );
        let finished = (posture, evidence);
        drop(state);
        self.finished = Some(finished.clone());
        finished
    }

    pub fn prior_frontmost_pid(&self) -> i32 {
        self.baseline
            .frontmost
            .expect("interaction posture baseline was validated")
    }
}

impl Drop for MacInteractionPostureWitness {
    fn drop(&mut self) {
        let _ = self.finish(Instant::now() + OBSERVER_CLOSE_TIMEOUT + OBSERVER_CLOSE_TIMEOUT);
    }
}

fn remaining_until(deadline: Instant, maximum: Duration) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .min(maximum)
}

fn posture_evidence(
    state: &PostureProofState,
    baseline: PostureSample,
    final_sample: PostureSample,
    target_pid: Option<i32>,
    containment: SuppressionCloseOutcome,
) -> NativeEvidence {
    let mut evidence = NativeEvidence::default();
    evidence.fields.insert(
        "prior_frontmost_pid".to_owned(),
        serde_json::to_value(baseline.frontmost).unwrap_or_default(),
    );
    evidence.fields.insert(
        "final_frontmost_pid".to_owned(),
        serde_json::to_value(final_sample.frontmost).unwrap_or_default(),
    );
    evidence.fields.insert(
        "prior_key_window_id".to_owned(),
        serde_json::to_value(baseline.key_window.ok().flatten()).unwrap_or_default(),
    );
    evidence.fields.insert(
        "final_key_window_id".to_owned(),
        serde_json::to_value(final_sample.key_window.ok().flatten()).unwrap_or_default(),
    );
    evidence.fields.insert(
        "prior_physical_cursor".to_owned(),
        serde_json::to_value(baseline.cursor.ok()).unwrap_or_default(),
    );
    evidence.fields.insert(
        "final_physical_cursor".to_owned(),
        serde_json::to_value(final_sample.cursor.ok()).unwrap_or_default(),
    );
    evidence.fields.insert(
        "activation_pids".to_owned(),
        serde_json::to_value(&state.activation_pids).unwrap_or_default(),
    );
    evidence.fields.insert(
        "target_pid".to_owned(),
        serde_json::to_value(target_pid).unwrap_or_default(),
    );
    evidence.fields.insert(
        "workspace_event_stream_complete".to_owned(),
        state.workspace_stream_complete.into(),
    );
    evidence.fields.insert(
        "ax_focus_event_stream_complete".to_owned(),
        state.ax_stream_complete.into(),
    );
    evidence.fields.insert(
        "hid_cursor_event_stream_complete".to_owned(),
        state.hid_stream_complete.into(),
    );
    evidence.fields.insert(
        "containment_event_stream_complete".to_owned(),
        state.containment_stream_complete.into(),
    );
    evidence.fields.insert(
        "workspace_event_lagged_count".to_owned(),
        state.workspace_lagged_events.into(),
    );
    evidence.fields.insert(
        "ax_focus_event_count".to_owned(),
        state.ax_focus_events.into(),
    );
    evidence.fields.insert(
        "hid_cursor_event_count".to_owned(),
        state.hid_cursor_events.into(),
    );
    evidence.fields.insert(
        "containment_callback_queue_drained".to_owned(),
        containment.callback_queue_drained.into(),
    );
    insert_suppression_evidence(&mut evidence, containment.evidence);
    evidence
}

fn insert_suppression_evidence(evidence: &mut NativeEvidence, outcome: SuppressionOutcome) {
    evidence.fields.insert(
        "containment_activations".to_owned(),
        outcome.activations.into(),
    );
    evidence.fields.insert(
        "containment_restore_attempts".to_owned(),
        outcome.restore_attempts.into(),
    );
    evidence.fields.insert(
        "containment_restore_failures".to_owned(),
        outcome.restore_failures.into(),
    );
}

#[derive(Debug, Clone, Copy)]
struct PostureSample {
    frontmost: Option<i32>,
    key_window: Result<Option<u32>, ()>,
    cursor: Result<(f64, f64), ()>,
}

impl PostureSample {
    fn complete(self) -> bool {
        self.frontmost.is_some() && self.key_window.is_ok() && self.cursor.is_ok()
    }
}

fn exact_posture_sample() -> PostureSample {
    let frontmost = apps::frontmost_pid();
    PostureSample {
        frontmost,
        key_window: frontmost.map(focused_window_for_pid).unwrap_or(Err(())),
        cursor: cursor_position().ok_or(()),
    }
}

fn focused_window_for_pid(pid: i32) -> Result<Option<u32>, ()> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err(());
        }
        let attribute = CFString::new("AXFocusedWindow");
        let mut value: CFTypeRef = std::ptr::null();
        let error = AXUIElementCopyAttributeValue(app, attribute.as_concrete_TypeRef(), &mut value);
        CFRelease(app as CFTypeRef);
        if error == kAXErrorNoValue {
            return Ok(None);
        }
        if error != kAXErrorSuccess || value.is_null() {
            return Err(());
        }
        if CFGetTypeID(value) != AXUIElementGetTypeID() {
            CFRelease(value);
            return Err(());
        }
        let window_id = bindings::ax_get_window_id(value.cast_mut().cast());
        CFRelease(value);
        window_id.map(Some).ok_or(())
    }
}

fn cursor_position() -> Option<(f64, f64)> {
    use core_graphics::{
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let event = CGEvent::new(source).ok()?;
    let point = event.location();
    Some((point.x, point.y))
}

fn posture_preflight(message: impl Into<String>) -> NativeError {
    NativeError::new(
        ErrorCode::PostureUnverifiable,
        ErrorPhase::Preflight,
        true,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PostureSample {
        PostureSample {
            frontmost: Some(10),
            key_window: Ok(Some(100)),
            cursor: Ok((5.0, 6.0)),
        }
    }

    #[test]
    fn ordered_event_excursion_remains_a_violation_after_exact_restoration() {
        let baseline = sample();
        let mut state = PostureProofState::new();
        state.frontmost_changed = true;
        state.activation_pids.extend([20, 10]);
        let posture = state.posture(baseline, baseline);
        assert!(!posture.held);
        assert!(posture.frontmost_changed);
        assert!(posture.restored_after_violation);
    }

    #[test]
    fn each_incomplete_native_stream_refuses_a_clean_posture_claim() {
        for stream in ["workspace", "ax", "hid", "containment"] {
            let baseline = sample();
            let mut state = PostureProofState::new();
            match stream {
                "workspace" => state.workspace_stream_complete = false,
                "ax" => state.ax_stream_complete = false,
                "hid" => state.hid_stream_complete = false,
                "containment" => state.containment_stream_complete = false,
                _ => unreachable!(),
            }
            assert!(!state.posture(baseline, baseline).held, "{stream}");
        }
    }

    #[test]
    fn ax_and_hid_events_are_primary_transient_proof_not_polling_samples() {
        let baseline = sample();
        let mut state = PostureProofState::new();
        state.key_window_changed = true;
        state.ax_focus_events = 1;
        state.physical_cursor_moved = true;
        state.hid_cursor_events = 1;
        let posture = state.posture(baseline, baseline);
        assert!(!posture.held);
        assert!(posture.key_window_changed);
        assert!(posture.physical_cursor_moved);
        assert!(posture.restored_after_violation);
    }

    #[test]
    fn unreadable_final_diagnostic_marks_all_corresponding_streams_unverifiable() {
        let baseline = sample();
        let mut state = PostureProofState::new();
        state.observe_final_sample(
            baseline,
            PostureSample {
                frontmost: None,
                key_window: Err(()),
                cursor: Err(()),
            },
        );
        assert!(!state.complete());
        assert!(!state.posture(baseline, baseline).held);
    }
}
