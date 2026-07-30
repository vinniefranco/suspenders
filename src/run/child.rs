//! [`ChildDeps`] - the [`RunDeps`] a child Run drives on (P4/F4, ADR-0061).
//!
//! A foreground subagent is a child Run driven INLINE off the captured
//! [`Llm`](crate::llm::Llm) boundary - it needs no Agent actor. Where the
//! parent Run's [`crate::agent::deps::AgentDeps`] threads every effect over the
//! Agent's mpsc, the child threads only `complete` (the model call) to a real
//! boundary; every other effect is a no-op or the safe/degraded answer. This is
//! the isolation seam: because the child's `emitter`, `checkpoint`, and
//! `set_plan` never touch the parent's channel or Conversation, the child's
//! whole reasoning loop is invisible to the parent's Session Log and Transcript,
//! and its settlement reaches the parent ONLY as the subagent's returned result
//! (ADR-0061).
//!
//! `complete` mirrors [`crate::agent::deps::AgentDeps::complete`]: it attaches
//! the Session's request settings and calls the captured Llm with the child's
//! captured Model. `emitter` emits into the child `sink` when one is present
//! (the background path, DEFERRED) or a NO-OP Emitter otherwise (the foreground
//! path) - NEVER the parent Agent mpsc. `request_approval` returns `false`: a
//! foreground subagent has no modal to answer a gated command (its tool subset
//! excludes `ask_user_question` and its Approver is a `DenyingApprover`), so it
//! denies. The rest ride the [`RunDeps`] defaults or are no-ops.

use std::future::Future;

use crate::content::Provenance;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::model::Model;
use crate::llm::response::Response;
use crate::llm::{Llm, LlmRequest, StreamEvent, ToolCallStyle};
use crate::run::deps::{Emitter, RunDeps};
use std::sync::Arc;

/// A child Run's event sink (P4/F4, ADR-0061): where the child's [`Event`]s go
/// when the caller wants to observe the child's progress (the background path's
/// live-output feed, DEFERRED). The foreground path passes `None`, so the child
/// [`Emitter`] is a NO-OP and the child's events go nowhere - the isolation that
/// keeps the child out of the parent's Session Log and Transcript.
pub type ChildSink = Box<dyn FnMut(Event) + Send>;

/// The [`RunDeps`] a child Run drives on (P4/F4, ADR-0061). Holds the captured
/// [`Llm`] boundary + the child's Model and request settings for `complete`, and
/// the optional child event `sink`. Every effect other than `complete` is a
/// no-op or the safe/degraded answer.
pub struct ChildDeps {
    /// The captured LLM boundary (ADR-0020): the SAME `Arc<dyn Llm>` the parent
    /// Run uses (the Dispatcher), so the child's Model routes to its Provider
    /// over the shared boundary - no per-child Llm is built.
    pub llm: Arc<dyn Llm>,
    /// The child's captured Model (the per-subagent seam, ADR-0061): every
    /// completion the child makes travels to it. For an `Inherit` subagent this
    /// is the parent's Model; for a `Scoped`/overridden subagent it is the
    /// resolved other Model.
    pub model: Model,
    /// The Session-resolved sampling temperature, applied to every child
    /// completion (like [`crate::agent::deps::AgentDeps`]).
    pub temperature: Option<f64>,
    /// The Session-resolved extended-thinking budget, applied to every child
    /// completion.
    pub thinking_budget: Option<u64>,
    /// The Session-resolved Tool Call style, applied to every child completion.
    pub tool_call_style: ToolCallStyle,
    /// The optional child event sink (background path). `None` on the foreground
    /// path, so the child [`Emitter`] is a no-op.
    pub sink: Option<ChildSink>,
}

