//! Agent - the actor driving Turns for a Session (baud's `Baud.Agent`, ADR-0017).
//!
//! A `tokio::spawn`ed task SOLELY owns the mutable state: the `Session`, the one
//! `Conversation`, the `Approvals` fold, the Steering queue, the latest
//! per-Tool-Result checkpoint, the Plan, the Session Log handle, and the
//! `broadcast` Sender to subscribers. The UI, the headless driver, and tests
//! talk ONLY to the Agent - never to a Turn - by sending [`Command`]s over an
//! `mpsc` channel and reading [`Event`]s off a `broadcast` channel. Single
//! ownership serializes the state without shared locks and fixes one
//! deterministic Event order (ADR-0017).
//!
//! Each Turn runs as a child task via `tokio::spawn`; the Agent holds its
//! [`JoinHandle`]. Cancellation is `JoinHandle::abort()`, which only cancels at
//! an `.await`; partial work survives because the Agent already holds the latest
//! checkpoint (delivered through the Deps `checkpoint` effect, ADR-0011). The
//! `JoinError`/[`Outcome`] the awaited handle yields distinguishes the three
//! Turn outcomes: completed (`Ok`), cancelled (abort + the cancel flag), and
//! panicked (`Err`, so Turn Settlement records failed).
//!
//! ## How the Turn talks back
//!
//! The Turn's [`crate::turn::AgentDeps`] sends [`TurnMsg`]s over the SAME `mpsc`
//! the public Commands use, so the single owner serializes them and Event order
//! is the owner's order. Fire-and-forget effects (`emit`, `checkpoint`,
//! `set_plan`, `compacted`) are plain sends; `drain_steering` and
//! `request_approval` carry a `oneshot` reply - the Agent owns the Steering
//! queue (a dead Turn cannot hand back its mailbox) and consults the Standing
//! Approvals when relaying (an auto-approve emits `approval_auto` and answers
//! the reply immediately; the Turn cannot tell the difference).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::approvals::{ApprovalId, Approvals, Decide, Decision, Request};
use crate::compaction::Compaction;
use crate::content::ContentBlock;
use crate::conversation::{Conversation, ConversationOpts};
use crate::event::Event;
use crate::llm::Llm;
use crate::llm::model::Model;
use crate::llm::response::StopReason as RespStopReason;
use crate::session::log::{self, Entry as LogEntry, Log, ResumeError, RiderTag, StopReason};
use crate::session::{RecoveryShape, Session};
use crate::turn::AgentDeps;
use crate::turn::governor::endgame::Recovery;
use crate::turn::loop_::{Outcome as LoopOutcome, OutcomeStop, RunOpts};
use crate::turn::settlement::{Event as SettleEvent, Outcome, Reason, Rollover, Settlement};
use crate::{tools, voice};

#[cfg(test)]
mod tests;

/// The default system prompt (baud's `Baud.Agent.system_prompt/0`). Public for
/// the UI and tests.
pub fn system_prompt() -> &'static str {
    voice::system_prompt()
}

/// The Agent's running status (baud's `:idle | :running`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
}

/// Returned by [`AgentHandle::submit`] when a Turn is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// Returned by [`AgentHandle::steer`] when no Turn is running (the caller should
/// [`AgentHandle::submit`] instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Idle;

/// Resume facts for the Transcript (baud's `resume_info/1`): the log path folded
/// and the header facts that yielded to the new Session's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeInfo {
    pub path: String,
    pub drift: Vec<log::Drift>,
}

/// How to resume (baud's `:resume` opt): a specific log path, or the newest log
/// in the Session's dir.
#[derive(Debug, Clone)]
pub enum Resume {
    Path(String),
    Latest,
}

/// The options [`AgentHandle::start`] resolves the Session's facts from (baud's
/// `start_link/1` opts). Either a prebuilt `session` or the `llm`/`root`/…
/// shorthand, plus the injected [`Llm`] boundary (ADR-0020), an optional system
/// prompt override, and an optional Resume.
pub struct StartOpts {
    /// A prebuilt Session (the fixed facts). When `None`, the caller must supply
    /// one - the Rust port has no ambient config default at this seam, so tests
    /// and the app build the Session and pass it here.
    pub session: Session,
    /// The LLM boundary, injected per instance (ADR-0020). The real
    /// `Dispatcher` or a test `FakeLlm` behind the same trait.
    pub llm: Arc<dyn Llm>,
    /// The system prompt (baud defaults to the Voice; callers/tests may
    /// override).
    pub system_prompt: String,
    /// Resume a prior Session Log before opening the new one, or `None` for a
    /// fresh Session.
    pub resume: Option<Resume>,
}

impl StartOpts {
    /// A StartOpts with the default system prompt and no Resume.
    pub fn new(session: Session, llm: Arc<dyn Llm>) -> Self {
        StartOpts {
            session,
            llm,
            system_prompt: system_prompt().to_string(),
            resume: None,
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_resume(mut self, resume: Resume) -> Self {
        self.resume = Some(resume);
        self
    }
}

/// Why [`AgentHandle::start`] failed. Only Resume can fail init loudly (baud
/// raises `ArgumentError`): a root mismatch or an unreadable/foreign log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// The Resume log's Project Root differs from the resuming Session's.
    ResumeRootMismatch(String),
    /// The Resume log could not be read/decoded, or no log was found.
    ResumeFailed(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::ResumeRootMismatch(p) => {
                write!(f, "resume: cannot resume {p}: root_mismatch")
            }
            StartError::ResumeFailed(m) => write!(f, "resume: {m}"),
        }
    }
}

