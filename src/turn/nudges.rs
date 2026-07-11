//! Bookkeeping for the Turn's Nudges (baud: `Baud.Turn.Nudges`; CONTEXT.md,
//! docs/DESIGN.md). The Turn owns *when* to consult this state; the wording
//! lives in [`crate::voice`]. This struct owns the WHEN/STATE — transitions and
//! predicates — and never authors nudge strings.
//!
//!   * **Duplicate Tool Call Nudge**: `prev_calls` remembers the previous
//!     response's calls whose results are still fresh; `fresh` accumulates this
//!     batch's. A successful edit_file/write_file clears both, because results
//!     from before a write are stale — an identical call after it (fix, then
//!     retest) is a legitimate re-run, not a loop symptom — and the fresh set
//!     restarts at the write itself.
//!   * **Consecutive-failure Nudge**: `failures` tracks consecutive `is_error`
//!     Tool Results per tool name (a success for that tool resets it). From the
//!     3rd onward a "step back" suffix rides the real result, summarising the
//!     kinds of errors seen.
//!   * **Verify Nudge**: a successful edit_file/write_file arms
//!     `unverified_writes`; any run_command afterwards disarms it — approved,
//!     denied, or failed alike. `verify_nudge` caps it at once per Turn UNTIL
//!     progress re-arms it.
//!   * **Verify-failed Nudge**: `command_failing` tracks the outcome of the most
//!     recent run_command this Turn. `verify_failed_nudge` caps it at once per
//!     Turn but RE-ARMS on progress.
//!   * **Empty-response Nudge**: `empty_response_nudged` caps it at once per
//!     Turn until progress re-arms it. The no-think rescue (`arm_rescue`,
//!     `rescue_armed`, `consume_rescue`) lives here too.
//!   * **Explore Nudge**: `explore_streak` counts consecutive Passes whose Tool
//!     Calls were ALL exploration — read-only Tools or a search-shaped
//!     run_command. Fires every 3rd such Pass (3rd, 6th, 9th, ...); the streak
//!     resets when it fires so each fire starts a fresh window.

pub mod failure_category;
pub mod search_command;

use serde_json::Value;

use crate::voice::{self, FailureCategory};

const FAILURE_NUDGE_FROM: u64 = 3;
const EXPLORE_NUDGE_EVERY: u64 = 3;
const STUCK_RECENCY: u64 = 3;
const WRITE_TOOLS: &[&str] = &["edit_file", "write_file"];
const EXPLORE_TOOLS: &[&str] = &["read_file", "list_files", "grep"];

/// One tool's consecutive-failure entry: how many in a row, the per-category
/// tally, and the recency stamp `stuck` checks (`last_pass`).
#[derive(Debug, Clone, Default)]
struct FailureEntry {
    count: u64,
    // Insertion-ordered category tallies. Order does not affect the Voice
    // output (it sorts by tally descending), so a Vec keeps parity while being
    // Hash-free.
    categories: Vec<(FailureCategory, u64)>,
    last_pass: u64,
}

impl FailureEntry {
    fn bump(&mut self, category: FailureCategory, pass: u64) {
        self.count += 1;
        self.last_pass = pass;
        match self.categories.iter_mut().find(|(c, _)| *c == category) {
            Some((_, n)) => *n += 1,
            None => self.categories.push((category, 1)),
        }
    }
}

/// The outcome of one executed Tool Call, as the Turn loop observes it.
pub struct ToolResult<'a> {
    pub content: &'a str,
    pub is_error: bool,
}

/// The Nudge bookkeeping threaded through the Turn loop. A plain value: methods
/// mutate `&mut self` (or read), no processes.
#[derive(Debug, Clone)]
pub struct Nudges {
    // Duplicate memory: the previous response's still-fresh calls, and this
    // batch's accumulating set. `{name, input}` pairs; membership is equality.
    prev_calls: Vec<(String, Value)>,
    fresh: Vec<(String, Value)>,
    failures: Vec<(String, FailureEntry)>,
    unverified_writes: bool,
    verify_nudged: bool,
    command_failing: bool,
    verify_failed_nudged: bool,
    empty_response_nudged: bool,
    explore_streak: u64,
    pass: u64,
    rescue_next: bool,
    rescue_sticky: bool,
    empty_count: u64,
}

