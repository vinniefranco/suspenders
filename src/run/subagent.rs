//! [`DirectSubagentSpawner`] - the real [`SubagentSpawner`] wired DIRECT to the
//! child-Run factory (P4/F4, ADR-0061).
//!
//! Like [`crate::run::side_query::LlmSideQuery`], this capability's real impl
//! bypasses the Agent mpsc: a foreground subagent is a child Run driven inline
//! off the captured [`Llm`](crate::llm::Llm), so the spawner needs only the
//! handles the Run already holds (the Llm, the parent Model, the Session's
//! request settings) plus the [`SubagentRegistry`] the Agent built. A
//! per-subagent `Scoped` Model resolves through the Session's own
//! [`resolve_model`](crate::session::Session::resolve_model), so the Session
//! already carries the Provider set the resolve reads - no separate slice is
//! held. It lives here, at the Run's Llm boundary, rather than in the Agent -
//! `caps.rs` stays free of any `agent`/`run` import (Ports and Adapters).
//!
//! ## The per-subagent Model seam (Opus-main / Qwen-scout)
//!
//! [`spawn`](DirectSubagentSpawner::spawn) resolves the child's Model in three
//! layers: an explicit `model` override on the request wins; else the def's
//! [`SubagentModel`] (Inherit -> the parent Model; Scoped(id) -> resolved against
//! the Session's Provider set). The SAME shared `Arc<dyn Llm>` (the Dispatcher)
//! routes whichever Model to its Provider, so no per-subagent Llm is ever built -
//! the seam is a Model VALUE over the shared boundary, exactly like the
//! SideQuery's pinned-Model path.

use std::sync::Arc;

use crate::llm::Llm;
use crate::llm::ToolCallStyle;
use crate::llm::model::Model;
use crate::run::{ChildRunRequest, run_child};
use crate::session::Session;
use crate::subagents::{SubagentModel, SubagentRegistry, subagent_tools};
use crate::tool::caps::{SubagentRequest, SubagentResult, SubagentSpawner};

/// The real [`SubagentSpawner`]: resolves a subagent definition and drives a
/// child Run to settlement (P4/F4, ADR-0061). Built by the Agent (which owns the
/// registry and the Session facts) and carried on the Run's
/// [`crate::run::Capture`] into the Tool [`crate::tool::caps::Capabilities`].
pub struct DirectSubagentSpawner {
    /// The shared LLM boundary (the Dispatcher): the SAME `Arc<dyn Llm>` the
    /// parent Run's completions travel, so any resolved child Model routes to its
    /// Provider over one boundary.
    pub llm: Arc<dyn Llm>,
    /// The parent Run's captured Model: the default an `Inherit` subagent runs
    /// on.
    pub parent_model: Model,
    /// The Session's request settings, threaded into each child Run (ADR-0037).
    pub temperature: Option<f64>,
    pub thinking_budget: Option<u64>,
    pub tool_call_style: ToolCallStyle,
    /// The parent [`Session`], cloned once: the child Run derives its Root,
    /// command timeout, budget knobs, and Provider set from it (a subagent is the
    /// parent's Run over a fresh Conversation and a narrowed tool set). A `Scoped`
    /// subagent Model resolves through the Session's own
    /// [`resolve_model`](Session::resolve_model) - the canonical `/model`-swap
    /// path - so no separate Provider slice is held here.
    pub session: Session,
    /// The subagent definitions (built-ins) the `agent` tool routes among.
    pub registry: Arc<SubagentRegistry>,
    /// The child Run's turn bound (qwen's per-subagent run cap).
    pub subagent_run_limit: usize,
}