impl std::error::Error for StartError {}

// ---------------------------------------------------------------------------
// The message protocol: public Commands + the Turn's talk-back, one mpsc.
// ---------------------------------------------------------------------------

/// A message the Turn task sends back to the Agent (baud's `{:turn_event, ...}`,
/// `{:turn_checkpoint, ...}`, `{:turn_plan, ...}`, `{:compacted, ...}`, and the
/// `:drain_steering`/approval reply calls). Routed through the same mpsc as
/// [`Command`] so Event order is the single owner's order.
pub enum TurnMsg {
    /// A turn Event: logged (at the contract points) then broadcast.
    Emit(Event),
    /// Partial-Turn snapshot, after every Tool Result; the latest wins.
    Checkpoint(Conversation),
    /// A successful plan Tool Call: hold it outside the Conversation and log it.
    SetPlan(String),
    /// The Loop's drain point: return all queued Steering and empty the queue.
    DrainSteering(oneshot::Sender<Vec<String>>),
    /// An Approval request: the Agent consults Standing Approvals and either
    /// answers immediately (auto) or holds the reply until `approve` arrives.
    RequestApproval {
        id: String,
        command: String,
        reply: oneshot::Sender<bool>,
    },
    /// A completed Compaction from the Turn task: log the `{:compacted, ...}`
    /// entry and update the accumulated state (ADR-0012).
    Compacted {
        new_state: Compaction,
        skip_count: u64,
        tokens_before: u64,
    },
    /// A Handoff finished seeding inside the Recovery Turn task (CONTEXT.md:
    /// Handoff): the fresh Conversation replaces the retired one, the
    /// compaction state updates, and the `handoff` + `recovery` entries are
    /// logged so Resume rebuilds the same seed.
    HandoffSeeded {
        conversation: Conversation,
        new_state: Compaction,
        narrative: Option<String>,
        verification: Option<String>,
        prompt: String,
    },
}

/// A public API Command (baud's `handle_call`s). Queries carry a `oneshot` reply
/// (ADR-0017).
pub enum Command {
    Submit(String, oneshot::Sender<Result<(), Busy>>),
    Steer(String, oneshot::Sender<Result<(), Idle>>),
    /// Swap the Active Model to a scoped `provider/model-id` (ADR-0033
    /// amendment): resolved against the Session's fixed Provider set, so an
    /// unknown Provider is an `Err` the caller surfaces. Takes effect on the
    /// next Turn - an in-flight Turn is unaffected. No-op semantics
    /// (re-selecting the current model) are the caller's job, not here.
    SetModel(String, oneshot::Sender<Result<(), String>>),
    /// The Active Model identifier the next Turn will call (ADR-0033), so a
    /// caller can mark "(current)".
    ActiveModel(oneshot::Sender<String>),
    /// List the models the Active Model's endpoint offers (ADR-0033, ADR-0002
    /// amendment). The Agent owns the `Llm` and the mutable `connection`, so the
    /// listed endpoint always matches the Active Model's; the fetch runs OFF the
    /// actor (a spawned task over clones) so the network never blocks the actor
    /// loop, and the oneshot carries the boundary's `Result<Vec<String>, String>`.
    ListModels(oneshot::Sender<Result<Vec<String>, String>>),
    Approve(String, Decision, oneshot::Sender<()>),
    Cancel(oneshot::Sender<()>),
    Status(oneshot::Sender<Status>),
    Conversation(oneshot::Sender<Conversation>),
    SessionQuery(oneshot::Sender<Session>),
    Plan(oneshot::Sender<Option<String>>),
    ResumeInfoQuery(oneshot::Sender<Option<ResumeInfo>>),
}

/// Both flavors ride the one mpsc (public so the Turn shell can post `Turn`
/// messages back to the Agent; only `Turn` is constructed outside this module).
pub enum Msg {
    Command(Command),
    Turn(TurnMsg),
    /// The awaited Turn task yielded (baud's `{ref, outcome}` / `{:DOWN, ...}`).
    Settle(LoopOutcome),
    /// The Turn task panicked or was aborted (baud's `:DOWN` with no reply).
    TurnDown(Reason),
}

/// The handle callers hold (baud's `GenServer.server()`): an mpsc sender for
/// Commands and a `broadcast` Sender to mint subscriber receivers. Cloneable.
#[derive(Clone)]
pub struct AgentHandle {
    tx: mpsc::UnboundedSender<Msg>,
    events: broadcast::Sender<Event>,
}

