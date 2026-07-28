//! Agent - the actor driving Runs for a Session (baud's `Baud.Agent`, ADR-0017).
//!
//! A `tokio::spawn`ed task SOLELY owns the mutable state: the `Session`, the one
//! `Conversation`, the `Approvals` fold, the Steering queue, the latest
//! per-Tool-Result checkpoint, the Plan, the Session Log handle, and the
//! `broadcast` Sender to subscribers. The UI, the headless driver, and tests
//! talk ONLY to the Agent - never to a Run - by sending [`Command`]s over an
//! `mpsc` channel and reading [`Event`]s off a `broadcast` channel. Single
//! ownership serializes the state without shared locks and fixes one
//! deterministic Event order (ADR-0017).
//!
//! Each Run runs as a child task via `tokio::spawn`; the Agent holds its
//! [`JoinHandle`]. Cancellation is `JoinHandle::abort()`, which only cancels at
//! an `.await`; partial work survives because the Agent already holds the latest
//! checkpoint (delivered through the Deps `checkpoint` effect, ADR-0011). The
//! `JoinError`/[`Outcome`] the awaited handle yields distinguishes the three
//! Run outcomes: completed (`Ok`), cancelled (abort + the cancel flag), and
//! panicked (`Err`, so Run Settlement records failed).
//!
//! ## How the Run talks back
//!
//! The Run's [`crate::run::AgentDeps`] sends [`RunMsg`]s over the SAME `mpsc`
//! the public Commands use, so the single owner serializes them and Event order
//! is the owner's order. Fire-and-forget effects (`emit`, `checkpoint`,
//! `set_plan`, `compacted`) are plain sends; `drain_steering` and
//! `request_approval` carry a `oneshot` reply - the Agent owns the Steering
//! queue (a dead Run cannot hand back its mailbox) and consults the Standing
//! Approvals when relaying (an auto-approve emits `approval_auto` and answers
//! the reply immediately; the Run cannot tell the difference).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::approvals::{ApprovalId, Approvals, Decide, Decision, Request};
use crate::compaction::Compaction;
use crate::content::{ContentBlock, Provenance};
use crate::conversation::{Conversation, ConversationOpts};
use crate::event::Event;
use crate::llm::model::Model;
use crate::llm::response::StopReason as RespStopReason;
use crate::llm::{Llm, ProviderModels};
use crate::run::AgentDeps;
use crate::run::governor::endgame::{Recovery, ReopenReason};
use crate::run::loop_::{Outcome as LoopOutcome, OutcomeStop, RunOpts};
use crate::run::settlement::{Event as SettleEvent, Outcome, Reason, Rollover, Settlement};
use crate::session::log::{self, Entry as LogEntry, Log, ResumeError, RiderTag, StopReason};
use crate::session::{RecoveryShape, Session};
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

/// Returned by [`AgentHandle::submit`] when a Run is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// Returned by [`AgentHandle::steer`] when no Run is running (the caller should
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
// The message protocol: public Commands + the Run's talk-back, one mpsc.
// ---------------------------------------------------------------------------

