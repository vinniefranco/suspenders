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
pub mod governor;
pub mod loop_;
mod offer;
pub mod settlement;

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
use crate::scout::{Scout, ScoutOpts};
use crate::session::Session;

/// The Model + Llm a Run captured at spawn (ADR-0033): the Model snapshot every
/// request travels to, and the Llm boundary the Run's tooling (the `scout`
/// capture) dispatches over. The host builds this once and hands it to [`run`]
/// alongside the [`deps::RunDeps`] adapter, so the Run reads its tooling inputs
/// from a plain value rather than reaching into a particular host's deps.
pub struct Capture {
    pub model: Model,
    pub llm: Arc<dyn Llm>,
}

/// Runs the Run: builds the Extension pipeline and
/// Tool ctx (the ctx's `scout` capture dispatches a [`Scout`] over the Session's
/// Llm/connection) and drives [`loop_::run`]. Returns the Loop outcome.
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

    // The Tool ctx: the Session's Root and timeout plus the Result Cap derived
    // from this Run's captured Model (ADR-0037), and the `scout` capture
    // wired to the Session's Llm and the same capture (a Scout runs against
    // the same captured Model and budget - CONTEXT.md: Scout).
    let mut tool_ctx = session.tool_ctx(&capture.model);
    tool_ctx.scout = Some(make_scout(
        Arc::clone(&capture.llm),
        capture.model.clone(),
        session.root.clone(),
        ScoutKnobs {
            pass_limit: session.scout_pass_limit,
            context_budget: session.context_budget_for(&capture.model),
            no_think: session.scout_no_think,
            temperature: session.temperature,
            command_timeout_ms: session.command_timeout_ms,
        },
    ));

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

// The Session knobs the `scout` capture carries into every dispatch, bundled
// so `make_scout` stays a few named facts rather than a positional parade.
struct ScoutKnobs {
    pass_limit: u64,
    context_budget: u64,
    no_think: bool,
    temperature: Option<f64>,
    command_timeout_ms: u64,
}

// Builds the `scout` capture on the Tool ctx: an effect that dispatches a Scout
// for a task against the Session's Llm and the captured Model and yields its
// outcome.
fn make_scout(
    llm: Arc<dyn Llm>,
    model: Model,
    root: String,
    knobs: ScoutKnobs,
) -> crate::scout_port::ScoutFn {
    Arc::new(move |task: String| {
        let llm = Arc::clone(&llm);
        let model = model.clone();
        let opts = ScoutOpts {
            root: std::path::PathBuf::from(&root),
            pass_limit: knobs.pass_limit,
            context_budget: knobs.context_budget,
            no_think: knobs.no_think,
            temperature: knobs.temperature,
            command_timeout_ms: knobs.command_timeout_ms,
        };
        Box::pin(async move { Scout::run(&task, llm.as_ref(), &model, opts).await })
    })
}
