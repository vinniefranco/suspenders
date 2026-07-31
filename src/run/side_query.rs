//! [`LlmSideQuery`] - the real [`SideQuery`] wired DIRECT to the [`Llm`]
//! boundary (P2b, ADR-0055).
//!
//! A side-query is a bounded model prompt a Tool Call runs OFF the main
//! Conversation (web_fetch's prompt-guided extraction). Unlike the Approver -
//! whose real impl relays over the Agent mpsc because approval is an Agent-owned
//! decision - a side-query mutates no Agent/Conversation state: it checkpoints
//! nothing, logs nothing, and never touches the next-speaker fold. Its only
//! effect is a completion the Run already owns (the captured [`Llm`] + [`Model`]),
//! so the real impl is simply that boundary called with a transient request. It
//! lives here, at the Llm boundary the Run captured, rather than in the Agent -
//! `caps.rs` stays free of any `agent`/`run` import (Ports and Adapters).
//!
//! This mirrors [`crate::run::next_speaker`], the other genuine side-query: it
//! builds its own [`LlmRequest`], disables Thinking (`no_think`), streams to a
//! no-op sink (invisible to the Transcript), and never enters the Conversation.

use std::sync::Arc;

use crate::content::{ContentBlock, Message};
use crate::llm::model::Model;
use crate::llm::{Llm, LlmRequest, StreamEvent};
use crate::tool::caps::{SideQuery, SideQueryRequest};

/// The Llm-backed [`SideQuery`]: the captured boundary plus the Run's main
/// [`Model`] and sampling temperature. Built by [`crate::run::run`] from the
/// [`crate::run::Capture`]'s `llm`/`model` (no new Capture field - the Run
/// already holds both), and carried on the Tool [`crate::tool::caps::Capabilities`].
pub struct LlmSideQuery {
    /// The captured LLM boundary (ADR-0020): the same `Arc<dyn Llm>` the Run's
    /// completions travel, so a side-query hits the same Provider set.
    pub llm: Arc<dyn Llm>,
    /// The Run's captured main Model (ADR-0033): the default a side-query runs
    /// on when the request pins none. web_fetch pins the main model by passing
    /// `None`, deferring to this (qwen's `model: this.config.getModel()`).
    pub model: Model,
    /// The Session-resolved sampling temperature (ADR-0037), applied to the
    /// side-query request like every other completion this Run makes.
    pub temperature: Option<f64>,
}

#[async_trait::async_trait]
impl SideQuery for LlmSideQuery {
    async fn run(&self, request: SideQueryRequest) -> Result<String, String> {
        // The transient request: the extraction system instruction, the single
        // user text part, NO tools. Thinking off (qwen's `includeThoughts:
        // false` for side queries) - a reasoning model would spend its whole
        // budget thinking and lose the extraction. Temperature rides it like any
        // completion.
        let req = LlmRequest::new(
            request.system,
            vec![Message::user(vec![ContentBlock::text(
                request.user_content,
            )])],
            Vec::new(),
        )
        .with_no_think(true)
        .with_temperature(self.temperature);

        // Pin the request's Model, or default to the Run's captured main Model
        // (web_fetch passes `None`, so this resolves to the main model - qwen
        // pins the main model for long, rich source material).
        let model = request.model.unwrap_or_else(|| self.model.clone());

        // A no-op sink: the side-query is invisible to the operator's Transcript,
        // exactly like next_speaker's check - no MessageStart/Update/End grammar,
        // nothing streamed.
        let mut sink = |_ev: &StreamEvent| {};

        // Best-effort retry loop up to `max_attempts`: a non-empty reply wins;
        // an empty one (or an error Response) retries until the budget is spent.
        // web_fetch passes 1, so this is a single attempt whose failure the tool
        // folds into its own error shape. `max_attempts` of 0 is treated as one
        // attempt - a bounded query always tries at least once.
        let attempts = request.max_attempts.max(1);
        let mut last_error: Option<String> = None;
        for _ in 0..attempts {
            let response = self.llm.complete(&req, &model, &mut sink).await;
            if let Some(reason) = response.error.as_ref() {
                last_error = Some(reason.clone());
                continue;
            }
            let text = reply_text(&response.content);
            if !text.is_empty() {
                return Ok(text);
            }
            last_error = Some("side query returned no text".to_string());
        }
        Err(last_error.unwrap_or_else(|| "side query returned no text".to_string()))
    }
}

/// The concatenated text of a side-query reply's Text blocks (Thinking and Tool
/// Use blocks ignored) - the same accessor shape [`crate::run::next_speaker`]
/// reads its reply through, kept local so neither side-query leaks a Response
/// text helper into the boundary.
fn reply_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
#[path = "../../tests/unit/run/side_query.rs"]
mod tests;
