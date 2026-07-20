//! The verify Governor: a Turn should not conclude with unverified writes,
//! nor while the last command it ran is failing (CONTEXT.md: Governor, Nudge;
//! ADR-0026). Two gates, one Governor: the Verify-failed Nudge (the model
//! finishing while its last run_command failed) and the Verify Nudge (changes
//! left unverified) share the trigger discipline and the re-arm rule.
//!
//! * **Trigger**: private once-per-Turn caps (`verify_nudged`,
//!   `verify_failed_nudged`), re-armed by progress - a Pass that made at
//!   least one Tool Call ([`Verify::note_progress`]). The unverified-writes
//!   and command-failing facts themselves are the Ledger's; this Governor
//!   only judges them against its caps.
//! * **Interventions**: stands alone as a user message at the finish
//!   settlement - the model gets one more Pass to act on it. The strict
//!   Verify-failed > Verify precedence is the arbiter's
//!   ([`super::settle_finish`]), not this module's.
//! * **Setpoints**: none - the once-per-Turn-until-progress cap is the
//!   trigger's mechanics, not a tuned value; no threshold here has ever
//!   demanded tuning.
//!
//! The wording lives in [`crate::voice`]; this module never authors nudge
//! strings.

use serde_json::Value;

use crate::turn::governor::ledger::Ledger;

/// The verify Governor's private trigger state, a plain value the loop
/// threads (methods mutate `&mut self` or read, no processes).
#[derive(Debug, Clone, Default)]
pub struct Verify {
    verify_nudged: bool,
    verify_failed_nudged: bool,
}

impl Verify {
    pub fn new() -> Self {
        Verify::default()
    }

    /// Does end-of-turn owe the Verify Nudge? The unverified writes are the
    /// Ledger's fact; the once-per-Turn cap is this Governor's trigger state.
    pub fn verify_nudge(&self, ledger: &Ledger) -> bool {
        ledger.unverified_writes() && !self.verify_nudged
    }

    /// Does end-of-turn owe the Verify-failed Nudge? True when the most recent
    /// run_command this Turn failed (the Ledger's fact) and this Nudge has not
    /// fired yet.
    pub fn verify_failed_nudge(&self, ledger: &Ledger) -> bool {
        ledger.command_failing() && !self.verify_failed_nudged
    }

    /// The Verify Nudge fired; fires at most once per Turn UNTIL progress
    /// re-arms it.
    pub fn note_verify_nudged(&mut self) {
        self.verify_nudged = true;
    }

    /// The Verify-failed Nudge fired; fires at most once per Turn UNTIL
    /// progress re-arms it.
    pub fn note_verify_failed_nudged(&mut self) {
        self.verify_failed_nudged = true;
    }

    /// Records a Pass's Tool Calls for the re-arming rule. A Pass that made at
    /// least one Tool Call is progress: it re-arms both gates. An idle Pass
    /// (no Tool Calls) never re-arms. This only re-arms; it never disarms an
    /// armed-but-unfired gate.
    pub fn note_progress(&mut self, calls: &[(String, Value)]) {
        if calls.is_empty() {
            return;
        }
        self.verify_nudged = false;
        self.verify_failed_nudged = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::governor::ledger::{CallOutcome, ToolResult};
    use serde_json::json;

    fn ok() -> ToolResult<'static> {
        ToolResult {
            content: "ok",
            is_error: false,
        }
    }

    fn err() -> ToolResult<'static> {
        ToolResult {
            content: "boom",
            is_error: true,
        }
    }

    // ----- verify nudge -----

    // One successful edit_file leaves the Turn with unverified writes.
    fn unverified() -> Ledger {
        let mut ledger = Ledger::new(25);
        ledger.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);
        ledger
    }

    #[test]
    fn verify_starts_disarmed() {
        assert!(!Verify::new().verify_nudge(&Ledger::new(25)));
    }

    #[test]
    fn an_unverified_write_arms_it() {
        assert!(Verify::new().verify_nudge(&unverified()));
    }

    #[test]
    fn verify_fires_at_most_once_per_turn() {
        let mut ledger = unverified();
        let mut verify = Verify::new();
        verify.note_verify_nudged();
        ledger.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);

        assert!(!verify.verify_nudge(&ledger));
    }

    // ----- verify-failed nudge -----

    // The most recent run_command this Turn failed.
    fn command_failing() -> Ledger {
        let mut ledger = Ledger::new(25);
        ledger.record("run_command", &json!({}), &err(), CallOutcome::Ran);
        ledger
    }

    #[test]
    fn verify_failed_starts_disarmed() {
        assert!(!Verify::new().verify_failed_nudge(&Ledger::new(25)));
    }

    #[test]
    fn a_failing_run_command_arms_it() {
        assert!(Verify::new().verify_failed_nudge(&command_failing()));
    }

    #[test]
    fn verify_failed_fires_at_most_once_while_idle() {
        let ledger = command_failing();
        let mut verify = Verify::new();
        verify.note_verify_failed_nudged();

        assert!(!verify.verify_failed_nudge(&ledger));
    }

    #[test]
    fn a_pass_with_a_tool_call_re_arms_verify_failed() {
        let ledger = command_failing();
        let mut verify = Verify::new();
        verify.note_verify_failed_nudged();
        verify.note_progress(&[("read_file".into(), json!({"path": "a.ex"}))]);

        assert!(verify.verify_failed_nudge(&ledger));
    }

    #[test]
    fn a_pass_with_no_tool_calls_does_not_re_arm_verify_failed() {
        let ledger = command_failing();
        let mut verify = Verify::new();
        verify.note_verify_failed_nudged();
        verify.note_progress(&[]);

        assert!(!verify.verify_failed_nudge(&ledger));
    }

    #[test]
    fn note_progress_before_firing_leaves_the_cap_untouched() {
        let ledger = command_failing();
        let mut verify = Verify::new();
        verify.note_progress(&[("read_file".into(), json!({}))]);

        assert!(verify.verify_failed_nudge(&ledger));
    }
}
