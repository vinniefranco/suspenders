//! The duplicate Governor: an identical Tool Call repeated while its previous
//! result is still fresh draws a replacement Tool Result instead of a rerun
//! (CONTEXT.md: Governor, Nudge; ADR-0026).
//!
//! * **Trigger**: private freshness memory - `prev_calls` remembers the
//!   previous response's calls whose results are still fresh, `fresh`
//!   accumulates this batch's; membership is `{name, input}` equality. The
//!   Governor keys on what the model SENT, before Extensions adjust it
//!   (CONTEXT.md: "the Nudge for duplicates keys on what the model sent").
//!   A successful edit_file/write_file clears both sets, because results from
//!   before a write are stale - an identical call after it (fix, then retest)
//!   is a legitimate re-run, not a loop symptom - and the fresh set restarts
//!   at the write itself. A fired finish Nudge clears the previous memory
//!   too: the finishing response's dropped tool_use blocks never produced
//!   results.
//! * **Interventions**: replaces a Tool Result at the answering moment,
//!   before the call executes ([`Duplicate::is_duplicate`] - the call never
//!   runs, and the model reads an error so it re-plans rather than trusting a
//!   stale echo).
//! * **Setpoints**: none - the one-Pass freshness window and the clearing
//!   rules are the trigger's mechanics; no threshold here has ever demanded
//!   tuning.
//!
//! Judgment call (from Step 2 of ADR-0026's strangler): the Tool Calls each
//! Pass carried are Ledger facts, and the Ledger records them - but "still
//! fresh" is this Governor's OPINION about which results the model may still
//! trust, and it is not derivable from the facts by a pure read: the memory
//! clears on a successful write and when a finish Nudge fires, and a fired
//! Nudge is a Governor's action, never a Ledger fact. So the freshness sets
//! live here as private trigger state. The wording lives in [`crate::voice`];
//! this module never authors nudge strings.

use serde_json::Value;

use crate::run::governor::ledger::WRITE_TOOLS;

/// The duplicate Governor's private trigger state, a plain value the loop
/// threads (methods mutate `&mut self` or read, no processes).
#[derive(Debug, Clone, Default)]
pub struct Duplicate {
    // The previous response's still-fresh calls, and this batch's
    // accumulating set.
    prev_calls: Vec<(String, Value)>,
    fresh: Vec<(String, Value)>,
}

impl Duplicate {
    pub fn new() -> Self {
        Duplicate::default()
    }

    /// The duplicate check: identical to a still-fresh call from the previous
    /// response?
    pub fn is_duplicate(&self, name: &str, input: &Value) -> bool {
        self.prev_calls.iter().any(|(n, i)| n == name && i == input)
    }

    /// Folds one answered Tool Call into the freshness memory: the fresh set
    /// restarts at a successful write (it invalidates every result before it
    /// in the batch, the previous Pass's memory included).
    pub fn note_answered(&mut self, name: &str, input: &Value, is_error: bool) {
        if WRITE_TOOLS.contains(&name) && !is_error {
            self.prev_calls.clear();
            self.fresh = vec![(name.to_string(), input.clone())];
        } else {
            self.push_fresh(name, input);
        }
    }

    /// Closes a tool batch: this response's still-fresh calls become the next
    /// Pass's duplicate memory.
    pub fn next_pass(&mut self) {
        self.prev_calls = std::mem::take(&mut self.fresh);
    }

    /// A finish Nudge fired (Verify-failed, Verify, or Empty alike): the
    /// finishing response's dropped tool_use blocks never produced results,
    /// so the previous memory clears.
    pub fn note_finish_nudged(&mut self) {
        self.prev_calls.clear();
    }

    fn push_fresh(&mut self, name: &str, input: &Value) {
        let pair = (name.to_string(), input.clone());
        if !self.fresh.iter().any(|p| p == &pair) {
            self.fresh.push(pair);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Folds an answered call into the freshness memory (results mirror the
    // firing site's success/error flag).
    fn note(dup: &mut Duplicate, name: &str, input: Value, is_error: bool) {
        dup.note_answered(name, &input, is_error);
    }

    #[test]
    fn a_call_becomes_a_duplicate_only_after_next_pass() {
        let input = json!({"path": "a.ex"});
        let mut dup = Duplicate::new();
        note(&mut dup, "read_file", input.clone(), false);

        assert!(!dup.is_duplicate("read_file", &input));
        dup.next_pass();
        assert!(dup.is_duplicate("read_file", &input));
    }

    #[test]
    fn a_different_name_or_input_is_not_a_duplicate() {
        let mut dup = Duplicate::new();
        note(&mut dup, "read_file", json!({"path": "a.ex"}), false);
        dup.next_pass();

        assert!(!dup.is_duplicate("read_file", &json!({"path": "b.ex"})));
        assert!(!dup.is_duplicate("grep", &json!({"path": "a.ex"})));
    }

    #[test]
    fn a_successful_write_clears_previous_memory_mid_batch() {
        let input = json!({"path": "a.ex"});
        let mut dup = Duplicate::new();
        note(&mut dup, "read_file", input.clone(), false);
        dup.next_pass();

        assert!(dup.is_duplicate("read_file", &input));

        // Fix-then-retest: results from before the write are stale.
        note(
            &mut dup,
            "write_file",
            json!({"path": "a.ex", "content": "x"}),
            false,
        );
        assert!(!dup.is_duplicate("read_file", &input));
    }

    #[test]
    fn the_fresh_set_restarts_at_a_successful_write() {
        let write_input = json!({"path": "a.ex", "content": "x"});
        let mut dup = Duplicate::new();
        note(&mut dup, "read_file", json!({"path": "a.ex"}), false);
        note(&mut dup, "write_file", write_input.clone(), false);
        dup.next_pass();

        assert!(!dup.is_duplicate("read_file", &json!({"path": "a.ex"})));
        assert!(dup.is_duplicate("write_file", &write_input));
    }

    #[test]
    fn a_failed_write_clears_nothing() {
        let input = json!({"path": "a.ex"});
        let mut dup = Duplicate::new();
        note(&mut dup, "read_file", input.clone(), false);
        dup.next_pass();
        note(
            &mut dup,
            "write_file",
            json!({"path": "a.ex", "content": "x"}),
            true,
        );

        assert!(dup.is_duplicate("read_file", &input));
    }

    #[test]
    fn a_fired_finish_nudge_clears_the_memory() {
        let input = json!({"path": "a.ex"});
        let mut dup = Duplicate::new();
        note(&mut dup, "read_file", input.clone(), false);
        dup.next_pass();
        dup.note_finish_nudged();

        assert!(!dup.is_duplicate("read_file", &input));
    }
}
