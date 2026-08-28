//! Pure classification for platform-observed foreground-window epochs.

use cua_driver_contract::{FocusEffect, FocusEffectKind};

pub fn classify_focus_epoch(
    initial: Option<u64>,
    target: Option<u64>,
    final_active: Option<u64>,
    transitions: &[(u64, Option<u64>)],
    measurement_complete: bool,
    end_ms: u64,
) -> FocusEffect {
    let (Some(initial), Some(target), Some(final_active)) = (initial, target, final_active) else {
        return indeterminate();
    };
    if !measurement_complete {
        return indeterminate();
    }
    let target_active_ms = active_duration_ms(initial, target, transitions, end_ms);
    if initial == target {
        return FocusEffect {
            kind: FocusEffectKind::NotEligible,
            transition_count: transitions.len() as u32,
            target_active_ms,
            measurement_complete: true,
        };
    }
    let target_seen = transitions
        .iter()
        .any(|(_, active)| *active == Some(target));
    let kind = if !target_seen {
        FocusEffectKind::Preserved
    } else if final_active == initial {
        FocusEffectKind::TemporarilyTakenAndRestored
    } else {
        FocusEffectKind::TakenAndNotRestored
    };
    FocusEffect {
        kind,
        transition_count: transitions.len() as u32,
        target_active_ms,
        measurement_complete: true,
    }
}

pub fn indeterminate() -> FocusEffect {
    FocusEffect {
        kind: FocusEffectKind::Indeterminate,
        transition_count: 0,
        target_active_ms: 0,
        measurement_complete: false,
    }
}

fn active_duration_ms(
    initial: u64,
    target: u64,
    transitions: &[(u64, Option<u64>)],
    end_ms: u64,
) -> u64 {
    let mut current = Some(initial);
    let mut since = 0_u64;
    let mut total = 0_u64;
    for (at_ms, active) in transitions {
        if current == Some(target) {
            total = total.saturating_add(at_ms.saturating_sub(since));
        }
        current = *active;
        since = *at_ms;
    }
    if current == Some(target) {
        total = total.saturating_add(end_ms.saturating_sub(since));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_temporary_takeover_even_when_focus_is_restored() {
        let effect = classify_focus_epoch(
            Some(10),
            Some(20),
            Some(10),
            &[(2, Some(20)), (7, Some(10))],
            true,
            9,
        );
        assert_eq!(effect.kind, FocusEffectKind::TemporarilyTakenAndRestored);
        assert_eq!(effect.target_active_ms, 5);
        assert!(effect.measurement_complete);
    }

    #[test]
    fn missing_target_fails_measurement_closed() {
        let effect = classify_focus_epoch(Some(10), None, Some(10), &[], true, 1);
        assert_eq!(effect.kind, FocusEffectKind::Indeterminate);
        assert!(!effect.measurement_complete);
    }
}