impl AgentHandle {
    /// Starts the Agent (baud's `start_link/1`). Resolves the Session facts
    /// once, resumes a prior log if asked (failing loudly on a root mismatch or
    /// unreadable log), opens a fresh log, spawns the owning task, and returns
    /// the handle.
    pub fn start(opts: StartOpts) -> Result<AgentHandle, StartError> {
        let StartOpts {
            session,
            llm,
            system_prompt,
            resume,
        } = opts;

        // Resume BEFORE opening the new log (baud, ADR-0010): root mismatch fails
        // loudly; other drift yields to the new Session and is reported. One fold
        // yields the messages, the Transcript-facing `ResumeInfo`, AND the two
        // governance facts below - no re-reading the log for the Plan or the
        // recovery count.
        let (resumed_messages, resume_info, governance) = maybe_resume(resume, &session)?;

        // The Plan is held outside the Conversation; a Resume restores the last
        // logged Plan (folded above) so the model keeps its goal across a restart.
        // A Resume also restores the recoveries the logged request consumed, so a
        // resumed Session cannot re-trigger them unboundedly. Both were computed
        // in the single fold, sharing its torn-line tolerance.
        let ResumedGovernance {
            plan,
            recoveries_used,
        } = governance;

        // The tool specs ride with every request but live outside the messages;
        // the estimate has to count them or Eviction fires late (baud's
        // `String.length(JSON.encode!(Baud.Tools.specs()))`). A ToolSpec
        // serializes to exactly its wire shape (name, description,
        // input_schema), so serde counts what a request would carry.
        let overhead = serde_json::to_string(&tools::specs())
            .map(|s| s.chars().count() as u64)
            .unwrap_or(0);

        let mut conversation = Conversation::new(
            system_prompt,
            ConversationOpts::new(session.context_budget, session.model.max_tokens)
                .overhead_chars(overhead)
                .eviction_slack(session.eviction_slack)
                .dead_mass_fraction(session.dead_mass_fraction)
                .compaction_keep(session.compaction_keep),
        );
        // A Resume seeds the messages verbatim ahead of the (empty) fresh ones.
        let mut seeded = resumed_messages.clone();
        seeded.extend(std::mem::take(&mut conversation.messages));
        conversation.messages = seeded;

        // Every Session gets a fresh log; a Resume seeds it with the folded
        // messages verbatim so the new file alone rebuilds the Conversation.
        let mut log = Log::open(&session).ok();
        if let Some(ref mut log) = log {
            for message in &resumed_messages {
                log.append(LogEntry::Message(message.clone()));
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let (events, _rx0) = broadcast::channel(1024);

        // The Active Model lives here as mutable Agent state (ADR-0033,
        // CONTEXT.md: Active Model), seeded from the Session's launch-resolved
        // Model. Each Turn is spawned with a snapshot of THIS Model, so a
        // `SetModel` between Turns lands on the next Turn and an in-flight
        // Turn finishes on the Model it captured.
        let model = session.model.clone();

        let state = AgentState {
            session,
            model,
            llm,
            conversation,
            log,
            resume_info,
            plan,
            events: events.clone(),
            task: None,
            cancel_flag: false,
            settlement: Settlement::new(),
            approvals: Approvals::new(),
            approval_replies: HashMap::new(),
            steering: Vec::new(),
            compaction: Compaction::new(),
            recoveries_used,
            self_tx: tx.clone(),
        };

        tokio::spawn(run_agent(state, rx));

        Ok(AgentHandle { tx, events })
    }

    /// Subscribes to the Agent's Event stream (baud's `subscribe/1`). A dropped
    /// receiver auto-cleans (tokio broadcast semantics - see the module note in
    /// `agent.rs` tests).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Submits a user prompt, starting a Turn (baud's `submit/2`). `Err(Busy)`
    /// while a Turn runs.
    pub async fn submit(&self, prompt: impl Into<String>) -> Result<(), Busy> {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Msg::Command(Command::Submit(prompt.into(), reply)))
            .is_err()
        {
            return Err(Busy); // dead Agent; treat as unable to accept work
        }
        rx.await.unwrap_or(Err(Busy))
    }

    /// Queues Steering for the running Turn (baud's `steer/2`). `Err(Idle)` when
    /// no Turn is running.
    pub async fn steer(&self, text: impl Into<String>) -> Result<(), Idle> {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Msg::Command(Command::Steer(text.into(), reply)))
            .is_err()
        {
            return Err(Idle);
        }
        rx.await.unwrap_or(Err(Idle))
    }

    /// Swaps the Active Model to a scoped `provider/model-id` (ADR-0033
    /// amendment). Takes effect on the next Turn; an in-flight Turn finishes
    /// on the Model it captured. An unresolvable id is an `Err` naming the
    /// reason; a dead Agent answers `Err` too, matching `list_models`.
    pub async fn set_model(&self, model: String) -> Result<(), String> {
        self.query(move |reply| Command::SetModel(model, reply))
            .await
            .unwrap_or_else(|| Err("agent unavailable".to_string()))
    }

    /// The Active Model identifier the next Turn will call (ADR-0033), for a
    /// caller marking "(current)".
    pub async fn active_model(&self) -> String {
        self.query(Command::ActiveModel).await.expect("agent alive")
    }

    /// Lists the models the Active Model's endpoint offers (ADR-0033), by
    /// asking the Agent - the owner of the `Llm` and the mutable `connection` -
    /// so the listed endpoint always matches the model the next Turn will
    /// call. The Agent fetches off its actor loop; this awaits the reply. A
    /// dead Agent (or a dropped reply) surfaces as `Err`, matching the
    /// boundary's fallible shape.
    pub async fn list_models(&self) -> Result<Vec<String>, String> {
        self.query(Command::ListModels)
            .await
            .unwrap_or_else(|| Err("agent unavailable".to_string()))
    }

