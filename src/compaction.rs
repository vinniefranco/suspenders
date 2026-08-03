//! Compaction - summarising history to fit the context window (ADR-0012).
//!
//! ## What compaction is
//!
//! When the Conversation's token estimate approaches the Context Budget, old
//! Runs are summarized by the LLM and replaced with a structured markdown
//! summary. Unlike Eviction (which mechanically hollows out Tool Results),
//! compaction is semantic - it extracts what was accomplished, what decisions
//! were made, and what files were touched.
//!
//! ## An effect, not part of the pure loop
//!
//! [`Compaction::run`] calls the [`Llm`] boundary directly - it is an effect,
//! invoked via the Run's `compact` Dep in production (ADR-0012), NOT inside
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
use crate::llm::model::Model;
use crate::llm::response::StopReason;
use crate::llm::{Llm, LlmRequest};
use crate::session::log::compose_summary;
use crate::voice::{self, FileOps};

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
    /// Compaction should fire before the Run's first Pass. The single
    /// definition of the trigger (baud's `Baud.Compaction.proactive?/1`):
    /// callers consult this rather than restating the comparison.
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
        model: &Model,
        temperature: Option<f64>,
    ) -> Result<(Conversation, Compaction), String> {
        let (to_summarize, cutoff_index, new_ops) = match conv.prepare_compaction() {
            None => return Err("nothing_to_compact".to_string()),
            Some(prepared) => prepared,
        };

        // Captured once, then carried in the struct across compactions - after
        // the first compaction the head of the Conversation is a summary
        // message, so only the carried value preserves it.
        let merged_ops = merge_ops(&self.file_ops, &new_ops);
        let original_task: Option<String> = self
            .original_task
            .clone()
            .or_else(|| conv.original_task().map(|s| s.to_string()));

        let narrative = self
            .summarize(&to_summarize, llm, model, temperature)
            .await?;
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

    // The one summarization call (ADR-0012's silent LLM call): serialize the
    // messages behind the previous summary, ask for the structured narrative,
    // and extract it. Used by [`run`](Compaction::run).
    async fn summarize(
        &self,
        messages: &[crate::content::Message],
        llm: &dyn Llm,
        model: &Model,
        temperature: Option<f64>,
    ) -> Result<String, String> {
        let serialized =
            voice::serialize_for_compaction(messages, self.previous_summary.as_deref());

        // qwen's `getCompressionPrompt()` is the system prompt; the serialized
        // conversation is the single user message. A tool-free request: no tools
        // offered (the adapter omits the `tools` key when empty), Thinking left on.
        let messages = vec![crate::content::Message::user(vec![
            crate::content::ContentBlock::text(serialized),
        ])];
        let req = LlmRequest::new(voice::compaction_prompt(), messages, Vec::new())
            .with_temperature(temperature);

        let response = llm.complete(&req, model, &mut |_ev| {}).await;

        if response.stop_reason == StopReason::Error {
            return Err(response.error.unwrap_or_else(|| "llm_error".to_string()));
        }

        Ok(extract_summary(&response.content))
    }

    /// Convenience wrapper for use as a Run `compact` Dep capture: runs and
    /// drops the new state - the caller fires the state update separately.
    /// Returns `Ok(conversation)` or `Err(reason)`.
    #[cfg(test)]
    pub async fn recovery_capture(
        &self,
        conv: &Conversation,
        llm: &dyn Llm,
        model: &Model,
        temperature: Option<f64>,
    ) -> Result<Conversation, String> {
        self.run(conv, llm, model, temperature)
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

// Extracts the summary text from the LLM response content blocks, stripping the
// model's `<analysis>` drafting scratchpad (qwen's `stripAnalysis`). The
// compression prompt asks the model to wrap its chain-of-thought in an
// `<analysis>...</analysis>` block that is purely for its own benefit; keeping it
// in history wastes tokens and degrades signal for the resuming agent.
fn extract_summary(content: &[crate::content::ContentBlock]) -> String {
    let joined = content
        .iter()
        .filter_map(|b| match b {
            crate::content::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    strip_analysis(&joined)
}

// Removes `<analysis>...</analysis>` blocks (and an unclosed trailing
// `<analysis>` with no matching close) from a raw summary. If stripping removes
// everything - the model produced ONLY an analysis block - fall back to the raw
// text so the caller sees something rather than an empty summary.
fn strip_analysis(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("<analysis>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</analysis>") {
            Some(end) => rest = &rest[start + end + "</analysis>".len()..],
            // Unclosed tag: drop everything from `<analysis>` to the end.
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    let stripped = out.trim();
    if stripped.is_empty() {
        raw.trim().to_string()
    } else {
        stripped.to_string()
    }
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
#[path = "../tests/compaction.rs"]
mod tests;
