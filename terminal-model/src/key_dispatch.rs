//! Hardware-independent push-to-talk key dispatch.
//!
//! The firmware's `keyboard.rs` reads keyboard reports off I2C and then calls
//! [`ptt_action`] to decide what (if anything) a report means for the
//! push-to-talk recording. Keeping the decision here, free of embassy/`crate`
//! dependencies, lets host unit tests exercise the exact function the
//! firmware runs (per AGENTS.md's "host-testable logic lives in
//! terminal-model" guidance).
//!
//! The binding is plain `F1`:
//! - pressing `F1` with no modifiers held starts a recording;
//! - releasing `F1` while a recording is active always stops it.
//!
//! Arming and stopping are deliberately independent. Stopping does *not*
//! re-check modifiers, because a modifier pressed while `F1` is already held
//! must still be able to end the recording (otherwise the device would keep
//! capturing indefinitely). Arming *does* require no modifiers so the
//! existing `Ctrl+F1` reboot shortcut is never shadowed.

/// The subset of key transitions push-to-talk cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttTransition {
    Pressed,
    Released,
    /// `Hold`/`Idle` and anything else: neither an arm nor a stop.
    Other,
}

/// What the caller should do with the keyboard report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttAction {
    Start,
    Stop,
    None,
}

/// Pure decision for push-to-talk handling.
///
/// `key_is_f1` is whether the report is for `F1`; `transition` is the key
/// state; `no_modifiers` is whether the report's modifier set is empty;
/// `is_recording` is whether a capture is currently active.
pub fn ptt_action(
    key_is_f1: bool,
    transition: PttTransition,
    no_modifiers: bool,
    is_recording: bool,
) -> PttAction {
    if key_is_f1 && transition == PttTransition::Pressed && no_modifiers {
        PttAction::Start
    } else if key_is_f1 && transition == PttTransition::Released && is_recording {
        PttAction::Stop
    } else {
        PttAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_f1_press_arms_recording() {
        assert_eq!(
            ptt_action(true, PttTransition::Pressed, true, false),
            PttAction::Start
        );
    }

    #[test]
    fn ctrl_f1_press_does_not_arm_so_reboot_shortcut_stays_reachable() {
        assert_eq!(
            ptt_action(true, PttTransition::Pressed, false, false),
            PttAction::None
        );
    }

    #[test]
    fn f1_release_with_no_modifier_stops_active_recording() {
        assert_eq!(
            ptt_action(true, PttTransition::Released, true, true),
            PttAction::Stop
        );
    }

    // Regression guard for the bug this change fixes: the stop check must not
    // re-check modifiers, so a `Ctrl` pressed while `F1` was held can still
    // end the recording instead of trapping the device in an endless capture.
    #[test]
    fn f1_release_with_modifier_still_stops_active_recording() {
        assert_eq!(
            ptt_action(true, PttTransition::Released, false, true),
            PttAction::Stop
        );
    }

    #[test]
    fn f1_release_without_active_recording_is_a_no_op() {
        assert_eq!(
            ptt_action(true, PttTransition::Released, true, false),
            PttAction::None
        );
        assert_eq!(
            ptt_action(true, PttTransition::Released, false, false),
            PttAction::None
        );
    }

    #[test]
    fn f1_hold_is_neither_arm_nor_stop() {
        assert_eq!(
            ptt_action(true, PttTransition::Other, true, true),
            PttAction::None
        );
    }

    #[test]
    fn other_keys_are_never_push_to_talk() {
        assert_eq!(
            ptt_action(false, PttTransition::Pressed, true, false),
            PttAction::None
        );
        assert_eq!(
            ptt_action(false, PttTransition::Released, true, true),
            PttAction::None
        );
    }

    /// Drives the same decision function through a full utterance the way the
    /// firmware's `keyboard.rs` + `mic.rs` pair does: `is_recording` is the
    /// state the firmware keeps in `mic.rs`'s `RECORDING` flag, updated by the
    /// actions returned here.
    #[test]
    fn hold_f1_then_add_ctrl_and_release_ends_the_utterance() {
        let mut recording = false;

        // Press plain F1: begins capture.
        let action = ptt_action(true, PttTransition::Pressed, true, recording);
        assert_eq!(action, PttAction::Start);
        recording = true;

        // Press Ctrl while F1 is still down: Ctrl is its own report and must
        // not arm a recording (nor reboot - the reboot arm only fires on F1's
        // own Pressed report).
        let action = ptt_action(false, PttTransition::Pressed, false, recording);
        assert_eq!(action, PttAction::None);
        assert!(recording);

        // Release F1 with Ctrl still held: the recording must stop even though
        // modifiers are non-empty.
        let action = ptt_action(true, PttTransition::Released, false, recording);
        assert_eq!(action, PttAction::Stop);
        recording = false;

        assert!(!recording);
    }
}
