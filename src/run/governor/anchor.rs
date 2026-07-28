//! The anchor Governor: when the Anchor - an injected copy of the Plan and
//! the original task - is placed near the Conversation's tail, so the goal
//! always sits where a small model actually attends (CONTEXT.md: Anchor,
//! Governor; ADR-0026).
//!
//! * **Trigger**: no private state - both inputs are Ledger facts: the Pass
//!   position for the periodic cadence, and the compacted-since-tail flag
//!   ("An Anchor is refreshed immediately after every Compaction" -
//!   CONTEXT.md).
//! * **Interventions**: rides the results tail at the answering moment
//!   ([`Anchor::due`] answers, the arbiter issues the
//!   [`Rider::Anchor`](super::Rider::Anchor)). Placement only: an Anchor's
//!   content is the Plan's - the model's voice, never authored by a Governor -
//!   and an Anchor is routine, not corrective (it is no Nudge). The one
//!   Voice line this Governor adds ([`Anchor::stale_plan`]) rides BELOW the
//!   Anchor, never inside the Plan's content.
//! * **Setpoints**: [`Setpoints`] - the placement interval, in Passes
//!   (0 disables the periodic cadence; the post-Compaction refresh still
//!   fires), and `plan_stale_after`, the Passes a Plan may go unchanged
//!   while writes land before each Anchor carries the stale-plan line.
//!   User-exposed: the Session's resolved `anchor_interval` and
//!   `plan_stale_after` knobs feed them at Run start, and the defaults
//!   match the Session's.

use crate::run::governor::ledger::Ledger;

/// The anchor Governor's Setpoints (CONTEXT.md: Setpoint). Fed by the
/// Session's resolved `anchor_interval` and `plan_stale_after` knobs at Run
/// start.
#[derive(Debug, Clone)]
pub struct Setpoints {
    /// Place an Anchor every this-many Passes; 0 disables the periodic
    /// cadence (the post-Compaction refresh is unconditional).
    pub interval: u64,
    /// The Passes a Plan may sit unchanged - while writes land - before an
    /// Anchor carries the stale-plan line (PROPOSALS.md #4: the f5 audit's
    /// pass-5 plan re-injected verbatim for 20 Passes of debugging).
    pub plan_stale_after: u64,
}

impl Default for Setpoints {
    fn default() -> Self {
        Setpoints {
            interval: 5,
            plan_stale_after: 8,
        }
    }
}

/// The anchor Governor: resolved Setpoints only, no trigger state.
#[derive(Debug, Clone, Default)]
pub struct Anchor {
    setpoints: Setpoints,
}

impl Anchor {
    pub fn new(setpoints: Setpoints) -> Self {
        Anchor { setpoints }
    }

    /// Does an Anchor ride this Pass's results tail? Every `interval` Passes,
    /// or the first tail after a Compaction.
    pub fn due(&self, ledger: &Ledger) -> bool {
        ledger.just_compacted()
            || (self.setpoints.interval != 0
                && ledger.pass().is_multiple_of(self.setpoints.interval))
    }

