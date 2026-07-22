//! Layer-3 focus-steal preventer — Rust port of Swift's
//! `SystemFocusStealPreventer.swift` plus the PR #1521 4-layer hardening
//! (closure scope, RAII lease, 5s monotonic deadline, 1s janitor).
//!
//! ## What this protects against
//!
//! `NSWorkspace.OpenConfiguration.activates = false` tells LaunchServices
//! "don't activate the target on launch". LaunchServices honors that.
//! What it does NOT do is stop the launched app from calling
//! `NSApp.activate(ignoringOtherApps:)` in its own
//! `applicationDidFinishLaunching`. Chrome, Electron, Safari, Calculator
//! all do exactly that — so a "background" launch flashes the target on
//! top of the user's work for a few frames.
//!
//! The preventer subscribes to
//! `NSWorkspace.didActivateApplicationNotification` and, when an activation
//! matches a registered suppression entry, immediately re-activates the
//! prior frontmost app on a background thread. AppKit's
//! `-[NSRunningApplication activateWithOptions:]` is documented thread-safe
//! — no main-thread hop required.
//!
//! ## Layered design (matches PR #1521)
//!
//! 1. **Closure API** — `with_suppression(target, restore_to, origin, f)`
//!    begins an entry, awaits `f`, ends the entry. Use this when the
//!    suppression scope is a single async block.
//! 2. **RAII API** — `begin_suppression(target, restore_to, origin)`
//!    returns a `SuppressionLease`. `Drop` ends the entry synchronously
//!    (no awaiting). Use this when the caller needs to hold the lease
//!    across multiple branches or when async cancellation may interrupt
//!    the closure path.
//! 3. **5s monotonic deadline** — every entry stamps an
//!    `Instant::now() + 5s`. The observer prunes expired entries before
//!    matching, so a leaked lease can't cause a stale entry to keep
//!    re-activating the prior frontmost app forever.
//! 4. **1s janitor** — a tokio interval task wakes up every second
//!    while the dispatcher is non-empty, prunes expired entries, and
//!    stops when the map drains. Re-starts when the next entry is
//!    added. Coordinated via `tokio::sync::watch`.
//!
//! ## Singleton
//!
//! `FocusStealPreventer::shared()` returns a process-wide
//! `Arc<FocusStealPreventer>`. The observer registration happens inside
//! `OnceLock::get_or_init`, so it's safe to call from any thread without
//! racing on observer install.
//!
//! ## Why a fresh background `NSOperationQueue` (not `mainQueue`)
//!
//! NSWorkspace's block-based observer fires on the queue you give it. If
//! the queue is `nil` (Swift default) or `mainQueue`, the block runs on
//! the main thread — which means it requires a live main run loop.
//! `cua-driver call` (one-shot subcommand) and `--no-overlay` mode don't
//! have one, so the activation observer would never fire. A fresh
//! background `NSOperationQueue` sidesteps that — the block runs on the
//! queue's own thread regardless of run-loop state.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification,
};
use objc2_foundation::NSOperationQueue;
use uuid::Uuid;

/// Per-entry deadline. After this much wall-clock time the dispatcher's
/// observer (and the janitor) treats the entry as leaked and prunes it
/// without firing. Mirrors Swift PR #1521.
const ENTRY_DEADLINE: Duration = Duration::from_secs(5);

/// Janitor tick interval. The task wakes up this often while the
/// dispatcher is non-empty and prunes expired entries.
const JANITOR_TICK: Duration = Duration::from_secs(1);

/// Identifier for a suppression. `with_suppression` and `begin_suppression`
/// hand one of these back; `end_suppression` consumes it.
#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub struct SuppressionHandle(Uuid);

/// Dispatcher-internal entry shape.
#[derive(Debug)]
struct Entry {
    /// `Some(pid)` matches only that pid's activations. `None` is a
    /// wildcard — matches any activation whose pid != `restore_to`.
    /// The wildcard variant is used while a launch is in flight and the
    /// real pid isn't known yet.
    target_pid: Option<i32>,
    /// Pid to restore focus to when an activation matches this entry.
    restore_to: i32,
    /// Monotonic deadline. After this, the entry is pruned without
    /// firing.
    deadline: Instant,
    /// Provenance for tracing — e.g. `"LaunchAppTool.pre"`.
    #[allow(dead_code)]
    origin: &'static str,
    evidence: Arc<SuppressionEvidenceState>,
}

