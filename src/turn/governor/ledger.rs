//! Turn Ledger — the record of facts about the running Turn (CONTEXT.md:
//! Turn Ledger; ADR-0026). Facts are written once as each thing happens: the
//! Tool Calls the Pass carried, per-Tool consecutive-failure tallies (with
//! error categories and a recency stamp for the last failure), writes and
//! whether a verification has run since, the Plan's recency (Passes and
//! successful writes since it last changed — absent while no Plan exists),
//! and the Pass position (current Pass,
//! Turn Limit). The Ledger holds facts, never opinions or setpoints —
//! Governors read it and judge; no Governor reads another Governor's state.
//!
//! Only the loop writes here, at the firing sites: `crate::turn::batch`
//! records each Tool Call outcome ([`Ledger::record_result`]) and closes the
//! batch, [`crate::turn::loop_`] records Pass advancement, Compaction, the
//! Pass's carried calls, and the truncated batch's close (ADR-0009), and
//! `crate::turn::finish` advances the Pass a finish Nudge grants.
//! Governors and the arbiter ([`super`]) READ the
//! Ledger; their thresholds (setpoints) and trigger state stay their own —
//! e.g. the annotation threshold that judges a failure tally lives with the
//! failure Governor in [`super::failure`], not here. The error-category
//! CLASSIFIER does live here ([`failure_category`]): the category is a fact
//! about the error, recorded as it happens — see that module's placement
//! judgment.

pub mod failure_category;

use serde_json::Value;

use crate::voice::{self, FailureCategory};

/// The Tools whose successful results are writes (they arm the
/// unverified-writes fact and invalidate results from before them).
pub(super) const WRITE_TOOLS: &[&str] = &["edit_file", "write_file"];

/// The outcome of one executed Tool Call, as the Turn loop observes it.
pub struct ToolResult<'a> {
    pub content: &'a str,
    pub is_error: bool,
}

// One Tool's consecutive-failure streak: how many in a row, the per-category
// tally, and the recency stamp (`last_batch`) the stuck predicate checks.
#[derive(Debug, Clone, Default)]
struct FailureStreak {
    count: u64,
    // Insertion-ordered category tallies. Order does not affect the Voice
    // output (it sorts by tally descending), so a Vec keeps parity while being
    // Hash-free.
    categories: Vec<(FailureCategory, u64)>,
    last_batch: u64,
}

impl FailureStreak {
    fn bump(&mut self, category: FailureCategory, batch: u64) {
        self.count += 1;
        self.last_batch = batch;
        match self.categories.iter_mut().find(|(c, _)| *c == category) {
            Some((_, n)) => *n += 1,
            None => self.categories.push((category, 1)),
        }
    }
}

// Plan recency: the Pass the Plan last changed on (the Turn's start Pass when
// the Plan was carried in from a previous Turn), and the successful writes
// since. Absent while no Plan exists — a Turn with no Plan has nothing to go
// stale, so the recency facts read as `None` rather than counting from Turn
// start.
#[derive(Debug, Clone)]
struct PlanRecency {
    updated_at_pass: u64,
    writes_since: u64,
}

/// The Turn Ledger. A plain value the loop owns beside the Governors' trigger
/// state: methods either write a fact once at its firing site (`&mut self`,
/// loop-only) or read one (`&self`, Governors and the arbiter).
#[derive(Debug, Clone)]
pub struct Ledger {
    // Pass position: the current Pass (1-based) and the Turn Limit (a Session
    // fact fixed at Turn start; Passes remaining is the difference).
    pass: u64,
    turn_limit: u64,
    // A Compaction has happened since the last results tail (the Anchor is
    // refreshed immediately after every Compaction — CONTEXT.md).
    compacted_since_tail: bool,
    // Closed Tool Call batches — the recency clock the failure streaks are
    // stamped against. Distinct from `pass`: finish-Nudge Passes advance the
    // Pass position but close no batch, and must not age a failure streak.
    batches: u64,
    // The Tool Calls the current Pass carried, as {name, input} pairs in
    // emission order. Each Pass's record is written once as its response
    // lands; only the running Pass's record is retained (no reader needs
    // history yet).
    pass_calls: Vec<(String, Value)>,
    failures: Vec<(String, FailureStreak)>,
    // Writes and whether a verification has run since: true from a successful
    // write until the next run_command.
    unverified_writes: bool,
    // The most recent run_command that actually RAN this Turn failed.
    command_failing: bool,
    // Recovery Turns already consumed serving the current user request — a
    // fact the Agent owns across Turns and stamps once at Turn start (the
    // Agent resets it when a genuine user prompt starts a new request).
    recoveries_used: u64,
    // Plan recency (see [`PlanRecency`]): `None` until a Plan exists.
    plan: Option<PlanRecency>,
}

