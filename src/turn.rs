//! Turn - one agent iteration (prompt to settlement).
//!
//! [`AgentDeps`] is the thin Turn shell: the concrete
//! [`TurnDeps`] that wires each effect to the Agent's channels and the Session's
//! injected [`Llm`] (ADR-0011, ADR-0017). It runs INSIDE the spawned Turn task,
//! so its effects talk back to the Agent over an `mpsc` (fire-and-forget for
//! the [`Emitter`](deps::Emitter) handle, `checkpoint`, and `set_plan`;
//! request/reply `oneshot` for `drain_steering`
//! and `request_approval`). `complete` and `compact` call the injected `Llm`
//! directly - in the Turn task, NEVER on the Agent (ADR-0012): an Agent-side
//! summarization call would block every caller for its duration.

mod batch;
pub mod deps;
mod finish;
pub mod governor;
pub mod loop_;
pub mod settlement;

// Shared test fixtures for the split Loop (today only `loop_`'s tests; any
// test module `batch`/`finish` grow shares this set instead of drifting
// copies). Private suffices: descendants reach a private ancestor module.
#[cfg(test)]
mod fixtures;

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::agent::{Msg, TurnMsg};
use crate::compaction::Compaction;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::Llm;
use crate::llm::request::{self, LlmRequest};
use crate::llm::response::Response;
use crate::llm::stream::StreamEvent;
use crate::plugins;
use crate::scout::{Scout, ScoutOpts};
use crate::session::Session;
use crate::session::connection::Connection;
use crate::turn::deps::{CompactError, Emitter, TurnDeps};
use crate::turn::loop_::{Outcome, RunOpts};

/// The Turn shell's [`TurnDeps`]: every effect wired
/// to the Agent's mpsc + the Session's Llm.
pub struct AgentDeps {
    tx: mpsc::UnboundedSender<Msg>,
    llm: Arc<dyn Llm>,
    connection: Connection,
    /// The accumulated Compaction state captured at Turn start; `compact`
    /// closes over it and, on success, notifies the Agent to log + update state
    /// (baud's compact_fn, ADR-0012).
    compaction: Compaction,
}

impl AgentDeps {
    pub fn new(
        tx: mpsc::UnboundedSender<Msg>,
        llm: Arc<dyn Llm>,
        connection: Connection,
        compaction: Compaction,
    ) -> Self {
        AgentDeps {
            tx,
            llm,
            connection,
            compaction,
        }
    }

    fn send(&self, msg: TurnMsg) {
        let _ = self.tx.send(Msg::Turn(msg));
    }
}

impl TurnDeps for AgentDeps {
    fn complete(
        &mut self,
        req: LlmRequest,
        on_event: &mut (dyn FnMut(&StreamEvent) + Send),
    ) -> impl Future<Output = Response> + Send {
        // Render the request to wire JSON, then call the injected boundary; each
        // StreamEvent forwards to on_event (the Loop turns it into a
        // message_update event).
        let wire = request::build_request(&req, &self.connection);
        let llm = Arc::clone(&self.llm);
        let connection = self.connection.clone();
        async move {
            let mut adapter = |ev: &StreamEvent| on_event(ev);
            llm.complete(wire, &connection, &mut adapter).await
        }
    }

    fn emitter(&mut self) -> Emitter {
        // Fire-and-forget to the Agent, which broadcasts AND logs - routing
        // through the single owner keeps Event order deterministic (ADR-0017):
        // the handle and the Turn task feed the SAME mpsc channel from the SAME
        // task, so detaching emission into a handle (ADR-0025) changes nothing
        // about ordering.
        let tx = self.tx.clone();
        Emitter::new(move |event| {
            let _ = tx.send(Msg::Turn(TurnMsg::Emit(event)));
        })
    }