impl RunDeps for ChildDeps {
    fn complete(
        &mut self,
        req: LlmRequest,
        on_event: &mut (dyn FnMut(&StreamEvent) + Send),
    ) -> impl Future<Output = Response> + Send {
        // Identical to AgentDeps::complete: attach the Session's temperature,
        // thinking budget, and Tool Call style, then call the captured boundary
        // with the child's Model, forwarding each StreamEvent to on_event.
        let req = req
            .with_temperature(self.temperature)
            .with_thinking_budget(self.thinking_budget)
            .with_tool_call_style(self.tool_call_style);
        let llm = Arc::clone(&self.llm);
        let model = self.model.clone();
        async move {
            let mut adapter = |ev: &StreamEvent| on_event(ev);
            llm.complete(&req, &model, &mut adapter).await
        }
    }

    fn provenance(&self) -> Provenance {
        self.model.provenance()
    }

    fn emitter(&mut self) -> Emitter {
        // The isolation seam (ADR-0061): emit into the child `sink` if the
        // caller supplied one (the background live-output feed, DEFERRED), else
        // a NO-OP. NEVER the parent Agent mpsc - this is what keeps the child's
        // events out of the parent's Session Log and Transcript. `Emitter` holds
        // a single `Box<dyn FnMut>`, so the sink moves into it once here; a child
        // has exactly one Emitter for its whole run.
        match self.sink.take() {
            Some(sink) => Emitter::new(sink),
            None => Emitter::new(|_event| {}),
        }
    }

    fn drain_steering(&mut self) -> impl Future<Output = Vec<String>> + Send {
        // A child Run has no Steering queue - the parent cannot steer it mid-run
        // (foreground subagents settle synchronously through the tool result).
        std::future::ready(Vec::new())
    }

    fn request_approval(
        &mut self,
        _id: String,
        _command: String,
    ) -> impl Future<Output = bool> + Send {
        // A foreground subagent has no modal to answer a gated command through,
        // so it denies (the safe answer). Its tool subset excludes
        // `ask_user_question` and its Approver is a DenyingApprover, so the model
        // never reaches an interactive gate anyway.
        std::future::ready(false)
    }

    fn checkpoint(&mut self, _conversation: &Conversation) {
        // No-op: a child Run's partial state is not persisted to the parent's
        // Session Log (the child's whole run is invisible until it settles).
    }

    fn set_plan(&mut self, _plan: String) {
        // No-op: a child Run holds no Plan outside its own Conversation.
    }

    // `after_pass` and `compact` take the RunDeps defaults (Continue; no
    // compactor) - a child Run runs to its turn bound without an after-Pass hook
    // and without Compaction.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::llm::model::Api;

    fn model() -> Model {
        Model::new("local", "m", Api::AnthropicMessages, 64_000, 100)
    }

    fn deps(sink: Option<ChildSink>) -> ChildDeps {
        ChildDeps {
            llm: Arc::new(crate::test_support::FakeLlm::script(vec![])),
            model: model(),
            temperature: None,
            thinking_budget: None,
            tool_call_style: ToolCallStyle::default(),
            sink,
        }
    }

    #[test]
    fn no_sink_emitter_is_a_no_op() {
        let mut d = deps(None);
        let mut emitter = d.emitter();
        // Emitting through a no-op Emitter neither panics nor reaches anywhere.
        emitter.emit(Event::run_started("ref"));
    }

    #[test]
    fn a_sink_receives_the_childs_events() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let sink: ChildSink = Box::new(move |event| sink_seen.lock().unwrap().push(event));
        let mut d = deps(Some(sink));
        let mut emitter = d.emitter();
        emitter.emit(Event::run_started("ref"));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn request_approval_denies() {
        let mut d = deps(None);
        assert!(!d.request_approval("id".into(), "rm -rf /".into()).await);
    }

    #[tokio::test]
    async fn drain_steering_is_empty() {
        let mut d = deps(None);
        assert!(d.drain_steering().await.is_empty());
    }

    #[test]
    fn provenance_is_the_models() {
        let d = deps(None);
        assert_eq!(d.provenance(), model().provenance());
    }
}
