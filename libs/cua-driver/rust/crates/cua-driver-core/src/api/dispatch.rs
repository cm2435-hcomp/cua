//! Exact, non-blocking native dispatch compatibility guards.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use super::{
    errors::{ErrorCode, ErrorPhase, NativeError},
    observation::NativeProcessHandle,
    target::TargetKey,
};

/// The identity breadth a platform recipe needs while it may affect native UI.
///
/// Providers choose only the kind. Core materializes the exact target and
/// process identities from the resolved window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchScopeKind {
    Target,
    Process,
    Desktop,
}

/// One exact native compatibility domain.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DispatchScope {
    Target {
        target: TargetKey,
        process: NativeProcessHandle,
    },
    Process(NativeProcessHandle),
    Desktop,
}

impl DispatchScope {
    pub fn materialize(
        kind: DispatchScopeKind,
        target: TargetKey,
        process: NativeProcessHandle,
    ) -> Self {
        match kind {
            DispatchScopeKind::Target => Self::Target { target, process },
            DispatchScopeKind::Process => Self::Process(process),
            DispatchScopeKind::Desktop => Self::Desktop,
        }
    }

    pub fn kind(&self) -> DispatchScopeKind {
        match self {
            Self::Target { .. } => DispatchScopeKind::Target,
            Self::Process(_) => DispatchScopeKind::Process,
            Self::Desktop => DispatchScopeKind::Desktop,
        }
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Desktop, _) | (_, Self::Desktop) => true,
            (Self::Process(left), Self::Process(right)) => left == right,
            (Self::Process(left), Self::Target { process: right, .. })
            | (Self::Target { process: right, .. }, Self::Process(left)) => left == right,
            (Self::Target { target: left, .. }, Self::Target { target: right, .. }) => {
                left == right
            }
        }
    }

    fn identity_detail(&self) -> String {
        match self {
            Self::Target { target, .. } => format!(
                "{}:{}:{}",
                target.app_id, target.window_id, target.window_generation.0
            ),
            Self::Process(process) => process.as_str().to_owned(),
            Self::Desktop => "desktop".to_owned(),
        }
    }
}

#[derive(Debug, Default)]
struct DispatchGuardState {
    active: HashSet<DispatchScope>,
}

#[derive(Debug, Default, Clone)]
pub struct DispatchGuardRegistry {
    state: Arc<Mutex<DispatchGuardState>>,
}

impl DispatchGuardRegistry {
    /// Attempts to register `scope` without waiting or queueing.
    pub fn try_acquire(&self, scope: DispatchScope) -> Result<DispatchPermit, NativeError> {
        let mut state = self.state.lock().expect("dispatch guard registry poisoned");
        if let Some(held) = state.active.iter().find(|held| held.conflicts_with(&scope)) {
            return Err(NativeError::new(
                ErrorCode::TargetBusy,
                ErrorPhase::Preflight,
                true,
                "the requested native dispatch scope is already in use",
            )
            .with_detail("native_side_effect_started", false)
            .with_detail(
                "requested_scope",
                format!("{:?}", scope.kind()).to_lowercase(),
            )
            .with_detail("requested_identity", scope.identity_detail())
            .with_detail("held_scope", format!("{:?}", held.kind()).to_lowercase()));
        }
        let inserted = state.active.insert(scope.clone());
        debug_assert!(inserted, "an exact duplicate scope must conflict");
        Ok(DispatchPermit {
            state: Arc::clone(&self.state),
            scope: Some(scope),
        })
    }
}

/// RAII ownership of one registered native dispatch scope.
#[derive(Debug)]
pub struct DispatchPermit {
    state: Arc<Mutex<DispatchGuardState>>,
    scope: Option<DispatchScope>,
}

impl DispatchPermit {
    pub fn scope(&self) -> &DispatchScope {
        self.scope
            .as_ref()
            .expect("a live dispatch permit always owns its scope")
    }
}

impl Drop for DispatchPermit {
    fn drop(&mut self) {
        let Some(scope) = self.scope.take() else {
            return;
        };
        let removed = self
            .state
            .lock()
            .expect("dispatch guard registry poisoned")
            .active
            .remove(&scope);
        debug_assert!(removed, "dispatch permit must release its exact scope");
    }
}