    fn drain_steering(&mut self) -> impl Future<Output = Vec<String>> + Send {
        let tx = self.tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            if tx.send(Msg::Turn(TurnMsg::DrainSteering(reply))).is_err() {
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
        // forwards (a per-Turn approval reply oneshot). The Agent owns the
        // request-side emission: it consults the Standing Approvals and emits
        // either `approval_request` (opening the modal) or, on an auto-approve,
        // `approval_auto` - the Turn cannot tell the difference. Once answered,
        // the Turn emits `approval_resolved` (baud's `Baud.Turn` dep), the same
        // on both paths.
        let tx = self.tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            if tx
                .send(Msg::Turn(TurnMsg::RequestApproval {
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
            let _ = tx.send(Msg::Turn(TurnMsg::Emit(Event::approval_resolved(
                id, approved,
            ))));
            approved
        }
    }

    fn checkpoint(&mut self, conversation: &Conversation) {
        self.send(TurnMsg::Checkpoint(conversation.clone()));
    }

    fn set_plan(&mut self, plan: String) {
        self.send(TurnMsg::SetPlan(plan));
    }

    fn compact(
        &mut self,
        conversation: Conversation,
    ) -> impl Future<Output = Result<Conversation, CompactError>> + Send {
        // The real compaction effect (ADR-0012): runs in the Turn task, calling
        // the injected Llm. On success, notify the Agent to append the
        // {:compacted, ...} Session Log entry and update the accumulated state.
        let tx = self.tx.clone();
        let llm = Arc::clone(&self.llm);
        let connection = self.connection.clone();
        let compaction = self.compaction.clone();
        async move {
            let tokens_before = conversation.token_estimate();
            match compaction
                .run(&conversation, llm.as_ref(), &connection)
                .await
            {
                Ok((compacted, new_state)) => {
                    let skip_count = Compaction::skip_count(&conversation, &compacted);
                    let _ = tx.send(Msg::Turn(TurnMsg::Compacted {
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

/// Runs the Turn: builds the Plugin pipeline and
/// Tool ctx (the ctx's `scout` capture dispatches a [`Scout`] over the Session's
/// Llm/connection) and drives [`loop_::run`]. Returns the Loop outcome.
pub async fn run(
    conversation: Conversation,
    session: Session,
    mut deps: AgentDeps,
    opts: RunOpts,
) -> Outcome {
    // Resolve the Session's ordered Plugin names into the live pipeline. The
    // shipped config carries `["diff"]`, so the live app runs the Turn with the
    // Diff plugin; the test config carries `[]`.
    let plugins = plugins::configured(&session.plugins);

    // The Tool ctx: the Session's Root/Result Cap/timeout, plus the `scout`
    // capture wired to the Session's Llm + connection (baud wires this so
    // explore can dispatch a Scout; the Rust Session carries the Llm via the
    // injected boundary).
    let mut tool_ctx = session.tool_ctx();
    tool_ctx.scout = Some(make_scout(
        Arc::clone(&deps.llm),
        session.connection.clone(),
        session.root.clone(),
        session.scout_pass_limit,
        session.context_budget,
        session.scout_no_think,
        session.command_timeout_ms,
    ));

    loop_::run(conversation, &session, &plugins, &tool_ctx, &mut deps, opts).await
}

// Builds the `scout` capture on the Tool ctx: an effect that dispatches a Scout
// for a task against the Session's Llm/connection and yields its outcome.
fn make_scout(
    llm: Arc<dyn Llm>,
    connection: Connection,
    root: String,
    pass_limit: u64,
    context_budget: u64,
    no_think: bool,
    command_timeout_ms: u64,
) -> crate::scout::ScoutFn {
    Arc::new(move |task: String| {
        let llm = Arc::clone(&llm);
        let connection = connection.clone();
        let opts = ScoutOpts {
            root: std::path::PathBuf::from(&root),
            pass_limit,
            context_budget,
            no_think,
            command_timeout_ms,
        };
        Box::pin(async move { Scout::run(&task, llm.as_ref(), &connection, opts).await })
    })
}
