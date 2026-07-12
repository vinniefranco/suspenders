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
//!   content is the Plan's - the model's voice, never authored by a Governor
//!   - and an Anchor is routine, not corrective (it is no Nudge).
//! * **Setpoints**: [`Setpoints`] - the placement interval, in Passes
//!   (0 disables the periodic cadence; the post-Compaction refresh still
//!   fires). User-exposed: the Session's resolved `anchor_interval` knob
//!   feeds it at Turn start, and the default matches the Session's.

use crate::turn::governor::ledger::Ledger;

/// The anchor Governor's Setpoints (CONTEXT.md: Setpoint). Fed by the
/// Session's resolved `anchor_interval` knob at Turn start.
#[derive(Debug, Clone)]
pub struct Setpoints {
    /// Place an Anchor every this-many Passes; 0 disables the periodic
    /// cadence (the post-Compaction refresh is unconditional).
    pub interval: u64,
}

impl Default for Setpoints {
    fn default() -> Self {
        Setpoints { interval: 5 }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every(interval: u64) -> Anchor {
        Anchor::new(Setpoints { interval })
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
}
