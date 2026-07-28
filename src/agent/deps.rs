//! [`AgentDeps`] - the Agent's adapter for the Run's [`RunDeps`] port
//! (ADR-0011, ADR-0017). The Run defines what it needs from its host
//! ([`crate::run::deps::RunDeps`]); this is the concrete wiring that fulfils it
//! by threading each effect over the Agent's `mpsc` and the Session's injected
//! [`Llm`]. It lives with the Agent (the host that owns the channel and the
//! [`Msg`]/[`RunMsg`] protocol), not with the Run that declares the port - so
//! the Run never depends on the Agent (Ports and Adapters: the adapter belongs
//! to the consumer, the port to the provider).

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::agent::{Msg, RunMsg};
use crate::compaction::Compaction;
use crate::content::Provenance;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::model::Model;
use crate::llm::response::Response;
use crate::llm::{Llm, LlmRequest, StreamEvent};
use crate::run::deps::{CompactError, Emitter, RunDeps};

/// The Run shell's [`RunDeps`]: every effect wired to the Agent's mpsc + the
/// Session's Llm.
pub(crate) struct AgentDeps {
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
    pub(crate) fn new(
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

    /// The Model + Llm this Run captured at spawn, handed to
    /// [`crate::run::run`] so it can build the Run's tooling (the `scout`
    /// capture, the Result Cap) without reaching into this adapter.
    pub(crate) fn capture(&self) -> crate::run::Capture {
        crate::run::Capture {
            model: self.model.clone(),
            llm: Arc::clone(&self.llm),
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

    fn provenance(&self) -> Provenance {
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