impl Default for Nudges {
    fn default() -> Self {
        Self::new()
    }
}

impl Nudges {
    pub fn new() -> Self {
        Nudges {
            prev_calls: Vec::new(),
            fresh: Vec::new(),
            failures: Vec::new(),
            unverified_writes: false,
            verify_nudged: false,
            command_failing: false,
            verify_failed_nudged: false,
            empty_response_nudged: false,
            explore_streak: 0,
            pass: 0,
            rescue_next: false,
            rescue_sticky: false,
            empty_count: 0,
        }
    }

    /// Is the Turn stuck in a failure loop? True when any tool has reached the
    /// consecutive-failure threshold AND that streak is recent — its last
    /// failure landed within the final `STUCK_RECENCY` Passes. A streak only
    /// resets on that tool's next success, so without the recency requirement a
    /// tool the model abandoned early and routed around would still label the
    /// Turn stuck at the limit.
    pub fn stuck(&self) -> bool {
        self.failures.iter().any(|(_tool, entry)| {
            entry.count >= FAILURE_NUDGE_FROM && self.pass - entry.last_pass <= STUCK_RECENCY
        })
    }

    /// Duplicate Nudge check: identical to a still-fresh call from the previous
    /// response?
    pub fn duplicate(&self, name: &str, input: &Value) -> bool {
        self.prev_calls
            .iter()
            .any(|(n, i)| n == name && i == input)
    }

    /// Folds one executed Tool Call's outcome into the bookkeeping. Returns the
    /// content to record, carrying the consecutive-failure suffix when earned.
    pub fn note_result(&mut self, name: &str, input: &Value, result: &ToolResult) -> String {
        let tracked = self.track_failures(result.content, result.is_error, name);
        self.note_write(name, result.is_error, input);
        self.note_command(name, result.is_error, result.content);
        self.note_fresh(name, input, result.is_error);
        tracked
    }

    /// Closes a tool batch: this response's still-fresh calls become the next
    /// pass's duplicate memory, and the pass counter (the failure-streak
    /// recency clock for `stuck`) advances.
    pub fn next_pass(&mut self) {
        self.prev_calls = std::mem::take(&mut self.fresh);
        self.pass += 1;
    }

    /// Explore Nudge bookkeeping: folds one Pass's Tool Calls into the
    /// exploration streak and returns whether the Nudge fires on this Pass. A
    /// Pass extends the streak only when it made at least one Tool Call and
    /// every one was exploration; any other Tool Call, or a Pass with no Tool
    /// Calls, resets it. Fires every `EXPLORE_NUDGE_EVERY`th consecutive such
    /// Pass, and firing resets the streak.
    pub fn note_pass_calls(&mut self, calls: &[(String, Value)]) -> bool {
        if exploration_only(calls) {
            let streak = self.explore_streak + 1;
            if streak.is_multiple_of(EXPLORE_NUDGE_EVERY) {
                self.explore_streak = 0;
                true
            } else {
                self.explore_streak = streak;
                false
            }
        } else {
            self.explore_streak = 0;
            false
        }
    }

    /// Does end-of-turn owe the Verify Nudge?
    pub fn verify_nudge(&self) -> bool {
        self.unverified_writes && !self.verify_nudged
    }

    /// Are there successful writes with no run_command since? The Verification
    /// Pass (ADR-0016) keys on this.
    pub fn unverified_writes(&self) -> bool {
        self.unverified_writes
    }

    /// Does end-of-turn owe the Verify-failed Nudge? True when the most recent
    /// run_command this Turn failed and this Nudge has not fired yet.
    pub fn verify_failed_nudge(&self) -> bool {
        self.command_failing && !self.verify_failed_nudged
    }