    /// Resolves a pending run_command Approval (baud's `approve/3`).
    /// `ApproveAlways` records the exact command string as a Standing Approval.
    pub async fn approve(&self, approval_id: impl Into<String>, decision: Decision) {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Command(Command::Approve(
            approval_id.into(),
            decision,
            reply,
        )));
        let _ = rx.await;
    }

    /// Cancels the running Turn (baud's `cancel/1`). No-op when idle.
    pub async fn cancel(&self) {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Command(Command::Cancel(reply)));
        let _ = rx.await;
    }

    /// The Agent's status (baud's `status/1`).
    pub async fn status(&self) -> Status {
        self.query(Command::Status).await.unwrap_or(Status::Idle)
    }

    /// The current Conversation (baud's `conversation/1`).
    pub async fn conversation(&self) -> Conversation {
        self.query(Command::Conversation)
            .await
            .expect("agent alive")
    }

    /// The Session's fixed facts (baud's `session/1`).
    pub async fn session(&self) -> Session {
        self.query(Command::SessionQuery)
            .await
            .expect("agent alive")
    }

    /// The current Plan, or `None` (baud's `plan/1`).
    pub async fn plan(&self) -> Option<String> {
        self.query(Command::Plan).await.flatten()
    }

    /// Resume facts for the Transcript (baud's `resume_info/1`).
    pub async fn resume_info(&self) -> Option<ResumeInfo> {
        self.query(Command::ResumeInfoQuery).await.flatten()
    }

    async fn query<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Command) -> Option<T> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Msg::Command(make(reply))).is_err() {
            return None;
        }
        rx.await.ok()
    }
}

// ---------------------------------------------------------------------------
// The owned state + the actor loop.
// ---------------------------------------------------------------------------

struct AgentState {
    session: Session,
    // The Active Model as mutable Agent state (ADR-0033 amendment, CONTEXT.md:
    // Active Model): an owned Model seeded from `session.model` at launch and
    // read - not `session.model` - when spawning a Turn. `Command::SetModel`
    // swaps the whole Model (resolved against the Session's fixed Provider
    // set); the budget figures keep their once-at-launch shape - per-Turn
    // recomputation from the capture is a later stage.
    model: Model,
    llm: Arc<dyn Llm>,
    conversation: Conversation,
    log: Option<Log>,
    resume_info: Option<ResumeInfo>,
    plan: Option<String>,
    events: broadcast::Sender<Event>,
    // The running Turn: the AbortHandle for `cancel` (the real JoinHandle lives
    // in the spawned watcher that awaits it and posts the outcome back).
    task: Option<tokio::task::AbortHandle>,
    // The user cancelled the running Turn; a following abort settles as
    // cancelled (Turn Settlement needs both the flag and the abort).
    cancel_flag: bool,
    settlement: Settlement,
    approvals: Approvals,
    // The per-Turn Approval reply channels, keyed by the Loop's ref string: the
    // Turn parks awaiting this oneshot; `approve` (or a Standing Approval hit)
    // answers it.
    approval_replies: HashMap<String, oneshot::Sender<bool>>,
    steering: Vec<String>,
    compaction: Compaction,
    // Recovery Turns consumed serving the CURRENT user request (CONTEXT.md:
    // Recovery Turn - the Setpoint bounds recoveries per user request, not
    // per Turn). Cross-Turn state lives with the Agent: reset when a genuine
    // user prompt starts a new request (`Command::Submit`), NOT by Rollover
    // or a Recovery Turn; a Resume restores it from the folded log.
    recoveries_used: u64,
    // A clone of the mpsc sender, handed to the Turn's AgentDeps so the Turn
    // talks back over the same channel, and used to post the Turn's outcome.
    self_tx: mpsc::UnboundedSender<Msg>,
}

async fn run_agent(mut state: AgentState, mut rx: mpsc::UnboundedReceiver<Msg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Command(cmd) => handle_command(&mut state, cmd),
            Msg::Turn(turn) => handle_turn(&mut state, turn),
            Msg::Settle(outcome) => settle(&mut state, LoopOrDown::Loop(outcome)),
            Msg::TurnDown(reason) => settle(&mut state, LoopOrDown::Down(reason)),
        }
    }
}

fn handle_command(state: &mut AgentState, cmd: Command) {
    match cmd {
        Command::Submit(prompt, reply) => {
            if state.task.is_some() {
                let _ = reply.send(Err(Busy));
            } else {
                // A genuine user prompt starts a new request: the recovery
                // budget resets. Rollover starts its Turn via `start_turn`
                // directly and keeps the count - its Steering missed a Turn
                // of the SAME request.
                state.recoveries_used = 0;
                start_turn(state, prompt);
                let _ = reply.send(Ok(()));
            }
        }
        Command::Steer(text, reply) => {
            if state.task.is_some() {
                state.steering.push(text.clone());
                broadcast(state, Event::steering_queued(text));
                let _ = reply.send(Ok(()));
            } else {
                let _ = reply.send(Err(Idle));
            }
        }
        Command::SetModel(scoped, reply) => {
            let _ = reply.send(swap_active_model(state, &scoped));
        }
        Command::ActiveModel(reply) => {
            let _ = reply.send(state.model.scoped_id());
        }
        Command::ListModels(reply) => spawn_list_models(state, reply),
        Command::Approve(id, decision, reply) => {
            approve(state, id, decision);
            let _ = reply.send(());
        }
        Command::Cancel(reply) => {
            if let Some(abort) = &state.task {
                abort.abort();
                state.cancel_flag = true;
                state.settlement = std::mem::take(&mut state.settlement).note_cancelled();
            }
            let _ = reply.send(());
        }
        Command::Status(reply) => {
            let _ = reply.send(if state.task.is_some() {
                Status::Running
            } else {
                Status::Idle
            });
        }
        Command::Conversation(reply) => {
            let _ = reply.send(state.conversation.clone());
        }
        Command::SessionQuery(reply) => {
            let _ = reply.send(state.session.clone());
        }
        Command::Plan(reply) => {
            let _ = reply.send(state.plan.clone());
        }
        Command::ResumeInfoQuery(reply) => {
            let _ = reply.send(state.resume_info.clone());
        }
    }
}

