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
use crate::llm::{Llm, LlmRequest, StreamEvent, ToolCallStyle};
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
    /// The Session-resolved extended-thinking budget (qwen-code parity),
    /// applied to every request this shell builds - it rides the request like
    /// temperature. The next-speaker side-query's `no_think` suppresses it at
    /// the wire, and Compaction never receives it (it does not flow through
    /// `complete`).
    thinking_budget: Option<u64>,
    /// The Session-resolved Tool Call style (qwen parity), applied to every
    /// request this shell builds - it rides the request like temperature.
    tool_call_style: ToolCallStyle,
    /// The accumulated Compaction state captured at Run start; `compact`
    /// closes over it and, on success, notifies the Agent to log + update state
    /// (baud's compact_fn, ADR-0012).
    compaction: Compaction,
    /// The Session-stable tool set (F8, ADR-0056): built-ins plus any MCP tools
    /// the Agent discovered at startup. Threaded into each Run's [`Capture`] so
    /// the Run's registry shares it (fresh reveal state per Run).
    session_tools: Arc<[Box<dyn crate::tool::Tool>]>,
}

impl AgentDeps {
    // Each argument is a distinct Agent-owned value the Run captures at spawn
    // (the channel, the Llm, the Model, the three request settings, the
    // Compaction state, and now the Session-stable tool set). A Parameter Object
    // would only rename the same bundle, so the targeted allow is clearer than
    // the indirection.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tx: mpsc::UnboundedSender<Msg>,
        llm: Arc<dyn Llm>,
        model: Model,
        temperature: Option<f64>,
        thinking_budget: Option<u64>,
        tool_call_style: ToolCallStyle,
        compaction: Compaction,
        session_tools: Arc<[Box<dyn crate::tool::Tool>]>,
    ) -> Self {
        AgentDeps {
            tx,
            llm,
            model,
            temperature,
            thinking_budget,
            tool_call_style,
            compaction,
            session_tools,
        }
    }

    /// The Model + Llm this Run captured at spawn, handed to
    /// [`crate::run::run`] so it can build the Run's tooling (the Result Cap)
    /// without reaching into this adapter.
    pub(crate) fn capture(&self) -> crate::run::Capture {
        crate::run::Capture {
            model: self.model.clone(),
            llm: Arc::clone(&self.llm),
            // The tool-initiated Approval seam (F1, ADR-0055): a tx-backed handle
            // over this Agent's mpsc. The Run assembles it into the Tool
            // Capabilities; the Agent owns the channel, so it builds the handle.
            approver: Arc::new(AgentApprover {
                tx: self.tx.clone(),
            }),
            // The tool-initiated Question seam (P2a, ADR-0057): a tx-backed
            // handle over this Agent's mpsc, like the Approver but with no
            // Standing-Approval / auto path. The Run assembles it into the Tool
            // Capabilities; the Agent owns the channel, so it builds the handle.
            questioner: Arc::new(AgentQuestioner {
                tx: self.tx.clone(),
            }),
            // The Session-stable tool set (F8, ADR-0056): the Run's registry
            // shares it via `with_shared`, so MCP tools ride every Run.
            tools: Arc::clone(&self.session_tools),
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
        // Attach the Session's temperature, thinking budget, and Tool Call
        // style, then call the injected boundary with the captured Model; each
        // StreamEvent forwards to on_event (the Loop runs it into a
        // message_update event). The next-speaker side-query also flows through
        // here, but its `no_think` request flag suppresses the thinking budget
        // at the Anthropic wire - the two are mutually exclusive there.
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

/// The tx-backed [`Approver`]: the Agent's fulfilment of the tool-initiated
/// Approval seam (F1, ADR-0055). It relays an Approval over the same mpsc the
/// [`AgentDeps::request_approval`] batch gate uses and awaits the same reply
/// oneshot - so a tool-initiated Approval and a gate Approval reach the Agent
/// identically. Built by [`AgentDeps::capture`] and carried on the Run's
/// [`crate::run::Capture`] into the Tool [`crate::tool::caps::Capabilities`].
///
/// Why kept without a consumer yet: P1b wires this into the Capabilities carrier
/// to prove the whole seam with a live wire, but no tool consumes the Approver in
/// P1b - the batch gate still drives approval through [`RunDeps::request_approval`].
/// The two share the exact request path; a later phase collapses the duplication
/// once a tool initiates its own Approval.
struct AgentApprover {
    tx: mpsc::UnboundedSender<Msg>,
}

#[async_trait::async_trait]
impl crate::tool::caps::Approver for AgentApprover {
    async fn approve(&self, id: String, command: String) -> bool {
        // A near-verbatim lift of `AgentDeps::request_approval`: ask the Agent to
        // relay this Approval (it consults the Standing Approvals and emits either
        // `approval_request` or, on an auto-approve, `approval_auto` - the caller
        // cannot tell the difference), await the decision it forwards, then emit
        // `approval_resolved`, the same on both paths.
        let tx = self.tx.clone();
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
        // No timeout - the user decides. A cancel aborts this task, so a pending
        // Approval dies with it.
        let approved = rx.await.unwrap_or(false);
        let _ = tx.send(Msg::Run(RunMsg::Emit(Event::approval_resolved(
            id, approved,
        ))));
        approved
    }
}

/// The tx-backed [`Questioner`]: the Agent's fulfilment of the tool-initiated
/// Question seam (P2a, ADR-0057). It mints a question id, relays an `AskQuestion`
/// over the same mpsc the Approver uses, and awaits the reply oneshot - so the
/// tool call parks until the user answers. Unlike the Approver there is NO
/// Standing-Approval / auto path: every question opens a modal (the Agent's
/// `ask_question` handler is unconditionally the pending leg). Built by
/// [`AgentDeps::capture`] and carried on the Run's [`crate::run::Capture`] into
/// the Tool [`crate::tool::caps::Capabilities`].
///
/// On a closed/dropped channel (a dead Agent, a cancelled Run) it returns the
/// VERBATIM decline string, the safe answer the tool folds into its content -
/// symmetric with [`AgentApprover`] returning `false`.
struct AgentQuestioner {
    tx: mpsc::UnboundedSender<Msg>,
}

/// The VERBATIM qwen decline string (askUserQuestion.ts): what the tool returns
/// when the user cancels - or when the channel is gone before an answer arrives.
const DECLINE_MESSAGE: &str = "User declined to answer the questions.";

#[async_trait::async_trait]
impl crate::tool::caps::Questioner for AgentQuestioner {
    async fn ask(
        &self,
        questions: Vec<crate::tool::caps::Question>,
    ) -> Result<Vec<(usize, String)>, String> {
        // Mint the per-call id (baud's `make_ref()`), relay the question, and
        // await the picks. A closed channel or dropped reply is a decline (the
        // safe answer the tool returns as content), not a panic.
        let id = mint_question_id();
        let tx = self.tx.clone();
        let (reply, rx) = oneshot::channel();
        if tx
            .send(Msg::Run(RunMsg::AskQuestion {
                id: id.clone(),
                questions,
                reply,
            }))
            .is_err()
        {
            return Err(DECLINE_MESSAGE.to_string());
        }
        // No timeout - the user decides. A cancel aborts this task, so a pending
        // question dies with it (a dropped sender yields the decline below).
        let answers = rx.await.unwrap_or_else(|_| Err(DECLINE_MESSAGE.to_string()));
        // Emit `question_resolved` after reading the reply, the same on both
        // paths (mirrors the Approver emitting `approval_resolved`).
        let _ = tx.send(Msg::Run(RunMsg::Emit(Event::question_resolved(id))));
        answers
    }
}

fn mint_question_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("question-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}
