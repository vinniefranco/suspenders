//! Run Ledger - the record of facts about the running Run (CONTEXT.md:
//! Run Ledger; ADR-0026). Facts are written once as each thing happens: the
//! Tool Calls the Pass carried, per-Tool consecutive-failure tallies (with
//! error categories and a recency stamp for the last failure), writes and
//! whether a verification has run since, the run_command outcomes (the most
//! recent run, and the most recent outcome per distinct command string),
//! the Plan's recency (Passes and
//! successful writes since it last changed - absent while no Plan exists),
//! and the Pass position (current Pass,
//! Run Limit). The Ledger holds facts, never opinions or setpoints -
//! Governors read it and judge; no Governor reads another Governor's state.
//!
//! Only the loop writes here, at the firing sites: `crate::run::batch`
//! records each Tool Call's Answer ([`Ledger::record`]) and closes the
//! batch, [`crate::run::loop_`] records Pass advancement, Compaction, the
//! Pass's carried calls, and the truncated batch's close (ADR-0009), and
//! `crate::run::finish` advances the Pass a finish Nudge grants.
//! Governors and the arbiter ([`super`]) READ the
//! Ledger; their thresholds (setpoints) and trigger state stay their own -
//! e.g. the annotation threshold that judges a failure tally lives with the
//! failure Governor in [`super::failure`], not here. The error-category
//! CLASSIFIER does live here ([`failure_category`]): the category is a fact
//! about the error, recorded as it happens - see that module's placement
//! judgment.

pub mod failure_category;

use serde_json::Value;

use crate::plan::PlanProgress;
use crate::voice::FailureCategory;

/// The Tools whose successful results are writes (they arm the
/// unverified-writes fact and invalidate results from before them).
pub(super) const WRITE_TOOLS: &[&str] = &["edit_file", "write_file"];

/// The outcome of one executed Tool Call, as the Run loop observes it.
pub struct ToolResult<'a> {
    pub content: &'a str,
    pub is_error: bool,
}

/// The batch's typed fact of whether an answered Tool Call ran (CONTEXT.md:
/// Answer): the batch states the fact, the Ledger owns what each fact moves
/// (the Run Ledger holds facts, never opinions).
#[derive(Debug, Clone, Copy)]
pub enum CallOutcome {
    /// The call ran.
    Ran,
    /// The Approval gate denied the command; it never ran (ADR-0005).
    Denied,
    /// The Pass did not offer the Tool the call named; it never ran
    /// (ADR-0035).
    Refused,
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

// Plan recency: the Pass the Plan last changed on (the Run's start Pass when
// the Plan was carried in from a previous Run), and the successful writes
// since. Absent while no Plan exists - a Run with no Plan has nothing to go
// stale, so the recency facts read as `None` rather than counting from Run
// start.
#[derive(Debug, Clone)]
struct PlanRecency {
    updated_at_pass: u64,
    writes_since: u64,
}

/// The Run Ledger. A plain value the loop owns beside the Governors' trigger
/// state: methods either write a fact once at its firing site (`&mut self`,
/// loop-only) or read one (`&self`, Governors and the arbiter).
#[derive(Debug, Clone)]
pub struct Ledger {
    // Pass position: the current Pass (1-based) and the Run Limit (a Session
    // fact fixed at Run start; Passes remaining is the difference).
    pass: u64,
    run_limit: u64,
    // A Compaction has happened since the last results tail (the Anchor is
    // refreshed immediately after every Compaction - CONTEXT.md).
    compacted_since_tail: bool,
    // Closed Tool Call batches - the recency clock the failure streaks are
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
    // A successful write landed this Run. Monotonic - set once and NEVER
    // cleared, unlike `unverified_writes` (which a run_command clears): it
    // records that implementation happened at all, the evidence the
    // dangling-failure recovery arm requires so a read-only Run that merely
    // ran a failing command opens no Recovery Run (ADR-0028 addendum
    // 2026-07-14).
    wrote_this_run: bool,
    // The most recent run_command that actually RAN this Run failed.
    command_failing: bool,
    // The most recent outcome per distinct run_command command string that
    // actually RAN this Run: `true` records a failure not yet followed by a
    // rerun of the SAME string.
    command_outcomes: Vec<(String, bool)>,
    // Recovery Runs already consumed serving the current user request - a
    // fact the Agent owns across Runs and stamps once at Run start (the
    // Agent resets it when a genuine user prompt starts a new request).
    recoveries_used: u64,
    // Malformed-tool-call re-draws consumed this Run (ADR-0030): the loop
    // increments this at the retry firing site, and the re-draw decision
    // reads it against the `malformed_retry_budget` Setpoint. Transient - a
    // resumed Run settles as failed regardless, so it is never restored.
    retries_used: u64,
    // Plan recency (see [`PlanRecency`]): `None` until a Plan exists.
    plan: Option<PlanRecency>,
    // The Plan's checkbox completeness (ADR-0043: the Open Plan), the fact the
    // Endgame's Open-Plan recovery arm reads. `None` while no Plan exists. The
    // Ledger STORES this computed fact; it never parses markdown - the caller
    // (batch's plan store, the loop's carried-plan path) reads
    // [`crate::plan::Plan::progress`] and passes the answer in, keeping the
    // facts-not-opinions invariant.
    plan_open: Option<PlanProgress>,
    // The made-progress baseline (ADR-0043): the Plan's checked-box count at
    // Run start (a carried Plan's), and the count now. `plan_advanced` is
    // `checked_now > checked_at_run_start` - the guard that a green-but-open
    // Run actually checked off a step this Run, mirroring the
    // `wrote_this_run` addendum: a Run showing no progress is one a
    // continuation would simply repeat.
    checked_at_run_start: u64,
    checked_now: u64,
    // Open-Plan continuations already consumed serving the current user
    // request - a fact the Agent owns across Runs and stamps once at Run
    // start, exactly like `recoveries_used`, but counted against the
    // `advance_limit` Setpoint (ADR-0043: a separate, larger budget than the
    // broken-state `recovery_limit`).
    advances_used: u64,
}

impl Ledger {
    pub fn new(run_limit: u64) -> Self {
        Ledger {
            pass: 1,
            run_limit,
            compacted_since_tail: false,
            batches: 0,
            pass_calls: Vec::new(),
            failures: Vec::new(),
            unverified_writes: false,
            wrote_this_run: false,
            command_failing: false,
            command_outcomes: Vec::new(),
            recoveries_used: 0,
            retries_used: 0,
            plan: None,
            plan_open: None,
            checked_at_run_start: 0,
            checked_now: 0,
            advances_used: 0,
        }
    }

