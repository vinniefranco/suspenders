//! Compaction — summarising history to fit the context window (ADR-0012).
//!
//! ## What compaction is
//!
//! When the Conversation's token estimate approaches the Context Budget, old
//! Turns are summarized by the LLM and replaced with a structured markdown
//! summary. Unlike Eviction (which mechanically hollows out Tool Results),
//! compaction is semantic — it extracts what was accomplished, what decisions
//! were made, and what files were touched.
//!
//! ## An effect, not part of the pure loop
//!
//! [`Compaction::run`] calls the [`Llm`] boundary directly — it is an effect,
//! invoked via the Turn's `compact` Dep in production (ADR-0012), NOT inside
//! the pure loop. It uses the Conversation's PURE helpers
//! ([`Conversation::prepare_compaction`], [`Conversation::apply_compaction`],
//! [`crate::conversation::extract_file_ops`]).
//!
//! ## Mechanical facts
//!
//! Compaction never asks the model to remember what the harness already knows.
//! The verbatim original task statement (the first user text, captured once on
//! the first compaction) and the accumulated `file_ops` are carried in the
//! [`Compaction`] state and appended to the summary message **mechanically**,
//! outside the LLM output (via [`crate::session::log::compose_summary`] /
//! [`voice::compaction_facts`]). The model writes narrative; the harness
//! guarantees the identifiers. Both travel with the compaction state so they
//! survive every subsequent compaction unchanged, exactly like
//! `previous_summary`.

use crate::conversation::Conversation;
use crate::llm::Llm;
use crate::llm::request;
use crate::llm::response::StopReason;
use crate::session::connection::Connection;
use crate::session::log::compose_summary;
use crate::voice::{self, FileOps};

/// The system prompt for the summarization call, mirroring baud's inline
/// prompt (the harness owns the wording; the Voice module owns the body).
const COMPACTION_SYSTEM: &str = "You are a summarization assistant. \
Extract only facts. Produce the structured sections requested.";

/// Compaction state and execution for semantic Conversation summarization.
///
/// Tracks `previous_summary` and accumulated `file_ops` for subsequent
/// compactions, and `original_task` (the verbatim first user text, captured
/// once) so the harness can guarantee the identifier the model can never be
/// trusted with. Mirrors baud's `%Baud.Compaction{}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    /// The text of the last compaction's narrative output, fed into the next
    /// compaction's prompt for a telescoping view. `None` before any.
    pub previous_summary: Option<String>,
    /// The verbatim original task statement, captured once on the first
    /// compaction. `None` before it is known.
    pub original_task: Option<String>,
    /// The accumulated read/modified files across compactions.
    pub file_ops: FileOps,
}

impl Compaction {
    /// Fresh compaction state: no prior summary, no original task, no file ops.
    pub fn new() -> Self {
        Compaction {
            previous_summary: None,
            original_task: None,
            file_ops: FileOps::default(),
        }
    }

    /// Checks whether the Conversation's token estimate exceeds the Compaction
    /// Target (the same low-water mark Eviction settles to), meaning Proactive
    /// Compaction should fire before the Turn's first Pass.
    pub fn proactive(conv: &Conversation) -> bool {
        conv.token_estimate() > conv.compaction_target()
    }

