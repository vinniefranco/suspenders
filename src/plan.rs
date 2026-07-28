//! The Plan and the original task as one value (CONTEXT.md: Plan, Anchor).
//!
//! The Plan is the model-maintained statement of the current goal, held by the
//! harness outside the Conversation; the original task is the user's verbatim
//! first prompt. The Anchor is an injected copy of both, placed near the
//! Conversation's tail. This module owns the value and its composition; the
//! Run loop keeps the *when* (Anchor cadence, when to fire `set_plan`), the
//! Agent keeps the storage, and `crate::voice` keeps the Anchor's framing.
//!
//! ## Where the original task comes from
//!
//! Captured once per Run from the Conversation's first user text
//! ([`crate::conversation::Conversation::original_task`]) - unless the caller
//! already holds a durable copy. After a Compaction the Conversation's head is
//! the summary message, whose first block is also user text: a fresh capture
//! there would anchor the summary blob, not the task. The durable copy lives in
//! the Compaction state (captured at the first Compaction), and the Agent
//! threads it into every later Run, so the Anchor keeps carrying the verbatim
//! task statement per CONTEXT.md.

use crate::conversation::Conversation;
use crate::voice;
use serde_json::Value;

/// The Plan value: the model's current goal statement (`content`) and the
/// user's verbatim first prompt (`original_task`), both optional before they
/// are known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub content: Option<String>,
    pub original_task: Option<String>,
}

/// The outcome of folding one Tool Call into the Plan: `Updated` carries the
/// new Plan (the caller persists it, firing the `set_plan` Dep); `Unchanged`
/// leaves the Plan alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    Updated(Plan),
    Unchanged,
}

/// The Plan's checkbox completeness, read syntactically from its content.
/// A fact about the Plan text, never a judgment: the Endgame decides what to
/// do with it (see the Open Plan, CONTEXT.md; ADR-0043).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanProgress {
    /// At least one unchecked `[ ]` step.
    Incomplete,
    /// At least one checkbox and none unchecked.
    Complete,
    /// No checkboxes at all - completeness is uninferrable.
    NoCheckboxes,
}

/// One line's checkbox reading: whether it is an open box, a checked box, or
/// not a checkbox line at all. The grammar is fail-safe toward stopping -
/// ambiguity never reads as `Open`, so a malformed token cannot make the Plan
/// look Incomplete (failing toward autonomy is the dangerous direction).
enum BoxKind {
    Open,
    Checked,
    None,
}

/// Reads one non-fenced line as a checkbox. Trims leading whitespace, strips an
/// optional list bullet (`- `/`* `/`+ ` or an ordered `12) `/`3. ` marker),
/// then requires a line-anchored checkbox token: `[ ]` open, `[x]`/`[X]`
/// checked, anything else no box.
fn read_box(line: &str) -> BoxKind {
    let rest = strip_bullet(line.trim_start());
    if rest.starts_with("[ ]") {
        BoxKind::Open
    } else if rest.starts_with("[x]") || rest.starts_with("[X]") {
        BoxKind::Checked
    } else {
        BoxKind::None
    }
}

/// Strips a leading list bullet from an already-left-trimmed line, returning the
/// remainder. Handles `- `/`* `/`+ ` and ordered markers (digits then `.` or `)`
/// then a space). Leaves the line untouched when no bullet is present.
fn strip_bullet(line: &str) -> &str {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            return rest;
        }
    }
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let after = &line[digits..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return rest;
        }
    }
    line
}

/// A line that toggles a fenced code block: trimmed-start begins with three
/// backticks or three tildes. Fence delimiters and their contents are ignored
/// so example markdown cannot trigger false incompleteness.
fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

impl Plan {
    /// Builds the Run's Plan value: `content` restored from the previous Run
    /// (the Agent holds it), `original_task` from a durable copy when one
    /// exists (the Compaction state after the first Compaction).
    pub fn new(content: Option<String>, original_task: Option<String>) -> Self {
        Plan {
            content,
            original_task,
        }
    }

    /// Captures the verbatim original task from the Conversation, once: a Plan
    /// that already carries one is returned unchanged. Called at Run start,
    /// before any Compaction can summarize the head away.
    pub fn capture_task(mut self, conv: &Conversation) -> Self {
        if self.original_task.is_none() {
            self.original_task = conv.original_task().map(|t| t.to_string());
        }
        self
    }

    /// Folds one executed Tool Call into the Plan: a successful plan Tool Call
    /// with non-empty content updates it, anything else leaves it alone. The
    /// Plan content is the model's voice, verbatim - never rewritten (a
    /// malformed input sentinel or an errored call stores nothing).
    pub fn update(&self, name: &str, input: &Value, is_error: bool) -> Update {
        if name == "plan"
            && !is_error
            && let Some(Value::String(content)) = input.get("plan")
            && !content.is_empty()
        {
            let mut updated = self.clone();
            updated.content = Some(content.clone());
            return Update::Updated(updated);
        }
        Update::Unchanged
    }