impl Ledger {
    pub fn new(turn_limit: u64) -> Self {
        Ledger {
            pass: 1,
            turn_limit,
            compacted_since_tail: false,
            batches: 0,
            pass_calls: Vec::new(),
            failures: Vec::new(),
            unverified_writes: false,
            command_failing: false,
            recoveries_used: 0,
            plan: None,
        }
    }

    // ---- writes: the loop's firing sites ---------------------------------

    /// The Turn moved to its next Pass (a tool-answering Pass looping, or a
    /// finish Nudge granting one more).
    pub fn advance_pass(&mut self) {
        self.pass += 1;
    }

    /// A Compaction rewrote the Conversation (proactively at Turn start, or
    /// recovering at the budget cliff).
    pub fn note_compacted(&mut self) {
        self.compacted_since_tail = true;
    }

    /// The results tail was delivered: a Compaction noted before it has been
    /// answered (the Anchor rode the tail), so the fact clears.
    pub fn note_tail_delivered(&mut self) {
        self.compacted_since_tail = false;
    }

    /// Records the Tool Calls this Pass carried (from the response content —
    /// a truncated batch's calls count even though none executed).
    pub fn record_pass_calls(&mut self, calls: Vec<(String, Value)>) {
        self.pass_calls = calls;
    }

    /// Records one executed (or replaced) Tool Call's outcome: the
    /// consecutive-failure tally for its Tool (a success resets it), the
    /// write/verification state, and the run_command outcome. Record EVERY
    /// outcome — a replaced duplicate still counts toward the failure tally,
    /// and a duplicated write/run_command still moves the verify state.
    pub fn record_result(&mut self, name: &str, result: &ToolResult) {
        self.record_failure(name, result);
        self.record_write(name, result.is_error);
        self.record_command(name, result);
    }

    /// A Tool Call batch closed: the failure-recency clock advances.
    pub fn close_batch(&mut self) {
        self.batches += 1;
    }

    /// The Turn began with a Plan carried in from a previous Turn: the
    /// recency clock starts at Turn start — nothing changed the Plan THIS
    /// Turn yet, but a Plan exists to go stale.
    pub fn note_plan_carried(&mut self) {
        self.note_plan_updated();
    }

    /// Stamped once at Turn start: the Recovery Turns already consumed
    /// serving the current user request (an Agent-owned cross-Turn count).
    pub fn note_recoveries_used(&mut self, n: u64) {
        self.recoveries_used = n;
    }

    /// A successful plan Tool Call landed: the Plan just changed, so the
    /// recency clock and the writes-since counter reset.
    pub fn note_plan_updated(&mut self) {
        self.plan = Some(PlanRecency {
            updated_at_pass: self.pass,
            writes_since: 0,
        });
    }

    // ---- reads: Governors and the arbiter --------------------------------

    /// The current Pass (1-based).
    pub fn pass(&self) -> u64 {
        self.pass
    }

    /// The Turn Limit, in Passes (CONTEXT.md).
    pub fn turn_limit(&self) -> u64 {
        self.turn_limit
    }

    /// Has a Compaction happened since the last results tail?
    pub fn just_compacted(&self) -> bool {
        self.compacted_since_tail
    }

    /// The Tool Calls the current Pass carried, in emission order.
    pub fn pass_calls(&self) -> &[(String, Value)] {
        &self.pass_calls
    }

    /// Are there successful writes with no run_command since?
    pub fn unverified_writes(&self) -> bool {
        self.unverified_writes
    }

    /// Did the most recent run_command this Turn fail?
    pub fn command_failing(&self) -> bool {
        self.command_failing
    }

    /// Recovery Turns already consumed serving the current user request.
    pub fn recoveries_used(&self) -> u64 {
        self.recoveries_used
    }

    /// Passes since the Plan last changed (since Turn start for a Plan
    /// carried in from a previous Turn). `None` while no Plan exists — a
    /// missing Plan cannot be stale.
    pub fn passes_since_plan_update(&self) -> Option<u64> {
        self.plan.as_ref().map(|p| self.pass - p.updated_at_pass)
    }

    /// Successful writes since the Plan last changed. `None` while no Plan
    /// exists.
    pub fn writes_since_plan_update(&self) -> Option<u64> {
        self.plan.as_ref().map(|p| p.writes_since)
    }

