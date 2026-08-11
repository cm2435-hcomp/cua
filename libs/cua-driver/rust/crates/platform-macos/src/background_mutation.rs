//! Exact, non-blocking compatibility guards for native UI mutations.
//!
//! A target-scoped AX mutation may overlap another target in the same process.
//! Keyboard, pointer, focus, and menu recipes are process-scoped and therefore
//! conflict with every mutation in that process. Conflicts are refused before
//! dispatch rather than queued: a delayed action would execute against state
//! the caller did not observe.

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

tokio::task_local! {
    static HELD_PID: i32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DispatchScope {
    Target {
        pid: i32,
        window_id: u32,
    },
    Process(i32),
    /// Reserved for recipes that genuinely mutate global desktop state.
    #[allow(dead_code)]
    Desktop,
}

impl DispatchScope {
    fn conflicts_with(self, other: Self) -> bool {
        match (self, other) {
            (Self::Desktop, _) | (_, Self::Desktop) => true,
            (Self::Process(left), Self::Process(right)) => left == right,
            (Self::Process(left), Self::Target { pid: right, .. })
            | (Self::Target { pid: right, .. }, Self::Process(left)) => left == right,
            (
                Self::Target {
                    pid: left_pid,
                    window_id: left_window,
                },
                Self::Target {
                    pid: right_pid,
                    window_id: right_window,
                },
            ) => left_pid == right_pid && left_window == right_window,
        }
    }

    pub(crate) fn kind(self) -> &'static str {
        match self {
            Self::Target { .. } => "target",
            Self::Process(_) => "process",
            Self::Desktop => "desktop",
        }
    }

    pub(crate) fn identity(self) -> String {
        match self {
            Self::Target { pid, window_id } => format!("{pid}:{window_id}"),
            Self::Process(pid) => pid.to_string(),
            Self::Desktop => "desktop".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DispatchConflict {
    pub(crate) requested: DispatchScope,
    pub(crate) held: DispatchScope,
}

#[derive(Clone, Default)]
struct DispatchRegistry {
    active: Arc<Mutex<HashSet<DispatchScope>>>,
}

impl DispatchRegistry {
    fn lock_active(&self) -> MutexGuard<'_, HashSet<DispatchScope>> {
        self.active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn try_acquire(&self, scope: DispatchScope) -> Result<DispatchPermit, DispatchConflict> {
        let mut active = self.lock_active();
        if let Some(held) = active
            .iter()
            .copied()
            .find(|held| held.conflicts_with(scope))
        {
            return Err(DispatchConflict {
                requested: scope,
                held,
            });
        }
        let inserted = active.insert(scope);
        debug_assert!(inserted, "an exact duplicate scope must conflict");
        Ok(DispatchPermit {
            active: Arc::clone(&self.active),
            scope: Some(scope),
        })
    }
}

/// Attempt to own one native compatibility domain without waiting or queueing.
pub(crate) fn try_acquire(scope: DispatchScope) -> Result<DispatchPermit, DispatchConflict> {
    static REGISTRY: OnceLock<DispatchRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(DispatchRegistry::default)
        .try_acquire(scope)
}

/// RAII ownership of one registered native dispatch scope.
#[derive(Debug)]
pub(crate) struct DispatchPermit {
    active: Arc<Mutex<HashSet<DispatchScope>>>,
    scope: Option<DispatchScope>,
}

impl Drop for DispatchPermit {
    fn drop(&mut self) {
        let Some(scope) = self.scope.take() else {
            return;
        };
        let removed = self
            .active
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&scope);
        debug_assert!(removed, "dispatch permit must release its exact scope");
    }
}

/// Run one nested tool call with non-forgeable proof that its caller already
/// owns this process's mutation lease.
pub(crate) async fn with_held_lease<T>(pid: i32, future: impl Future<Output = T>) -> T {
    HELD_PID.scope(pid, future).await
}

pub(crate) fn held_by_current_task(pid: i32) -> bool {
    HELD_PID
        .try_with(|held_pid| *held_pid == pid)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_process_conflicts_immediately_and_drop_releases_it() {
        let registry = DispatchRegistry::default();
        let first = registry
            .try_acquire(DispatchScope::Process(-91_001))
            .unwrap();
        let conflict = registry
            .try_acquire(DispatchScope::Process(-91_001))
            .unwrap_err();
        assert_eq!(conflict.requested, DispatchScope::Process(-91_001));
        assert_eq!(conflict.held, DispatchScope::Process(-91_001));

        drop(first);
        registry
            .try_acquire(DispatchScope::Process(-91_001))
            .unwrap();
    }

    #[test]
    fn different_targets_in_one_process_may_overlap() {
        let registry = DispatchRegistry::default();
        let _first = registry
            .try_acquire(DispatchScope::Target {
                pid: -91_002,
                window_id: 7,
            })
            .unwrap();
        let _second = registry
            .try_acquire(DispatchScope::Target {
                pid: -91_002,
                window_id: 8,
            })
            .unwrap();
    }

    #[test]
    fn process_scope_conflicts_with_target_scope_for_that_process_only() {
        let registry = DispatchRegistry::default();
        let _target = registry
            .try_acquire(DispatchScope::Target {
                pid: -91_003,
                window_id: 7,
            })
            .unwrap();
        assert!(registry
            .try_acquire(DispatchScope::Process(-91_003))
            .is_err());
        registry
            .try_acquire(DispatchScope::Process(-91_004))
            .unwrap();
    }

    #[test]
    fn desktop_scope_conflicts_with_every_native_dispatch() {
        let registry = DispatchRegistry::default();
        let _desktop = registry.try_acquire(DispatchScope::Desktop).unwrap();
        assert!(registry
            .try_acquire(DispatchScope::Target {
                pid: -91_005,
                window_id: 7,
            })
            .is_err());
        assert!(registry
            .try_acquire(DispatchScope::Process(-91_006))
            .is_err());
    }

    #[tokio::test]
    async fn nested_lease_proof_is_task_local_and_pid_bound() {
        assert!(!held_by_current_task(-91_007));
        with_held_lease(-91_007, async {
            assert!(held_by_current_task(-91_007));
            assert!(!held_by_current_task(-91_008));
        })
        .await;
        assert!(!held_by_current_task(-91_007));
    }
}