    // ---- writes: the loop's firing sites ---------------------------------

    /// The Run moved to its next Pass (a tool-answering Pass looping, or a
    /// finish Nudge granting one more).
    pub fn advance_pass(&mut self) {
        self.pass += 1;
    }

    /// A Compaction rewrote the Conversation (proactively at Run start, or
    /// recovering at the budget cliff).
    pub fn note_compacted(&mut self) {
        self.compacted_since_tail = true;
    }

    /// The results tail was delivered: a Compaction noted before it has been
    /// answered (the Anchor rode the tail), so the fact clears.
    pub fn note_tail_delivered(&mut self) {
        self.compacted_since_tail = false;
    }

    /// Records the Tool Calls this Pass carried (from the response content -
    /// a truncated batch's calls count even though none executed).
    pub fn record_pass_calls(&mut self, calls: Vec<(String, Value)>) {
        self.pass_calls = calls;
    }

    /// Records how the batch answered one Tool Call (CONTEXT.md: Answer):
    /// the batch states the typed [`CallOutcome`] fact, this method owns
    /// what each fact moves. The batch knows structurally which fact it
    /// answered, so the Ledger never sniffs Voice wording (facts, never
    /// opinions - and the Voice is tuned per model, so wording must never
    /// be load-bearing here).
    ///
    /// Record EVERY `Ran` outcome - a replaced duplicate still counts toward
    /// the failure tally, and a duplicated write/run_command still moves the
    /// verify state (ADR-0026).
    pub fn record(&mut self, name: &str, input: &Value, result: &ToolResult, outcome: CallOutcome) {
        match outcome {
            // An executed (or replaced) call moves every fact: the
            // consecutive-failure tally for its Tool (a success resets it),
            // the write/verification state, and the run_command outcomes -
            // the last run and the per-command-string record.
            CallOutcome::Ran => {
                self.record_failure(name, result);
                self.record_write(name, result.is_error);
                self.record_command(name, input, result);
            }
            // An Approval denial never ran, so the run_command outcomes
            // stand - but a denial still answers the verify demand (a
            // run_command verifies, denied alike, ADR-0005), so the write
            // state moves like any run_command outcome, and the denial
            // counts toward the failure tally.
            CallOutcome::Denied => {
                self.record_failure(name, result);
                self.record_write(name, result.is_error);
            }
            // An offered-set refusal never ran, so only the
            // consecutive-failure tally moves - the write/verify facts and
            // the run_command outcomes stand (ADR-0035).
            CallOutcome::Refused => self.record_failure(name, result),
        }
    }