    /// Runs one compaction cycle:
    ///
    /// 1. Prepares the compaction span via [`Conversation::prepare_compaction`]
    /// 2. Serializes the messages for the summarization prompt
    /// 3. Calls the LLM via the [`Llm`] boundary
    /// 4. Applies the summary via [`Conversation::apply_compaction`]
    /// 5. Returns the compacted Conversation and the updated compaction state
    ///
    /// Returns `Ok((compacted, new_state))` or `Err(reason)`.
    pub async fn run(
        &self,
        conv: &Conversation,
        llm: &dyn Llm,
        connection: &Connection,
    ) -> Result<(Conversation, Compaction), String> {
        let (to_summarize, cutoff_index, new_ops) = match conv.prepare_compaction() {
            None => return Err("nothing_to_compact".to_string()),
            Some(prepared) => prepared,
        };

        // Captured once, then carried in the struct across compactions — after
        // the first compaction the head of the Conversation is a summary
        // message, so only the carried value preserves it.
        let merged_ops = merge_ops(&self.file_ops, &new_ops);
        let original_task: Option<String> = self
            .original_task
            .clone()
            .or_else(|| conv.original_task().map(|s| s.to_string()));

        let serialized =
            voice::serialize_for_compaction(&to_summarize, self.previous_summary.as_deref());

        let prompt = format!(
            "You are a summarization assistant. Summarize the coding session below. \
Extract only facts. Produce the structured sections requested.\n\n{}\n\n{}",
            voice::compaction_prompt(),
            serialized
        );

        // A tool-free request over a single user message (no `tools` key, per
        // build's omit-when-empty rule).
        let messages = vec![crate::content::Message::user(vec![
            crate::content::ContentBlock::text(prompt),
        ])];
        let wire = request::build(COMPACTION_SYSTEM, &messages, &[], connection, false);

        let response = llm.complete(wire, connection, &mut |_ev| {}).await;

        if response.stop_reason == StopReason::Error {
            return Err(response.error.unwrap_or_else(|| "llm_error".to_string()));
        }

        let narrative = extract_summary(&response.content);
        // The model writes narrative; the harness appends the verbatim task
        // and accumulated file_ops mechanically, outside the LLM output.
        // compose_summary is the SINGLE source (reused by the log fold).
        let summary = compose_summary(&narrative, original_task.as_deref(), &merged_ops);
        let compacted = conv.apply_compaction(&summary, cutoff_index);

        let new_state = Compaction {
            previous_summary: Some(narrative),
            original_task,
            file_ops: merged_ops,
        };

        Ok((compacted, new_state))
    }

    /// Convenience wrapper for use as a Turn `compact` Dep capture: runs and
    /// drops the new state — the caller fires the state update separately.
    /// Returns `Ok(conversation)` or `Err(reason)`.
    pub async fn recovery_capture(
        &self,
        conv: &Conversation,
        llm: &dyn Llm,
        connection: &Connection,
    ) -> Result<Conversation, String> {
        self.run(conv, llm, connection)
            .await
            .map(|(compacted, _new_state)| compacted)
    }

    /// The number of messages a compaction replaced with its single summary
    /// message: the count folded away, used as the log entry's `skip_count`.
    /// `apply_compaction` swaps `cutoff_index` messages for one summary, so the
    /// drop is `before - (after - 1)`.
    pub fn skip_count(before: &Conversation, compacted: &Conversation) -> usize {
        before.messages.len() - compacted.messages.len() + 1
    }

    /// Builds the [`SessionLogEntry`] from state plus the two forensic counts.
    ///
    /// `summary` is the model's narrative ALONE (`previous_summary`); the
    /// mechanical facts ride as their own fields so the fold recomposes a
    /// byte-identical message. `skip_count` and `tokens_before` are forensic
    /// only.
    pub fn session_log_entry(&self, skip_count: usize, tokens_before: u64) -> SessionLogEntry {
        SessionLogEntry {
            summary: self.previous_summary.clone(),
            skip_count,
            tokens_before,
            file_ops: self.file_ops.clone(),
            original_task: self.original_task.clone(),
        }
    }
}

impl Default for Compaction {
    fn default() -> Self {
        Compaction::new()
    }
}

/// The `{:compacted, ...}` Session Log entry for a completed compaction. The
/// Agent appends this so a Session that compacts round-trips through Resume:
/// the fold discards everything before the marker and reconstructs the summary
/// message from these fields via [`compose_summary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLogEntry {
    pub summary: Option<String>,
    pub skip_count: usize,
    pub tokens_before: u64,
    pub file_ops: FileOps,
    pub original_task: Option<String>,
}