#[derive(Debug, Default)]
struct SuppressionEvidenceState {
    activations: AtomicUsize,
    restore_attempts: AtomicUsize,
    restore_failures: AtomicUsize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuppressionOutcome {
    pub activations: usize,
    pub restore_attempts: usize,
    pub restore_failures: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuppressionCloseOutcome {
    pub evidence: SuppressionOutcome,
    /// True only when removal ran on the serial observer queue after every
    /// containment callback already queued there.
    pub callback_queue_drained: bool,
}

/// Singleton focus-steal preventer.
///
/// Constructed lazily on first `shared()` call. Owns the dispatcher state
/// (Sync via the inner Mutex); the observer token is process-lifetime and the
/// serial callback queue is retained for bounded evidence barriers.
pub struct FocusStealPreventer {
    dispatcher: Arc<Dispatcher>,
    observer_queue: Retained<NSOperationQueue>,
}

impl FocusStealPreventer {
    /// Return (or initialize on first call) the process-wide singleton.
    pub fn shared() -> Arc<Self> {
        static SINGLETON: OnceLock<Arc<FocusStealPreventer>> = OnceLock::new();
        SINGLETON
            .get_or_init(|| {
                let dispatcher = Arc::new(Dispatcher::new());
                let observer_queue = install_observer(&dispatcher);
                Arc::new(FocusStealPreventer {
                    dispatcher,
                    observer_queue,
                })
            })
            .clone()
    }

    /// Begin suppressing focus-steals targeting `target_pid` (or any pid
    /// when `None`, the wildcard). Returns a `SuppressionLease` whose
    /// `Drop` ends the entry synchronously.
    ///
    /// `restore_to` is the pid the preventer re-activates if it matches
    /// a notification. `origin` is a static label for tracing.
    pub fn begin_suppression(
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
    ) -> SuppressionLease {
        Self::begin_suppression_until(
            target_pid,
            restore_to,
            origin,
            Instant::now() + ENTRY_DEADLINE,
        )
    }

    /// Begin a caller-bounded suppression whose leak deadline covers the
    /// complete native operation span. Long-running v2 operations use their
    /// own deadline instead of silently losing containment after five seconds.
    pub fn begin_suppression_until(
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
        deadline: Instant,
    ) -> SuppressionLease {
        let shared = Self::shared();
        let handle = shared
            .dispatcher
            .add_until(target_pid, restore_to, origin, deadline);
        SuppressionLease {
            handle,
            dispatcher: Arc::clone(&shared.dispatcher),
            evidence: shared.dispatcher.evidence(handle),
            released: false,
        }
    }

    /// Register containment whose dispatcher entry, rather than a future or
    /// callback stack frame, owns the native deadline. Dropping every Rust
    /// handle intentionally leaves the entry armed until its deadline; normal
    /// completion must call [`DeadlineSuppression::close_with_evidence`].
    pub fn begin_deadline_owned_suppression_until(
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
        deadline: Instant,
    ) -> DeadlineSuppression {
        let shared = Self::shared();
        let handle = shared
            .dispatcher
            .add_until(target_pid, restore_to, origin, deadline);
        DeadlineSuppression {
            handle,
            dispatcher: Arc::clone(&shared.dispatcher),
            evidence: shared.dispatcher.evidence(handle),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run `f` with a suppression entry active. Equivalent to
    /// `begin_suppression(...)` + run `f` + drop the lease — but expressed
    /// as a single async call site.
    pub async fn with_suppression<R, F, Fut>(
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
        f: F,
    ) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        let _lease = Self::begin_suppression(target_pid, restore_to, origin);
        f().await
    }

    /// Wait until every focus-containment callback queued before this call has
    /// run. Callers fail closed when the bounded barrier does not complete.
    pub fn barrier(timeout: Duration) -> bool {
        let shared = Self::shared();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let block = block2::RcBlock::new(move || {
            let _ = sender.send(());
        });
        unsafe { shared.observer_queue.addBarrierBlock(&block) };
        receiver.recv_timeout(timeout).is_ok()
    }

    fn close_handle(
        dispatcher: Arc<Dispatcher>,
        handle: SuppressionHandle,
        evidence: Arc<SuppressionEvidenceState>,
        timeout: Duration,
    ) -> SuppressionCloseOutcome {
        let shared = Self::shared();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let queued_dispatcher = Arc::clone(&dispatcher);
        let queued_evidence = Arc::clone(&evidence);
        let block = block2::RcBlock::new(move || {
            queued_dispatcher.remove(handle);
            let _ = sender.send(suppression_outcome(&queued_evidence));
        });
        unsafe { shared.observer_queue.addBarrierBlock(&block) };
        match receiver.recv_timeout(timeout) {
            Ok(evidence) => SuppressionCloseOutcome {
                evidence,
                callback_queue_drained: true,
            },
            Err(_) => {
                // Never strand containment just because the proof barrier
                // timed out. Removal and the best evidence currently visible
                // are still returned; the caller must classify this as
                // posture_unverifiable rather than success.
                dispatcher.remove(handle);
                SuppressionCloseOutcome {
                    evidence: suppression_outcome(&evidence),
                    callback_queue_drained: false,
                }
            }
        }
    }

    fn narrow_handle(
        dispatcher: Arc<Dispatcher>,
        handle: SuppressionHandle,
        target_pid: i32,
        timeout: Duration,
    ) -> bool {
        let shared = Self::shared();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let queued_dispatcher = Arc::clone(&dispatcher);
        let block = block2::RcBlock::new(move || {
            queued_dispatcher.retarget(handle, Some(target_pid));
            let _ = sender.send(());
        });
        unsafe { shared.observer_queue.addBarrierBlock(&block) };
        if receiver.recv_timeout(timeout).is_ok() {
            true
        } else {
            // Preserve containment even when the ordering proof is lost.
            dispatcher.retarget(handle, Some(target_pid));
            false
        }
    }
}

/// Begin-suppression convenience that bounces through the singleton.
pub fn begin_suppression(
    target_pid: Option<i32>,
    restore_to: i32,
    origin: &'static str,
) -> SuppressionLease {
    FocusStealPreventer::begin_suppression(target_pid, restore_to, origin)
}

/// Begin suppression through a caller-owned bounded native deadline.
pub fn begin_suppression_until(
    target_pid: Option<i32>,
    restore_to: i32,
    origin: &'static str,
    deadline: Instant,
) -> SuppressionLease {
    FocusStealPreventer::begin_suppression_until(target_pid, restore_to, origin, deadline)
}

pub fn begin_deadline_owned_suppression_until(
    target_pid: Option<i32>,
    restore_to: i32,
    origin: &'static str,
    deadline: Instant,
) -> DeadlineSuppression {
    FocusStealPreventer::begin_deadline_owned_suppression_until(
        target_pid, restore_to, origin, deadline,
    )
}

/// Cloneable handle to a dispatcher-owned containment entry. `Drop` is
/// intentionally a no-op: cancellation must not disarm a LaunchServices
/// callback that can still activate an app before the native deadline.
#[derive(Clone)]
pub struct DeadlineSuppression {
    handle: SuppressionHandle,
    dispatcher: Arc<Dispatcher>,
    evidence: Arc<SuppressionEvidenceState>,
    closed: Arc<AtomicBool>,
}

impl DeadlineSuppression {
    /// Narrow a launch wildcard to the pid returned by LaunchServices without
    /// a remove/add gap. The serial-queue result is evidence, not best effort.
    pub fn narrow_to_target(&self, target_pid: i32, timeout: Duration) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        FocusStealPreventer::narrow_handle(
            Arc::clone(&self.dispatcher),
            self.handle,
            target_pid,
            timeout,
        )
    }

    pub fn close_with_evidence(&self, timeout: Duration) -> SuppressionCloseOutcome {
        if self.closed.swap(true, Ordering::AcqRel) {
            return SuppressionCloseOutcome {
                evidence: suppression_outcome(&self.evidence),
                callback_queue_drained: true,
            };
        }
        FocusStealPreventer::close_handle(
            Arc::clone(&self.dispatcher),
            self.handle,
            Arc::clone(&self.evidence),
            timeout,
        )
    }

    pub fn outcome(&self) -> SuppressionOutcome {
        suppression_outcome(&self.evidence)
    }
}

/// RAII lease. `Drop` ends the entry synchronously, so the entry is
/// removed even if a future is cancelled mid-await.
pub struct SuppressionLease {
    handle: SuppressionHandle,
    dispatcher: Arc<Dispatcher>,
    evidence: Arc<SuppressionEvidenceState>,
    released: bool,
}

impl SuppressionLease {
    /// Explicit release. Useful if the caller wants to drop the lease
    /// before its scope ends without taking the `Drop` path.
    pub fn release(mut self) {
        self.dispatcher.remove(self.handle);
        self.released = true;
    }

    pub fn release_with_evidence(mut self) -> SuppressionOutcome {
        self.dispatcher.remove(self.handle);
        self.released = true;
        suppression_outcome(&self.evidence)
    }

    pub fn close_with_evidence(mut self, timeout: Duration) -> SuppressionCloseOutcome {
        let outcome = FocusStealPreventer::close_handle(
            Arc::clone(&self.dispatcher),
            self.handle,
            Arc::clone(&self.evidence),
            timeout,
        );
        self.released = true;
        outcome
    }

    pub fn outcome(&self) -> SuppressionOutcome {
        suppression_outcome(&self.evidence)
    }
}

fn suppression_outcome(state: &SuppressionEvidenceState) -> SuppressionOutcome {
    SuppressionOutcome {
        activations: state.activations.load(Ordering::Acquire),
        restore_attempts: state.restore_attempts.load(Ordering::Acquire),
        restore_failures: state.restore_failures.load(Ordering::Acquire),
    }
}

impl Drop for SuppressionLease {
    fn drop(&mut self) {
        if !self.released {
            self.dispatcher.remove(self.handle);
        }
    }
}

// ── Dispatcher ──────────────────────────────────────────────────────────────

/// Holds the suppression entries plus the janitor lifecycle.
///
/// `entries` is a `HashMap<Uuid, Entry>` so add/remove are O(1) by handle.
/// Lookups by `(target_pid, restore_to)` during a notification are O(N) —
/// N is at most a handful of in-flight launches at a time so a linear
/// scan is fine.
pub(crate) struct Dispatcher {
    entries: Mutex<HashMap<Uuid, Entry>>,
    /// `true` while the janitor task should keep running. The janitor
    /// loop watches for transitions to detect when to start/stop.
    janitor_active: tokio::sync::watch::Sender<bool>,
    janitor_started: Mutex<bool>,
}

impl Dispatcher {
    fn new() -> Self {
        let (tx, _rx) = tokio::sync::watch::channel(false);
        Self {
            entries: Mutex::new(HashMap::new()),
            janitor_active: tx,
            janitor_started: Mutex::new(false),
        }
    }

    /// Add an entry, return its handle. Always attempts to start the
    /// janitor task — `kick_janitor()` is idempotent and is the only
    /// reliable path to recover if the very first add happened before
    /// a tokio runtime was ready. Gating the kick on "map was empty"
    /// (as we used to) lost the janitor permanently in that case:
    /// subsequent adds would skip the kick and the janitor never
    /// started, leaving deadline-reaping entirely up to the
    /// `snapshot_matches` reap fallback (only fires on an activation).
    #[cfg(test)]
    fn add(
        self: &Arc<Self>,
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
    ) -> SuppressionHandle {
        self.add_until(
            target_pid,
            restore_to,
            origin,
            Instant::now() + ENTRY_DEADLINE,
        )
    }

    fn add_until(
        self: &Arc<Self>,
        target_pid: Option<i32>,
        restore_to: i32,
        origin: &'static str,
        deadline: Instant,
    ) -> SuppressionHandle {
        let id = Uuid::new_v4();
        let entry = Entry {
            target_pid,
            restore_to,
            deadline,
            origin,
            evidence: Arc::new(SuppressionEvidenceState::default()),
        };
        {
            let mut guard = self.entries.lock().unwrap();
            guard.insert(id, entry);
        }
        // Always kick — idempotent if the task is already running.
        self.kick_janitor();
        // Signal the janitor that there's work to do (it will start a
        // fresh tokio interval on the next tick).
        let _ = self.janitor_active.send(true);
        SuppressionHandle(id)
    }

    fn evidence(&self, handle: SuppressionHandle) -> Arc<SuppressionEvidenceState> {
        Arc::clone(
            &self
                .entries
                .lock()
                .expect("focus suppression dispatcher poisoned")
                .get(&handle.0)
                .expect("new suppression entry must retain evidence")
                .evidence,
        )
    }

    /// Remove an entry. When the map drains to empty, signals the janitor
    /// to stop until the next add.
    fn remove(&self, handle: SuppressionHandle) {
        let now_empty = {
            let mut guard = self.entries.lock().unwrap();
            guard.remove(&handle.0);
            guard.is_empty()
        };
        if now_empty {
            let _ = self.janitor_active.send(false);
        }
    }

    fn retarget(&self, handle: SuppressionHandle, target_pid: Option<i32>) {
        if let Some(entry) = self
            .entries
            .lock()
            .expect("focus suppression dispatcher poisoned")
            .get_mut(&handle.0)
        {
            entry.target_pid = target_pid;
        }
    }

    /// Snapshot the entries (cloned to a small Vec) — used by tests
    /// and the activation handler to evaluate matches without holding
    /// the lock across the restore call.
    fn snapshot_matches(&self, activated_pid: i32) -> Vec<(i32, Arc<SuppressionEvidenceState>)> {
        let mut guard = self.entries.lock().unwrap();
        // Reap expired entries first — keeps the dispatcher honest even
        // if the janitor hasn't ticked yet.
        let now = Instant::now();
        guard.retain(|_, e| e.deadline > now);
        guard
            .values()
            .filter(|e| {
                match e.target_pid {
                    Some(p) => p == activated_pid,
                    // Wildcard: match any activation except the restore_to
                    // pid (don't fight ourselves when we re-activate the
                    // prior frontmost).
                    None => activated_pid != e.restore_to,
                }
            })
            .map(|e| (e.restore_to, Arc::clone(&e.evidence)))
            .collect()
    }

    /// Number of entries (for tests).
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Reap entries whose deadline is past. Returns the number reaped.
    fn reap_expired(&self) -> usize {
        let mut guard = self.entries.lock().unwrap();
        let now = Instant::now();
        let before = guard.len();
        guard.retain(|_, e| e.deadline > now);
        let after = guard.len();
        let reaped = before - after;
        if after == 0 && reaped > 0 {
            let _ = self.janitor_active.send(false);
        }
        reaped
    }

    /// Start the janitor task on the current tokio runtime (idempotent).
    ///
    /// Safe to call from `add()` on every entry — returns immediately
    /// when the task is already up. If the spawn cannot proceed (no
    /// tokio runtime available — e.g. the first `add()` raced the
    /// binary's runtime init), the `started` flag is intentionally left
    /// `false` so the next add from a tokio-aware caller retries.
    /// Without that retry path the janitor could go permanently
    /// un-spawned and deadline-reaping would degrade to the
    /// `snapshot_matches` reap fallback (only runs on an activation).
    fn kick_janitor(self: &Arc<Self>) {
        let mut started = self.janitor_started.lock().unwrap();
        if *started {
            return;
        }
        // If there's no tokio runtime available (e.g. the binary is in
        // the middle of an init path that runs before Tokio is up), skip
        // — the next add from a tokio-aware caller will retry. We do NOT
        // set `*started = true` in this branch so the retry actually
        // takes the spawn path.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        // Mark started ONLY after a successful `tokio::spawn`. The
        // spawn itself is infallible under the current API but we still
        // sequence the flag update after the spawn so any future
        // panic-from-spawn path would leave `started = false` and the
        // next add would retry.
        let weak = Arc::downgrade(self);
        let mut rx = self.janitor_active.subscribe();
        tokio::spawn(async move {
            loop {
                // Block until the dispatcher is non-empty.
                if !*rx.borrow_and_update() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                    continue;
                }
                let mut tick = tokio::time::interval(JANITOR_TICK);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tick.tick().await; // immediate first tick
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            let Some(d) = weak.upgrade() else { return };
                            let _ = d.reap_expired();
                            if d.len() == 0 {
                                // Map drained — break to outer select, wait
                                // for next add.
                                break;
                            }
                        }
                        ch = rx.changed() => {
                            if ch.is_err() { return; }
                            // Active flag may have flipped; loop top will
                            // re-check via `borrow_and_update`.
                            break;
                        }
                    }
                }
            }
        });
        *started = true;
    }
}

