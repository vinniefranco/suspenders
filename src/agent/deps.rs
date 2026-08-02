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

use crate::agent::capabilities::{
    AgentApprover, AgentBackgroundShellSpawner, AgentQuestioner, AgentSubagentSpawner,
};
use crate::agent::{Msg, RunMsg};
use crate::compaction::Compaction;
use crate::content::Provenance;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::model::Model;
use crate::llm::response::Response;
use crate::llm::{Llm, LlmRequest, StreamEvent, ToolCallStyle};
use crate::run::deps::{CompactError, Emitter, RunDeps};

/// The three per-request sampling settings the Session resolves once and every
/// request this shell builds carries (ADR-0037: they ride the request). Grouped
/// as one value because they are always resolved together, threaded together
/// into both `complete` and the subagent spawner, and never vary independently -
/// a cohesive cluster, not three loose knobs.
#[derive(Clone)]
pub(crate) struct RequestSettings {
    /// The Session-resolved sampling temperature.
    pub(crate) temperature: Option<f64>,
    /// The Session-resolved extended-thinking budget (qwen-code parity). The
    /// next-speaker side-query's `no_think` suppresses it at the wire, and
    /// Compaction never receives it (it does not flow through `complete`).
    pub(crate) thinking_budget: Option<u64>,
    /// The Session-resolved Tool Call style (qwen parity).
    pub(crate) tool_call_style: ToolCallStyle,
}

/// The Agent-owned values a Run captures at spawn (the Parameter Object for
/// [`AgentDeps::new`], ADR-0011/0017): the wire (channel + Llm boundary), the
/// captured Model, the per-request [`RequestSettings`], the Compaction state,
/// and the three Session-stable registries the Run threads into its [`Capture`].
/// Grouped so construction takes one cohesive value instead of a data clump of
/// ten positional arguments; the caller ([`crate::agent::spawn_run`]) fills it
/// from its `AgentState`.
pub(crate) struct RunSpawn {
    pub(crate) tx: mpsc::UnboundedSender<Msg>,
    pub(crate) llm: Arc<dyn Llm>,
    pub(crate) model: Model,
    pub(crate) settings: RequestSettings,
    pub(crate) compaction: Compaction,
    pub(crate) session_tools: Arc<[Box<dyn crate::tool::Tool>]>,
    pub(crate) subagents: Arc<crate::subagents::SubagentRegistry>,
    pub(crate) session: crate::session::Session,
    pub(crate) hooks: Arc<crate::hooks::HookManager>,
    /// The disk-skill manager (ADR-0058): the Agent discovered it once at
    /// launch. Threaded into each Run's [`crate::run::Capture`] so
    /// `crate::run::batch` can activate a conditional skill at the tool-success
    /// seam, and the same shared `Arc` the `skill` tool reads its catalog off.
    pub(crate) skills: Arc<crate::skills::SkillManager>,
}

/// The Run shell's [`RunDeps`]: every effect wired to the Agent's mpsc + the
/// Session's Llm.
pub(crate) struct AgentDeps {
    tx: mpsc::UnboundedSender<Msg>,
    llm: Arc<dyn Llm>,
    /// The Model this Run captured at spawn (ADR-0033 amendment): every
    /// request the Run makes travels to it, and a `/model` swap mid-flight
    /// never touches it.
    model: Model,
    /// The per-request sampling settings (temperature, thinking budget, Tool
    /// Call style), applied to every request this shell builds.
    settings: RequestSettings,
    /// The accumulated Compaction state captured at Run start; `compact`
    /// closes over it and, on success, notifies the Agent to log + update state
    /// (baud's compact_fn, ADR-0012).
    compaction: Compaction,
    /// The Session-stable tool set (F8, ADR-0056): built-ins plus any MCP tools
    /// the Agent discovered at startup. Threaded into each Run's [`Capture`] so
    /// the Run's registry shares it (fresh reveal state per Run).
    session_tools: Arc<[Box<dyn crate::tool::Tool>]>,
    /// The subagent definitions (P4/F4, ADR-0061): the built-in registry the
    /// Agent built once at startup. Threaded into each Run's [`Capture`] so the
    /// Run's `DirectSubagentSpawner` can resolve a subagent def by name.
    subagents: Arc<crate::subagents::SubagentRegistry>,
    /// The parent [`Session`] the child Run derives its Root/timeout/budget knobs
    /// from (P4/F4, ADR-0061). Cloned once at Run spawn.
    session: crate::session::Session,
    /// The child Run's turn bound (qwen's per-subagent run cap): the Session's
    /// own `run_limit`, so a subagent runs the same bound a top-level Run does.
    subagent_run_limit: usize,
    /// The standing hook manager (Phase 3a, ADR-0066): the Agent resolved it once
    /// at launch. Threaded into each Run's [`crate::run::Capture`] so `run` builds
    /// the Run's hook firing handle over it.
    hooks: Arc<crate::hooks::HookManager>,
    /// The disk-skill manager (ADR-0058): threaded into each Run's
    /// [`crate::run::Capture`] so `crate::run::batch` can activate a conditional
    /// skill by touched path. Shared by `Arc` with the `skill` tool.
    skills: Arc<crate::skills::SkillManager>,
}