fn handle_turn(state: &mut AgentState, turn: TurnMsg) {
    match turn {
        TurnMsg::Emit(event) => {
            log_event(state, &event);
            broadcast(state, event);
        }
        TurnMsg::Checkpoint(conversation) => {
            // Only meaningful while a Turn is running.
            if state.task.is_some() {
                state.settlement =
                    std::mem::take(&mut state.settlement).note_checkpoint(conversation);
            }
        }
        TurnMsg::SetPlan(plan) => {
            log_entry(state, LogEntry::Plan(plan.clone()));
            state.plan = Some(plan);
        }
        TurnMsg::DrainSteering(reply) => {
            let drained = std::mem::take(&mut state.steering);
            let _ = reply.send(drained);
        }
        TurnMsg::RequestApproval { id, command, reply } => {
            request_approval(state, id, command, reply);
        }
        TurnMsg::Compacted {
            new_state,
            skip_count,
            tokens_before,
        } => {
            let entry = new_state.session_log_entry(skip_count as usize, tokens_before);
            log_entry(
                state,
                LogEntry::Compacted {
                    summary: entry.summary.unwrap_or_default(),
                    skip_count: entry.skip_count as u64,
                    tokens_before: entry.tokens_before,
                    file_ops: entry.file_ops,
                    original_task: entry.original_task,
                },
            );
            broadcast(state, Event::compaction_progress("done"));
            state.compaction = new_state;
        }
        TurnMsg::HandoffSeeded {
            conversation,
            new_state,
            narrative,
            verification,
            prompt,
        } => {
            // The seed and its prompt enter the log the way Compaction does:
            // the narrative alone plus the mechanical facts as their own
            // fields, so the fold recomposes a byte-identical seed message,
            // then the `recovery` entry merges the prompt onto it.
            log_entry(
                state,
                LogEntry::Handoff {
                    summary: narrative,
                    file_ops: new_state.file_ops.clone(),
                    original_task: new_state.original_task.clone(),
                    verification,
                },
            );
            log_entry(
                state,
                LogEntry::Recovery {
                    shape: RecoveryShape::Handoff,
                    text: prompt,
                },
            );
            state.compaction = new_state;
            // The fresh Conversation is the new base: a cancel before any
            // checkpoint must settle on the seeded state the log now holds,
            // not on the retired one.
            state.conversation = conversation;
        }
    }
}

// The SetModel swap (ADR-0033 amendment): the whole Model swaps - the scoped
// id resolves against the Session's fixed Provider set (Catalog figures for
// known built-in models, config synthesis otherwise). The next spawned Turn
// snapshots it; an in-flight Turn is unaffected. An unresolvable id leaves
// the Active Model as-is and the Err rides back to the caller.
fn swap_active_model(state: &mut AgentState, scoped: &str) -> Result<(), String> {
    state.model = state.session.resolve_model(scoped)?;
    Ok(())
}

// The ListModels fetch, OFF the actor (ADR-0011/0017: never block the actor
// loop on the network). Clone the boundary and the Active Model's Provider so
// the listed endpoint always matches the model the next Turn will call, then
// answer the oneshot from the spawned task.
fn spawn_list_models(state: &AgentState, reply: oneshot::Sender<Result<Vec<String>, String>>) {
    let Some(provider) = state.session.provider_of(&state.model).cloned() else {
        // Unreachable while resolution guards SetModel; answer rather than
        // panic if it ever regresses.
        let _ = reply.send(Err(format!("unknown provider {:?}", state.model.provider)));
        return;
    };
    let llm = Arc::clone(&state.llm);
    tokio::spawn(async move {
        let _ = reply.send(llm.list_models(&provider).await);
    });
}

// A Standing Approval covering the exact string answers the Turn immediately -
// no modal, an `approval_auto` event; the Turn cannot tell the difference.
// Otherwise the request becomes pending and its reply channel is held until the
// user's `approve` arrives (baud's approval_request handler).
fn request_approval(
    state: &mut AgentState,
    id: String,
    command: String,
    reply: oneshot::Sender<bool>,
) {
    let approval_id = ApprovalId::from_ref(id.clone());
    match std::mem::take(&mut state.approvals).request(approval_id, command.clone()) {
        Request::Auto(approvals) => {
            state.approvals = approvals;
            let _ = reply.send(true);
            broadcast(state, Event::approval_auto(command));
        }
        Request::Pending(approvals) => {
            state.approvals = approvals;
            state.approval_replies.insert(id.clone(), reply);
            broadcast(state, Event::approval_request(id, command));
        }
    }
}