    /// The Verify-failed Nudge fired; fires at most once per Turn UNTIL progress
    /// re-arms it. Clears the duplicate memory: the finishing response's dropped
    /// tool_use blocks never produced results.
    pub fn note_verify_failed_nudged(&mut self) {
        self.verify_failed_nudged = true;
        self.prev_calls.clear();
    }

    /// Records a Pass's Tool Calls for the re-arming rule. A Pass that made at
    /// least one Tool Call is progress: it re-arms the Verify Nudge, the
    /// Verify-failed gate, and the Empty-response Nudge. An idle Pass (no Tool
    /// Calls) never re-arms. This only re-arms; it never disarms an
    /// armed-but-unfired gate.
    pub fn note_progress(&mut self, calls: &[(String, Value)]) {
        if calls.is_empty() {
            return;
        }
        self.verify_nudged = false;
        self.verify_failed_nudged = false;
        self.empty_response_nudged = false;
    }

    /// Does end-of-turn owe the Empty-response Nudge?
    pub fn empty_response_nudge(&self) -> bool {
        !self.empty_response_nudged
    }

    /// The Empty-response Nudge fired; fires at most once per Turn UNTIL progress
    /// re-arms it. Clears the duplicate memory like the other finish Nudges.
    pub fn note_empty_response_nudged(&mut self) {
        self.empty_response_nudged = true;
        self.prev_calls.clear();
    }

    /// Arms the break-glass no-think rescue when the Session knob allows it:
    /// after an Empty-response Nudge fires, the very next model call carries
    /// no_think, then reverts. The SECOND empty of the Turn makes the rescue
    /// STICKY. With the knob off the count still advances but nothing arms.
    /// Progress never disarms the rescue.
    pub fn arm_rescue(&mut self, enabled: bool) {
        self.empty_count += 1;
        self.rescue_next = enabled;
        self.rescue_sticky = self.rescue_sticky || (enabled && self.empty_count >= 2);
    }

    /// Should the next model call carry no_think? (armed or sticky)
    pub fn rescue_armed(&self) -> bool {
        self.rescue_next || self.rescue_sticky
    }

    /// Consumes the one-Pass rescue arm: the rescue rides exactly the Pass right
    /// after the Empty-response Nudge fired — unless it has gone sticky.
    pub fn consume_rescue(&mut self) {
        self.rescue_next = false;
    }

    /// The Verify Nudge fired; fires at most once per Turn UNTIL progress
    /// re-arms it. Also clears the duplicate memory.
    pub fn note_verify_nudged(&mut self) {
        self.verify_nudged = true;
        self.prev_calls.clear();
    }

    // The Verify-failed Nudge tracks the outcome of the most recent run_command
    // that actually RAN this Turn: a failing one arms the gate, a passing one
    // clears it. A denied command never ran, so it leaves the state untouched.
    // Only run_command outcomes touch this state.
    fn note_command(&mut self, name: &str, is_error: bool, content: &str) {
        if name == "run_command" {
            if content == voice::command_denied() {
                return;
            }
            self.command_failing = is_error;
        }
    }

    // A run_command disarms the Verify Nudge; a successful write arms it and
    // clears the duplicate memory mid-batch (results before it are stale).
    fn note_write(&mut self, name: &str, is_error: bool, _input: &Value) {
        if name == "run_command" {
            self.unverified_writes = false;
        } else if WRITE_TOOLS.contains(&name) && !is_error {
            self.unverified_writes = true;
            self.prev_calls.clear();
        }
    }

    // The fresh set restarts at a successful write: it invalidates every result
    // before it in the batch.
    fn note_fresh(&mut self, name: &str, input: &Value, is_error: bool) {
        if WRITE_TOOLS.contains(&name) && !is_error {
            self.fresh = vec![(name.to_string(), input.clone())];
        } else {
            self.push_fresh(name, input);
        }
    }

