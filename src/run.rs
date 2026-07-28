//! Run - one agent iteration (prompt to settlement).
//!
//! [`AgentDeps`] is the thin Run shell: the concrete
//! [`RunDeps`] that wires each effect to the Agent's channels and the Session's
//! injected [`Llm`] (ADR-0011, ADR-0017). It runs INSIDE the spawned Run task,
//! so its effects talk back to the Agent over an `mpsc` (fire-and-forget for
//! the [`Emitter`](deps::Emitter) handle, `checkpoint`, and `set_plan`;
//! request/reply `oneshot` for `drain_steering`
//! and `request_approval`). `complete` and `compact` call the injected `Llm`
//! directly - in the Run task, NEVER on the Agent (ADR-0012): an Agent-side
//! summarization call would block every caller for its duration.

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

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::agent::{Msg, RunMsg};
use crate::compaction::Compaction;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::model::Model;
use crate::llm::response::Response;
use crate::llm::{Llm, LlmRequest, StreamEvent};
use crate::extensions;
use crate::scout::{Scout, ScoutOpts};
use crate::session::Session;
use crate::run::deps::{CompactError, Emitter, RunDeps};
use crate::run::loop_::{Outcome, RunOpts};

/// The Run shell's [`RunDeps`]: every effect wired
/// to the Agent's mpsc + the Session's Llm.
pub struct AgentDeps {
    tx: mpsc::UnboundedSender<Msg>,
    llm: Arc<dyn Llm>,
    /// The Model this Run captured at spawn (ADR-0033 amendment): every
    /// request the Run makes travels to it, and a `/model` swap mid-flight
    /// never touches it.
    model: Model,
    /// The Session-resolved sampling temperature, applied to every request
    /// this shell builds (ADR-0037: temperature rides the request).
    temperature: Option<f64>,
    /// The accumulated Compaction state captured at Run start; `compact`
    /// closes over it and, on success, notifies the Agent to log + update state
    /// (baud's compact_fn, ADR-0012).
    compaction: Compaction,
}

impl AgentDeps {
    pub fn new(
        tx: mpsc::UnboundedSender<Msg>,
        llm: Arc<dyn Llm>,
        model: Model,
        temperature: Option<f64>,
        compaction: Compaction,
    ) -> Self {
        AgentDeps {
            tx,
            llm,
            model,
            temperature,
            compaction,
        }
    }

    fn send(&self, msg: RunMsg) {
        let _ = self.tx.send(Msg::Run(msg));
    }
}

impl RunDeps for AgentDeps {
    fn complete(
        &mut self,
        req: LlmRequest,
        on_event: &mut (dyn FnMut(&StreamEvent) + Send),
    ) -> impl Future<Output = Response> + Send {
        // Attach the Session's temperature and call the injected boundary with
        // the captured Model; each StreamEvent forwards to on_event (the Loop
        // runs it into a message_update event).
        let req = req.with_temperature(self.temperature);
        let llm = Arc::clone(&self.llm);
        let model = self.model.clone();
        async move {
            let mut adapter = |ev: &StreamEvent| on_event(ev);
            llm.complete(&req, &model, &mut adapter).await
        }
    }

    fn provenance(&self) -> crate::content::Provenance {
        self.model.provenance()
    }

    fn emitter(&mut self) -> Emitter {
        // Fire-and-forget to the Agent, which broadcasts AND logs - routing
        // through the single owner keeps Event order deterministic (ADR-0017):
        // the handle and the Run task feed the SAME mpsc channel from the SAME
        // task, so detaching emission into a handle (ADR-0025) changes nothing
        // about ordering.
        let tx = self.tx.clone();
        Emitter::new(move |event| {
            let _ = tx.send(Msg::Run(RunMsg::Emit(event)));
        })
    }

    fn drain_steering(&mut self) -> impl Future<Output = Vec<String>> + Send {
        let tx = self.tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            if tx.send(Msg::Run(RunMsg::DrainSteering(reply))).is_err() {
                return Vec::new();
            }
            rx.await.unwrap_or_default()
        }
    }

    fn request_approval(
        &mut self,
        id: String,
        command: String,
    ) -> impl Future<Output = bool> + Send {
        // Ask the Agent to relay this Approval, then await the decision it
        // forwards (a per-Run approval reply oneshot). The Agent owns the
        // request-side emission: it consults the Standing Approvals and emits
        // either `approval_request` (opening the modal) or, on an auto-approve,
        // `approval_auto` - the Run cannot tell the difference. Once answered,
        // the Run emits `approval_resolved` (baud's `Baud.Turn` dep), the same
        // on both paths.
        let tx = self.tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            if tx
                .send(Msg::Run(RunMsg::RequestApproval {
                    id: id.clone(),
                    command,
                    reply,
                }))
                .is_err()
            {
                return false;
            }
            // No timeout - the user decides. A cancel aborts this task, so a
            // pending Approval dies with it.
            let approved = rx.await.unwrap_or(false);
            let _ = tx.send(Msg::Run(RunMsg::Emit(Event::approval_resolved(
                id, approved,
            ))));
            approved
        }
    }

    fn checkpoint(&mut self, conversation: &Conversation) {
        self.send(RunMsg::Checkpoint(conversation.clone()));
    }

    fn set_plan(&mut self, plan: String) {
        self.send(RunMsg::SetPlan(plan));
    }

    fn compact(
        &mut self,
        conversation: Conversation,
    ) -> impl Future<Output = Result<Conversation, CompactError>> + Send {
        // The real compaction effect (ADR-0012): runs in the Run task, calling
        // the injected Llm. On success, notify the Agent to append the
        // {:compacted, ...} Session Log entry and update the accumulated state.
        let tx = self.tx.clone();
        let llm = Arc::clone(&self.llm);
        let model = self.model.clone();
        let temperature = self.temperature;
        let compaction = self.compaction.clone();
        async move {
            let tokens_before = conversation.token_estimate();
            match compaction
                .run(&conversation, llm.as_ref(), &model, temperature)
                .await
            {
                Ok((compacted, new_state)) => {
                    let skip_count = Compaction::skip_count(&conversation, &compacted);
                    let _ = tx.send(Msg::Run(RunMsg::Compacted {
                        new_state,
                        skip_count: skip_count as u64,
                        tokens_before,
                    }));
                    Ok(compacted)
                }
                Err(reason) => Err(CompactError(reason)),
            }
        }
    }
}

/// Runs the Run: builds the Extension pipeline and
/// Tool ctx (the ctx's `scout` capture dispatches a [`Scout`] over the Session's
/// Llm/connection) and drives [`loop_::run`]. Returns the Loop outcome.
pub async fn run(
    conversation: Conversation,
    session: Session,
    mut deps: AgentDeps,
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
    let mut tool_ctx = session.tool_ctx(&deps.model);
    tool_ctx.scout = Some(make_scout(
        Arc::clone(&deps.llm),
        deps.model.clone(),
        session.root.clone(),
        ScoutKnobs {
            pass_limit: session.scout_pass_limit,
            context_budget: session.context_budget_for(&deps.model),
            no_think: session.scout_no_think,
            temperature: session.temperature,
            command_timeout_ms: session.command_timeout_ms,
        },
    ));

    loop_::run(
        conversation,
        &session,
        &extensions,
        &tool_ctx,
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
) -> crate::scout::ScoutFn {
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