// ── Observer registration ────────────────────────────────────────────────────

/// Register the NSWorkspace.didActivateApplicationNotification observer.
///
/// The returned token and the queue are intentionally `mem::forget`-leaked
/// — the singleton is process-lifetime, so we never tear the observer
/// down. Forgetting avoids having to thread `!Send` `Retained<...>`
/// handles through `FocusStealPreventer` (which lives in `Arc<...>` /
/// `OnceLock<...>` and therefore needs to be `Send + Sync`).
fn install_observer(dispatcher: &Arc<Dispatcher>) -> Retained<NSOperationQueue> {
    use block2::RcBlock;
    use objc2_foundation::NSNotification;
    use std::ptr::NonNull;

    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    let center = unsafe { ws.notificationCenter() };

    // Fresh background NSOperationQueue. Critical: with `nil` queue,
    // AppKit delivers synchronously on the posting thread (typically main);
    // with `mainQueue`, the block requires a running main run loop. A
    // fresh queue runs the block on a private background thread no matter
    // what run loop the binary has up.
    let queue = unsafe { NSOperationQueue::new() };
    // setMaxConcurrentOperationCount: 1 means activations are processed
    // serially — they're cheap so contention isn't a worry, but serial
    // processing keeps the restore order deterministic if two come in
    // back to back.
    unsafe { queue.setMaxConcurrentOperationCount(1) };

    let dispatcher_clone = Arc::clone(dispatcher);
    let block = RcBlock::new(move |note_ptr: NonNull<NSNotification>| {
        // SAFETY: AppKit gives us a borrowed NSNotification for the
        // duration of the block. We don't escape the reference.
        let note = unsafe { note_ptr.as_ref() };
        handle_activation(&dispatcher_clone, note);
    });

    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidActivateApplicationNotification),
            None,
            Some(&queue),
            &block,
        )
    };

    // The observer token is process-lifetime. The singleton retains the queue
    // so bounded callers can enqueue an exact barrier on this same callback
    // stream before reading suppression evidence.
    std::mem::forget(token);
    queue
}