/// A message the Run task sends back to the Agent (baud's `{:turn_event, ...}`,
/// `{:turn_checkpoint, ...}`, `{:turn_plan, ...}`, `{:compacted, ...}`, and the
/// `:drain_steering`/approval reply calls). Routed through the same mpsc as
/// [`Command`] so Event order is the single owner's order.
pub enum RunMsg {
    /// A run Event: logged (at the contract points) then broadcast.
    Emit(Event),
    /// Partial-Run snapshot, after every Tool Result; the latest wins.
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
    /// A completed Compaction from the Run task: log the `{:compacted, ...}`
    /// entry and update the accumulated state (ADR-0012).
    Compacted {
        new_state: Compaction,
        skip_count: u64,
        tokens_before: u64,
    },
    /// A Handoff finished seeding inside the Recovery Run task (CONTEXT.md:
    /// Handoff): the fresh Conversation replaces the retired one, the
    /// compaction state updates, and the `handoff` + `recovery` entries are
    /// logged so Resume rebuilds the same seed.
    HandoffSeeded {
        conversation: Conversation,
        new_state: Compaction,
        narrative: Option<String>,
        verification: Option<String>,
        prompt: String,
        /// Why the Recovery Run reopened (ADR-0043), so the logged `recovery`
        /// entry carries the reason and Resume restores the right budget. A
        /// Handoff is always a broken-state recovery (the Open-Plan arm is
        /// Continuation-shaped), but the reason still distinguishes unverified
        /// writes from a Dangling Failure.
        reason: ReopenReason,
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
    /// next Run - an in-flight Run is unaffected. No-op semantics
    /// (re-selecting the current model) are the caller's job, not here.
    SetModel(String, oneshot::Sender<Result<(), String>>),
    /// The Active Model identifier the next Run will call (ADR-0033), so a
    /// caller can mark "(current)".
    ActiveModel(oneshot::Sender<String>),
    /// List every Provider's models for the `/model` selector (ADR-0033,
    /// ADR-0037): custom Providers by live `GET /models` discovery, built-ins
    /// from the Catalog, credential-less built-ins marked unavailable with
    /// the environment key to set. The fetch runs OFF the actor (a spawned
    /// task over clones) so the network never blocks the actor loop; the
    /// oneshot carries [`llm::offerings`]' listings, which always come back
    /// whole - a failed discovery is its group's unreachable note.
    ListModels(oneshot::Sender<Vec<ProviderModels>>),
    Approve(String, Decision, oneshot::Sender<()>),
    Cancel(oneshot::Sender<()>),
    Status(oneshot::Sender<Status>),
    Conversation(oneshot::Sender<Conversation>),
    SessionQuery(oneshot::Sender<Session>),
    Plan(oneshot::Sender<Option<String>>),
    ResumeInfoQuery(oneshot::Sender<Option<ResumeInfo>>),
}

/// Both flavors ride the one mpsc (public so the Run shell can post `Run`
/// messages back to the Agent; only `Run` is constructed outside this module).
pub enum Msg {
    Command(Command),
    Run(RunMsg),
    /// The awaited Run task yielded (baud's `{ref, outcome}` / `{:DOWN, ...}`).
    Settle(LoopOutcome),
    /// The Run task panicked or was aborted (baud's `:DOWN` with no reply).
    RunDown(Reason),
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
            advances_used,
        } = governance;

        // The tool specs ride with every request but live outside the messages;
        // the estimate has to count them or Eviction fires late (baud's
        // `String.length(JSON.encode!(Baud.Tools.specs()))`). A ToolSpec
        // serializes to exactly its wire shape (name, description,
        // input_schema), so serde counts what a request would carry.
        let overhead = serde_json::to_string(&tools::specs())
            .map(|s| s.chars().count() as u64)
            .unwrap_or(0);

        // The Active Model lives here as mutable Agent state (ADR-0033,
        // CONTEXT.md: Active Model), seeded from the Session's launch-resolved
        // Model. Each Run is spawned with a snapshot of THIS Model, so a
        // `SetModel` between Runs lands on the next Run and an in-flight
        // Run finishes on the Model it captured.
        let model = session.model.clone();