// Only forward the Approval the Turn is actually waiting on; the fold ignores
// duplicate or stale approve calls (baud's approve handler).
fn approve(state: &mut AgentState, id: String, decision: Decision) {
    if state.task.is_none() {
        return; // No Turn: drop it.
    }
    let approval_id = ApprovalId::from_ref(id.clone());
    match std::mem::take(&mut state.approvals).decide(approval_id, decision) {
        Decide::Forward(approved, approvals) => {
            state.approvals = approvals;
            if let Some(reply) = state.approval_replies.remove(&id) {
                let _ = reply.send(approved);
            }
        }
        Decide::Ignore(approvals) => {
            state.approvals = approvals;
        }
    }
}

fn broadcast(state: &AgentState, event: Event) {
    // A dropped receiver auto-cleans; `send` erroring (no live subscribers) is
    // not our problem (ADR-0017 / tokio broadcast).
    let _ = state.events.send(event);
}

// ---- Session Log append points (baud's log_event / log_entry) --------------

// Append fail-open: an IO failure kills the log, never the Turn. The Transcript
// hears about it once (baud's log_entry rescue).
fn log_entry(state: &mut AgentState, entry: LogEntry) {
    if let Some(log) = &mut state.log {
        log.append(entry);
    }
}

// The Conversation events worth persisting, picked off the relay path (baud's
// log_event). Nudges ride as user-role {:nudge, ...} entries.
fn log_event(state: &mut AgentState, event: &Event) {
    match event {
        Event::MessageEnd { content, .. } => {
            log_entry(state, LogEntry::AssistantBlocks(content.clone()));
        }
        Event::ToolResult {
            id,
            content,
            is_error,
            ..
        } => {
            log_entry(
                state,
                LogEntry::ToolResult(ContentBlock::tool_result(
                    id.clone(),
                    content.clone(),
                    *is_error,
                )),
            );
        }
        Event::SteeringDelivered { text } => {
            log_entry(state, LogEntry::Steering(text.clone()));
        }
        Event::VerifyNudge { text }
        | Event::VerifyFailedNudge { text }
        | Event::EmptyResponseNudge { text }
        | Event::ExploreNudge { text } => {
            log_entry(state, LogEntry::Nudge(text.clone()));
        }
        // Anchors and Endgame prompts are Conversation events the model read:
        // they persist like Nudges, tagged, so Resume rebuilds the same bytes
        // (CONTEXT.md: every rider is logged to the Session Log).
        Event::Anchor { text } => log_rider(state, RiderTag::Anchor, text),
        Event::WrapUpWarning { text } => log_rider(state, RiderTag::WrapUpWarning, text),
        Event::VerificationPass { text } => log_rider(state, RiderTag::VerificationPass, text),
        Event::FinalPass { text } => log_rider(state, RiderTag::FinalPass, text),
        // A malformed-tool-call re-draw (ADR-0030): silent to the Conversation
        // but durable in the log, so a resumed Session can be audited for it.
        Event::Retry {
            error,
            attempt,
            budget,
        } => {
            log_entry(
                state,
                LogEntry::Retry {
                    error: error.clone(),
                    attempt: *attempt,
                    budget: *budget,
                },
            );
        }
        _ => {}
    }
}

fn log_rider(state: &mut AgentState, tag: RiderTag, text: &str) {
    log_entry(
        state,
        LogEntry::Rider {
            tag,
            text: text.to_string(),
        },
    );
}

// ---- Turn start (submit + Rollover + Recovery) ------------------------------

// One Turn start for submit and Rollover alike (baud's start_turn).
fn start_turn(state: &mut AgentState, prompt: String) {
    log_entry(state, LogEntry::UserText(prompt.clone()));
    state.conversation.add_user_text(prompt);
    spawn_turn(state);
}

// Spawns a Turn over the Agent's CURRENT Conversation (the prompt - user or
// Voice - is already appended and logged by the caller).
fn spawn_turn(state: &mut AgentState) {
    reset_turn_state(state);
    // The AgentDeps wires each effect to the Agent's mpsc + the Session's Llm.
    // The Turn captures a SNAPSHOT of the Agent's mutable Model (the Active
    // Model), not `session.model`: a `SetModel` between Turns lands on this
    // next Turn, and an in-flight Turn keeps the Model it already captured
    // (ADR-0017's read-only guest; ADR-0033).
    let deps = AgentDeps::new(
        state.self_tx.clone(),
        Arc::clone(&state.llm),
        state.model.clone(),
        state.session.temperature,
        state.compaction.clone(),
    );
    let conversation = state.conversation.clone();
    let session = state.session.clone();
    let opts = run_opts(state, state.compaction.original_task.clone());

    let turn =
        tokio::spawn(async move { crate::turn::run(conversation, session, deps, opts).await });
    watch_turn(state, turn);
}

// Resets the per-Turn state before a Turn task spawns.
fn reset_turn_state(state: &mut AgentState) {
    state.settlement = Settlement::new();
    state.approvals = std::mem::take(&mut state.approvals).reset();
    state.approval_replies.clear();
    state.cancel_flag = false;
}

fn run_opts(state: &AgentState, original_task: Option<String>) -> RunOpts {
    RunOpts {
        plan: state.plan.clone(),
        original_task,
        recoveries_used: state.recoveries_used,
    }
}