    /// Composes the Anchor text: the original task and the current Plan inside
    /// the Voice-owned framing ([`crate::voice::anchor`]). The framing belongs
    /// to the Voice; the content is this value's.
    pub fn anchor(&self) -> String {
        voice::anchor(
            self.original_task.as_deref().unwrap_or(""),
            self.content.as_deref(),
        )
    }

    /// Reads the Plan's checkbox completeness syntactically (see [`PlanProgress`]):
    /// any open box makes it `Incomplete`, otherwise a present-but-all-checked
    /// Plan is `Complete`, and a Plan without checkboxes is `NoCheckboxes`. No
    /// content reads as `NoCheckboxes`. Fenced code blocks are excluded so an
    /// example `[ ]` cannot make the Plan look Incomplete.
    pub fn progress(&self) -> PlanProgress {
        let Some(content) = self.content.as_deref() else {
            return PlanProgress::NoCheckboxes;
        };
        let mut in_fence = false;
        let mut checked = false;
        for line in content.lines() {
            if is_fence(line) {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            match read_box(line) {
                BoxKind::Open => return PlanProgress::Incomplete,
                BoxKind::Checked => checked = true,
                BoxKind::None => {}
            }
        }
        if checked {
            PlanProgress::Complete
        } else {
            PlanProgress::NoCheckboxes
        }
    }

    /// Counts the Plan's checked-box lines (`[x]`/`[X]`), skipping fenced code
    /// blocks. No content counts as zero.
    pub fn checked_steps(&self) -> u64 {
        let Some(content) = self.content.as_deref() else {
            return 0;
        };
        let mut in_fence = false;
        let mut count = 0;
        for line in content.lines() {
            if is_fence(line) {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            if matches!(read_box(line), BoxKind::Checked) {
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{Conversation, ConversationOpts};
    use serde_json::json;

    fn conversation() -> Conversation {
        Conversation::new("sys", ConversationOpts::new(10_000, 0))
    }

    // ---- capture_task/2 ----

    #[test]
    fn captures_the_conversations_first_user_text_once() {
        let mut conv = conversation();
        conv.add_user_text("fix the flaky test");

        let plan = Plan::default().capture_task(&conv);
        assert_eq!(plan.original_task.as_deref(), Some("fix the flaky test"));
    }

    #[test]
    fn a_plan_already_carrying_a_task_is_unchanged() {
        let mut conv = conversation();
        conv.add_user_text("second turn prompt");

        let plan = Plan::new(None, Some("the real task".to_string())).capture_task(&conv);
        assert_eq!(plan.original_task.as_deref(), Some("the real task"));
    }

    #[test]
    fn the_durable_copy_wins_over_a_summary_head_post_compaction() {
        // After a Compaction the Conversation's head is the summary message -
        // also user text. A fresh capture would anchor the summary blob; the
        // carried copy from the Compaction state keeps the verbatim task.
        let mut base = conversation();
        base.add_user_text("original task");
        let conv = base.apply_compaction("what happened so far", 1);

        let fresh = Plan::default().capture_task(&conv);
        assert!(
            fresh
                .original_task
                .unwrap()
                .contains("what happened so far")
        );

        let durable = Plan::new(None, Some("original task".to_string())).capture_task(&conv);
        assert_eq!(durable.original_task.as_deref(), Some("original task"));
    }

    // ---- update/4 ----

    #[test]
    fn a_successful_plan_tool_call_updates_the_content() {
        let result = Plan::default().update("plan", &json!({ "plan": "1. read 2. edit" }), false);
        match result {
            Update::Updated(plan) => {
                assert_eq!(plan.content.as_deref(), Some("1. read 2. edit"));
            }
            Update::Unchanged => panic!("expected Updated"),
        }
    }

    #[test]
    fn other_tools_errored_calls_and_empty_or_malformed_content_do_not() {
        let plan = Plan::new(Some("keep me".to_string()), Some("task".to_string()));

        assert_eq!(
            plan.update("read_file", &json!({ "path": "x" }), false),
            Update::Unchanged
        );
        assert_eq!(
            plan.update("plan", &json!({ "plan": "ignored" }), true),
            Update::Unchanged
        );
        assert_eq!(
            plan.update("plan", &json!({ "plan": "" }), false),
            Update::Unchanged
        );
        assert_eq!(
            plan.update("plan", &json!({ "plan": 42 }), false),
            Update::Unchanged
        );
        assert_eq!(
            plan.update("plan", &crate::llm::malformed_input_marker("raw"), false),
            Update::Unchanged
        );
    }

    // ---- anchor/1 ----

    #[test]
    fn composes_the_task_and_plan_inside_the_voice_framing() {
        assert_eq!(
            Plan::new(Some("the plan".to_string()), Some("the task".to_string())).anchor(),
            voice::anchor("the task", Some("the plan"))
        );
    }

    #[test]
    fn a_missing_task_anchors_on_an_empty_string() {
        assert_eq!(
            Plan::new(Some("the plan".to_string()), None).anchor(),
            voice::anchor("", Some("the plan"))
        );
    }

    // ---- progress/1 + checked_steps/1 ----

    fn plan_with(content: &str) -> Plan {
        Plan::new(Some(content.to_string()), None)
    }

    #[test]
    fn a_dash_open_box_reads_as_incomplete() {
        assert_eq!(plan_with("- [ ] read the file").progress(), PlanProgress::Incomplete);
    }

    #[test]
    fn a_star_open_box_reads_as_incomplete() {
        assert_eq!(plan_with("* [ ] read the file").progress(), PlanProgress::Incomplete);
    }

    #[test]
    fn a_plus_open_box_reads_as_incomplete() {
        assert_eq!(plan_with("+ [ ] read the file").progress(), PlanProgress::Incomplete);
    }

    #[test]
    fn a_dotted_ordered_open_box_reads_as_incomplete() {
        assert_eq!(plan_with("1. [ ] read the file").progress(), PlanProgress::Incomplete);
    }

    #[test]
    fn a_paren_ordered_open_box_reads_as_incomplete() {
        assert_eq!(plan_with("2) [ ] read the file").progress(), PlanProgress::Incomplete);
    }

    #[test]
    fn a_bare_open_box_without_a_bullet_reads_as_incomplete() {
        assert_eq!(plan_with("[ ] read the file").progress(), PlanProgress::Incomplete);
    }

    #[test]
    fn all_checked_boxes_read_as_complete() {
        assert_eq!(
            plan_with("- [x] read\n- [X] edit").progress(),
            PlanProgress::Complete
        );
    }

    #[test]
    fn checked_steps_counts_the_checked_boxes() {
        assert_eq!(plan_with("- [x] read\n- [X] edit\n- [x] build").checked_steps(), 3);
    }

    #[test]
    fn a_mix_of_checked_and_open_reads_as_incomplete() {
        assert_eq!(
            plan_with("- [x] read\n- [ ] edit").progress(),
            PlanProgress::Incomplete
        );
    }

    #[test]
    fn a_prose_plan_has_no_checkboxes() {
        assert_eq!(
            plan_with("First read the file, then edit it, then build.").progress(),
            PlanProgress::NoCheckboxes
        );
    }

    #[test]
    fn the_done_heading_style_has_no_checkboxes() {
        let plan = plan_with("## Step 6: DONE ✅\n## Step 7: in progress");
        assert_eq!(plan.progress(), PlanProgress::NoCheckboxes);
        assert_eq!(plan.checked_steps(), 0);
    }

    #[test]
    fn an_open_box_inside_a_code_fence_does_not_read_as_incomplete() {
        // The load-bearing fail-safe: example markdown must never make the Plan
        // look Incomplete. This fence holds the only box, so the Plan has none.
        let plan = plan_with("Write markdown like:\n```\n- [ ] a step\n```\nDone.");
        assert_eq!(plan.progress(), PlanProgress::NoCheckboxes);
    }

    #[test]
    fn a_tilde_code_fence_also_hides_its_boxes() {
        let plan = plan_with("~~~\n- [ ] a step\n~~~");
        assert_eq!(plan.progress(), PlanProgress::NoCheckboxes);
    }

    #[test]
    fn a_real_open_box_outside_a_fence_still_reads_as_incomplete() {
        let plan = plan_with("```\n- [x] example\n```\n- [ ] real step");
        assert_eq!(plan.progress(), PlanProgress::Incomplete);
    }

    #[test]
    fn none_content_has_no_checkboxes_and_zero_checked_steps() {
        let plan = Plan::default();
        assert_eq!(plan.progress(), PlanProgress::NoCheckboxes);
        assert_eq!(plan.checked_steps(), 0);
    }

    #[test]
    fn malformed_boxes_are_ignored_not_open() {
        // `[-]`, `[]`, `[ x]` are none of open/checked; the Plan reads as having
        // no checkboxes rather than as Incomplete (fail-safe toward stopping).
        let plan = plan_with("- [-] one\n- [] two\n- [ x] three");
        assert_eq!(plan.progress(), PlanProgress::NoCheckboxes);
        assert_eq!(plan.checked_steps(), 0);
    }
}