impl DirectSubagentSpawner {
    /// Resolves a [`SubagentRequest`] into the [`ChildRunRequest`] that drives
    /// the child Run (P4/F4 + P4b/4c, ADR-0061, ADR-0063). The SHARED resolution
    /// the foreground [`spawn`](DirectSubagentSpawner::spawn) AND the Agent's
    /// background launch both go through, so the two can never drift on how a
    /// def/Model/tool-subset is resolved. `sink` is `None` for the foreground
    /// path (the child's whole run is invisible until it settles) and would carry
    /// the live-output feed for a background launch (DEFERRED - background passes
    /// `None` too today).
    pub fn build_child_request(
        &self,
        request: SubagentRequest,
        sink: Option<crate::run::child::ChildSink>,
    ) -> Result<ChildRunRequest, String> {
        // 1. Resolve the def by name (case-insensitive). An unknown type is the
        //    verbatim qwen not-found wording, with the available names.
        let def = self.registry.get(&request.subagent_type).ok_or_else(|| {
            format!(
                "Subagent \"{}\" not found. Available subagents: {}",
                request.subagent_type,
                self.registry.names().join(", ")
            )
        })?;

        // 2. Resolve the child Model: an explicit override wins; else the def's
        //    own choice (Inherit -> the parent Model; Scoped -> resolved through
        //    the Session's own `resolve_model` - the canonical `/model`-swap path
        //    over the Session's Provider set - an unresolvable Provider surfacing
        //    as an Err).
        let model = match request.model {
            Some(model) => model,
            None => match &def.model {
                SubagentModel::Inherit => self.parent_model.clone(),
                SubagentModel::Scoped(scoped) => self.session.resolve_model(scoped)?,
            },
        };

        // 3. Build the child tool subset from the def's selector (built-ins minus
        //    the excluded set).
        let tools = subagent_tools(&def.tools);

        Ok(ChildRunRequest {
            model,
            llm: Arc::clone(&self.llm),
            system_prompt: def.system_prompt.clone(),
            tools,
            prompt: request.prompt,
            max_turns: self.subagent_run_limit,
            temperature: self.temperature,
            thinking_budget: self.thinking_budget,
            tool_call_style: self.tool_call_style,
            session: self.session.clone(),
            sink,
            // depth 1: the child's own subagents capability is already degraded,
            // so this is defence in depth.
            depth: 1,
        })
    }
}

#[async_trait::async_trait]
impl SubagentSpawner for DirectSubagentSpawner {
    async fn spawn(&self, request: SubagentRequest) -> Result<SubagentResult, String> {
        // Resolve the def/Model/tool-subset into the child request (the shared
        // path), then drive the child Run to settlement inline (foreground).
        let child = self.build_child_request(request, None)?;
        Ok(run_child(child).await)
    }

    async fn spawn_background(
        &self,
        _request: SubagentRequest,
        _description: String,
    ) -> Result<String, String> {
        // The DIRECT spawner is foreground-only: it drives a child Run INLINE
        // off the captured Llm and holds no Agent actor, so it has nowhere to
        // register a detached task or queue a notification. The real background
        // path is the tx-backed `AgentSubagentSpawner` (agent/deps.rs), which
        // relays a `SpawnBackground` to the Agent that owns the registry. A host
        // wired straight to the DIRECT spawner (a test) cannot background, so it
        // returns the degraded Err.
        Err("background subagents are unavailable in this environment".into())
    }

    async fn stop_background(&self, id: String) -> Result<String, String> {
        // No registry here (foreground-only), so any id is not-found (the
        // VERBATIM qwen wording). The real path is the tx-backed spawner.
        Ok(format!("Error: No background task found with ID \"{id}\"."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::content::ContentBlock;
    use crate::llm::LlmRequest;
    use crate::llm::model::Api;
    use crate::llm::provider::Provider;
    use crate::llm::response::{Response, StopReason};
    use crate::session::{Session, SessionConfig, SessionOpts};
    use crate::subagents::{SubagentDef, SubagentModel, ToolSelector, builtins};
    use crate::test_support::{Entry, FakeLlm};

    fn session() -> Session {
        let tmp = std::env::temp_dir();
        let opts = SessionOpts {
            root: Some(tmp.to_string_lossy().to_string()),
            ..SessionOpts::default()
        };
        Session::build(opts, &SessionConfig::test_defaults()).expect("session builds")
    }

    fn model(provider: &str, id: &str) -> Model {
        Model::new(provider, id, Api::AnthropicMessages, 64_000, 100)
    }

    // A custom Provider carrying its own window, so a `Scoped` def naming it
    // resolves through `Session::resolve_model` without a Catalog entry.
    fn provider(id: &str) -> Provider {
        Provider {
            id: id.into(),
            base_url: "http://localhost:1234/v1".into(),
            token: String::new(),
            api: Api::AnthropicMessages,
            context_window: Some(32_000),
            custom: true,
        }
    }

    // A registry holding one `Scoped` def pinned to `provider/model-id`.
    fn scoped_registry(scoped: &str) -> SubagentRegistry {
        SubagentRegistry::new(vec![SubagentDef {
            name: "scout".into(),
            description: "a scoped scout".into(),
            system_prompt: "explore".into(),
            model: SubagentModel::Scoped(scoped.into()),
            tools: ToolSelector::All,
        }])
    }

    fn text_response(text: &str) -> Response {
        Response {
            content: vec![ContentBlock::text(text)],
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
            error: None,
        }
    }

    type Captured = Arc<Mutex<Vec<Model>>>;

    fn recording(captured: Captured, reply: Response) -> Entry {
        Entry::dynamic(vec![], move |_req: &LlmRequest, model: &Model| {
            captured.lock().unwrap().push(model.clone());
            reply.clone()
        })
    }

    fn spawner_with(llm: Arc<dyn Llm>, providers: Vec<Provider>) -> DirectSubagentSpawner {
        spawner_with_registry(llm, providers, SubagentRegistry::new(builtins()))
    }

    // As `spawner_with`, but over a caller-supplied registry so a test can pin a
    // `Scoped` def and observe the resolved child Model.
    fn spawner_with_registry(
        llm: Arc<dyn Llm>,
        providers: Vec<Provider>,
        registry: SubagentRegistry,
    ) -> DirectSubagentSpawner {
        let mut session = session();
        session.providers = providers;
        DirectSubagentSpawner {
            llm,
            parent_model: model("local", "main"),
            temperature: None,
            thinking_budget: None,
            tool_call_style: ToolCallStyle::default(),
            session,
            registry: Arc::new(registry),
            subagent_run_limit: 5,
        }
    }

    #[tokio::test]
    async fn an_unknown_subagent_type_is_the_verbatim_not_found_error() {
        let spawner = spawner_with(Arc::new(FakeLlm::script(vec![])), vec![]);
        let err = spawner
            .spawn(SubagentRequest {
                subagent_type: "nope".into(),
                prompt: "do it".into(),
                model: None,
            })
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "Subagent \"nope\" not found. Available subagents: general-purpose, Explore"
        );
    }

    #[tokio::test]
    async fn an_inherit_subagent_runs_on_the_parent_model() {
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeLlm::script(vec![recording(
            Arc::clone(&captured),
            text_response("the findings"),
        )]);
        let spawner = spawner_with(Arc::new(fake), vec![]);

        let out = spawner
            .spawn(SubagentRequest {
                subagent_type: "general-purpose".into(),
                prompt: "investigate".into(),
                model: None,
            })
            .await
            .unwrap();
        assert_eq!(out.terminate_reason, "GOAL");
        assert_eq!(out.result, "the findings");
        // The child completion ran on the parent Model (Inherit).
        let seen = captured.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].scoped_id(), model("local", "main").scoped_id());
    }