// The Turn task's watcher. The Agent holds only the AbortHandle (for
// `cancel`); the spawned watcher OWNS the JoinHandle, awaits it, and posts the
// outcome back through the mpsc (baud's `{ref, outcome}` / `:DOWN`). This lets
// the Agent both abort (cancel) and observe the outcome without co-owning one
// handle.
fn watch_turn(state: &mut AgentState, turn: tokio::task::JoinHandle<LoopOutcome>) {
    let reference = mint_turn_ref();
    let abort = turn.abort_handle();
    let out_tx = state.self_tx.clone();

    tokio::spawn(async move {
        match turn.await {
            Ok(outcome) => {
                let _ = out_tx.send(Msg::Settle(outcome));
            }
            Err(join_err) => {
                let reason = if join_err.is_cancelled() {
                    // abort() - Turn Settlement pairs this with the cancel flag.
                    Reason::atom("shutdown")
                } else {
                    // A panic; close with the failure marker (baud's turn_error
                    // + "[turn failed]").
                    Reason::tuple("turn_panic")
                };
                let _ = out_tx.send(Msg::TurnDown(reason));
            }
        }
    });

    state.task = Some(abort);
    broadcast(state, Event::turn_started(reference));
}

// ---- Recovery Turn (CONTEXT.md: Recovery Turn) -------------------------------

// Executes the Endgame Governor's close-and-open-a-Recovery-Turn Intervention:
// the Governor judged (trigger + both Setpoints); the Agent - owner of the
// Conversation and the Turn lifecycle - opens the next Turn. The prompt is the
// Voice's: the only Turn whose prompt Suspenders authors.
fn start_recovery(state: &mut AgentState, recovery: Recovery) {
    state.recoveries_used += 1;
    let prompt = voice::recovery_prompt(recovery.verification_failing).to_string();
    broadcast(state, Event::recovery_turn(recovery.shape, prompt.clone()));

    match recovery.shape {
        // Continuation keeps the Conversation: the recovery prompt is the
        // next Turn's prompt, mechanically like Rollover's auto-submit but
        // logged as the Voice's, not the user's.
        RecoveryShape::Continuation => {
            log_entry(
                state,
                LogEntry::Recovery {
                    shape: RecoveryShape::Continuation,
                    text: prompt.clone(),
                },
            );
            state.conversation.merge_user_text(prompt);
            spawn_turn(state);
        }
        RecoveryShape::Handoff => spawn_handoff_turn(state, prompt, recovery.failing_command),
    }
}

// The Handoff arm: the Recovery Turn task first seeds the fresh Conversation
// (the compaction machinery's LLM narrative + mechanical facts + the
// verification verbatim + the prompt - a long LLM call, so it runs in the Turn
// task, never on the Agent actor, per ADR-0012), posts the seed back
// (`HandoffSeeded` logs it and retires the old Conversation), then runs the
// Turn over the seeded Conversation. The Plan is harness-owned and rides
// RunOpts verbatim - it survives the retirement untouched. `failing_command`
// (the Dangling Failure the recovery names, `None` on an unverified-writes
// recovery) tells the seed which command's result to carry verbatim.
fn spawn_handoff_turn(state: &mut AgentState, prompt: String, failing_command: Option<String>) {
    reset_turn_state(state);
    let dying = state.conversation.clone();
    let compaction = state.compaction.clone();
    let llm = Arc::clone(&state.llm);
    // A snapshot of the Agent's mutable Model (the Active Model), as
    // `spawn_turn` does - the seed narrative and the Recovery Turn both run on
    // the model current at spawn (ADR-0033).
    let model = state.model.clone();
    let temperature = state.session.temperature;
    let session = state.session.clone();
    let opts = run_opts(state, None);
    let tx = state.self_tx.clone();

    let turn = tokio::spawn(async move {
        let seeded = compaction
            .seed_handoff(
                &dying,
                &prompt,
                failing_command.as_deref(),
                llm.as_ref(),
                &model,
                temperature,
            )
            .await;
        let _ = tx.send(Msg::Turn(TurnMsg::HandoffSeeded {
            conversation: seeded.conversation.clone(),
            new_state: seeded.state.clone(),
            narrative: seeded.narrative,
            verification: seeded.verification,
            prompt,
        }));
        // Built here, not before the spawn: the Turn's deps must carry the
        // SEEDED compaction state so a later compaction telescopes from it.
        let deps = AgentDeps::new(
            tx.clone(),
            Arc::clone(&llm),
            model.clone(),
            temperature,
            seeded.state.clone(),
        );
        let opts = RunOpts {
            original_task: seeded.state.original_task.clone(),
            ..opts
        };
        crate::turn::run(seeded.conversation, session, deps, opts).await
    });
    watch_turn(state, turn);
}