    /// One Tool's consecutive-failure streak: the count and the per-category
    /// tallies. `None` when the Tool has no live streak.
    pub fn failure_streak(&self, name: &str) -> Option<(u64, &[(FailureCategory, u64)])> {
        self.failures
            .iter()
            .find(|(tool, _)| tool == name)
            .map(|(_, streak)| (streak.count, streak.categories.as_slice()))
    }

    /// Every live failure streak as (count, batches since its last failure) —
    /// the facts the stuck predicate judges against its setpoints.
    pub fn failure_tallies(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.failures
            .iter()
            .map(|(_, streak)| (streak.count, self.batches - streak.last_batch))
    }

    // A success for the Tool resets its streak; an error bumps it, stamped
    // with the current batch and its classified category (the category is a
    // fact about the error, recorded as it happens).
    fn record_failure(&mut self, name: &str, result: &ToolResult) {
        if !result.is_error {
            self.failures.retain(|(tool, _)| tool != name);
            return;
        }

        let category = failure_category::classify(result.content);
        let batch = self.batches;
        let streak = match self.failures.iter_mut().find(|(tool, _)| tool == name) {
            Some((_, s)) => s,
            None => {
                self.failures
                    .push((name.to_string(), FailureStreak::default()));
                &mut self.failures.last_mut().unwrap().1
            }
        };
        streak.bump(category, batch);
    }

    // A run_command verifies (approved, denied, or failed alike); a successful
    // write leaves the Turn unverified until the next one.
    fn record_write(&mut self, name: &str, is_error: bool) {
        if name == "run_command" {
            self.unverified_writes = false;
        } else if WRITE_TOOLS.contains(&name) && !is_error {
            self.unverified_writes = true;
            if let Some(plan) = &mut self.plan {
                plan.writes_since += 1;
            }
        }
    }

    // The most recent run_command this Turn: a failing one sets the fact, a
    // passing one clears it. Exactly one exemption: an Approval-denied
    // command never ran, so it leaves the fact untouched. A plugin-halted
    // run_command also never ran, but its halt DOES set the fact — the halt
    // reads as a failed run and the verify-failed gate follows suit. Only
    // run_command outcomes touch this.
    fn record_command(&mut self, name: &str, result: &ToolResult) {
        if name == "run_command" {
            if result.content == voice::command_denied() {
                return;
            }
            self.command_failing = result.is_error;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ----- pass position -----

    #[test]
    fn a_turn_starts_on_pass_one_and_advances() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.pass(), 1);
        assert_eq!(ledger.turn_limit(), 25);

        ledger.advance_pass();
        assert_eq!(ledger.pass(), 2);
    }

    // ----- consecutive-failure tallies -----

    #[test]
    fn consecutive_failures_tally_per_tool() {
        let mut ledger = Ledger::new(25);
        ledger.record_result("read_file", &err());
        ledger.record_result("read_file", &err());
        ledger.record_result("grep", &err());

        assert_eq!(ledger.failure_streak("read_file").map(|(n, _)| n), Some(2));
        assert_eq!(ledger.failure_streak("grep").map(|(n, _)| n), Some(1));
        assert_eq!(ledger.failure_streak("list_files"), None);
    }

    #[test]
    fn a_success_for_the_tool_resets_its_streak() {
        let mut ledger = Ledger::new(25);
        ledger.record_result("read_file", &err());
        ledger.record_result("read_file", &err());
        ledger.record_result("read_file", &ok());

        assert_eq!(ledger.failure_streak("read_file"), None);
    }