    /// The stale-plan opinion: the Plan exists, has not changed in more than
    /// `plan_stale_after` Passes, and writes have landed since - the Anchor
    /// being placed carries the stale-plan line, parameterized by the
    /// returned Pass count (the Voice owns the wording). A stale Plan during
    /// pure reading is not stale, just not yet actionable - hence the writes
    /// gate - and while the condition holds the line rides EVERY Anchor
    /// (Anchors are periodic; a one-shot line would scroll into the same
    /// rot). Both inputs are Ledger facts: `None` from the Ledger means no
    /// Plan exists, and no Plan cannot be stale.
    pub fn stale_plan(&self, ledger: &Ledger) -> Option<u64> {
        let passes = ledger.passes_since_plan_update()?;
        let writes = ledger.writes_since_plan_update()?;
        (passes > self.setpoints.plan_stale_after && writes > 0).then_some(passes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn every(interval: u64) -> Anchor {
        Anchor::new(Setpoints {
            interval,
            ..Setpoints::default()
        })
    }

    // A Ledger at a Pass position.
    fn ledger_at(pass: u64) -> Ledger {
        let mut ledger = Ledger::new(25);
        for _ in 1..pass {
            ledger.advance_pass();
        }
        ledger
    }

    #[test]
    fn due_on_every_interval_hit_and_only_there() {
        let anchor = every(5);
        assert!(!anchor.due(&ledger_at(4)));
        assert!(anchor.due(&ledger_at(5)));
        assert!(!anchor.due(&ledger_at(6)));
        assert!(anchor.due(&ledger_at(10)));
    }

    #[test]
    fn interval_zero_disables_the_periodic_cadence() {
        let anchor = every(0);
        assert!(!anchor.due(&ledger_at(5)));
        assert!(!anchor.due(&ledger_at(10)));
    }

    #[test]
    fn a_compaction_makes_the_anchor_due_off_interval() {
        let mut ledger = ledger_at(3);
        ledger.note_compacted();

        assert!(every(999).due(&ledger));
        assert!(every(0).due(&ledger));
    }

    // ---- stale_plan ----

    use crate::run::governor::ledger::{CallOutcome, ToolResult};

    fn ok_write() -> ToolResult<'static> {
        ToolResult {
            content: "ok",
            is_error: false,
        }
    }

    fn stale_after(threshold: u64) -> Anchor {
        Anchor::new(Setpoints {
            plan_stale_after: threshold,
            ..Setpoints::default()
        })
    }

    // A Ledger whose Plan landed on Pass 1, with one successful write, at a
    // later Pass position.
    fn planned_then_wrote(pass: u64) -> Ledger {
        let mut ledger = ledger_at(1);
        ledger.note_plan_updated(crate::plan::PlanProgress::NoCheckboxes, 0);
        ledger.record("edit_file", &json!({}), &ok_write(), CallOutcome::Ran);
        for _ in 1..pass {
            ledger.advance_pass();
        }
        ledger
    }

    #[test]
    fn a_plan_unchanged_past_the_threshold_with_writes_is_stale() {
        // Plan on Pass 1, now Pass 10: 9 Passes since, threshold 8.
        assert_eq!(stale_after(8).stale_plan(&planned_then_wrote(10)), Some(9));
    }

    #[test]
    fn no_plan_is_never_stale() {
        let mut ledger = ledger_at(20);
        ledger.record("edit_file", &json!({}), &ok_write(), CallOutcome::Ran);
        assert_eq!(stale_after(8).stale_plan(&ledger), None);
    }

    #[test]
    fn a_plan_during_pure_reading_is_not_stale() {
        // Plan on Pass 1, now Pass 20, zero writes since: analysis, not rot.
        let mut ledger = ledger_at(1);
        ledger.note_plan_updated(crate::plan::PlanProgress::NoCheckboxes, 0);
        for _ in 1..20 {
            ledger.advance_pass();
        }
        assert_eq!(stale_after(8).stale_plan(&ledger), None);
    }

    #[test]
    fn at_or_below_the_threshold_the_plan_is_fresh() {
        // 8 Passes since (Pass 9) is not MORE than 8.
        assert_eq!(stale_after(8).stale_plan(&planned_then_wrote(9)), None);
        assert_eq!(stale_after(8).stale_plan(&planned_then_wrote(5)), None);
    }

    #[test]
    fn a_plan_update_resets_the_opinion() {
        let mut ledger = planned_then_wrote(10);
        assert!(stale_after(8).stale_plan(&ledger).is_some());

        ledger.note_plan_updated(crate::plan::PlanProgress::NoCheckboxes, 0);
        assert_eq!(stale_after(8).stale_plan(&ledger), None);
    }

    #[test]
    fn the_opinion_holds_on_every_later_pass_while_unaddressed() {
        // The line rides every qualifying Anchor, not once: the condition
        // keeps answering (with a growing count) until the Plan changes.
        assert_eq!(stale_after(8).stale_plan(&planned_then_wrote(10)), Some(9));
        assert_eq!(stale_after(8).stale_plan(&planned_then_wrote(15)), Some(14));
    }
}