        // The budget figures derive from the launch Model here and are
        // re-derived from the captured Model at every Run start (ADR-0037,
        // `reset_run_state`).
        let mut conversation = Conversation::new(
            system_prompt,
            ConversationOpts::new(session.context_budget_for(&model), model.max_tokens)
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

        // Session cost metering (ADR-0037): every model call this Session
        // makes - Run Passes, Scouts, Compaction, Handoff seeds - flows
        // through this one Arc, so a single decorator prices them all against
        // each call's captured Model. The running total rides the Agent's own
        // mpsc like every Run event, so Event order stays the single owner's;
        // it is display-side only and never enters the Session Log.
        let llm: Arc<dyn Llm> = {
            let tx = tx.clone();
            Arc::new(crate::llm::metered::Metered::new(llm, move |total| {
                let _ = tx.send(Msg::Run(RunMsg::Emit(Event::session_cost(total))));
            }))
        };

        let state = AgentState {
            session,
            run_provenance: model.provenance(),
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
            advances_used,
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

    /// Submits a user prompt, starting a Run (baud's `submit/2`). `Err(Busy)`
    /// while a Run runs.
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

    /// Queues Steering for the running Run (baud's `steer/2`). `Err(Idle)` when
    /// no Run is running.
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
    /// amendment). Takes effect on the next Run; an in-flight Run finishes
    /// on the Model it captured. An unresolvable id is an `Err` naming the
    /// reason; a dead Agent answers `Err` too, matching `list_models`.
    pub async fn set_model(&self, model: String) -> Result<(), String> {
        self.query(move |reply| Command::SetModel(model, reply))
            .await
            .unwrap_or_else(|| Err("agent unavailable".to_string()))
    }

    /// The Active Model identifier the next Run will call (ADR-0033), for a
    /// caller marking "(current)".
    pub async fn active_model(&self) -> String {
        self.query(Command::ActiveModel).await.expect("agent alive")
    }

    /// Lists every Provider's models for the `/model` selector (ADR-0033,
    /// ADR-0037), grouped by Provider: custom Providers by live discovery,
    /// built-ins from the Catalog, credentialed or not. The Agent -
    /// the owner of the `Llm` and the Session's Provider set - fetches off its
    /// actor loop; this awaits the reply. The listings themselves always come
    /// back whole (a down host is its group's unreachable note), so the one
    /// `Err` left - what the selector's Failed state shows - is a dead Agent
    /// or a dropped reply.
    pub async fn list_models(&self) -> Result<Vec<ProviderModels>, String> {
        self.query(Command::ListModels)
            .await
            .ok_or_else(|| "agent unavailable".to_string())
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

    /// Cancels the running Run (baud's `cancel/1`). No-op when idle.
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
    // read - not `session.model` - when spawning a Run. `Command::SetModel`
    // swaps the whole Model (resolved against the Session's fixed Provider
    // set, budget-checked); the budget figures re-derive from this capture at
    // every Run start (ADR-0037, `reset_run_state`).
    model: Model,
    // The Provenance of the Model the RUNNING Run captured at spawn
    // (ADR-0037): stamps assistant events in the Session Log. Snapshotted in
    // `reset_run_state`, NOT read from `model` at log time - a mid-flight
    // `SetModel` swaps `model` while the in-flight Run keeps its capture.
    run_provenance: Provenance,
    llm: Arc<dyn Llm>,
    conversation: Conversation,
    log: Option<Log>,
    resume_info: Option<ResumeInfo>,
    plan: Option<String>,
    events: broadcast::Sender<Event>,
    // The running Run: the AbortHandle for `cancel` (the real JoinHandle lives
    // in the spawned watcher that awaits it and posts the outcome back).
    task: Option<tokio::task::AbortHandle>,
    // The user cancelled the running Run; a following abort settles as
    // cancelled (Run Settlement needs both the flag and the abort).
    cancel_flag: bool,
    settlement: Settlement,
    approvals: Approvals,
    // The per-Run Approval reply channels, keyed by the Loop's ref string: the
    // Run parks awaiting this oneshot; `approve` (or a Standing Approval hit)
    // answers it.
    approval_replies: HashMap<String, oneshot::Sender<bool>>,
    steering: Vec<String>,
    compaction: Compaction,
    // Recovery Runs consumed serving the CURRENT user request (CONTEXT.md:
    // Recovery Run - the Setpoint bounds recoveries per user request, not
    // per Run). Cross-Run state lives with the Agent: reset when a genuine
    // user prompt starts a new request (`Command::Submit`), NOT by Rollover
    // or a Recovery Run; a Resume restores it from the folded log.
    recoveries_used: u64,
    // Open-Plan continuations consumed serving the CURRENT user request
    // (ADR-0043): the sibling of `recoveries_used` for the Open-Plan arm, with
    // its own `advance_limit` budget. Same lifecycle - reset only on a genuine
    // `Command::Submit`, never by Rollover or a Recovery Run, and restored
    // symmetrically on Resume.
    advances_used: u64,
    // A clone of the mpsc sender, handed to the Run's AgentDeps so the Run
    // talks back over the same channel, and used to post the Run's outcome.
    self_tx: mpsc::UnboundedSender<Msg>,
}

async fn run_agent(mut state: AgentState, mut rx: mpsc::UnboundedReceiver<Msg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Command(cmd) => handle_command(&mut state, cmd),
            Msg::Run(run) => handle_run(&mut state, run),
            Msg::Settle(outcome) => settle(&mut state, LoopOrDown::Loop(outcome)),
            Msg::RunDown(reason) => settle(&mut state, LoopOrDown::Down(reason)),
        }
    }
}

fn handle_command(state: &mut AgentState, cmd: Command) {
    match cmd {
        Command::Submit(prompt, reply) => {
            if state.task.is_some() {
                let _ = reply.send(Err(Busy));
            } else {
                // A genuine user prompt starts a new request: BOTH recovery
                // budgets reset (ADR-0043). Rollover starts its Run via
                // `start_run` directly and keeps the counts - its Steering
                // missed a Run of the SAME request.
                state.recoveries_used = 0;
                state.advances_used = 0;
                start_run(state, prompt);
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

fn handle_run(state: &mut AgentState, run: RunMsg) {
    match run {
        RunMsg::Emit(event) => {
            log_event(state, &event);
            broadcast(state, event);
        }
        RunMsg::Checkpoint(conversation) => {
            // Only meaningful while a Run is running.
            if state.task.is_some() {
                state.settlement =
                    std::mem::take(&mut state.settlement).note_checkpoint(conversation);
            }
        }
        RunMsg::SetPlan(plan) => {
            log_entry(state, LogEntry::Plan(plan.clone()));
            state.plan = Some(plan);
        }
        RunMsg::DrainSteering(reply) => {
            let drained = std::mem::take(&mut state.steering);
            let _ = reply.send(drained);
        }
        RunMsg::RequestApproval { id, command, reply } => {
            request_approval(state, id, command, reply);
        }
        RunMsg::Compacted {
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
        RunMsg::HandoffSeeded {
            conversation,
            new_state,
            narrative,
            verification,
            prompt,
            reason,
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
                    reason,
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
// known built-in models, config synthesis otherwise), and the per-Model
// budget invariants are re-checked (ADR-0037) so a pick that cannot fit is
// rejected here with the reason, never accepted and exploded later. The next
// spawned Run snapshots it; an in-flight Run is unaffected. A rejected pick
// leaves the Active Model as-is and the Err rides back to the caller.
fn swap_active_model(state: &mut AgentState, scoped: &str) -> Result<(), String> {
    let model = state.session.resolve_model(scoped)?;
    state.session.validate_model_budget(&model)?;
    state.model = model;
    Ok(())
}

// The ListModels fetch, OFF the actor (ADR-0011/0017: never block the actor
// loop on the network). Clone the boundary and the Session's fixed Provider
// set, then answer the oneshot from the spawned task: `llm::offerings` walks
// every Provider - live discovery for customs, the Catalog for built-ins
// (ADR-0037).
fn spawn_list_models(state: &AgentState, reply: oneshot::Sender<Vec<ProviderModels>>) {
    let llm = Arc::clone(&state.llm);
    let providers = state.session.providers.clone();
    tokio::spawn(async move {
        let _ = reply.send(crate::llm::offerings(llm.as_ref(), &providers).await);
    });
}

// A Standing Approval covering the exact string answers the Run immediately -
// no modal, an `approval_auto` event; the Run cannot tell the difference.
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

// Only forward the Approval the Run is actually waiting on; the fold ignores
// duplicate or stale approve calls (baud's approve handler).
fn approve(state: &mut AgentState, id: String, decision: Decision) {
    if state.task.is_none() {
        return; // No Run: drop it.
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

// Append fail-open: an IO failure kills the log, never the Run. The Transcript
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
            let provenance = Some(state.run_provenance.clone());
            log_entry(
                state,
                LogEntry::AssistantBlocks {
                    blocks: content.clone(),
                    provenance,
                },
            );
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

// ---- Run start (submit + Rollover + Recovery) ------------------------------

// One Run start for submit and Rollover alike (baud's start_run).
fn start_run(state: &mut AgentState, prompt: String) {
    log_entry(state, LogEntry::UserText(prompt.clone()));
    state.conversation.add_user_text(prompt);
    spawn_run(state);
}

// Spawns a Run over the Agent's CURRENT Conversation (the prompt - user or
// Voice - is already appended and logged by the caller).
fn spawn_run(state: &mut AgentState) {
    reset_run_state(state);
    // The AgentDeps wires each effect to the Agent's mpsc + the Session's Llm.
    // The Run captures a SNAPSHOT of the Agent's mutable Model (the Active
    // Model), not `session.model`: a `SetModel` between Runs lands on this
    // next Run, and an in-flight Run keeps the Model it already captured
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

    let run = tokio::spawn(async move { crate::run::run(conversation, session, deps, opts).await });
    watch_run(state, run);
}

// Resets the per-Run state before a Run task spawns. The Provenance
// snapshot here matches the Model the spawned Run captures (both read
// `state.model` at spawn), so logged assistant events carry the Run's
// capture even across a mid-flight SetModel.
fn reset_run_state(state: &mut AgentState) {
    state.settlement = Settlement::new();
    state.approvals = std::mem::take(&mut state.approvals).reset();
    state.approval_replies.clear();
    state.cancel_flag = false;
    state.run_provenance = state.model.provenance();
    // The captured Model's budget figures land at Run start (ADR-0037): the
    // Context Budget from its window (config may cap it), the Eviction reserve
    // from its output cap. The Run task clones this Conversation, so an
    // in-flight Run keeps the figures it captured, and a switch to a smaller
    // window lands here as ordinary pressure on the next Run.
    state.conversation.context_budget = state.session.context_budget_for(&state.model);
    state.conversation.max_tokens_reserve = state.model.max_tokens;
}

fn run_opts(state: &AgentState, original_task: Option<String>) -> RunOpts {
    RunOpts {
        plan: state.plan.clone(),
        original_task,
        recoveries_used: state.recoveries_used,
        advances_used: state.advances_used,
    }
}

// The Run task's watcher. The Agent holds only the AbortHandle (for
// `cancel`); the spawned watcher OWNS the JoinHandle, awaits it, and posts the
// outcome back through the mpsc (baud's `{ref, outcome}` / `:DOWN`). This lets
// the Agent both abort (cancel) and observe the outcome without co-owning one
// handle.
fn watch_run(state: &mut AgentState, run: tokio::task::JoinHandle<LoopOutcome>) {
    let reference = mint_run_ref();
    let abort = run.abort_handle();
    let out_tx = state.self_tx.clone();

    tokio::spawn(async move {
        match run.await {
            Ok(outcome) => {
                let _ = out_tx.send(Msg::Settle(outcome));
            }
            Err(join_err) => {
                let reason = if join_err.is_cancelled() {
                    // abort() - Run Settlement pairs this with the cancel flag.
                    Reason::atom("shutdown")
                } else {
                    // A panic; close with the failure marker (baud's run_error
                    // + "[turn failed]").
                    Reason::tuple("turn_panic")
                };
                let _ = out_tx.send(Msg::RunDown(reason));
            }
        }
    });

    state.task = Some(abort);
    broadcast(state, Event::run_started(reference));
}

// ---- Recovery Run (CONTEXT.md: Recovery Run) -------------------------------

// Executes the Endgame Governor's close-and-open-a-Recovery-Run Intervention:
// the Governor judged (trigger + both Setpoints); the Agent - owner of the
// Conversation and the Run lifecycle - opens the next Run. The prompt is the
// Voice's: the only Run whose prompt Suspenders authors.
fn start_recovery(state: &mut AgentState, recovery: Recovery) {
    // Each arm spends its OWN budget (ADR-0043): an Open-Plan continuation
    // burns `advances_used` against `advance_limit`; a broken-state recovery
    // burns `recoveries_used` against `repair_limit`.
    if recovery.reason == ReopenReason::OpenPlan {
        state.advances_used += 1;
    } else {
        state.recoveries_used += 1;
    }
    let prompt = voice::recovery_prompt(recovery.reason).to_string();
    broadcast(
        state,
        Event::recovery_run(recovery.shape, recovery.reason, prompt.clone()),
    );

    match recovery.shape {
        // Continuation keeps the Conversation: the recovery prompt is the
        // next Run's prompt, mechanically like Rollover's auto-submit but
        // logged as the Voice's, not the user's. Both the Open-Plan arm and a
        // Continuation-shaped broken-state recovery pass here.
        RecoveryShape::Continuation => {
            log_entry(
                state,
                LogEntry::Recovery {
                    shape: RecoveryShape::Continuation,
                    reason: recovery.reason,
                    text: prompt.clone(),
                },
            );
            state.conversation.merge_user_text(prompt);
            spawn_run(state);
        }
        RecoveryShape::Handoff => {
            spawn_handoff_run(state, prompt, recovery.reason, recovery.failing_command)
        }
    }
}

// The Handoff arm: the Recovery Run task first seeds the fresh Conversation
// (the compaction machinery's LLM narrative + mechanical facts + the
// verification verbatim + the prompt - a long LLM call, so it runs in the Run
// task, never on the Agent actor, per ADR-0012), posts the seed back
// (`HandoffSeeded` logs it and retires the old Conversation), then runs the
// Run over the seeded Conversation. The Plan is harness-owned and rides
// RunOpts verbatim - it survives the retirement untouched. `failing_command`
// (the Dangling Failure the recovery names, `None` on an unverified-writes
// recovery) tells the seed which command's result to carry verbatim.
fn spawn_handoff_run(
    state: &mut AgentState,
    prompt: String,
    reason: ReopenReason,
    failing_command: Option<String>,
) {
    reset_run_state(state);
    let dying = state.conversation.clone();
    let compaction = state.compaction.clone();
    let llm = Arc::clone(&state.llm);
    // A snapshot of the Agent's mutable Model (the Active Model), as
    // `spawn_run` does - the seed narrative and the Recovery Run both run on
    // the model current at spawn (ADR-0033).
    let model = state.model.clone();
    let temperature = state.session.temperature;
    let session = state.session.clone();
    let opts = run_opts(state, None);
    let tx = state.self_tx.clone();

    let run = tokio::spawn(async move {
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
        let _ = tx.send(Msg::Run(RunMsg::HandoffSeeded {
            conversation: seeded.conversation.clone(),
            new_state: seeded.state.clone(),
            narrative: seeded.narrative,
            verification: seeded.verification,
            prompt,
            reason,
        }));
        // Built here, not before the spawn: the Run's deps must carry the
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
        crate::run::run(seeded.conversation, session, deps, opts).await
    });
    watch_run(state, run);
}

fn mint_run_ref() -> String {
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
    // If there is no running Run, this is a stale outcome (e.g. the watcher for
    // an already-settled Run); ignore it.
    if state.task.is_none() {
        return;
    }

    // The Endgame Governor's recovery directive, held aside: the Run settles
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
        Rollover::Submit(prompt) => start_run(state, prompt),
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
        SettleEvent::RunFinished {
            stop_reason,
            token_estimate,
            context_budget,
        } => Event::RunFinished {
            stop_reason: log_stop_to_resp(*stop_reason),
            token_estimate: *token_estimate,
            context_budget: *context_budget,
        },
        SettleEvent::RunError(reason) => Event::RunError {
            reason: reason.inspect(),
        },
        SettleEvent::RunCancelled => Event::RunCancelled,
    }
}

fn log_stop_to_resp(stop: StopReason) -> RespStopReason {
    match stop {
        StopReason::EndTurn => RespStopReason::EndTurn,
        StopReason::ToolUse => RespStopReason::ToolUse,
        StopReason::MaxTokens => RespStopReason::MaxTokens,
        StopReason::StopSequence => RespStopReason::StopSequence,
        StopReason::RunLimit
        | StopReason::RunLimitStuck
        | StopReason::Error
        | StopReason::Unknown => RespStopReason::Unknown,
    }
}

// ---- Resume ----------------------------------------------------------------

/// The governance facts a Resume restores alongside the Conversation: the last
/// logged Plan (held outside the Conversation) and the two per-request recovery
/// counts the logged request consumed - broken-state recoveries and Open-Plan
/// continuations (ADR-0043). Computed in the single fold, so they never belong
/// on the Transcript-facing [`ResumeInfo`] - the Agent threads them privately.
#[derive(Default)]
struct ResumedGovernance {
    plan: Option<String>,
    recoveries_used: u64,
    advances_used: u64,
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
                advances_used: r.advances,
            },
        )),
        Err(ResumeError::RootMismatch) => Err(StartError::ResumeRootMismatch(path)),
        Err(e) => Err(StartError::ResumeFailed(format!(
            "cannot resume {path}: {e:?}"
        ))),
    }
}