    /// A Tool Call batch closed: the failure-recency clock advances.
    pub fn close_batch(&mut self) {
        self.batches += 1;
    }

    /// The Run began with a Plan carried in from a previous Run: the
    /// recency clock starts at Run start - nothing changed the Plan THIS
    /// Run yet, but a Plan exists to go stale. The carried Plan's checkbox
    /// facts (ADR-0043) also seed the made-progress baseline: `progress` is
    /// the Open-Plan fact, `checked` is BOTH the Run-start baseline and the
    /// current count (no step has been checked off this Run yet). The caller
    /// reads these off the carried [`crate::plan::Plan`] - the Ledger never
    /// parses markdown.
    pub fn note_plan_carried(&mut self, progress: PlanProgress, checked: u64) {
        self.note_plan_updated(progress, checked);
        self.checked_at_run_start = checked;
    }

    /// Stamped once at Run start: the Recovery Runs already consumed
    /// serving the current user request (an Agent-owned cross-Run count).
    pub fn note_recoveries_used(&mut self, n: u64) {
        self.recoveries_used = n;
    }

    /// Stamped once at Run start: the Open-Plan continuations already consumed
    /// serving the current user request (an Agent-owned cross-Run count,
    /// ADR-0043), read against the `advance_limit` Setpoint.
    pub fn note_advances_used(&mut self, n: u64) {
        self.advances_used = n;
    }

    /// A malformed-tool-call generation was re-drawn in-band (ADR-0030): one
    /// more of the per-Run retry budget is spent.
    pub fn note_retry(&mut self) {
        self.retries_used += 1;
    }

    /// A successful plan Tool Call landed: the Plan just changed, so the
    /// recency clock and the writes-since counter reset. The Plan's checkbox
    /// facts (ADR-0043) update too: `progress` is the Open-Plan fact and
    /// `checked` is the current count (the made-progress baseline is set only
    /// at [`note_plan_carried`], never here - the baseline is Run-start, not
    /// this-update). The caller reads both off the updated
    /// [`crate::plan::Plan`]; the Ledger never parses markdown.
    pub fn note_plan_updated(&mut self, progress: PlanProgress, checked: u64) {
        self.plan = Some(PlanRecency {
            updated_at_pass: self.pass,
            writes_since: 0,
        });
        self.plan_open = Some(progress);
        self.checked_now = checked;
    }

    // ---- reads: Governors and the arbiter --------------------------------

    /// The current Pass (1-based).
    pub fn pass(&self) -> u64 {
        self.pass
    }