fn mint_turn_ref() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("turn-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

// ---- Settlement ------------------------------------------------------------

enum LoopOrDown {
    Loop(LoopOutcome),
    Down(Reason),
}

fn settle(state: &mut AgentState, outcome: LoopOrDown) {
    // If there is no running Turn, this is a stale outcome (e.g. the watcher for
    // an already-settled Turn); ignore it.
    if state.task.is_none() {
        return;
    }

    // The Endgame Governor's recovery directive, held aside: the Turn settles
    // exactly like an Ok limit close first, then the Agent executes the
    // opening. A cancel that raced the close wins - cancel means stop
    // everything, recovery included.
    let recovery = match (&outcome, state.cancel_flag) {
        (LoopOrDown::Loop(LoopOutcome::Recover(_, _, recovery)), false) => Some(recovery.clone()),
        _ => None,
    };

    // The cancel flag rides the Settlement (note_cancelled was called on
    // `cancel`); the outcome only needs mapping into the settlement vocabulary.
    let settle_outcome = to_settlement_outcome(outcome);

    let resolution = state
        .settlement
        .settle(settle_outcome, &state.conversation, &state.steering);

    state.task = None;
    state.conversation = resolution.conversation;
    state.settlement = Settlement::new();
    state.approvals = std::mem::take(&mut state.approvals).reset();
    state.approval_replies.clear();
    state.steering.clear();
    state.cancel_flag = false;

    log_entry(
        state,
        LogEntry::Settled {
            outcome: resolution.log_entry.outcome,
            stop_reason: resolution.log_entry.stop_reason,
            reason: resolution.log_entry.reason,
        },
    );
    broadcast(state, settle_event_to_event(&resolution.event));

    // Rollover outranks recovery: rolled-over Steering is the user's voice
    // continuing the same request, which is itself the bounded continuation
    // the recovery would have bought - and the recovery budget stays unspent.
    match resolution.rollover {
        Rollover::Submit(prompt) => start_turn(state, prompt),
        Rollover::None => {
            if let Some(recovery) = recovery {
                start_recovery(state, recovery);
            }
        }
    }
}

// Maps the Loop's outcome (or the watcher's Down) into the settlement's outcome
// vocabulary.
fn to_settlement_outcome(outcome: LoopOrDown) -> Outcome {
    match outcome {
        LoopOrDown::Loop(LoopOutcome::Ok(conv, stop)) => {
            Outcome::Ok(conv, outcome_stop_to_log(stop))
        }
        // A recovery close settles exactly like an Ok limit close (the marker
        // is already appended); the directive was held aside by `settle`.
        LoopOrDown::Loop(LoopOutcome::Recover(conv, reason, _)) => Outcome::Ok(conv, reason),
        // The Loop already closed the Conversation with the failure marker and
        // kept the errored response's partial text (the LLM error algebra).
        LoopOrDown::Loop(LoopOutcome::Failed(reason, conv)) => {
            Outcome::Failed(Reason::tuple(reason), conv)
        }
        // Eviction + Compaction together could not fit the request.
        LoopOrDown::Loop(LoopOutcome::Error) => {
            Outcome::Error(Reason::atom("context_budget_exhausted"))
        }
        // A panic or an abort - settlement tells them apart via the cancel flag.
        LoopOrDown::Down(reason) => Outcome::Down(reason),
    }
}

fn outcome_stop_to_log(stop: OutcomeStop) -> StopReason {
    match stop {
        OutcomeStop::Reason(r) => r,
        // A custom after-Pass Stop atom - the wired AgentDeps never produces one
        // (its after_pass defaults to Continue). Degrade to Unknown.
        OutcomeStop::Custom(_) => StopReason::Unknown,
    }
}

// The settlement's Event → the broadcast event::Event (baud shares one shape;
// the Rust port has two typed enums that carry the same facts).
fn settle_event_to_event(event: &SettleEvent) -> Event {
    match event {
        SettleEvent::TurnFinished {
            stop_reason,
            token_estimate,
            context_budget,
        } => Event::TurnFinished {
            stop_reason: log_stop_to_resp(*stop_reason),
            token_estimate: *token_estimate,
            context_budget: *context_budget,
        },
        SettleEvent::TurnError(reason) => Event::TurnError {
            reason: reason.inspect(),
        },
        SettleEvent::TurnCancelled => Event::TurnCancelled,
    }
}

fn log_stop_to_resp(stop: StopReason) -> RespStopReason {
    match stop {
        StopReason::EndTurn => RespStopReason::EndTurn,
        StopReason::ToolUse => RespStopReason::ToolUse,
        StopReason::MaxTokens => RespStopReason::MaxTokens,
        StopReason::StopSequence => RespStopReason::StopSequence,
        StopReason::TurnLimit
        | StopReason::TurnLimitStuck
        | StopReason::Error
        | StopReason::Unknown => RespStopReason::Unknown,
    }
}

// ---- Resume ----------------------------------------------------------------

/// The governance facts a Resume restores alongside the Conversation: the last
/// logged Plan (held outside the Conversation) and the recoveries the logged
/// request consumed (per-request bound). Computed in the single fold, so they
/// never belong on the Transcript-facing [`ResumeInfo`] - the Agent threads
/// them privately instead.
#[derive(Default)]
struct ResumedGovernance {
    plan: Option<String>,
    recoveries_used: u64,
}

fn maybe_resume(
    resume: Option<Resume>,
    session: &Session,
) -> Result<
    (
        Vec<crate::content::Message>,
        Option<ResumeInfo>,
        ResumedGovernance,
    ),
    StartError,
> {
    let path = match resume {
        None => return Ok((Vec::new(), None, ResumedGovernance::default())),
        Some(Resume::Path(p)) => p,
        Some(Resume::Latest) => log::latest(&session.session_dir).ok_or_else(|| {
            StartError::ResumeFailed(format!("no Session Log found in {}", session.session_dir))
        })?,
    };

    match log::resume_governed(&path, session) {
        Ok(r) => Ok((
            r.messages,
            Some(ResumeInfo {
                path,
                drift: r.drift,
            }),
            ResumedGovernance {
                plan: r.plan,
                recoveries_used: r.recoveries,
            },
        )),
        Err(ResumeError::RootMismatch) => Err(StartError::ResumeRootMismatch(path)),
        Err(e) => Err(StartError::ResumeFailed(format!(
            "cannot resume {path}: {e:?}"
        ))),
    }
}
