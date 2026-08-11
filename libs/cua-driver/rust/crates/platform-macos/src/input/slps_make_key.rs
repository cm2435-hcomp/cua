//! Exact target-only `SLPSPostEventRecordTo` make-key/remove-key records.
//!
//! The recovered bytes prove only these two marker states. They do not prove
//! distinct CPS `new-front`, `key-focus-returned`, or other event subtypes, so
//! this module deliberately does not name or claim them.

use super::skylight;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlpsMakeKeyState {
    MakeKey,
    RemoveKey,
}

impl SlpsMakeKeyState {
    pub fn marker(self) -> u8 {
        match self {
            Self::MakeKey => skylight::SLPS_MAKE_KEY_MARKER,
            Self::RemoveKey => skylight::SLPS_REMOVE_KEY_MARKER,
        }
    }

    pub fn evidence_name(self) -> &'static str {
        match self {
            Self::MakeKey => "slps_make_key",
            Self::RemoveKey => "slps_remove_key",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SlpsMakeKeyError {
    #[error("target-only SLPS make-key symbols are unavailable")]
    SymbolsUnavailable,
    #[error("target-only SLPS {state:?} record failed for pid {pid}, window {window_id}")]
    PostFailed {
        pid: i32,
        window_id: u32,
        state: SlpsMakeKeyState,
    },
}

pub fn available() -> bool {
    skylight::is_target_slps_make_key_available()
}

pub fn post_target_only(
    pid: i32,
    window_id: u32,
    states: &[SlpsMakeKeyState],
) -> Result<(), SlpsMakeKeyError> {
    if !available() {
        return Err(SlpsMakeKeyError::SymbolsUnavailable);
    }
    for state in states {
        if !skylight::post_slps_make_key_record_to_pid(pid, window_id, state.marker()) {
            return Err(SlpsMakeKeyError::PostFailed {
                pid,
                window_id,
                state: *state,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_evidence_claims_only_make_key_and_remove_key_marker_shapes() {
        let window_id = 0x0102_0304;
        let grant =
            skylight::build_slps_make_key_record(window_id, SlpsMakeKeyState::MakeKey.marker());
        let revoke =
            skylight::build_slps_make_key_record(window_id, SlpsMakeKeyState::RemoveKey.marker());
        assert_eq!(&grant[0x3C..=0x3F], &window_id.to_le_bytes());
        assert_eq!(grant[0x8A], skylight::SLPS_MAKE_KEY_MARKER);
        assert_eq!(revoke[0x8A], skylight::SLPS_REMOVE_KEY_MARKER);
        assert_eq!(
            grant
                .iter()
                .zip(revoke.iter())
                .filter(|(left, right)| left != right)
                .count(),
            1
        );
    }
}