    /// The Run Limit, in Passes (CONTEXT.md).
    pub fn run_limit(&self) -> u64 {
        self.run_limit
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

    /// Did any successful write land this Run? Monotonic - once true it
    /// stays true, even after a later run_command clears `unverified_writes`.
    /// The evidence the dangling-failure recovery arm requires: a failing
    /// command during pure exploration is not unfinished implementation
    /// (ADR-0028 addendum 2026-07-14).
    pub fn wrote_this_run(&self) -> bool {
        self.wrote_this_run
    }

    /// Did the most recent run_command this Run fail?
    pub fn command_failing(&self) -> bool {
        self.command_failing
    }

    /// Is there a command string whose most recent run this Run failed? A
    /// passing run clears only its own command string.
    pub fn dangling_failure(&self) -> bool {
        self.command_outcomes.iter().any(|(_, failing)| *failing)
    }

    /// The command string of the Dangling Failure: the most-recently-recorded
    /// `command_outcomes` entry whose most recent run still failed. `None`
    /// when nothing dangles. When several dangle, the last one recorded wins -
    /// the command the recovery prompt names, so its own failing result seeds
    /// the Handoff (ADR-0028 addendum 2026-07-14), never merely the last
    /// command run.
    pub fn dangling_command(&self) -> Option<&str> {
        self.command_outcomes
            .iter()
            .rev()
            .find(|(_, failing)| *failing)
            .map(|(command, _)| command.as_str())
    }

    /// Recovery Runs already consumed serving the current user request.
    pub fn recoveries_used(&self) -> u64 {
        self.recoveries_used
    }

    /// The Plan's checkbox completeness (ADR-0043: the Open Plan), or `None`
    /// while no Plan exists. The Endgame's Open-Plan recovery arm reads it; a
    /// Plan showing an unchecked `[ ]` step is [`PlanProgress::Incomplete`].
    pub fn plan_open(&self) -> Option<PlanProgress> {
        self.plan_open
    }

    /// Did the Run check off a Plan step this Run (ADR-0043)? The made-progress
    /// guard: `checked_now > checked_at_run_start`. False on a Plan set fresh
    /// this Run (baseline 0, but the arm requires a CARRIED Incomplete Plan
    /// advanced further) and false on a Run that edited but checked no new box -
    /// a continuation would simply repeat it.
    pub fn plan_advanced(&self) -> bool {
        self.checked_now > self.checked_at_run_start
    }

    /// Open-Plan continuations already consumed serving the current user
    /// request (ADR-0043), read against the `advance_limit` Setpoint.
    pub fn advances_used(&self) -> u64 {
        self.advances_used
    }

    /// Malformed-tool-call re-draws already spent this Run (ADR-0030), read
    /// against the `malformed_retry_budget` Setpoint by the loop's re-draw
    /// decision.
    pub fn retries_used(&self) -> u64 {
        self.retries_used
    }

    /// Passes since the Plan last changed (since Run start for a Plan
    /// carried in from a previous Run). `None` while no Plan exists - a
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

    /// Every live failure streak as (count, batches since its last failure) -
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
    // write leaves the Run unverified until the next one.
    fn record_write(&mut self, name: &str, is_error: bool) {
        if name == "run_command" {
            self.unverified_writes = false;
        } else if WRITE_TOOLS.contains(&name) && !is_error {
            self.unverified_writes = true;
            self.wrote_this_run = true;
            if let Some(plan) = &mut self.plan {
                plan.writes_since += 1;
            }
        }
    }

    // The most recent run_command this Run: a failing one sets the fact, a
    // passing one clears it. The per-command-string record moves the same
    // way, keyed by the call's command string - a passing run clears only
    // its own entry. Calls that never ran never reach here: [`Ledger::record`]
    // routes a Denied or Refused Answer past this method (ADR-0005,
    // ADR-0035). A extension-halted run_command also never
    // ran, but its halt DOES set the facts - the halt reads as a failed run
    // and the verify-failed gate follows suit. Only run_command outcomes
    // touch this.
    fn record_command(&mut self, name: &str, input: &Value, result: &ToolResult) {
        if name != "run_command" {
            return;
        }
        self.command_failing = result.is_error;

        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
        match self.command_outcomes.iter_mut().find(|(c, _)| c == command) {
            Some((_, failing)) => *failing = result.is_error,
            None => self
                .command_outcomes
                .push((command.to_string(), result.is_error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // ----- pass position -----

    #[test]
    fn a_run_starts_on_pass_one_and_advances() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.pass(), 1);
        assert_eq!(ledger.run_limit(), 25);

        ledger.advance_pass();
        assert_eq!(ledger.pass(), 2);
    }

    // ----- consecutive-failure tallies -----

    #[test]
    fn consecutive_failures_tally_per_tool() {
        let mut ledger = Ledger::new(25);
        ledger.record("read_file", &json!({}), &err(), CallOutcome::Ran);
        ledger.record("read_file", &json!({}), &err(), CallOutcome::Ran);
        ledger.record("grep", &json!({}), &err(), CallOutcome::Ran);

        assert_eq!(ledger.failure_streak("read_file").map(|(n, _)| n), Some(2));
        assert_eq!(ledger.failure_streak("grep").map(|(n, _)| n), Some(1));
        assert_eq!(ledger.failure_streak("list_files"), None);
    }

    #[test]
    fn a_success_for_the_tool_resets_its_streak() {
        let mut ledger = Ledger::new(25);
        ledger.record("read_file", &json!({}), &err(), CallOutcome::Ran);
        ledger.record("read_file", &json!({}), &err(), CallOutcome::Ran);
        ledger.record("read_file", &json!({}), &ok(), CallOutcome::Ran);

        assert_eq!(ledger.failure_streak("read_file"), None);
    }

    #[test]
    fn failures_carry_their_error_categories() {
        let mut ledger = Ledger::new(25);
        ledger.record("read_file", &json!({}), &err(), CallOutcome::Ran);
        ledger.record(
            "read_file",
            &json!({}),
            &ToolResult {
                content: "enoent: no such file",
                is_error: true,
            },
            CallOutcome::Ran,
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
        ledger.record("read_file", &json!({}), &err(), CallOutcome::Ran);
        ledger.close_batch();
        ledger.close_batch();

        let tallies: Vec<(u64, u64)> = ledger.failure_tallies().collect();
        assert_eq!(tallies, vec![(1, 2)]);
    }

    // ----- writes and verification -----

    #[test]
    fn a_successful_write_arms_unverified_a_failed_one_doesnt() {
        let mut a = Ledger::new(25);
        a.record("write_file", &json!({}), &ok(), CallOutcome::Ran);
        assert!(a.unverified_writes());

        let mut b = Ledger::new(25);
        b.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);
        assert!(b.unverified_writes());

        let mut c = Ledger::new(25);
        c.record("write_file", &json!({}), &err(), CallOutcome::Ran);
        assert!(!c.unverified_writes());
    }

    #[test]
    fn any_run_command_verifies() {
        let mut base = Ledger::new(25);
        base.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);

        let mut a = base.clone();
        a.record("run_command", &json!({}), &ok(), CallOutcome::Ran);
        assert!(!a.unverified_writes());

        let mut b = base.clone();
        b.record("run_command", &json!({}), &err(), CallOutcome::Ran);
        assert!(!b.unverified_writes());
    }

    #[test]
    fn a_write_after_the_run_command_is_unverified_again() {
        let mut ledger = Ledger::new(25);
        ledger.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);
        ledger.record("run_command", &json!({}), &ok(), CallOutcome::Ran);
        ledger.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);

        assert!(ledger.unverified_writes());
    }