    #[test]
    fn failures_carry_their_error_categories() {
        let mut ledger = Ledger::new(25);
        ledger.record_result("read_file", &err());
        ledger.record_result(
            "read_file",
            &ToolResult {
                content: "enoent: no such file",
                is_error: true,
            },
        );

        let (count, categories) = ledger.failure_streak("read_file").unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            categories,
            &[(FailureCategory::Unknown, 1), (FailureCategory::Enoent, 1)]
        );
    }

    #[test]
    fn tallies_stamp_their_recency_against_the_batch_clock() {
        let mut ledger = Ledger::new(25);
        ledger.record_result("read_file", &err());
        ledger.close_batch();
        ledger.close_batch();

        let tallies: Vec<(u64, u64)> = ledger.failure_tallies().collect();
        assert_eq!(tallies, vec![(1, 2)]);
    }

    // ----- writes and verification -----

    #[test]
    fn a_successful_write_arms_unverified_a_failed_one_doesnt() {
        let mut a = Ledger::new(25);
        a.record_result("write_file", &ok());
        assert!(a.unverified_writes());

        let mut b = Ledger::new(25);
        b.record_result("edit_file", &ok());
        assert!(b.unverified_writes());

        let mut c = Ledger::new(25);
        c.record_result("write_file", &err());
        assert!(!c.unverified_writes());
    }

    #[test]
    fn any_run_command_verifies() {
        let mut base = Ledger::new(25);
        base.record_result("edit_file", &ok());

        let mut a = base.clone();
        a.record_result("run_command", &ok());
        assert!(!a.unverified_writes());

        let mut b = base.clone();
        b.record_result("run_command", &err());
        assert!(!b.unverified_writes());
    }

    #[test]
    fn a_write_after_the_run_command_is_unverified_again() {
        let mut ledger = Ledger::new(25);
        ledger.record_result("edit_file", &ok());
        ledger.record_result("run_command", &ok());
        ledger.record_result("edit_file", &ok());

        assert!(ledger.unverified_writes());
    }

    // ----- the most recent run_command -----

    #[test]
    fn a_failing_run_command_sets_the_fact_a_passing_one_clears_it() {
        let mut ledger = Ledger::new(25);
        ledger.record_result("run_command", &err());
        assert!(ledger.command_failing());

        ledger.record_result("run_command", &ok());
        assert!(!ledger.command_failing());
    }

    #[test]
    fn a_denied_command_never_ran_so_the_fact_stands() {
        let mut ledger = Ledger::new(25);
        ledger.record_result("run_command", &err());
        ledger.record_result(
            "run_command",
            &ToolResult {
                content: crate::voice::command_denied(),
                is_error: true,
            },
        );

        assert!(ledger.command_failing());
    }

    // ----- recoveries used -----

    #[test]
    fn recoveries_used_starts_at_zero_and_holds_the_stamped_count() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.recoveries_used(), 0);

        ledger.note_recoveries_used(2);
        assert_eq!(ledger.recoveries_used(), 2);
    }

    // ----- plan recency -----

    #[test]
    fn plan_recency_is_absent_while_no_plan_exists() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.passes_since_plan_update(), None);
        assert_eq!(ledger.writes_since_plan_update(), None);

        // Writes before any Plan exists do not start a counter.
        ledger.record_result("edit_file", &ok());
        ledger.advance_pass();
        assert_eq!(ledger.passes_since_plan_update(), None);
        assert_eq!(ledger.writes_since_plan_update(), None);
    }

    #[test]
    fn a_carried_plan_counts_passes_since_turn_start() {
        let mut ledger = Ledger::new(25);
        ledger.note_plan_carried();
        assert_eq!(ledger.passes_since_plan_update(), Some(0));

        ledger.advance_pass();
        ledger.advance_pass();
        assert_eq!(ledger.passes_since_plan_update(), Some(2));
    }

    #[test]
    fn a_plan_update_resets_both_counters_to_its_pass() {
        let mut ledger = Ledger::new(25);
        ledger.note_plan_carried();
        ledger.record_result("edit_file", &ok());
        ledger.advance_pass();
        ledger.advance_pass();
        assert_eq!(ledger.passes_since_plan_update(), Some(2));
        assert_eq!(ledger.writes_since_plan_update(), Some(1));

        ledger.note_plan_updated();
        assert_eq!(ledger.passes_since_plan_update(), Some(0));
        assert_eq!(ledger.writes_since_plan_update(), Some(0));

        ledger.advance_pass();
        assert_eq!(ledger.passes_since_plan_update(), Some(1));
    }

    #[test]
    fn only_successful_writes_count_since_the_plan_update() {
        let mut ledger = Ledger::new(25);
        ledger.note_plan_updated();

        ledger.record_result("edit_file", &ok());
        ledger.record_result("write_file", &ok());
        ledger.record_result("edit_file", &err());
        ledger.record_result("run_command", &ok());
        ledger.record_result("read_file", &ok());

        assert_eq!(ledger.writes_since_plan_update(), Some(2));
    }

    // ----- compaction -----

    #[test]
    fn a_compaction_stands_until_the_next_results_tail() {
        let mut ledger = Ledger::new(25);
        assert!(!ledger.just_compacted());

        ledger.note_compacted();
        assert!(ledger.just_compacted());

        ledger.note_tail_delivered();
        assert!(!ledger.just_compacted());
    }
}
