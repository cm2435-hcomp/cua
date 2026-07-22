//! Long-lived per-client/window target ownership and process mutation locks.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

use tokio::sync::Mutex as AsyncMutex;

use super::{
    contracts::{AppId, ClientId, WindowGeneration, WindowId},
    errors::{ErrorCode, NativeError},
    interaction::TargetCursorHandle,
    menu::MenuControllerState,
    observation::{
        AxRevisionState, InvalidationReason, NativeProcessHandle, ObservationStore, ResolvedWindow,
    },
    platform::{InvalidationSubscription, PlatformDriver, TargetInvalidation},
    settlement::SettlementState,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetKey {
    pub client_id: ClientId,
    pub app_id: AppId,
    pub window_id: WindowId,
    pub window_generation: WindowGeneration,
}

impl TargetKey {
    pub fn from_window(client_id: ClientId, window: &ResolvedWindow) -> Self {
        Self {
            client_id,
            app_id: window.public.app.id.clone(),
            window_id: window.public.id.clone(),
            window_generation: window.generation,
        }
    }
}

pub type SharedProcessMutationLock = Arc<AsyncMutex<()>>;

#[derive(Debug, Clone)]
pub(crate) struct TargetValidityHandle(Arc<AtomicBool>);

impl TargetValidityHandle {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    pub(crate) fn invalidate(&self) {
        self.0.store(false, Ordering::Release);
    }

    fn is_valid(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Default)]
pub struct ProcessMutationLockRegistry {
    locks: Mutex<HashMap<NativeProcessHandle, Weak<AsyncMutex<()>>>>,
}

impl ProcessMutationLockRegistry {
    pub fn lock_for(&self, process: &NativeProcessHandle) -> SharedProcessMutationLock {
        let mut locks = self.locks.lock().expect("process lock registry poisoned");
        if let Some(lock) = locks.get(process).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        locks.insert(process.clone(), Arc::downgrade(&lock));
        lock
    }
}

pub struct TargetControllerState<P: PlatformDriver> {
    pub window: ResolvedWindow,
    pub platform: P::TargetState,
    pub focus: P::TargetFocusCoordinator,
    pub ax_revisions: AxRevisionState,
    pub observations: ObservationStore,
    pub logical_cursor: TargetCursorHandle,
    pub menu: MenuControllerState,
    pub settlement: SettlementState,
}

pub struct TargetController<P: PlatformDriver> {
    pub key: TargetKey,
    pub process: NativeProcessHandle,
    pub mutation_lock: SharedProcessMutationLock,
    pub state: AsyncMutex<TargetControllerState<P>>,
    last_used: Mutex<Instant>,
    validity: TargetValidityHandle,
    torn_down: AtomicBool,
}

impl<P: PlatformDriver> TargetController<P> {
    fn new(
        key: TargetKey,
        window: ResolvedWindow,
        platform: P::TargetState,
        focus: P::TargetFocusCoordinator,
        mutation_lock: SharedProcessMutationLock,
    ) -> Self {
        Self {
            key,
            process: window.process.clone(),
            mutation_lock,
            state: AsyncMutex::new(TargetControllerState {
                window,
                platform,
                focus,
                ax_revisions: AxRevisionState::default(),
                observations: ObservationStore::default(),
                logical_cursor: TargetCursorHandle::default(),
                menu: MenuControllerState::default(),
                settlement: SettlementState::default(),
            }),
            last_used: Mutex::new(Instant::now()),
            validity: TargetValidityHandle::new(),
            torn_down: AtomicBool::new(false),
        }
    }

    pub fn touch(&self) {
        *self
            .last_used
            .lock()
            .expect("target last-used lock poisoned") = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .expect("target last-used lock poisoned")
            .elapsed()
    }

    pub fn ensure_valid(&self) -> Result<(), NativeError> {
        if self.validity.is_valid() {
            Ok(())
        } else {
            Err(NativeError::stale(
                ErrorCode::WindowIdentityChanged,
                "target controller was invalidated by native lifecycle state",
            ))
        }
    }

    pub fn invalidate(&self) {
        self.validity.invalidate();
    }

    pub(crate) fn validity_handle(&self) -> TargetValidityHandle {
        self.validity.clone()
    }

    pub async fn teardown(&self, platform: &P) -> Result<(), NativeError> {
        self.invalidate();
        if self.torn_down.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        state
            .observations
            .invalidate_all(InvalidationReason::WindowChanged);
        state.ax_revisions.reset();
        state.menu.close();
        let TargetControllerState {
            platform: target_state,
            focus,
            ..
        } = &mut *state;
        platform.destroy_target_state(target_state, focus).await
    }
}

pub struct TargetControllerRegistry<P: PlatformDriver> {
    targets: AsyncMutex<HashMap<TargetKey, Arc<TargetController<P>>>>,
    process_locks: Arc<ProcessMutationLockRegistry>,
    idle_ttl: Duration,
}

impl<P: PlatformDriver> TargetControllerRegistry<P> {
    pub fn new(process_locks: Arc<ProcessMutationLockRegistry>, idle_ttl: Duration) -> Self {
        Self {
            targets: AsyncMutex::new(HashMap::new()),
            process_locks,
            idle_ttl,
        }
    }

    pub async fn get_or_create(
        &self,
        platform: &P,
        key: TargetKey,
        window: ResolvedWindow,
    ) -> Result<Arc<TargetController<P>>, NativeError> {
        let existing = { self.targets.lock().await.get(&key).cloned() };
        if let Some(target) = existing {
            if target.ensure_valid().is_ok() {
                target.touch();
                return Ok(target);
            }
            let invalid = self.remove_matching(|candidate, _| candidate == &key).await;
            teardown_all(platform, invalid).await?;
        }

        let superseded = self
            .remove_matching(|candidate, _| {
                candidate.client_id == key.client_id
                    && candidate.app_id == key.app_id
                    && candidate.window_id == key.window_id
                    && candidate.window_generation != key.window_generation
            })
            .await;
        teardown_all(platform, superseded).await?;

        // Native creation/teardown never runs while the registry mutex is
        // held. A racing creator is resolved by discarding this extra state.
        let (mut target_state, mut focus) = platform.create_target_state(&window).await?;
        let mutation_lock = self.process_locks.lock_for(&window.process);
        let mut targets = self.targets.lock().await;
        if let Some(target) = targets.get(&key).cloned() {
            drop(targets);
            platform
                .destroy_target_state(&mut target_state, &mut focus)
                .await?;
            target.ensure_valid()?;
            target.touch();
            return Ok(target);
        }
        let target = Arc::new(TargetController::new(
            key.clone(),
            window,
            target_state,
            focus,
            mutation_lock,
        ));
        targets.insert(key, Arc::clone(&target));
        Ok(target)
    }

    pub async fn get(&self, key: &TargetKey) -> Result<Arc<TargetController<P>>, NativeError> {
        let targets = self.targets.lock().await;
        let target = targets.get(key).cloned().ok_or_else(|| {
            NativeError::stale(
                ErrorCode::ObservationStale,
                "target controller does not exist; observe the window before mutating it",
            )
        })?;
        target.ensure_valid()?;
        target.touch();
        Ok(target)
    }

    /// Removes and tears down one exact target after a native lease failure.
    /// The target is invalidated before callers release their state lock, so
    /// no racing request can reuse it between failure detection and removal.
    pub async fn remove_invalid_target(
        &self,
        platform: &P,
        key: &TargetKey,
    ) -> Result<bool, NativeError> {
        let target = self.targets.lock().await.remove(key);
        let Some(target) = target else {
            return Ok(false);
        };
        target.teardown(platform).await?;
        Ok(true)
    }

    pub async fn close_connection(
        &self,
        platform: &P,
        client_id: &ClientId,
    ) -> Result<usize, NativeError> {
        let targets = self
            .remove_matching(|key, _| &key.client_id == client_id)
            .await;
        teardown_all(platform, targets).await
    }

    pub async fn expire_idle(&self, platform: &P) -> Result<usize, NativeError> {
        let idle_ttl = self.idle_ttl;
        let targets = self
            .remove_matching(|_, target| target.idle_for() >= idle_ttl)
            .await;
        teardown_all(platform, targets).await
    }

    pub async fn handle_invalidation(
        &self,
        platform: &P,
        invalidation: TargetInvalidation,
    ) -> Result<usize, NativeError> {
        if let TargetInvalidation::ObservationChanged {
            app_id,
            window_id,
            generation,
            reason,
        } = &invalidation
        {
            let targets: Vec<_> = {
                let registry = self.targets.lock().await;
                registry
                    .iter()
                    .filter(|(key, _)| {
                        &key.app_id == app_id
                            && &key.window_id == window_id
                            && &key.window_generation == generation
                    })
                    .map(|(_, target)| Arc::clone(target))
                    .collect()
            };
            for target in &targets {
                let mut state = target.state.lock().await;
                state.observations.invalidate_all(reason.clone());
                if matches!(
                    reason,
                    &InvalidationReason::AccessibilityInvalidated
                        | &InvalidationReason::DiffBaseInvalidated
                        | &InvalidationReason::DisplayChanged
                ) {
                    state.ax_revisions.invalidate_base();
                }
                if matches!(
                    reason,
                    &InvalidationReason::TransientDismissed | &InvalidationReason::MenuChanged
                ) {
                    state.menu.close();
                }
            }
            return Ok(targets.len());
        }
        let targets = match invalidation {
            TargetInvalidation::NativeStateResyncRequired => {
                self.remove_matching(|_, _| true).await
            }
            TargetInvalidation::ProcessExited { process } => {
                self.remove_matching(|_, target| target.process == process)
                    .await
            }
            TargetInvalidation::WindowGenerationChanged {
                app_id,
                window_id,
                previous,
                ..
            } => {
                self.remove_matching(|key, _| {
                    key.app_id == app_id
                        && key.window_id == window_id
                        && key.window_generation == previous
                })
                .await
            }
            TargetInvalidation::ObservationChanged { .. } => unreachable!(
                "observation invalidations return before destructive invalidation matching"
            ),
        };
        teardown_all(platform, targets).await
    }

    pub async fn invalidation_loop(
        self: Arc<Self>,
        platform: Arc<P>,
        mut subscription: P::Invalidations,
    ) {
        while let Some(invalidation) = subscription.next().await {
            if let Err(error) = self.handle_invalidation(&platform, invalidation).await {
                tracing::error!(error = %error, "failed to tear down invalidated v2 target");
            }
        }
    }

    pub async fn len(&self) -> usize {
        self.targets.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.targets.lock().await.is_empty()
    }

    async fn remove_matching(
        &self,
        predicate: impl Fn(&TargetKey, &TargetController<P>) -> bool,
    ) -> Vec<Arc<TargetController<P>>> {
        let mut targets = self.targets.lock().await;
        let keys: Vec<_> = targets
            .iter()
            .filter_map(|(key, target)| predicate(key, target).then_some(key.clone()))
            .collect();
        keys.into_iter()
            .filter_map(|key| targets.remove(&key))
            .collect()
    }
}

async fn teardown_all<P: PlatformDriver>(
    platform: &P,
    targets: Vec<Arc<TargetController<P>>>,
) -> Result<usize, NativeError> {
    let count = targets.len();
    let mut failures = Vec::new();
    for target in targets {
        if let Err(error) = target.teardown(platform).await {
            failures.push(error);
        }
    }
    if let Some(error) = NativeError::primary(failures) {
        return Err(error);
    }
    Ok(count)
}