    #[test]
    fn a_successful_write_marks_the_run_and_the_mark_survives_a_run_command() {
        let mut ledger = Ledger::new(25);
        assert!(!ledger.wrote_this_run());

        ledger.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);
        assert!(ledger.wrote_this_run());

        // Monotonic: the run_command clears unverified_writes but never the
        // wrote-this-run mark.
        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &ok(),
            CallOutcome::Ran,
        );
        assert!(!ledger.unverified_writes());
        assert!(ledger.wrote_this_run());
    }

    #[test]
    fn a_failed_write_does_not_mark_the_run() {
        let mut ledger = Ledger::new(25);
        ledger.record("write_file", &json!({}), &err(), CallOutcome::Ran);
        assert!(!ledger.wrote_this_run());
    }

    #[test]
    fn a_read_only_run_never_marks_the_run() {
        let mut ledger = Ledger::new(25);
        ledger.record("read_file", &json!({}), &ok(), CallOutcome::Ran);
        ledger.record("grep", &json!({}), &ok(), CallOutcome::Ran);
        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &err(),
            CallOutcome::Ran,
        );
        assert!(!ledger.wrote_this_run());
    }

    // ----- the most recent run_command -----

    #[test]
    fn a_failing_run_command_sets_the_fact_a_passing_one_clears_it() {
        let mut ledger = Ledger::new(25);
        ledger.record("run_command", &json!({}), &err(), CallOutcome::Ran);
        assert!(ledger.command_failing());

        ledger.record("run_command", &json!({}), &ok(), CallOutcome::Ran);
        assert!(!ledger.command_failing());
    }

    #[test]
    fn a_denied_command_never_ran_so_the_fact_stands() {
        let mut ledger = Ledger::new(25);
        ledger.record("run_command", &json!({}), &err(), CallOutcome::Ran);
        ledger.record(
            "run_command",
            &json!({}),
            &ToolResult {
                content: crate::voice::command_denied(),
                is_error: true,
            },
            CallOutcome::Denied,
        );

        assert!(ledger.command_failing());
    }

    #[test]
    fn a_denied_command_still_answers_the_verify_demand() {
        let mut ledger = Ledger::new(25);
        ledger.record(
            "write_file",
            &json!({"path": "a.txt"}),
            &ok(),
            CallOutcome::Ran,
        );
        assert!(ledger.unverified_writes());
        ledger.record("run_command", &json!({}), &err(), CallOutcome::Denied);

        assert!(!ledger.unverified_writes());
    }

    #[test]
    fn a_refused_command_never_ran_so_the_fact_stands() {
        let mut ledger = Ledger::new(25);
        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &err(),
            CallOutcome::Ran,
        );
        ledger.record("run_command", &json!({}), &err(), CallOutcome::Refused);

        // The phantom run clears nothing and fails nothing: the genuine
        // failure still dangles under ITS command string.
        assert!(ledger.command_failing());
        assert_eq!(ledger.dangling_command(), Some("cargo test"));
    }

    #[test]
    fn a_refused_command_never_ran_so_unverified_writes_stand() {
        let mut ledger = Ledger::new(25);
        ledger.record(
            "write_file",
            &json!({"path": "a.txt"}),
            &ok(),
            CallOutcome::Ran,
        );
        assert!(ledger.unverified_writes());
        ledger.record("run_command", &json!({}), &err(), CallOutcome::Refused);

        assert!(ledger.unverified_writes());
    }

    // ----- dangling failures per command string -----

    #[test]
    fn a_green_filtered_rerun_leaves_the_failure_dangling() {
        let mut ledger = Ledger::new(25);
        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &err(),
            CallOutcome::Ran,
        );
        ledger.record(
            "run_command",
            &json!({"command": "cargo test one_test_name"}),
            &ok(),
            CallOutcome::Ran,
        );

        assert!(!ledger.command_failing());
        assert!(ledger.dangling_failure());
    }

    #[test]
    fn a_green_rerun_of_the_same_command_clears_its_failure() {
        let mut ledger = Ledger::new(25);
        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &err(),
            CallOutcome::Ran,
        );
        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &ok(),
            CallOutcome::Ran,
        );

        assert!(!ledger.dangling_failure());
    }

    #[test]
    fn a_run_without_commands_dangles_nothing() {
        let mut ledger = Ledger::new(25);
        assert!(!ledger.dangling_failure());

        // Only run_command outcomes touch the record.
        ledger.record(
            "edit_file",
            &json!({"path": "a.ex"}),
            &err(),
            CallOutcome::Ran,
        );
        assert!(!ledger.dangling_failure());
    }

    #[test]
    fn the_dangling_command_is_the_failing_command_string() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.dangling_command(), None);

        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &err(),
            CallOutcome::Ran,
        );
        assert_eq!(ledger.dangling_command(), Some("cargo test"));

        // A green rerun of the same string clears it - nothing dangles.
        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &ok(),
            CallOutcome::Ran,
        );
        assert_eq!(ledger.dangling_command(), None);
    }

    #[test]
    fn the_dangling_command_is_the_last_recorded_that_still_fails() {
        // cmd A red, cmd B red, B green → A still dangles. Two distinct strings
        // failed; B cleared its own entry, so the newest-recorded entry that
        // still fails is A.
        let mut ledger = Ledger::new(25);
        ledger.record(
            "run_command",
            &json!({"command": "cmd A"}),
            &err(),
            CallOutcome::Ran,
        );
        ledger.record(
            "run_command",
            &json!({"command": "cmd B"}),
            &err(),
            CallOutcome::Ran,
        );
        ledger.record(
            "run_command",
            &json!({"command": "cmd B"}),
            &ok(),
            CallOutcome::Ran,
        );

        assert_eq!(ledger.dangling_command(), Some("cmd A"));

        // Now A red, B red (both dangle): the most-recently-recorded wins.
        let mut two = Ledger::new(25);
        two.record(
            "run_command",
            &json!({"command": "cmd A"}),
            &err(),
            CallOutcome::Ran,
        );
        two.record(
            "run_command",
            &json!({"command": "cmd B"}),
            &err(),
            CallOutcome::Ran,
        );
        assert_eq!(two.dangling_command(), Some("cmd B"));
    }

    // ----- recoveries used -----

    #[test]
    fn recoveries_used_starts_at_zero_and_holds_the_stamped_count() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.recoveries_used(), 0);

        ledger.note_recoveries_used(2);
        assert_eq!(ledger.recoveries_used(), 2);
    }

    // ----- the Open Plan facts (ADR-0043) -----

    #[test]
    fn plan_open_and_the_baseline_are_absent_and_zero_until_a_plan_exists() {
        let ledger = Ledger::new(25);
        assert_eq!(ledger.plan_open(), None);
        assert!(!ledger.plan_advanced());
        assert_eq!(ledger.advances_used(), 0);
    }

    #[test]
    fn note_plan_carried_sets_plan_open_and_the_made_progress_baseline() {
        // A carried Incomplete Plan with 2 boxes already checked: the baseline
        // and the current count both start at 2, so no progress shows yet.
        let mut ledger = Ledger::new(25);
        ledger.note_plan_carried(PlanProgress::Incomplete, 2);
        assert_eq!(ledger.plan_open(), Some(PlanProgress::Incomplete));
        assert!(!ledger.plan_advanced());
    }

    #[test]
    fn note_plan_updated_moves_the_current_count_but_not_the_baseline() {
        // Carried at 2, then a plan Tool Call this Run checks a third box:
        // checked_now rises to 3, the Run-start baseline stays 2, so the Run
        // has advanced.
        let mut ledger = Ledger::new(25);
        ledger.note_plan_carried(PlanProgress::Incomplete, 2);
        ledger.note_plan_updated(PlanProgress::Incomplete, 3);
        assert_eq!(ledger.plan_open(), Some(PlanProgress::Incomplete));
        assert!(ledger.plan_advanced());
    }

    #[test]
    fn plan_advanced_is_false_when_the_checked_count_did_not_rise() {
        // Carried at 2, an update that unchecked a box (or held): no progress.
        let mut ledger = Ledger::new(25);
        ledger.note_plan_carried(PlanProgress::Incomplete, 2);
        ledger.note_plan_updated(PlanProgress::Incomplete, 2);
        assert!(!ledger.plan_advanced());
    }

    #[test]
    fn advances_used_starts_at_zero_and_holds_the_stamped_count() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.advances_used(), 0);

        ledger.note_advances_used(3);
        assert_eq!(ledger.advances_used(), 3);
    }

    // ----- retries used (ADR-0030) -----

    #[test]
    fn retries_used_starts_at_zero_and_note_retry_increments() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.retries_used(), 0);

        ledger.note_retry();
        assert_eq!(ledger.retries_used(), 1);

        ledger.note_retry();
        assert_eq!(ledger.retries_used(), 2);
    }

    // ----- plan recency -----

    #[test]
    fn plan_recency_is_absent_while_no_plan_exists() {
        let mut ledger = Ledger::new(25);
        assert_eq!(ledger.passes_since_plan_update(), None);
        assert_eq!(ledger.writes_since_plan_update(), None);

        // Writes before any Plan exists do not start a counter.
        ledger.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);
        ledger.advance_pass();
        assert_eq!(ledger.passes_since_plan_update(), None);
        assert_eq!(ledger.writes_since_plan_update(), None);
    }

    #[test]
    fn a_carried_plan_counts_passes_since_run_start() {
        let mut ledger = Ledger::new(25);
        ledger.note_plan_carried(PlanProgress::NoCheckboxes, 0);
        assert_eq!(ledger.passes_since_plan_update(), Some(0));

        ledger.advance_pass();
        ledger.advance_pass();
        assert_eq!(ledger.passes_since_plan_update(), Some(2));
    }

    #[test]
    fn a_plan_update_resets_both_counters_to_its_pass() {
        let mut ledger = Ledger::new(25);
        ledger.note_plan_carried(PlanProgress::NoCheckboxes, 0);
        ledger.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);
        ledger.advance_pass();
        ledger.advance_pass();
        assert_eq!(ledger.passes_since_plan_update(), Some(2));
        assert_eq!(ledger.writes_since_plan_update(), Some(1));

        ledger.note_plan_updated(PlanProgress::NoCheckboxes, 0);
        assert_eq!(ledger.passes_since_plan_update(), Some(0));
        assert_eq!(ledger.writes_since_plan_update(), Some(0));

        ledger.advance_pass();
        assert_eq!(ledger.passes_since_plan_update(), Some(1));
    }

    #[test]
    fn only_successful_writes_count_since_the_plan_update() {
        let mut ledger = Ledger::new(25);
        ledger.note_plan_updated(PlanProgress::NoCheckboxes, 0);

        ledger.record("edit_file", &json!({}), &ok(), CallOutcome::Ran);
        ledger.record("write_file", &json!({}), &ok(), CallOutcome::Ran);
        ledger.record("edit_file", &json!({}), &err(), CallOutcome::Ran);
        ledger.record("run_command", &json!({}), &ok(), CallOutcome::Ran);
        ledger.record("read_file", &json!({}), &ok(), CallOutcome::Ran);

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
