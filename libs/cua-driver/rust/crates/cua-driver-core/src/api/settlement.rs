//! Notification-driven dirty and settlement state.

use std::{collections::BTreeSet, time::Instant};

use serde::{Deserialize, Serialize};

use super::{contracts::ActionId, errors::NativeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementSignal {
    DispatchStarted,
    DispatchComplete,
    AxAction,
    AxValueChanged,
    FocusChanged,
    MenuOpened,
    MenuDismissed,
    WindowListChanged,
    WindowGeometryChanged,
    ScrollChanged,
    FreshFrame,
    /// A promised verification readback reached a terminal result. The
    /// signal records completion, not success: an exact mismatch still lets
    /// settlement finish so `VerificationFailed` remains the primary error.
    VerificationReadbackComplete,
    RunLoopDrained,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementProfile {
    pub name: String,
    pub required_terminal_signals: BTreeSet<SettlementSignal>,
    /// Native target signals that reset the quiet window after terminal
    /// evidence exists. Keeping this profile-specific prevents unrelated or
    /// continuously animated state from holding every action dirty.
    pub relevant_signals: BTreeSet<SettlementSignal>,
    pub quiet_window_ms: u64,
    pub deadline_ms: u64,
}

impl SettlementProfile {
    pub fn dispatch_only(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required_terminal_signals: BTreeSet::new(),
            relevant_signals: BTreeSet::new(),
            quiet_window_ms: 30,
            deadline_ms: 2_000,
        }
    }

    pub fn requiring(
        name: impl Into<String>,
        required_terminal_signals: impl IntoIterator<Item = SettlementSignal>,
    ) -> Self {
        let required_terminal_signals: BTreeSet<_> =
            required_terminal_signals.into_iter().collect();
        Self {
            name: name.into(),
            relevant_signals: required_terminal_signals.clone(),
            required_terminal_signals,
            quiet_window_ms: 30,
            deadline_ms: 2_000,
        }
    }

    pub fn with_relevant_signals(
        mut self,
        relevant_signals: impl IntoIterator<Item = SettlementSignal>,
    ) -> Self {
        self.relevant_signals = relevant_signals.into_iter().collect();
        self
    }
}

#[derive(Debug, Clone)]
pub struct DirtyState {
    pub action_id: ActionId,
    pub profile: SettlementProfile,
    pub since: Instant,
    pub observed_signals: BTreeSet<SettlementSignal>,
    pub resumed_from_prior_call: bool,
}

impl DirtyState {
    pub fn pending_evidence(&self) -> PendingSettlementEvidence {
        let mut missing_signals: Vec<_> = self
            .profile
            .required_terminal_signals
            .difference(&self.observed_signals)
            .copied()
            .collect();
        if !self
            .observed_signals
            .contains(&SettlementSignal::DispatchComplete)
        {
            missing_signals.push(SettlementSignal::DispatchComplete);
        }
        PendingSettlementEvidence {
            state: PendingSettlementState::Pending,
            trigger_action_id: self.action_id.clone(),
            profile: self.profile.name.clone(),
            elapsed_ms: self.since.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            observed_signals: self.observed_signals.iter().copied().collect(),
            missing_signals,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettledState {
    Settled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettlementEvidence {
    pub state: SettledState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_action_id: Option<ActionId>,
    pub profile: String,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub observed_signals: Vec<SettlementSignal>,
    pub terminal_signal: String,
    pub quiet_window_ms: u64,
    #[serde(default)]
    pub resumed_from_prior_call: bool,
}

impl SettlementEvidence {
    pub fn initial() -> Self {
        Self {
            state: SettledState::Settled,
            trigger_action_id: None,
            profile: "initial".to_owned(),
            elapsed_ms: 0,
            observed_signals: Vec::new(),
            terminal_signal: "already_settled".to_owned(),
            quiet_window_ms: 0,
            resumed_from_prior_call: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingSettlementState {
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingSettlementEvidence {
    pub state: PendingSettlementState,
    pub trigger_action_id: ActionId,
    pub profile: String,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub observed_signals: Vec<SettlementSignal>,
    #[serde(default)]
    pub missing_signals: Vec<SettlementSignal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementAttempt {
    Settled(SettlementEvidence),
    Pending(PendingSettlementEvidence),
}

#[derive(Debug, Clone)]
pub enum SettlementState {
    Settled(SettlementEvidence),
    Dirty(DirtyState),
    Settling(DirtyState),
}

impl Default for SettlementState {
    fn default() -> Self {
        Self::Settled(SettlementEvidence::initial())
    }
}

impl SettlementState {
    pub fn mark_dirty(
        &mut self,
        action_id: ActionId,
        profile: SettlementProfile,
    ) -> Result<(), NativeError> {
        if !matches!(self, Self::Settled(_)) {
            return Err(NativeError::invalid(
                "cannot dispatch a new mutation while prior target settlement is pending",
            ));
        }
        let mut observed_signals = BTreeSet::new();
        observed_signals.insert(SettlementSignal::DispatchStarted);
        *self = Self::Dirty(DirtyState {
            action_id,
            profile,
            since: Instant::now(),
            observed_signals,
            resumed_from_prior_call: false,
        });
        Ok(())
    }

    pub fn begin(&mut self, resumed_from_prior_call: bool) -> Option<DirtyState> {
        let current = match self {
            Self::Dirty(dirty) | Self::Settling(dirty) => dirty.clone(),
            Self::Settled(_) => return None,
        };
        let mut settling = current;
        settling.resumed_from_prior_call |= resumed_from_prior_call;
        *self = Self::Settling(settling.clone());
        Some(settling)
    }

    pub fn record_signal(&mut self, signal: SettlementSignal) -> Result<(), NativeError> {
        match self {
            Self::Dirty(dirty) | Self::Settling(dirty) => {
                dirty.observed_signals.insert(signal);
                Ok(())
            }
            Self::Settled(_) => Err(NativeError::invalid(
                "cannot record a settlement signal for a clean target",
            )),
        }
    }

    pub fn complete(
        &mut self,
        evidence: SettlementEvidence,
    ) -> Result<SettlementEvidence, NativeError> {
        let dirty = match self {
            Self::Dirty(dirty) | Self::Settling(dirty) => dirty,
            Self::Settled(_) => {
                return Err(NativeError::invalid(
                    "cannot complete settlement for a target that is already settled",
                ))
            }
        };
        if evidence.trigger_action_id.as_ref() != Some(&dirty.action_id)
            || evidence.profile != dirty.profile.name
        {
            return Err(NativeError::invalid(
                "settlement evidence does not match the dirty action/profile",
            ));
        }
        let observed: BTreeSet<_> = evidence.observed_signals.iter().copied().collect();
        if !observed.contains(&SettlementSignal::DispatchComplete) {
            return Err(NativeError::invalid(
                "settlement evidence cannot complete before dispatch completion",
            ));
        }
        if !dirty.profile.required_terminal_signals.is_subset(&observed) {
            return Err(NativeError::invalid(
                "settlement evidence is missing required terminal signals",
            ));
        }
        *self = Self::Settled(evidence.clone());
        Ok(evidence)
    }

    pub fn preserve_dirty_after_timeout(&mut self) {
        if let Self::Settling(dirty) = self {
            let mut dirty = dirty.clone();
            dirty.resumed_from_prior_call = true;
            *self = Self::Dirty(dirty);
        }
    }

    pub fn preserve_pending(
        &mut self,
        pending: &PendingSettlementEvidence,
    ) -> Result<(), NativeError> {
        let dirty = match self {
            Self::Dirty(dirty) | Self::Settling(dirty) => dirty,
            Self::Settled(_) => {
                return Err(NativeError::invalid(
                    "cannot preserve progress for an already-settled target",
                ))
            }
        };
        if pending.trigger_action_id != dirty.action_id || pending.profile != dirty.profile.name {
            return Err(NativeError::invalid(
                "pending settlement evidence does not match the dirty action/profile",
            ));
        }
        let mut next = dirty.clone();
        next.observed_signals
            .extend(pending.observed_signals.iter().copied());
        next.resumed_from_prior_call = true;
        *self = Self::Dirty(next);
        Ok(())
    }

    pub fn settled_evidence(&self) -> Option<&SettlementEvidence> {
        match self {
            Self::Settled(evidence) => Some(evidence),
            _ => None,
        }
    }

    pub fn pending_evidence(&self) -> Option<PendingSettlementEvidence> {
        match self {
            Self::Dirty(dirty) | Self::Settling(dirty) => Some(dirty.pending_evidence()),
            Self::Settled(_) => None,
        }
    }
}
