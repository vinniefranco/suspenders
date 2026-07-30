//! Run - one agent iteration (prompt to settlement).
//!
//! [`run`] declares the Run's [`deps::RunDeps`] port (what a Run needs from its
//! host) and drives [`loop_::run`] over it. The host supplies both the concrete
//! [`deps::RunDeps`] adapter and the [`Capture`] (the Model + Llm snapshot the
//! Run took at spawn) it builds its tooling from - so the Run never depends on
//! any one host. The Agent's adapter is [`crate::agent::deps::AgentDeps`].

mod batch;
pub mod deps;
mod finish;
pub mod loop_;
pub mod next_speaker;
pub mod settlement;
pub mod side_query;

// Shared test fixtures for the split Loop (today only `loop_`'s tests; any
// test module `batch`/`finish` grow shares this set instead of drifting
// copies). Private suffices: descendants reach a private ancestor module.
#[cfg(test)]
mod fixtures;

use std::sync::Arc;

use crate::conversation::Conversation;
use crate::extensions;
use crate::llm::Llm;
use crate::llm::model::Model;
use crate::run::deps::RunDeps;
use crate::run::loop_::{Outcome, RunOpts};
use crate::session::Session;

/// The Model + Llm a Run captured at spawn (ADR-0033): the Model snapshot every
/// request travels to, and the Llm boundary the Run's tooling dispatches over.
/// The host builds this once and hands it to [`run`] alongside the
/// [`deps::RunDeps`] adapter, so the Run reads its tooling inputs from a plain
/// value rather than reaching into a particular host's deps.
pub struct Capture {
    pub model: Model,
    pub llm: Arc<dyn Llm>,
    /// The tool-initiated Approval seam (F1, ADR-0055), built by the host that
    /// owns the effect channel (the Agent builds a tx-backed `AgentApprover`).
    /// The Run assembles it into the Tool [`crate::tool::caps::Capabilities`]
    /// alongside the registry it builds itself. `Arc<dyn Approver>` is Send+Sync,
    /// so the [`Capture`] stays `Send` for the `tokio::spawn` at the Agent.
    pub approver: Arc<dyn crate::tool::caps::Approver>,
    /// The tool-initiated Question seam (P2a, ADR-0057), built by the host that
    /// owns the effect channel (the Agent builds a tx-backed `AgentQuestioner`).
    /// The Run assembles it into the Tool [`crate::tool::caps::Capabilities`]
    /// alongside the Approver. `Arc<dyn Questioner>` is Send+Sync, so the
    /// [`Capture`] stays `Send` for the `tokio::spawn` at the Agent.
    pub questioner: Arc<dyn crate::tool::caps::Questioner>,
    /// The Session-stable tool set the Agent built once (F8, ADR-0056):
    /// built-ins plus any discovered MCP tools. The Run builds its per-Run
    /// [`crate::tool_registry::ToolRegistry`] over this shared `Arc` (fresh
    /// revealed set per Run), so MCP tools ride every Run without re-boxing.
    pub tools: Arc<[Box<dyn crate::tool::Tool>]>,
}

/// Runs the Run: builds the Extension pipeline and Tool ctx and drives
/// [`loop_::run`]. Returns the Loop outcome.
pub async fn run(
    conversation: Conversation,
    session: Session,
    capture: Capture,
    mut deps: impl RunDeps,
    opts: RunOpts,
) -> Outcome {
    // Resolve the Session's ordered Extension names into the live pipeline. The
    // shipped config carries `["diff"]`, so the live app runs the Run with the
    // Diff extension; the test config carries `[]`.
    let extensions = extensions::configured(&session.extensions);

    // The Tool Registry, built once per Run (F3, F8). Reveals are Run-scoped: a
    // fresh registry per Run resets them, matching qwen's
    // clearRevealedDeferredTools on session reset. It shares the Agent's
    // Session-stable tool set (built-ins + discovered MCP tools, ADR-0056) via
    // `with_shared`, so MCP tools ride every Run. It rides the Tool ctx so
    // `tool_search` can reveal deferred tools into the next request's wire list.
    let registry = std::sync::Arc::new(crate::tool_registry::ToolRegistry::with_shared(
        Arc::clone(&capture.tools),
    ));

    // The Side-Query effect seam (P2b, ADR-0055): the real impl is the captured
    // Llm boundary called OFF the main Conversation. Unlike the Approver, it does
    // NOT travel the Agent mpsc - a side-query touches no Agent/Conversation state
    // - so the Run builds it here from the Capture's own `llm`/`model` (no new
    // Capture field). `None` temperature defers a side-query to the server's own
    // default, matching qwen's side-query (which sets no temperature).
    let side_query: Arc<dyn crate::tool::caps::SideQuery> =
        Arc::new(crate::run::side_query::LlmSideQuery {
            llm: Arc::clone(&capture.llm),
            model: capture.model.clone(),
            temperature: None,
        });

    // The Tool Capability Context (F1, ADR-0055): the registry the Run built plus
    // the effect seams a Tool Call reaches its host through. P1b carries the
    // registry (concrete, Run-scoped state) and the Approver (the host's
    // tx-backed handle, threaded through the Capture); P2b adds the SideQuery
    // (built here at the Llm boundary). The other two capabilities land in their
    // consuming phases.
    let caps = crate::tool::caps::Capabilities {
        registry,
        approver: Arc::clone(&capture.approver),
        side_query,
        // The Question seam (P2a, ADR-0057): the Agent's tx-backed handle,
        // threaded through the Capture like the Approver.
        questioner: Arc::clone(&capture.questioner),
    };

    // The Tool ctx: the Session's Root and timeout, the Result Cap derived from
    // this Run's captured Model (ADR-0037), and the Run's Capabilities.
    let tool_ctx = session.tool_ctx(&capture.model, caps);

    loop_::run(
        conversation,
        &session,
        loop_::RunEnv {
            extensions: &extensions,
            tool_ctx: &tool_ctx,
        },
        &mut deps,
        opts,
    )
    .await
}
