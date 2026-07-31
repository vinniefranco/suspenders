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
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::llm::model::Api;
    use crate::llm::response::{Response, StopReason};
    use crate::test_support::{Entry, FakeLlm};

    fn model() -> Model {
        Model::new("local", "m", Api::AnthropicMessages, 64_000, 100)
    }

    fn text_response(text: &str) -> Response {
        Response {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
            error: None,
        }
    }

    // A recording of one captured (request, model) pair - `Entry::dynamic` writes
    // it as the side-query calls `complete` (FakeLlm records nothing itself).
    type Captured = Arc<Mutex<Vec<(LlmRequest, Model)>>>;

    // A `dynamic` entry that records the request+model it saw, then returns
    // `reply`.
    fn recording(captured: Captured, reply: Response) -> Entry {
        Entry::dynamic(vec![], move |req: &LlmRequest, model: &Model| {
            captured.lock().unwrap().push((req.clone(), model.clone()));
            reply.clone()
        })
    }

    // A side-query over a scripted FakeLlm, on the default main model.
    fn side_query_over(entries: Vec<Entry>) -> LlmSideQuery {
        LlmSideQuery {
            llm: Arc::new(FakeLlm::script(entries)),
            model: model(),
            temperature: None,
        }
    }

    // Run a standard two-attempt request against the scripted side-query and
    // surface the error - the shared body of the retry-exhaustion tests.
    async fn err_after(entries: Vec<Entry>) -> String {
        side_query_over(entries)
            .run(SideQueryRequest {
                system: "sys".into(),
                user_content: "content".into(),
                model: None,
                max_attempts: 2,
            })
            .await
            .unwrap_err()
    }

    // The side-query runs the captured Llm with the request's system + single
    // user part, NO tools, Thinking off - and returns the concatenated reply.
    #[tokio::test]
    async fn runs_the_llm_and_returns_the_reply() {
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeLlm::script(vec![recording(
            Arc::clone(&captured),
            text_response("extracted"),
        )]);
        let sq = LlmSideQuery {
            llm: Arc::new(fake),
            model: model(),
            temperature: Some(0.3),
        };

        let out = sq
            .run(SideQueryRequest {
                system: "extract it".into(),
                user_content: "the content".into(),
                model: None,
                max_attempts: 1,
            })
            .await
            .unwrap();
        assert_eq!(out, "extracted");

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let (req, seen_model) = &captured[0];
        assert_eq!(req.system, "extract it");
        assert!(req.tools.is_empty());
        assert!(req.no_think);
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.messages.len(), 1);
        assert!(matches!(req.messages[0].role, crate::content::Role::User));
        assert!(
            matches!(&req.messages[0].content[0], ContentBlock::Text { text } if text == "the content")
        );
        // `None` on the request defaulted to the captured main Model.
        assert_eq!(seen_model.scoped_id(), model().scoped_id());
    }

    // An empty reply retries up to `max_attempts`; a later non-empty reply wins.
    #[tokio::test]
    async fn retries_on_empty_up_to_max_attempts_then_succeeds() {
        let fake = FakeLlm::script(vec![
            Entry::just(text_response("")),
            Entry::just(text_response("second")),
        ]);
        let sq = LlmSideQuery {
            llm: Arc::new(fake),
            model: model(),
            temperature: None,
        };

        let out = sq
            .run(SideQueryRequest {
                system: "sys".into(),
                user_content: "content".into(),
                model: None,
                max_attempts: 2,
            })
            .await
            .unwrap();
        assert_eq!(out, "second");
    }

    // Every attempt empty: the bounded loop gives up with an Err, never looping
    // forever.
    #[tokio::test]
    async fn all_attempts_empty_errs() {
        let err = err_after(vec![
            Entry::just(text_response("")),
            Entry::just(text_response("")),
        ])
        .await;
        assert_eq!(err, "side query returned no text");
    }

    // An error Response retries too, then surfaces the last error string.
    #[tokio::test]
    async fn an_error_response_retries_then_surfaces_the_error() {
        let err = err_after(vec![Entry::error("boom"), Entry::error("boom again")]).await;
        assert_eq!(err, "boom again");
    }

    // A pinned Model on the request overrides the captured main Model.
    #[tokio::test]
    async fn a_pinned_request_model_overrides_the_captured_main_model() {
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeLlm::script(vec![recording(Arc::clone(&captured), text_response("ok"))]);
        let sq = LlmSideQuery {
            llm: Arc::new(fake),
            model: model(),
            temperature: None,
        };
        let pinned = Model::new("other", "fast", Api::OpenaiCompletions, 32_000, 100);

        sq.run(SideQueryRequest {
            system: "sys".into(),
            user_content: "content".into(),
            model: Some(pinned.clone()),
            max_attempts: 1,
        })
        .await
        .unwrap();

        assert_eq!(
            captured.lock().unwrap()[0].1.scoped_id(),
            pinned.scoped_id()
        );
    }
}