    fn push_fresh(&mut self, name: &str, input: &Value) {
        let pair = (name.to_string(), input.clone());
        if !self.fresh.iter().any(|p| p == &pair) {
            self.fresh.push(pair);
        }
    }

    // Consecutive-failure Nudge: from the 3rd consecutive failure of one tool
    // onward, the "step back" suffix rides the real result.
    fn track_failures(&mut self, content: &str, is_error: bool, name: &str) -> String {
        if !is_error {
            self.failures.retain(|(t, _)| t != name);
            return content.to_string();
        }

        let category = failure_category::classify(content);
        let pass = self.pass;
        let entry = match self.failures.iter_mut().find(|(t, _)| t == name) {
            Some((_, e)) => e,
            None => {
                self.failures.push((name.to_string(), FailureEntry::default()));
                &mut self.failures.last_mut().unwrap().1
            }
        };
        entry.bump(category, pass);
        let count = entry.count;
        let categories = entry.categories.clone();

        if count >= FAILURE_NUDGE_FROM {
            format!("{}{}", content, voice::failure_nudge(count, name, &categories))
        } else {
            content.to_string()
        }
    }
}

// Exploration: at least one Tool Call, all of them exploration. An empty batch
// is not exploration (it resets).
fn exploration_only(calls: &[(String, Value)]) -> bool {
    !calls.is_empty() && calls.iter().all(exploration_call)
}

// A read-only Tool, or a run_command whose command string is search-shaped.
fn exploration_call(call: &(String, Value)) -> bool {
    let (name, input) = call;
    if EXPLORE_TOOLS.contains(&name.as_str()) {
        true
    } else if name == "run_command" {
        search_command::search_shaped(command_string(input))
    } else {
        false
    }
}

