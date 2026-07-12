//! The failure Governor: consecutive failures of one Tool draw the step-back
//! annotation, and a recent threshold streak marks the Turn stuck
//! (CONTEXT.md: Governor, Setpoint, Nudge; ADR-0026).
//!
//! * **Trigger**: the Ledger's per-Tool consecutive-failure tallies and their
//!   recency stamps - facts only; this Governor keeps no trigger state of its
//!   own (a streak resets on that Tool's next success, recorded at the firing
//!   site).
//! * **Interventions**: annotates a Tool Result at the answering moment -
//!   from the threshold failure of one Tool onward, the "step back" suffix
//!   rides the real result, summarising the kinds of errors seen
//!   ([`annotation`]). Its judgment also informs the close at the finish
//!   settlement: the Endgame's turn-limit stop reason reads [`stuck`] to
//!   distinguish a stuck Turn from a productive one.
//! * **Setpoints**: [`Setpoints`] - the annotation threshold and the stuck
//!   recency window. Neither is user-exposed: resolution at launch is the
//!   [`Default`], because a Setpoint becomes user-configurable only when a
//!   real model has demanded a different value (CONTEXT.md: Setpoint).
//!
//! [`stuck`] is deliberately this Governor's one exported pure predicate with
//! two readers - the answering moment's annotation shares its threshold, and
//! Settlement's stop reason (via [`super::endgame::limit_stop_reason`]) shares
//! the whole judgment - over one set of setpoints (ADR-0026). The wording
//! lives in [`crate::voice`]; this module never authors nudge strings.

use crate::turn::governor::ledger::Ledger;
use crate::voice;

/// The failure Governor's Setpoints (CONTEXT.md: Setpoint): thresholds tuned
/// from observed model behavior, carried with their defaults.
#[derive(Debug, Clone)]
pub struct Setpoints {
    /// From this consecutive failure of one Tool onward, the step-back
    /// annotation rides the result (and the streak can mark the Turn stuck).
    pub nudge_from: u64,
    /// A threshold streak marks the Turn stuck only while its last failure
    /// landed within this many closed batches.
    pub stuck_recency: u64,
}

impl Default for Setpoints {
    fn default() -> Self {
        Setpoints {
            nudge_from: 3,
            stuck_recency: 3,
        }
    }
}

/// Is the Turn stuck in a failure loop? True when any Tool has reached the
/// consecutive-failure threshold AND that streak is recent - its last failure
/// landed within the final `stuck_recency` batches. A streak only resets on
/// that Tool's next success, so without the recency requirement a Tool the
/// model abandoned early and routed around would still label the Turn stuck
/// at the limit. A pure read over the Ledger's facts against this Governor's
/// setpoints (ADR-0026: one exported predicate, two readers).
pub fn stuck(ledger: &Ledger) -> bool {
    let setpoints = Setpoints::default();
    ledger
        .failure_tallies()
        .any(|(count, since)| count >= setpoints.nudge_from && since <= setpoints.stuck_recency)
}

/// The consecutive-failure opinion over the Ledger's tallies: from the
/// threshold failure of one Tool onward, the content the model reads carries
/// the "step back" suffix summarising the kinds of errors seen. `None` when
/// the result rides unannotated.
pub fn annotation(ledger: &Ledger, name: &str, content: &str) -> Option<String> {
    let setpoints = Setpoints::default();
    let (count, categories) = ledger.failure_streak(name)?;
    (count >= setpoints.nudge_from).then(|| {
        format!(
            "{}{}",
            content,
            voice::failure_nudge(count, name, categories)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::governor::ledger::ToolResult;

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

    // ----- consecutive-failure annotation -----

    // Records one outcome and asks for the annotation the model would read -
    // the fact write (Ledger) and the opinion (this module), composed the way
    // the answering arbiter composes them.
    fn annotated(ledger: &mut Ledger, name: &str, result: &ToolResult) -> Option<String> {
        ledger.record_result(name, result);
        annotation(ledger, name, result.content)
    }

    #[test]
    fn the_3rd_consecutive_failure_earns_the_suffix() {
        let mut ledger = Ledger::new(25);
        let c1 = annotated(&mut ledger, "read_file", &err());
        let c2 = annotated(&mut ledger, "read_file", &err());
        let c3 = annotated(&mut ledger, "read_file", &err()).unwrap();
        let c4 = annotated(&mut ledger, "read_file", &err()).unwrap();

        assert_eq!(c1, None);
        assert_eq!(c2, None);
        assert!(c3.starts_with("boom"));
        assert!(c3.contains("3 consecutive read_file failures"));
        assert!(c3.contains("step back:"));
        assert!(c4.contains("4 consecutive read_file failures"));
    }

    #[test]
    fn a_success_for_the_tool_resets_its_counter() {
        let mut ledger = Ledger::new(25);
        annotated(&mut ledger, "read_file", &err());
        annotated(&mut ledger, "read_file", &err());
        annotated(&mut ledger, "read_file", &ok());
        annotated(&mut ledger, "read_file", &err());
        annotated(&mut ledger, "read_file", &err());

        let content = annotated(&mut ledger, "read_file", &err()).unwrap();
        assert!(content.contains("3 consecutive read_file failures"));
    }

    #[test]
    fn counters_are_per_tool_name() {
        let mut ledger = Ledger::new(25);
        annotated(&mut ledger, "read_file", &err());
        annotated(&mut ledger, "read_file", &err());

        assert_eq!(annotated(&mut ledger, "grep", &err()), None);
    }

    // ----- stuck? -----

    fn fail_thrice(ledger: &mut Ledger, name: &str) {
        ledger.record_result(name, &err());
        ledger.record_result(name, &err());
        ledger.record_result(name, &err());
    }

    fn batches(ledger: &mut Ledger, n: u64) {
        for _ in 0..n {
            ledger.close_batch();
        }
    }

    #[test]
    fn below_the_failure_threshold_is_never_stuck() {
        let mut ledger = Ledger::new(25);
        ledger.record_result("read_file", &err());
        ledger.record_result("read_file", &err());
        ledger.close_batch();

        assert!(!stuck(&ledger));
    }

    #[test]
    fn a_threshold_streak_that_just_failed_marks_stuck() {
        let mut ledger = Ledger::new(25);
        fail_thrice(&mut ledger, "read_file");
        ledger.close_batch();

        assert!(stuck(&ledger));
    }

    #[test]
    fn an_abandoned_early_streak_goes_stale() {
        let mut ledger = Ledger::new(25);
        fail_thrice(&mut ledger, "explore");
        batches(&mut ledger, 20);

        assert!(!stuck(&ledger));
    }

    #[test]
    fn the_streak_stays_live_for_exactly_the_recency_window() {
        let mut a = Ledger::new(25);
        fail_thrice(&mut a, "read_file");
        let mut b = a.clone();
        batches(&mut a, 3);
        assert!(stuck(&a));
        batches(&mut b, 4);
        assert!(!stuck(&b));
    }

    #[test]
    fn a_new_failure_revives_a_stale_streak() {
        let mut ledger = Ledger::new(25);
        fail_thrice(&mut ledger, "read_file");
        batches(&mut ledger, 10);
        ledger.record_result("read_file", &err());
        ledger.close_batch();

        assert!(stuck(&ledger));
    }

    #[test]
    fn a_success_clears_the_streak_entirely() {
        let mut ledger = Ledger::new(25);
        fail_thrice(&mut ledger, "read_file");
        ledger.record_result("read_file", &ok());
        ledger.close_batch();

        assert!(!stuck(&ledger));
    }
}
