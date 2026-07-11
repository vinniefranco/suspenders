//! The Plan and the original task as one value (CONTEXT.md: Plan, Anchor).
//!
//! The Plan is the model-maintained statement of the current goal, held by the
//! harness outside the Conversation; the original task is the user's verbatim
//! first prompt. The Anchor is an injected copy of both, placed near the
//! Conversation's tail. This module owns the value and its composition; the
//! Turn loop keeps the *when* (Anchor cadence, when to fire `set_plan`), the
//! Agent keeps the storage, and `crate::voice` keeps the Anchor's framing.
//!
//! ## Where the original task comes from
//!
//! Captured once per Turn from the Conversation's first user text
//! ([`crate::conversation::Conversation::original_task`]) - unless the caller
//! already holds a durable copy. After a Compaction the Conversation's head is
//! the summary message, whose first block is also user text: a fresh capture
//! there would anchor the summary blob, not the task. The durable copy lives in
//! the Compaction state (captured at the first Compaction), and the Agent
//! threads it into every later Turn, so the Anchor keeps carrying the verbatim
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

impl Plan {
    /// Builds the Turn's Plan value: `content` restored from the previous Turn
    /// (the Agent holds it), `original_task` from a durable copy when one
    /// exists (the Compaction state after the first Compaction).
    pub fn new(content: Option<String>, original_task: Option<String>) -> Self {
        Plan {
            content,
            original_task,
        }
    }

    /// Captures the verbatim original task from the Conversation, once: a Plan
    /// that already carries one is returned unchanged. Called at Turn start,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{Conversation, ConversationOpts};
    use serde_json::json;

    fn conversation() -> Conversation {
        Conversation::new(
            "sys",
            ConversationOpts::new(10_000, 0),
        )
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
        assert!(fresh.original_task.unwrap().contains("what happened so far"));

        let durable =
            Plan::new(None, Some("original task".to_string())).capture_task(&conv);
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
            plan.update(
                "plan",
                &json!({ crate::llm::stream::MALFORMED_INPUT_SENTINEL: "raw" }),
                false
            ),
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
}
