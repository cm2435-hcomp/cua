//! Stable, phase-aware v2 error envelope.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    contracts::{ActionId, AppRef, ObservationId, Route, VerificationLevel, WindowRef},
    interaction::{NativeEvidence, PostureResult},
    settlement::PendingSettlementEvidence,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPhase {
    Validate,
    Preflight,
    Dispatch,
    Settle,
    Verify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtocolMismatch,
    InvalidRequest,
    PermissionDenied,
    AppNotFound,
    AppLaunchFailed,
    WindowNotFound,
    WindowIdentityChanged,
    ObservationStale,
    ObservationRaced,
    SurfaceStale,
    ElementStale,
    AxRevisionMismatch,
    UiNotSettled,
    MenuStateStale,
    UnsupportedInBackground,
    TargetBusy,
    DispatchFailed,
    VerificationFailed,
    /// A required posture witness was unavailable, incomplete, or lagged,
    /// and no foreground, key-window, or physical-cursor disturbance was
    /// observed. This is distinct from an observed posture violation.
    PostureUnverifiable,
    PostureViolated,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeError {
    pub code: ErrorCode,
    pub phase: ErrorPhase,
    pub retryable: bool,
    pub message: String,
    #[serde(default)]
    pub details: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_evidence: Option<Box<PartialEvidence>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_settlement: Option<Box<PendingSettlementEvidence>>,
    #[serde(default)]
    pub related_failures: Vec<NativeErrorSummary>,
    /// Internal platform-to-core containment signal. This is deliberately not
    /// serialized: callers need the durable failure evidence above, while the
    /// in-process controller needs a typed instruction never to reuse native
    /// state whose cleanup could not be proved.
    #[serde(skip)]
    target_invalidated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeErrorSummary {
    pub code: ErrorCode,
    pub phase: ErrorPhase,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PartialEvidence {
    Action {
        action_id: ActionId,
        window: WindowRef,
        consumed_observation_id: ObservationId,
        route: Route,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dispatch: Option<PartialNativeDispatch>,
        #[serde(default)]
        posture: PostureResult,
        #[serde(default)]
        native_evidence: NativeEvidence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_settlement: Option<Box<PendingSettlementEvidence>>,
    },
    Launch {
        action_id: ActionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<AppRef>,
        #[serde(default)]
        windows: Vec<WindowRef>,
        #[serde(default)]
        posture: PostureResult,
        #[serde(default)]
        native_evidence: NativeEvidence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_settlement: Option<Box<PendingSettlementEvidence>>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialNativeDispatch {
    pub verification: VerificationLevel,
    #[serde(default)]
    pub native_evidence: NativeEvidence,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl NativeError {
    pub fn new(
        code: ErrorCode,
        phase: ErrorPhase,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            phase,
            retryable,
            message: message.into(),
            details: BTreeMap::new(),
            partial_evidence: None,
            pending_settlement: None,
            related_failures: Vec::new(),
            target_invalidated: false,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(
            ErrorCode::InvalidRequest,
            ErrorPhase::Validate,
            false,
            message,
        )
    }

    pub fn stale(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(code, ErrorPhase::Preflight, true, message)
    }

    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::new(
            ErrorCode::UnsupportedInBackground,
            ErrorPhase::Preflight,
            false,
            reason,
        )
    }

    pub fn from_posture(posture: &PostureResult) -> Option<Self> {
        let observed_disturbance = posture.frontmost_changed
            || posture.key_window_changed
            || posture.physical_cursor_moved
            || posture.restored_after_violation;
        if observed_disturbance {
            return Some(Self::new(
                ErrorCode::PostureViolated,
                ErrorPhase::Verify,
                false,
                "interaction changed the user's foreground, key window, or physical cursor posture",
            ));
        }
        if !posture.held {
            return Some(Self::new(
                ErrorCode::PostureUnverifiable,
                ErrorPhase::Verify,
                false,
                "required posture witness was unavailable, incomplete, or lagged without an observed disturbance",
            ));
        }
        None
    }

    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }

    pub fn with_related(mut self, failure: &NativeError) -> Self {
        self.related_failures.push(NativeErrorSummary {
            code: failure.code,
            phase: failure.phase,
            message: failure.message.clone(),
        });
        self
    }

    pub fn with_target_invalidated(mut self) -> Self {
        self.target_invalidated = true;
        self
    }

    pub fn target_invalidated(&self) -> bool {
        self.target_invalidated
    }

    pub fn precedence(&self) -> u8 {
        match self.code {
            ErrorCode::PostureViolated => 5,
            ErrorCode::PostureUnverifiable => 4,
            ErrorCode::DispatchFailed => 3,
            ErrorCode::UiNotSettled => 2,
            ErrorCode::VerificationFailed => 1,
            _ => 0,
        }
    }

    pub fn primary(mut failures: Vec<Self>) -> Option<Self> {
        if failures.is_empty() {
            return None;
        }
        let primary_index = failures
            .iter()
            .enumerate()
            .max_by_key(|(_, error)| error.precedence())
            .map(|(index, _)| index)?;
        let mut primary = failures.remove(primary_index);
        primary.target_invalidated |= failures.iter().any(Self::target_invalidated);
        primary
            .related_failures
            .extend(failures.iter().map(|failure| NativeErrorSummary {
                code: failure.code,
                phase: failure.phase,
                message: failure.message.clone(),
            }));
        Some(primary)
    }
}

impl fmt::Display for NativeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?}/{:?}: {}",
            self.code, self.phase, self.message
        )
    }
}

impl std::error::Error for NativeError {}