    #[tokio::test]
    async fn an_explicit_model_override_routes_the_child_to_that_model() {
        // Two-Provider set; the override pins the OTHER provider's model, and the
        // child's `complete` must be called with exactly that Model (the shared
        // Dispatcher routes it) - the Opus-main / Qwen-scout seam.
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeLlm::script(vec![recording(Arc::clone(&captured), text_response("ok"))]);
        let other = model("scout", "fast");
        let spawner = spawner_with(Arc::new(fake), vec![]);

        spawner
            .spawn(SubagentRequest {
                subagent_type: "general-purpose".into(),
                prompt: "explore".into(),
                model: Some(other.clone()),
            })
            .await
            .unwrap();

        let seen = captured.lock().unwrap();
        assert_eq!(seen[0].scoped_id(), other.scoped_id());
    }

    #[tokio::test]
    async fn a_scoped_subagent_resolves_and_routes_to_the_scoped_model() {
        // A two-Provider set; the def is Scoped to the OTHER provider. The
        // resolve runs through `Session::resolve_model` (the canonical path, not
        // a hand-rolled 1-token fallback), and the child's `complete` is called
        // with exactly that resolved Model - the Opus-main / Qwen-scout seam over
        // a def-level pin.
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeLlm::script(vec![recording(
            Arc::clone(&captured),
            text_response("scouted"),
        )]);
        let providers = vec![provider("local"), provider("scout")];
        let spawner =
            spawner_with_registry(Arc::new(fake), providers, scoped_registry("scout/fast"));

        let out = spawner
            .spawn(SubagentRequest {
                subagent_type: "scout".into(),
                prompt: "explore".into(),
                model: None,
            })
            .await
            .unwrap();
        assert_eq!(out.result, "scouted");
        let seen = captured.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].scoped_id(), "scout/fast");
        // The resolved window is the scoped Provider's, NOT a 1-token fallback.
        assert_eq!(seen[0].context_window, 32_000);
    }

    #[tokio::test]
    async fn a_scoped_subagent_naming_an_unknown_provider_surfaces_the_err() {
        // The Provider set has no `ghost`, so `Session::resolve_model` returns an
        // Err that `spawn` propagates rather than swallowing.
        let spawner = spawner_with_registry(
            Arc::new(FakeLlm::script(vec![])),
            vec![provider("local")],
            scoped_registry("ghost/model"),
        );
        let err = spawner
            .spawn(SubagentRequest {
                subagent_type: "scout".into(),
                prompt: "explore".into(),
                model: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.contains("ghost"),
            "the unknown-provider Err surfaces up through spawn: {err}"
        );
    }
}