impl AgentDeps {
    /// Builds the Run shell's deps from the [`RunSpawn`] Parameter Object: the
    /// caller assembles the Agent-owned values it captures at spawn into one
    /// value, and this derives the subagent run bound from the Session.
    pub(crate) fn new(spawn: RunSpawn) -> Self {
        let RunSpawn {
            tx,
            llm,
            model,
            settings,
            compaction,
            session_tools,
            subagents,
            session,
            hooks,
            skills,
        } = spawn;
        let subagent_run_limit = session.run_limit as usize;
        AgentDeps {
            tx,
            llm,
            model,
            settings,
            compaction,
            session_tools,
            subagents,
            session,
            subagent_run_limit,
            hooks,
            skills,
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
            // The Subagent seam (P4/F4 + P4b, ADR-0061, ADR-0063): the
            // two-channel `AgentSubagentSpawner`. Its FOREGROUND `spawn` is still
            // the DIRECT (Llm-boundary, non-mpsc) path - it holds the same
            // `DirectSubagentSpawner` handles and drives a child Run inline, a
            // foreground subagent touching no Agent/Conversation state. Its
            // BACKGROUND `spawn_background`/`stop_background` are tx-backed (like
            // the Approver/Questioner): they relay a `SpawnBackground`/
            // `StopBackground` to the Agent that owns the registry, because a
            // detached child Run and the task registry ARE Agent-owned mutable
            // state (ADR-0017). A `Scoped` subagent Model resolves through the
            // Session's own `resolve_model`, so no separate Provider slice is
            // threaded.
            subagents: Arc::new(AgentSubagentSpawner {
                direct: crate::run::subagent::DirectSubagentSpawner {
                    llm: Arc::clone(&self.llm),
                    parent_model: self.model.clone(),
                    temperature: self.settings.temperature,
                    thinking_budget: self.settings.thinking_budget,
                    tool_call_style: self.settings.tool_call_style,
                    session: self.session.clone(),
                    registry: Arc::clone(&self.subagents),
                    subagent_run_limit: self.subagent_run_limit,
                },
                tx: self.tx.clone(),
            }),
            // The Background-Shell seam (Phase 9, ADR-0063): a tx-backed handle
            // over this Agent's mpsc, like the Approver/Questioner. The Agent owns
            // the detached process lifecycle + the shell registry (ADR-0017), so
            // both legs relay a `SpawnBackgroundShell`/`StopBackgroundShell` to the
            // Agent that owns them.
            bg_shells: Arc::new(AgentBackgroundShellSpawner {
                tx: self.tx.clone(),
            }),
            // The standing hook manager (Phase 3a, ADR-0066): threaded so `run`
            // builds the Run's firing handle over it. Shared by Arc, like the tool
            // set - the manager is immutable for the Session's lifetime this phase.
            hooks: Arc::clone(&self.hooks),
            // The disk-skill manager (ADR-0058): shared by Arc so `crate::run::batch`
            // can activate a conditional skill by touched path and the `skill` tool
            // reads the same activation state. The skill list is immutable for the
            // Session; only its interior activation registry mutates.
            skills: Arc::clone(&self.skills),
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
            .with_temperature(self.settings.temperature)
            .with_thinking_budget(self.settings.thinking_budget)
            .with_tool_call_style(self.settings.tool_call_style);
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

    fn drain_notifications(&mut self) -> impl Future<Output = Vec<String>> + Send {
        // The parallel channel to `drain_steering` (P4b, ADR-0063): ask the Agent
        // (which owns the background registry) for its queued
        // `<task-notification>` envelopes and empty the queue. A closed channel
        // yields none - the same safe answer as `drain_steering`.
        let tx = self.tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            if tx
                .send(Msg::Run(RunMsg::DrainNotifications(reply)))
                .is_err()
            {
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
        let temperature = self.settings.temperature;
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