/// Match a single activation notification against the dispatcher and,
/// for each matching entry, re-activate the entry's `restore_to` pid.
///
/// Runs on the observer queue's background thread — safe to call
/// blocking system APIs.
fn handle_activation(dispatcher: &Arc<Dispatcher>, note: &objc2_foundation::NSNotification) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    let activated_pid: i32 = unsafe {
        let info = match note.userInfo() {
            Some(i) => i,
            None => return,
        };
        // userInfo[NSWorkspaceApplicationKey] -> NSRunningApplication*.
        // We go through a raw msg_send to avoid Retained generic
        // bookkeeping for the cross-cast.
        let app_ptr: *mut AnyObject = msg_send![&*info, objectForKey: NSWorkspaceApplicationKey];
        if app_ptr.is_null() {
            return;
        }
        let pid: libc::pid_t = msg_send![app_ptr, processIdentifier];
        pid as i32
    };

    let restore_pids = dispatcher.snapshot_matches(activated_pid);
    for (pid, evidence) in restore_pids {
        evidence.activations.fetch_add(1, Ordering::AcqRel);
        evidence.restore_attempts.fetch_add(1, Ordering::AcqRel);
        if !restore_focus(pid) {
            evidence.restore_failures.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Re-activate `pid` if it's still running. Safe to call from any
/// thread — Apple documents `activateWithOptions:` as thread-safe.
fn restore_focus(pid: i32) -> bool {
    unsafe {
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
            return app.activateWithOptions(NSApplicationActivationOptions(0));
        }
    }
    false
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// These tests exercise the pure-Rust dispatcher half — no Cocoa
// observers, no real notifications. They run under `cargo test
// -p platform-macos focus_steal::`.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Dispatcher::add returns a handle, the entry is reachable by
    /// match, and remove() drops it.
    #[test]
    fn dispatcher_add_match_remove() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(42), 7, "test.add");
        assert_eq!(d.len(), 1);
        let matches = d.snapshot_matches(42);
        assert_eq!(
            matches.into_iter().map(|(pid, _)| pid).collect::<Vec<_>>(),
            vec![7]
        );
        // Non-matching pid: no restore candidates.
        assert!(d.snapshot_matches(99).is_empty());
        d.remove(h);
        assert_eq!(d.len(), 0);
    }

    /// Wildcard entries (`target_pid = None`) match every activation
    /// except the entry's own restore_to pid.
    #[test]
    fn wildcard_matches_all_but_restore_to() {
        let d = Arc::new(Dispatcher::new());
        let _h = d.add(None, 7, "test.wild");
        // pid 99 != restore_to 7 → should match.
        assert_eq!(
            d.snapshot_matches(99)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect::<Vec<_>>(),
            vec![7]
        );
        // pid 7 == restore_to → must NOT match (don't fight ourselves).
        assert!(d.snapshot_matches(7).is_empty());
    }

    /// Lease Drop is the standard remove path.
    #[test]
    fn lease_drop_removes_entry() {
        // Use a private dispatcher to avoid singleton coupling.
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(1), 2, "test.lease");
        let lease = SuppressionLease {
            handle: h,
            dispatcher: Arc::clone(&d),
            evidence: d.evidence(h),
            released: false,
        };
        assert_eq!(d.len(), 1);
        drop(lease);
        assert_eq!(d.len(), 0);
    }

    /// Explicit release() short-circuits the Drop path.
    #[test]
    fn lease_release_removes_entry() {
        let d = Arc::new(Dispatcher::new());
        let h = d.add(Some(1), 2, "test.lease");
        let lease = SuppressionLease {
            handle: h,
            dispatcher: Arc::clone(&d),
            evidence: d.evidence(h),
            released: false,
        };
        lease.release();
        assert_eq!(d.len(), 0);
    }

    /// Force a leaked entry whose deadline is already past, then call
    /// reap_expired and snapshot_matches — both must purge it.
    #[test]
    fn deadline_reaps_leaked_entry() {
        let d = Arc::new(Dispatcher::new());
        // Insert a handle manually with a past deadline.
        let id = Uuid::new_v4();
        {
            let mut guard = d.entries.lock().unwrap();
            guard.insert(
                id,
                Entry {
                    target_pid: Some(42),
                    restore_to: 7,
                    deadline: Instant::now() - Duration::from_secs(1),
                    origin: "test.leak",
                    evidence: Arc::new(SuppressionEvidenceState::default()),
                },
            );
        }
        assert_eq!(d.len(), 1);
        // snapshot_matches reaps expired entries before matching.
        let matches = d.snapshot_matches(42);
        assert!(matches.is_empty(), "expired entry should not fire");
        assert_eq!(d.len(), 0, "snapshot_matches should purge expired");
    }

    #[test]
    fn caller_deadline_covers_long_native_operation_span() {
        let d = Arc::new(Dispatcher::new());
        let requested = Instant::now() + Duration::from_secs(12);
        let handle = d.add_until(Some(42), 7, "test.long_operation", requested);
        let recorded = d.entries.lock().unwrap().get(&handle.0).unwrap().deadline;
        assert_eq!(recorded, requested);
        d.remove(handle);
    }

    #[test]
    fn deadline_owned_containment_survives_owner_cancellation_until_deadline() {
        let d = Arc::new(Dispatcher::new());
        let deadline = Instant::now() + Duration::from_secs(12);
        let handle = d.add_until(None, 7, "test.detached_launch", deadline);
        let containment = DeadlineSuppression {
            handle,
            dispatcher: Arc::clone(&d),
            evidence: d.evidence(handle),
            closed: Arc::new(AtomicBool::new(false)),
        };
        drop(containment);
        assert_eq!(d.len(), 1, "Drop must not disarm a late LS callback");
        assert_eq!(d.entries.lock().unwrap()[&handle.0].deadline, deadline);
        d.remove(handle);
    }

    #[test]
    fn wildcard_narrows_in_place_without_a_remove_add_gap() {
        let d = Arc::new(Dispatcher::new());
        let handle = d.add(None, 7, "test.narrow");
        d.retarget(handle, Some(42));
        assert!(d.snapshot_matches(99).is_empty());
        assert_eq!(d.snapshot_matches(42).len(), 1);
        assert_eq!(d.len(), 1);
        d.remove(handle);
    }

    #[test]
    fn serial_close_removes_after_preceding_containment_callbacks() {
        let d = Arc::new(Dispatcher::new());
        let handle = d.add(Some(42), 7, "test.serial_close");
        let lease = SuppressionLease {
            handle,
            dispatcher: Arc::clone(&d),
            evidence: d.evidence(handle),
            released: false,
        };
        let close = lease.close_with_evidence(Duration::from_secs(1));
        assert!(close.callback_queue_drained);
        assert_eq!(d.len(), 0);
    }

    /// Janitor lifecycle: starts on first add, stops when empty,
    /// restarts on next add. Spin up a tokio runtime to host the task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn janitor_starts_stops_restarts() {
        let d = Arc::new(Dispatcher::new());
        // First add → janitor starts.
        let h1 = d.add(Some(1), 2, "test.j1");
        d.kick_janitor();
        // Give the janitor task time to spin up.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The dispatcher should still hold the entry.
        assert_eq!(d.len(), 1);
        // Now remove → janitor goes idle.
        d.remove(h1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(d.len(), 0);
        // Add again — same kick, same lifecycle. (start_janitor is
        // idempotent — already-started task picks up new adds via watch.)
        let _h2 = d.add(Some(3), 4, "test.j2");
        d.kick_janitor();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(d.len(), 1);
    }

    /// Verifies the CodeRabbit #2 fix: `add()` always calls
    /// `kick_janitor()`, regardless of whether the map was empty.
    ///
    /// Scenario: first `add()` happens outside a tokio runtime —
    /// `kick_janitor()` short-circuits via `Handle::try_current()` and
    /// leaves `started = false`. A second `add()` from a tokio-aware
    /// caller (the more common case in practice) must retry the spawn.
    /// The old code skipped the kick because the map was non-empty,
    /// stranding the janitor forever.
    #[test]
    fn add_always_kicks_janitor_after_initial_runtime_miss() {
        let d = Arc::new(Dispatcher::new());
        // Outside any tokio runtime — kick_janitor's `try_current` guard
        // returns Err, the function returns without setting started.
        let h1 = d.add(Some(1), 2, "test.no_runtime");
        assert_eq!(d.len(), 1);
        assert!(
            !*d.janitor_started.lock().unwrap(),
            "kick without a runtime must leave started=false so the next \
             add retries"
        );

        // Now spin up a tokio runtime and add a second entry. The fix
        // is that this *second* add still calls kick_janitor (the old
        // code skipped because the map was already non-empty). Verify
        // by asserting started flips to true.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime");
        let d_in = Arc::clone(&d);
        rt.block_on(async move {
            let _h2 = d_in.add(Some(3), 4, "test.with_runtime");
            assert_eq!(d_in.len(), 2);
            assert!(
                *d_in.janitor_started.lock().unwrap(),
                "second add from inside the runtime must retry the spawn \
                 (regression guard for CodeRabbit #2)"
            );
        });

        // Clean up so the dispatcher doesn't outlive the runtime with
        // a still-armed entry — not strictly needed (Dispatcher is
        // `Send + Sync` and the spawned task holds only a Weak ref),
        // but keeps the test self-contained.
        d.remove(h1);
    }

    /// Snapshot ordering doesn't matter, but the restore pid set
    /// must contain every match. Multiple concurrent suppressions
    /// targeting the same pid should both fire.
    #[test]
    fn multiple_entries_match_independently() {
        let d = Arc::new(Dispatcher::new());
        let _a = d.add(Some(42), 1, "test.m1");
        let _b = d.add(Some(42), 2, "test.m2");
        let matches = d.snapshot_matches(42);
        assert_eq!(matches.len(), 2);
        // Set equality — order is HashMap-dependent.
        let restore_pids: Vec<_> = matches.into_iter().map(|(pid, _)| pid).collect();
        assert!(restore_pids.contains(&1));
        assert!(restore_pids.contains(&2));
    }

    #[test]
    fn retained_observer_queue_exposes_a_bounded_teardown_barrier() {
        assert!(FocusStealPreventer::barrier(Duration::from_secs(1)));
    }
}