// Extracts the summary text from the LLM response content blocks.
fn extract_summary(content: &[crate::content::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            crate::content::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Merges new file ops into current ones, deduplicating and sorting.
fn merge_ops(a: &FileOps, b: &FileOps) -> FileOps {
    use std::collections::BTreeSet;
    let reads: BTreeSet<String> = a
        .read_files
        .iter()
        .chain(b.read_files.iter())
        .cloned()
        .collect();
    let mods: BTreeSet<String> = a
        .modified_files
        .iter()
        .chain(b.modified_files.iter())
        .cloned()
        .collect();
    FileOps {
        read_files: reads.into_iter().collect(),
        modified_files: mods.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ContentBlock, Role, Usage};
    use crate::conversation::ConversationOpts;
    use crate::llm::response::{Response, StopReason};
    use crate::test_support::{Entry, FakeLlm};

    fn connection() -> Connection {
        Connection::new("http://test:4000/v1", "", "test-model", 4000)
    }

    fn ok_response(text: &str) -> Response {
        Response {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            error: None,
        }
    }

    fn opts() -> ConversationOpts {
        ConversationOpts::new(2000, 500).eviction_slack(0.0)
    }

    fn conversation_with_turns(count: u64) -> Conversation {
        let mut conv = Conversation::new("You are Baud.", opts());
        add_turns(&mut conv, count);
        conv
    }

    fn add_turns(conv: &mut Conversation, n: u64) {
        for i in (1..=n).rev() {
            let content = format!("{}: turn {i}", "line ".repeat(50));
            conv.add_user_text(content.clone());
            conv.add_assistant_blocks(vec![ContentBlock::text(content)]);
        }
    }

    fn conversation_with_file_ops() -> Conversation {
        let mut conv = Conversation::new("You are Baud.", opts());
        for _ in 0..10 {
            conv.add_user_text("edit a file with many lines of content to fill budget");
            conv.add_assistant_blocks(vec![ContentBlock::tool_use(
                "t1",
                "edit_file",
                serde_json::json!({"path": "lib/foo.ex"}),
            )]);
            conv.add_tool_results(
                vec![ContentBlock::tool_result("t1", "edited".repeat(100), false)],
                Vec::new(),
            );
            conv.add_assistant_blocks(vec![ContentBlock::text("done ".repeat(50))]);
        }
        conv
    }

    fn message_text(msg: &crate::content::Message) -> String {
        msg.content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ---- new ----

    #[test]
    fn new_returns_fresh_state_with_no_summary_and_empty_file_ops() {
        let state = Compaction::new();
        assert_eq!(state.previous_summary, None);
        assert_eq!(state.file_ops, FileOps::default());
    }

    // ---- proactive ----

    #[test]
    fn fires_at_the_compaction_target_not_the_budget_target() {
        let mut conv = Conversation::new("", ConversationOpts::new(1000, 200).eviction_slack(0.3));
        conv.add_user_text("a".repeat(2100));

        assert!(conv.token_estimate() > conv.compaction_target());
        assert!(conv.token_estimate() <= 800);
        assert!(Compaction::proactive(&conv));
    }

    #[test]
    fn returns_true_when_estimate_exceeds_target() {
        let mut conv = Conversation::new("You are Baud.", ConversationOpts::new(1000, 200));
        conv.add_user_text("a ".repeat(700));
        conv.add_assistant_blocks(vec![ContentBlock::text("b ".repeat(700))]);
        assert!(Compaction::proactive(&conv));
    }

    #[test]
    fn returns_false_when_estimate_is_within_budget() {
        let mut conv = Conversation::new("You are Baud.", ConversationOpts::new(100_000, 4000));
        conv.add_user_text("short");
        assert!(!Compaction::proactive(&conv));
    }

    // ---- run ----

    #[tokio::test]
    async fn returns_nothing_to_compact_for_a_bare_conversation() {
        let conv = Conversation::new("You are Baud.", ConversationOpts::new(2000, 500));
        let fake = FakeLlm::script(Vec::<Entry>::new());
        let result = Compaction::new().run(&conv, &fake, &connection()).await;
        assert_eq!(result, Err("nothing_to_compact".to_string()));
    }

    #[tokio::test]
    async fn returns_ok_compacted_new_state_with_a_scripted_llm() {
        let fake = FakeLlm::script(vec![Entry::just(ok_response(
            "## Goal\nTest compaction\n## Progress\n### Done\n- completed",
        ))]);
        let conv = conversation_with_turns(5);
        let (compacted, new_state) = Compaction::new()
            .run(&conv, &fake, &connection())
            .await
            .unwrap();

        assert!(compacted.messages.len() < conv.messages.len());

        let summary_msg = &compacted.messages[0];
        assert_eq!(summary_msg.role, Role::User);
        assert!(
            summary_msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. }))
        );

        assert!(new_state.previous_summary.is_some());
        assert!(
            new_state
                .previous_summary
                .as_deref()
                .unwrap()
                .contains("Goal")
        );
        assert_eq!(new_state.file_ops, FileOps::default());
    }

    #[tokio::test]
    async fn returns_error_when_llm_fails() {
        let fake = FakeLlm::script(vec![Entry::error("server_busy")]);
        let conv = conversation_with_turns(5);
        let result = Compaction::new().run(&conv, &fake, &connection()).await;
        assert_eq!(result, Err("server_busy".to_string()));
    }

    #[tokio::test]
    async fn merges_file_ops_from_the_compacted_messages() {
        let fake = FakeLlm::script(vec![Entry::just(ok_response("summary of edits"))]);
        let conv = conversation_with_file_ops();

        let prev_state = Compaction {
            previous_summary: None,
            original_task: None,
            file_ops: FileOps {
                read_files: vec!["lib/other.ex".to_string()],
                modified_files: Vec::new(),
            },
        };

        let (_compacted, new_state) = prev_state.run(&conv, &fake, &connection()).await.unwrap();

        assert_eq!(
            new_state.file_ops.read_files,
            vec!["lib/other.ex".to_string()]
        );
        assert_eq!(
            new_state.file_ops.modified_files,
            vec!["lib/foo.ex".to_string()]
        );
    }

    // ---- original task capture and mechanical facts ----

    #[tokio::test]
    async fn captures_the_verbatim_first_user_text_on_the_first_compaction() {
        let fake = FakeLlm::script(vec![Entry::just(ok_response("narrative summary"))]);
        let task = "Implement the widget factory in lib/widget.ex";

        let mut conv = Conversation::new("You are Baud.", opts());
        conv.add_user_text(task);
        add_turns(&mut conv, 5);

        let (_compacted, new_state) = Compaction::new()
            .run(&conv, &fake, &connection())
            .await
            .unwrap();
        assert_eq!(new_state.original_task.as_deref(), Some(task));
    }

    #[tokio::test]
    async fn the_summary_message_carries_the_verbatim_task_even_when_the_model_returns_garbage() {
        let fake = FakeLlm::script(vec![Entry::just(ok_response(
            "!!! nonsense narrative that ignores the template",
        ))]);
        let task = "Rename the User struct to Account everywhere";

        let mut conv = Conversation::new("You are Baud.", opts());
        conv.add_user_text(task);
        add_turns(&mut conv, 5);

        let (compacted, _new_state) = Compaction::new()
            .run(&conv, &fake, &connection())
            .await
            .unwrap();

        let summary_text = message_text(&compacted.messages[0]);
        assert!(summary_text.contains(task));
    }

    #[tokio::test]
    async fn the_summary_message_mechanically_appends_accumulated_file_ops() {
        let fake = FakeLlm::script(vec![Entry::just(ok_response("narrative"))]);
        let conv = conversation_with_file_ops();

        let prev_state = Compaction {
            previous_summary: None,
            original_task: Some("the original task".to_string()),
            file_ops: FileOps {
                read_files: vec!["lib/other.ex".to_string()],
                modified_files: Vec::new(),
            },
        };

        let (compacted, _new_state) = prev_state.run(&conv, &fake, &connection()).await.unwrap();

        let summary_text = message_text(&compacted.messages[0]);
        assert!(summary_text.contains("lib/other.ex"));
        assert!(summary_text.contains("lib/foo.ex"));
    }

    #[tokio::test]
    async fn the_original_task_survives_a_second_compaction_unchanged() {
        let fake = FakeLlm::script(vec![
            Entry::just(ok_response("first")),
            Entry::just(ok_response("second")),
        ]);
        let task = "Add pagination to the reports endpoint";

        let mut conv = Conversation::new("You are Baud.", opts());
        conv.add_user_text(task);
        add_turns(&mut conv, 5);

        let (mut compacted1, state1) = Compaction::new()
            .run(&conv, &fake, &connection())
            .await
            .unwrap();
        assert_eq!(state1.original_task.as_deref(), Some(task));

        // Grow the conversation again and compact a second time. The summary
        // message the first compaction produced no longer starts with the
        // original user text, so only the carried state can preserve it.
        add_turns(&mut compacted1, 5);
        let (compacted2, state2) = state1.run(&compacted1, &fake, &connection()).await.unwrap();

        assert_eq!(state2.original_task.as_deref(), Some(task));

        let summary_text = message_text(&compacted2.messages[0]);
        assert!(summary_text.contains(task));
    }

    // ---- recovery_capture ----

    #[tokio::test]
    async fn recovery_capture_returns_ok_conversation_on_success() {
        let fake = FakeLlm::script(vec![Entry::just(ok_response("summary data"))]);
        let conv = conversation_with_turns(5);

        let compacted = Compaction::new()
            .recovery_capture(&conv, &fake, &connection())
            .await
            .unwrap();
        assert!(compacted.messages.len() < conv.messages.len());
    }

    #[tokio::test]
    async fn recovery_capture_returns_error_on_llm_failure() {
        let fake = FakeLlm::script(vec![Entry::error("timeout")]);
        let conv = conversation_with_turns(5);
        let result = Compaction::new()
            .recovery_capture(&conv, &fake, &connection())
            .await;
        assert_eq!(result, Err("timeout".to_string()));
    }

    // ---- session_log_entry ----

    #[test]
    fn builds_the_compacted_log_entry_from_state_and_forensic_counts() {
        let state = Compaction {
            previous_summary: Some("the model narrative".to_string()),
            original_task: Some("the verbatim task".to_string()),
            file_ops: FileOps {
                read_files: vec!["lib/a.ex".to_string()],
                modified_files: vec!["lib/b.ex".to_string()],
            },
        };

        let entry = state.session_log_entry(7, 1234);
        assert_eq!(
            entry,
            SessionLogEntry {
                summary: Some("the model narrative".to_string()),
                skip_count: 7,
                tokens_before: 1234,
                file_ops: FileOps {
                    read_files: vec!["lib/a.ex".to_string()],
                    modified_files: vec!["lib/b.ex".to_string()],
                },
                original_task: Some("the verbatim task".to_string()),
            }
        );
    }

    // ---- skip_count ----

    #[tokio::test]
    async fn counts_the_messages_a_compaction_folded_into_the_single_summary() {
        let fake = FakeLlm::script(vec![Entry::just(ok_response("narr"))]);
        let conv = conversation_with_turns(5);
        let (compacted, _state) = Compaction::new()
            .run(&conv, &fake, &connection())
            .await
            .unwrap();

        let expected = conv.messages.len() - (compacted.messages.len() - 1);
        assert_eq!(Compaction::skip_count(&conv, &compacted), expected);
        assert!(Compaction::skip_count(&conv, &compacted) > 0);
    }
}
