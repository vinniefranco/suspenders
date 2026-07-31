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
#[path = "../../tests/unit/run/subagent.rs"]
mod tests;