fn command_string(input: &Value) -> &str {
    input.get("command").and_then(Value::as_str).unwrap_or("")
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

    // Folds a result in, keeping only the bookkeeping.
    fn note(nudges: &mut Nudges, name: &str, input: Value, result: ToolResult) {
        nudges.note_result(name, &input, &result);
    }

    fn note_empty(nudges: &mut Nudges, name: &str, result: ToolResult) {
        note(nudges, name, json!({}), result);
    }

    // ----- duplicate memory -----

    #[test]
    fn a_call_becomes_a_duplicate_only_after_next_pass() {
        let input = json!({"path": "a.ex"});
        let mut nudges = Nudges::new();
        note(&mut nudges, "read_file", input.clone(), ok());

        assert!(!nudges.duplicate("read_file", &input));
        nudges.next_pass();
        assert!(nudges.duplicate("read_file", &input));
    }

    #[test]
    fn a_different_name_or_input_is_not_a_duplicate() {
        let mut nudges = Nudges::new();
        note(&mut nudges, "read_file", json!({"path": "a.ex"}), ok());
        nudges.next_pass();

        assert!(!nudges.duplicate("read_file", &json!({"path": "b.ex"})));
        assert!(!nudges.duplicate("grep", &json!({"path": "a.ex"})));
    }

    #[test]
    fn a_successful_write_clears_previous_memory_mid_batch() {
        let input = json!({"path": "a.ex"});
        let mut nudges = Nudges::new();
        note(&mut nudges, "read_file", input.clone(), ok());
        nudges.next_pass();

        assert!(nudges.duplicate("read_file", &input));

        // Fix-then-retest: results from before the write are stale.
        note(
            &mut nudges,
            "write_file",
            json!({"path": "a.ex", "content": "x"}),
            ok(),
        );
        assert!(!nudges.duplicate("read_file", &input));
    }

    #[test]
    fn the_fresh_set_restarts_at_a_successful_write() {
        let write_input = json!({"path": "a.ex", "content": "x"});
        let mut nudges = Nudges::new();
        note(&mut nudges, "read_file", json!({"path": "a.ex"}), ok());
        note(&mut nudges, "write_file", write_input.clone(), ok());
        nudges.next_pass();

        assert!(!nudges.duplicate("read_file", &json!({"path": "a.ex"})));
        assert!(nudges.duplicate("write_file", &write_input));
    }

    #[test]
    fn a_failed_write_clears_nothing() {
        let input = json!({"path": "a.ex"});
        let mut nudges = Nudges::new();
        note(&mut nudges, "read_file", input.clone(), ok());
        nudges.next_pass();
        note(
            &mut nudges,
            "write_file",
            json!({"path": "a.ex", "content": "x"}),
            err(),
        );

        assert!(nudges.duplicate("read_file", &input));
    }

    #[test]
    fn note_verify_nudged_clears_the_memory() {
        let input = json!({"path": "a.ex"});
        let mut nudges = Nudges::new();
        note(&mut nudges, "read_file", input.clone(), ok());
        nudges.next_pass();
        nudges.note_verify_nudged();

        assert!(!nudges.duplicate("read_file", &input));
    }

    // ----- consecutive-failure counter -----

    #[test]
    fn the_3rd_consecutive_failure_earns_the_suffix() {
        let mut nudges = Nudges::new();
        let c1 = nudges.note_result("read_file", &json!({}), &err());
        let c2 = nudges.note_result("read_file", &json!({}), &err());
        let c3 = nudges.note_result("read_file", &json!({}), &err());
        let c4 = nudges.note_result("read_file", &json!({}), &err());

        assert_eq!(c1, "boom");
        assert_eq!(c2, "boom");
        assert!(c3.contains("3 consecutive read_file failures"));
        assert!(c3.contains("step back:"));
        assert!(c4.contains("4 consecutive read_file failures"));
    }

    #[test]
    fn a_success_for_the_tool_resets_its_counter() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "read_file", err());
        note_empty(&mut nudges, "read_file", err());
        note_empty(&mut nudges, "read_file", ok());
        note_empty(&mut nudges, "read_file", err());
        note_empty(&mut nudges, "read_file", err());

        let content = nudges.note_result("read_file", &json!({}), &err());
        assert!(content.contains("3 consecutive read_file failures"));
    }

    #[test]
    fn counters_are_per_tool_name() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "read_file", err());
        note_empty(&mut nudges, "read_file", err());

        let content = nudges.note_result("grep", &json!({}), &err());
        assert_eq!(content, "boom");
    }

    // ----- stuck? -----

    fn fail_thrice(nudges: &mut Nudges, name: &str) {
        note_empty(nudges, name, err());
        note_empty(nudges, name, err());
        note_empty(nudges, name, err());
    }

    fn passes(nudges: &mut Nudges, n: u64) {
        for _ in 0..n {
            nudges.next_pass();
        }
    }

    #[test]
    fn below_the_failure_threshold_is_never_stuck() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "read_file", err());
        note_empty(&mut nudges, "read_file", err());
        nudges.next_pass();

        assert!(!nudges.stuck());
    }

    #[test]
    fn a_threshold_streak_that_just_failed_marks_stuck() {
        let mut nudges = Nudges::new();
        fail_thrice(&mut nudges, "read_file");
        nudges.next_pass();

        assert!(nudges.stuck());
    }

    #[test]
    fn an_abandoned_early_streak_goes_stale() {
        let mut nudges = Nudges::new();
        fail_thrice(&mut nudges, "explore");
        passes(&mut nudges, 20);

        assert!(!nudges.stuck());
    }

    #[test]
    fn the_streak_stays_live_for_exactly_the_recency_window() {
        let mut a = Nudges::new();
        fail_thrice(&mut a, "read_file");
        let mut b = a.clone();
        passes(&mut a, 3);
        assert!(a.stuck());
        passes(&mut b, 4);
        assert!(!b.stuck());
    }

    #[test]
    fn a_new_failure_revives_a_stale_streak() {
        let mut nudges = Nudges::new();
        fail_thrice(&mut nudges, "read_file");
        passes(&mut nudges, 10);
        note_empty(&mut nudges, "read_file", err());
        nudges.next_pass();

        assert!(nudges.stuck());
    }

    #[test]
    fn a_success_clears_the_streak_entirely() {
        let mut nudges = Nudges::new();
        fail_thrice(&mut nudges, "read_file");
        note_empty(&mut nudges, "read_file", ok());
        nudges.next_pass();

        assert!(!nudges.stuck());
    }

    // ----- verify nudge -----

    #[test]
    fn verify_starts_disarmed() {
        assert!(!Nudges::new().verify_nudge());
    }

    #[test]
    fn a_successful_write_arms_it_a_failed_one_doesnt() {
        let mut a = Nudges::new();
        note_empty(&mut a, "write_file", ok());
        assert!(a.verify_nudge());

        let mut b = Nudges::new();
        note_empty(&mut b, "edit_file", ok());
        assert!(b.verify_nudge());

        let mut c = Nudges::new();
        note_empty(&mut c, "write_file", err());
        assert!(!c.verify_nudge());
    }

    #[test]
    fn any_run_command_disarms_it() {
        let mut base = Nudges::new();
        note_empty(&mut base, "edit_file", ok());

        let mut a = base.clone();
        note_empty(&mut a, "run_command", ok());
        assert!(!a.verify_nudge());

        let mut b = base.clone();
        note_empty(&mut b, "run_command", err());
        assert!(!b.verify_nudge());
    }

    #[test]
    fn a_write_after_the_run_command_re_arms_it() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "edit_file", ok());
        note_empty(&mut nudges, "run_command", ok());
        note_empty(&mut nudges, "edit_file", ok());

        assert!(nudges.verify_nudge());
    }

    #[test]
    fn verify_fires_at_most_once_per_turn() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "edit_file", ok());
        nudges.note_verify_nudged();
        note_empty(&mut nudges, "edit_file", ok());

        assert!(!nudges.verify_nudge());
    }

    // ----- verify-failed nudge -----

    #[test]
    fn verify_failed_starts_disarmed() {
        assert!(!Nudges::new().verify_failed_nudge());
    }

    #[test]
    fn a_failing_run_command_arms_it_a_passing_one_clears_it() {
        let mut a = Nudges::new();
        note_empty(&mut a, "run_command", err());
        assert!(a.verify_failed_nudge());

        let mut b = Nudges::new();
        note_empty(&mut b, "run_command", ok());
        assert!(!b.verify_failed_nudge());
    }

    #[test]
    fn verify_failed_fires_at_most_once_while_idle() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "run_command", err());
        nudges.note_verify_failed_nudged();

        assert!(!nudges.verify_failed_nudge());
    }

    #[test]
    fn a_pass_with_a_tool_call_re_arms_verify_failed() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "run_command", err());
        nudges.note_verify_failed_nudged();
        nudges.note_progress(&[("read_file".into(), json!({"path": "a.ex"}))]);

        assert!(nudges.verify_failed_nudge());
    }

    #[test]
    fn a_pass_with_no_tool_calls_does_not_re_arm_verify_failed() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "run_command", err());
        nudges.note_verify_failed_nudged();
        nudges.note_progress(&[]);

        assert!(!nudges.verify_failed_nudge());
    }

    #[test]
    fn note_progress_before_firing_leaves_the_cap_untouched() {
        let mut nudges = Nudges::new();
        note_empty(&mut nudges, "run_command", err());
        nudges.note_progress(&[("read_file".into(), json!({}))]);

        assert!(nudges.verify_failed_nudge());
    }

    // ----- empty-response nudge re-arm -----

    #[test]
    fn empty_a_pass_with_a_tool_call_re_arms_it() {
        let mut nudges = Nudges::new();
        nudges.note_empty_response_nudged();
        nudges.note_progress(&[("read_file".into(), json!({"path": "a.ex"}))]);

        assert!(nudges.empty_response_nudge());
    }

    #[test]
    fn empty_a_pass_with_no_tool_calls_does_not_re_arm_it() {
        let mut nudges = Nudges::new();
        nudges.note_empty_response_nudged();
        nudges.note_progress(&[]);

        assert!(!nudges.empty_response_nudge());
    }

    // ----- no-think rescue -----

    #[test]
    fn the_first_empty_arms_one_pass_consuming_reverts() {
        let mut nudges = Nudges::new();
        nudges.arm_rescue(true);
        assert!(nudges.rescue_armed());

        nudges.consume_rescue();
        assert!(!nudges.rescue_armed());
    }

    #[test]
    fn the_second_empty_makes_the_rescue_sticky() {
        let mut nudges = Nudges::new();
        nudges.arm_rescue(true);
        nudges.consume_rescue();
        nudges.arm_rescue(true);
        nudges.consume_rescue();

        assert!(nudges.rescue_armed());
    }

    #[test]
    fn an_off_knob_never_arms_but_the_count_advances() {
        let mut nudges = Nudges::new();
        nudges.arm_rescue(false);
        nudges.arm_rescue(false);

        assert!(!nudges.rescue_armed());
        assert_eq!(nudges.empty_count, 2);
    }

    #[test]
    fn progress_never_disarms_the_sticky_rescue() {
        let mut nudges = Nudges::new();
        nudges.arm_rescue(true);
        nudges.arm_rescue(true);
        nudges.note_progress(&[("read_file".into(), json!({"path": "a.ex"}))]);

        assert!(nudges.rescue_armed());
    }

    // ----- explore nudge -----

    // note_pass folds one Pass's Tool Calls into the streak and returns whether
    // the Explore Nudge fires. Accepts bare names (input defaults to {}).
    fn note_pass(nudges: &mut Nudges, calls: &[&str]) -> bool {
        let calls: Vec<(String, Value)> =
            calls.iter().map(|n| (n.to_string(), json!({}))).collect();
        nudges.note_pass_calls(&calls)
    }

    fn note_pass_pairs(nudges: &mut Nudges, calls: &[(&str, Value)]) -> bool {
        let calls: Vec<(String, Value)> = calls
            .iter()
            .map(|(n, i)| (n.to_string(), i.clone()))
            .collect();
        nudges.note_pass_calls(&calls)
    }

    const READONLY: &[&str] = &["read_file", "list_files", "grep"];

    #[test]
    fn fires_on_the_3rd_consecutive_exploration_pass() {
        let mut nudges = Nudges::new();
        let f1 = note_pass(&mut nudges, &["read_file"]);
        let f2 = note_pass(&mut nudges, &["list_files"]);
        let f3 = note_pass(&mut nudges, &["grep"]);

        assert!(!f1);
        assert!(!f2);
        assert!(f3);
    }

    #[test]
    fn fires_again_on_the_6th_9th() {
        let mut nudges = Nudges::new();
        note_pass(&mut nudges, &["read_file"]);
        note_pass(&mut nudges, &["read_file"]);
        assert!(note_pass(&mut nudges, &["read_file"]));

        assert!(!note_pass(&mut nudges, &["read_file"]));
        assert!(!note_pass(&mut nudges, &["read_file"]));
        assert!(note_pass(&mut nudges, &["read_file"]));

        assert!(!note_pass(&mut nudges, &["read_file"]));
        assert!(!note_pass(&mut nudges, &["read_file"]));
        assert!(note_pass(&mut nudges, &["read_file"]));
    }

    #[test]
    fn a_mix_of_all_three_readonly_tools_in_one_pass_counts() {
        let mut nudges = Nudges::new();
        note_pass(&mut nudges, READONLY);
        note_pass(&mut nudges, READONLY);
        assert!(note_pass(&mut nudges, READONLY));
    }

    #[test]
    fn a_pass_with_no_tool_calls_resets_the_streak() {
        let mut nudges = Nudges::new();
        note_pass(&mut nudges, &["read_file"]);
        note_pass(&mut nudges, &["read_file"]);
        note_pass(&mut nudges, &[]);

        assert!(!note_pass(&mut nudges, &["read_file"]));
        assert!(!note_pass(&mut nudges, &["read_file"]));
        assert!(note_pass(&mut nudges, &["read_file"]));
    }

    #[test]
    fn a_pass_containing_a_non_readonly_tool_resets_the_streak() {
        for non_readonly in ["explore", "plan", "edit_file", "write_file", "run_command"] {
            let mut nudges = Nudges::new();
            note_pass(&mut nudges, &["read_file"]);
            note_pass(&mut nudges, &["grep"]);
            let fired = note_pass(&mut nudges, &["read_file", non_readonly]);
            assert!(!fired, "expected reset with {non_readonly}");
        }
    }

    #[test]
    fn a_search_shaped_run_command_counts_as_exploration() {
        let grep = ("run_command", json!({"command": "grep -rn foo lib"}));
        let find = (
            "run_command",
            json!({"command": "find . -name '*.ex' | xargs grep foo"}),
        );

        let mut nudges = Nudges::new();
        let f1 = note_pass_pairs(&mut nudges, &[grep]);
        let f2 = note_pass_pairs(&mut nudges, &[find]);
        let f3 = note_pass_pairs(&mut nudges, &[("read_file", json!({"path": "a.ex"}))]);
        assert!(!f1);
        assert!(!f2);
        assert!(f3);
    }

    #[test]
    fn a_mix_test_run_command_resets_the_streak() {
        let mut nudges = Nudges::new();
        note_pass_pairs(&mut nudges, &[("run_command", json!({"command": "grep foo lib"}))]);
        note_pass_pairs(&mut nudges, &[("run_command", json!({"command": "grep bar lib"}))]);
        let fired =
            note_pass_pairs(&mut nudges, &[("run_command", json!({"command": "mix test"}))]);
        assert!(!fired);

        assert!(!note_pass_pairs(
            &mut nudges,
            &[("run_command", json!({"command": "grep a lib"}))]
        ));
        assert!(!note_pass_pairs(
            &mut nudges,
            &[("run_command", json!({"command": "grep b lib"}))]
        ));
        assert!(note_pass_pairs(
            &mut nudges,
            &[("run_command", json!({"command": "grep c lib"}))]
        ));
    }

    #[test]
    fn a_search_run_command_mixed_with_a_readonly_tool_counts() {
        let call: &[(&str, Value)] = &[
            ("read_file", json!({"path": "a.ex"})),
            ("run_command", json!({"command": "grep x lib"})),
        ];
        let mut nudges = Nudges::new();
        note_pass_pairs(&mut nudges, call);
        note_pass_pairs(&mut nudges, call);
        assert!(note_pass_pairs(&mut nudges, call));
    }

    #[test]
    fn a_non_search_run_command_resets_even_alongside_readonly_tools() {
        let mut nudges = Nudges::new();
        note_pass(&mut nudges, &["read_file"]);
        note_pass(&mut nudges, &["grep"]);

        let reset: &[(&str, Value)] = &[
            ("read_file", json!({"path": "a.ex"})),
            ("run_command", json!({"command": "git status"})),
        ];
        assert!(!note_pass_pairs(&mut nudges, reset));
    }

    #[test]
    fn the_streak_resets_after_firing() {
        let mut nudges = Nudges::new();
        note_pass(&mut nudges, &["grep"]);
        note_pass(&mut nudges, &["grep"]);
        note_pass(&mut nudges, &["grep"]);

        // A non-exploration Pass right after a fire resets cleanly.
        note_pass(&mut nudges, &["edit_file"]);
        assert!(!note_pass(&mut nudges, &["read_file"]));
        assert!(!note_pass(&mut nudges, &["read_file"]));
        assert!(note_pass(&mut nudges, &["read_file"]));
    }
}
