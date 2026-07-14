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
use crate::llm::response::StopReason as RespStopReason;
use crate::session::log::{self, Entry as LogEntry, Log, ResumeError, RiderTag, StopReason};
use crate::session::{RecoveryShape, Session};
use crate::turn::AgentDeps;
use crate::turn::governor::endgame::Recovery;
use crate::turn::loop_::{Outcome as LoopOutcome, OutcomeStop, RunOpts};
use crate::turn::settlement::{Event as SettleEvent, Outcome, Reason, Rollover, Settlement};
use crate::{tools, voice};

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
    /// `AnthropicLlm` or a test `FakeLlm` behind the same trait.
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
        // loudly; other drift yields to the new Session and is reported.
        let (resumed_messages, resume_info) = maybe_resume(resume, &session)?;

        // The Plan is held outside the Conversation; a Resume restores the last
        // logged Plan so the model keeps its goal across a restart.
        let plan = resume_info.as_ref().and_then(|ri| log::plan(&ri.path));

        // A Resume also restores the recoveries consumed by the logged
        // request, so a resumed Session cannot re-trigger them unboundedly.
        let recoveries_used = resume_info
            .as_ref()
            .map(|ri| log::recoveries_used(&ri.path))
            .unwrap_or(0);

        // The tool specs ride with every request but live outside the messages;
        // the estimate has to count them or Eviction fires late (baud's
        // `String.length(JSON.encode!(Baud.Tools.specs()))`). Serialize each
        // spec to its wire shape, exactly as a request would.
        let specs_wire: Vec<_> = tools::specs()
            .iter()
            .map(crate::llm::request::wire_tool)
            .collect();
        let overhead = serde_json::to_string(&specs_wire)
            .map(|s| s.chars().count() as u64)
            .unwrap_or(0);

        let mut conversation = Conversation::new(
            system_prompt,
            ConversationOpts::new(session.context_budget, session.connection.max_tokens)
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

        let state = AgentState {
            session,
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
    let deps = AgentDeps::new(
        state.self_tx.clone(),
        Arc::clone(&state.llm),
        state.session.connection.clone(),
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
        RecoveryShape::Handoff => spawn_handoff_turn(state, prompt),
    }
}

// The Handoff arm: the Recovery Turn task first seeds the fresh Conversation
// (the compaction machinery's LLM narrative + mechanical facts + the final
// verification verbatim + the prompt - a long LLM call, so it runs in the Turn
// task, never on the Agent actor, per ADR-0012), posts the seed back
// (`HandoffSeeded` logs it and retires the old Conversation), then runs the
// Turn over the seeded Conversation. The Plan is harness-owned and rides
// RunOpts verbatim - it survives the retirement untouched.
fn spawn_handoff_turn(state: &mut AgentState, prompt: String) {
    reset_turn_state(state);
    let dying = state.conversation.clone();
    let compaction = state.compaction.clone();
    let llm = Arc::clone(&state.llm);
    let connection = state.session.connection.clone();
    let session = state.session.clone();
    let opts = run_opts(state, None);
    let tx = state.self_tx.clone();

    let turn = tokio::spawn(async move {
        let seeded = compaction
            .seed_handoff(&dying, &prompt, llm.as_ref(), &connection)
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
            connection.clone(),
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
        (LoopOrDown::Loop(LoopOutcome::Recover(_, _, recovery)), false) => Some(*recovery),
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

fn maybe_resume(
    resume: Option<Resume>,
    session: &Session,
) -> Result<(Vec<crate::content::Message>, Option<ResumeInfo>), StartError> {
    let path = match resume {
        None => return Ok((Vec::new(), None)),
        Some(Resume::Path(p)) => p,
        Some(Resume::Latest) => log::latest(&session.session_dir).ok_or_else(|| {
            StartError::ResumeFailed(format!("no Session Log found in {}", session.session_dir))
        })?,
    };

    match log::resume(&path, session) {
        Ok((messages, drift)) => Ok((messages, Some(ResumeInfo { path, drift }))),
        Err(ResumeError::RootMismatch) => Err(StartError::ResumeRootMismatch(path)),
        Err(e) => Err(StartError::ResumeFailed(format!(
            "cannot resume {path}: {e:?}"
        ))),
    }
}

// ===========================================================================
// Tests - ported 1:1 from baud/test/baud/agent_test.exs (ADR-0017). baud's
// process primitives translate to their tokio analogs, preserving OBSERVABLE
// behavior: `assert_receive` → a broadcast recv with a timeout helper;
// `GenServer.call` → the request/reply Commands; `spawn` + `Process.monitor`
// for the dead-subscriber test → tokio's auto-cleaning broadcast (a dropped
// Receiver is pruned on the next send), noted where it adapts baud's monitor.
// The busy/steer/cancel handshakes use the FakeLlm `Barrier` entry: the test
// observes the Turn parked mid-`complete`, then releases (or aborts) it.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ContentBlock, Message, Role, Usage};
    use crate::llm::response::{Response, StopReason as RStop};
    use crate::llm::stream::Delta;
    use crate::session::connection::Connection;
    use crate::session::{Session, SessionConfig, SessionOpts};
    use crate::test_support::{Entry, FakeLlm, InFlight, Release};
    use serde_json::{Value, json};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::broadcast::error::RecvError;

    // ---- harness ----------------------------------------------------------

    fn session_in(dir: &TempDir) -> Session {
        let root = dir.path().to_string_lossy().into_owned();
        let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
        Session::build(
            SessionOpts {
                root: Some(root),
                session_dir: Some(session_dir),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .expect("session builds")
    }

    // Starts an Agent over the given FakeLlm with a fixed test system prompt (so
    // the ported conversation assertions don't depend on the Voice default).
    fn start(session: Session, fake: FakeLlm) -> AgentHandle {
        AgentHandle::start(
            StartOpts::new(session, Arc::new(fake)).with_system_prompt("You are a test agent."),
        )
        .expect("agent starts")
    }

    // Starts an Agent over the DEFAULT (Voice) system prompt - the compaction
    // test needs the real prompt's bulk in the token estimate so the small
    // Turns cross the compaction target (baud's agent runs the default prompt +
    // context files; the Rust test uses the Voice default alone).
    fn start_voiced(session: Session, fake: FakeLlm) -> AgentHandle {
        AgentHandle::start(StartOpts::new(session, Arc::new(fake))).expect("agent starts")
    }

    fn text_result(text: &str, stop: RStop) -> Response {
        Response {
            content: vec![ContentBlock::text(text)],
            stop_reason: stop,
            usage: Usage::default(),
            error: None,
        }
    }

    fn text_end(text: &str) -> Response {
        text_result(text, RStop::EndTurn)
    }

    fn tool_use_result(id: &str, name: &str, input: Value) -> Response {
        Response {
            content: vec![ContentBlock::tool_use(id, name, input)],
            stop_reason: RStop::ToolUse,
            usage: Usage::default(),
            error: None,
        }
    }

    // baud's assert_receive: pull events off a broadcast Receiver until one
    // matches the predicate or the deadline passes. Skips Lagged (never in
    // these tests) and returns the matched event.
    async fn recv_match(
        rx: &mut broadcast::Receiver<Event>,
        pred: impl Fn(&Event) -> bool,
    ) -> Event {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if pred(&ev) {
                        return ev;
                    }
                }
                Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) => panic!("event channel closed"),
                Err(_) => panic!("timed out waiting for an event"),
            }
        }
    }

    // Asserts NO matching event arrives within a short window (baud's
    // refute_receive / refute_received).
    async fn refute_match(rx: &mut broadcast::Receiver<Event>, pred: impl Fn(&Event) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) => {
                    if pred(&ev) {
                        panic!("unexpectedly received a matching event: {ev:?}");
                    }
                }
                Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) => return,
                Err(_) => return,
            }
        }
    }

    fn is_turn_started(e: &Event) -> bool {
        matches!(e, Event::TurnStarted(_))
    }
    fn is_turn_finished(e: &Event) -> bool {
        matches!(e, Event::TurnFinished { .. })
    }

    // ---- subscribe + submit happy path -----------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn relays_deltas_in_order_updates_the_conversation_returns_to_idle() {
        let dir = TempDir::new().unwrap();
        let fake = FakeLlm::script(vec![Entry::response(
            vec![
                Delta::Thinking("let me think".into()),
                Delta::Text("Hel".into()),
                Delta::Text("lo".into()),
            ],
            text_end("Hello"),
        )]);
        let agent = start(session_in(&dir), fake);
        let mut rx = agent.subscribe();

        assert_eq!(agent.status().await, Status::Idle);
        agent.submit("hi there").await.unwrap();

        let started = recv_match(&mut rx, is_turn_started).await;
        assert!(matches!(started, Event::TurnStarted(_)));

        recv_match(&mut rx, |e| matches!(e, Event::MessageStart { pass: 1 })).await;
        recv_match(&mut rx, |e| {
            matches!(e, Event::MessageUpdate { delta: Delta::Thinking(t), .. } if t == "let me think")
        })
        .await;
        recv_match(
            &mut rx,
            |e| matches!(e, Event::MessageUpdate { delta: Delta::Text(t), .. } if t == "Hel"),
        )
        .await;
        let last_update = recv_match(
            &mut rx,
            |e| matches!(e, Event::MessageUpdate { delta: Delta::Text(t), .. } if t == "lo"),
        )
        .await;
        if let Event::MessageUpdate { content, .. } = last_update {
            assert_eq!(content.last(), Some(&ContentBlock::text("Hello")));
        }

        recv_match(&mut rx, |e| {
            matches!(
                e,
                Event::MessageEnd {
                    stop_reason: RStop::EndTurn,
                    ..
                }
            )
        })
        .await;

        let finished = recv_match(&mut rx, is_turn_finished).await;
        if let Event::TurnFinished {
            stop_reason,
            token_estimate,
            context_budget,
        } = finished
        {
            assert_eq!(stop_reason, RStop::EndTurn);
            assert!(context_budget > 0);
            let _ = token_estimate; // >= 0 always for u64
        }

        assert_eq!(agent.status().await, Status::Idle);

        let conv = agent.conversation().await;
        assert_eq!(
            conv.messages,
            vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::text("hi there")],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::text("Hello")],
                },
            ]
        );
    }

    // ---- busy rejection ---------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn submit_while_running_is_busy_idle_again_after_the_turn() {
        let dir = TempDir::new().unwrap();
        let (barrier, mut inflight) = Entry::barrier();
        let fake = FakeLlm::script(vec![barrier]);
        let agent = start(session_in(&dir), fake);
        let mut rx = agent.subscribe();

        agent.submit("first").await.unwrap();

        // The Turn is parked mid-complete.
        let InFlight { release, .. } = inflight.recv().await.expect("in-flight signal");
        assert_eq!(agent.status().await, Status::Running);
        assert_eq!(agent.submit("second").await, Err(Busy));

        release
            .send(Release {
                deltas: vec![],
                response: text_end("done"),
            })
            .ok();

        recv_match(&mut rx, is_turn_finished).await;
        assert_eq!(agent.status().await, Status::Idle);
    }

    // ---- approval flow ----------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn denied_run_command_is_never_executed_and_yields_the_denial_tool_result() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("deny_marker");
        let script = vec![
            Entry::just(tool_use_result(
                "tu_run",
                "run_command",
                json!({ "command": format!("touch {}", marker.display()) }),
            )),
            Entry::just(text_end("understood")),
        ];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("touch that file").await.unwrap();

        let req = recv_match(&mut rx, |e| matches!(e, Event::ApprovalRequest { .. })).await;
        let id = match &req {
            Event::ApprovalRequest {
                approval_id,
                command,
            } => {
                assert!(command.contains("touch"));
                approval_id.clone()
            }
            _ => unreachable!(),
        };

        agent.approve(id.clone(), Decision::Deny).await;

        recv_match(&mut rx, |e| {
            matches!(e, Event::ApprovalResolved { approval_id, approved: false } if *approval_id == id)
        })
        .await;
        recv_match(&mut rx, |e| {
            matches!(e, Event::ToolResult { id, content, is_error: true, .. }
                if id == "tu_run" && content == "[command denied by user]")
        })
        .await;
        recv_match(&mut rx, is_turn_finished).await;

        assert!(!marker.exists(), "the command never ran");

        let conv = agent.conversation().await;
        assert!(conv.messages.iter().any(|m| {
            m.role == Role::User
                && m.content.iter().any(|b| {
                    matches!(b,
                    ContentBlock::ToolResult { tool_use_id, is_error: true, content }
                        if tool_use_id == "tu_run" && content == "[command denied by user]")
                })
        }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn approved_run_command_executes_and_returns_its_output() {
        let dir = TempDir::new().unwrap();
        let script = vec![
            Entry::just(tool_use_result(
                "tu_run",
                "run_command",
                json!({ "command": "echo hi" }),
            )),
            Entry::just(text_end("it said hi")),
        ];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("say hi").await.unwrap();

        let req = recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
        )
        .await;
        let id = match req {
            Event::ApprovalRequest { approval_id, .. } => approval_id,
            _ => unreachable!(),
        };

        agent.approve(id.clone(), Decision::Approve).await;
        recv_match(&mut rx, |e| {
            matches!(e, Event::ApprovalResolved { approval_id, approved: true } if *approval_id == id)
        })
        .await;
        let result = recv_match(
            &mut rx,
            |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "tu_run"),
        )
        .await;
        if let Event::ToolResult { content, .. } = result {
            assert!(content.contains("hi"));
        }
        recv_match(&mut rx, is_turn_finished).await;
    }

    // ---- standing approval ------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn approve_always_records_the_command_the_identical_command_is_auto_approved() {
        let dir = TempDir::new().unwrap();
        let script = vec![
            Entry::just(tool_use_result(
                "r1",
                "run_command",
                json!({ "command": "echo hi" }),
            )),
            Entry::just(tool_use_result("ls", "list_files", json!({ "path": "." }))),
            Entry::just(tool_use_result(
                "r2",
                "run_command",
                json!({ "command": "echo hi" }),
            )),
            Entry::just(text_end("done")),
        ];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("run it twice").await.unwrap();

        let req = recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
        )
        .await;
        let id = match req {
            Event::ApprovalRequest { approval_id, .. } => approval_id,
            _ => unreachable!(),
        };
        agent.approve(id.clone(), Decision::ApproveAlways).await;
        recv_match(&mut rx, |e| {
            matches!(e, Event::ApprovalResolved { approval_id, approved: true } if *approval_id == id)
        })
        .await;
        recv_match(
            &mut rx,
            |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "r1"),
        )
        .await;

        // The identical second command: no modal, an approval_auto, still runs.
        recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalAuto { command } if command == "echo hi"),
        )
        .await;
        let r2 = recv_match(
            &mut rx,
            |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "r2"),
        )
        .await;
        if let Event::ToolResult { content, .. } = r2 {
            assert!(content.contains("hi"));
        }
        recv_match(&mut rx, is_turn_finished).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_standing_approval_never_widens_beyond_the_identical_string() {
        let dir = TempDir::new().unwrap();
        let script = vec![
            Entry::just(tool_use_result(
                "r1",
                "run_command",
                json!({ "command": "echo hi" }),
            )),
            Entry::just(tool_use_result(
                "r2",
                "run_command",
                json!({ "command": "echo  hi" }),
            )),
            Entry::just(text_end("done")),
        ];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("run variants").await.unwrap();

        let req1 = recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
        )
        .await;
        let id1 = match req1 {
            Event::ApprovalRequest { approval_id, .. } => approval_id,
            _ => unreachable!(),
        };
        agent.approve(id1, Decision::ApproveAlways).await;
        recv_match(
            &mut rx,
            |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "r1"),
        )
        .await;

        // Two spaces is a different command: the modal comes back.
        let req2 = recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo  hi"),
        )
        .await;
        let id2 = match req2 {
            Event::ApprovalRequest { approval_id, .. } => approval_id,
            _ => unreachable!(),
        };
        agent.approve(id2, Decision::Deny).await;
        recv_match(&mut rx, |e| {
            matches!(e, Event::ToolResult { id, is_error: true, content, .. }
                if id == "r2" && content == "[command denied by user]")
        })
        .await;
        recv_match(&mut rx, is_turn_finished).await;
    }

    // ---- steering ---------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn steer_while_idle_is_idle() {
        let dir = TempDir::new().unwrap();
        let agent = start(session_in(&dir), FakeLlm::script(vec![]));
        assert_eq!(agent.steer("too early").await, Err(Idle));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn steer_mid_turn_is_drained_after_the_tool_batch_and_delivered_unadorned() {
        let dir = TempDir::new().unwrap();
        let (barrier, mut inflight) = Entry::barrier();
        let (second_tx, mut second_rx) = mpsc::unbounded_channel::<Value>();
        let script = vec![
            barrier,
            Entry::dynamic(vec![], move |req: &Value| {
                let _ = second_tx.send(req.clone());
                text_end("done")
            }),
        ];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("look around").await.unwrap();

        // First call is parked; steer, then release into a tool_use.
        let InFlight { release, .. } = inflight.recv().await.expect("first call parked");
        agent.steer("also check the README").await.unwrap();
        recv_match(
            &mut rx,
            |e| matches!(e, Event::SteeringQueued { text } if text == "also check the README"),
        )
        .await;

        release
            .send(Release {
                deltas: vec![],
                response: tool_use_result("t1", "list_files", json!({ "path": "." })),
            })
            .ok();

        recv_match(
            &mut rx,
            |e| matches!(e, Event::SteeringDelivered { text } if text == "also check the README"),
        )
        .await;

        // Unadorned, riding the SAME user message as the tool results.
        let request = second_rx.recv().await.expect("second request");
        let messages = request["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        let content = last["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "t1");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "also check the README");

        recv_match(&mut rx, is_turn_finished).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rollover_steering_the_turn_never_drained_auto_submits_the_next_turn() {
        let dir = TempDir::new().unwrap();
        let (barrier, mut inflight) = Entry::barrier();
        let (roll_tx, mut roll_rx) = mpsc::unbounded_channel::<Value>();
        let script = vec![
            barrier,
            Entry::dynamic(vec![], move |req: &Value| {
                let _ = roll_tx.send(req.clone());
                text_end("second done")
            }),
        ];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("first thing").await.unwrap();

        let InFlight { release, .. } = inflight.recv().await.expect("first call parked");
        // No tool batch ever runs, so this steering misses its Turn.
        agent.steer("and then this").await.unwrap();
        release
            .send(Release {
                deltas: vec![],
                response: text_end("first done"),
            })
            .ok();

        recv_match(&mut rx, is_turn_finished).await;
        recv_match(&mut rx, is_turn_started).await;

        let request = roll_rx.recv().await.expect("rollover request");
        let messages = request["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        let content = last["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "and then this");

        recv_match(&mut rx, is_turn_finished).await;
        assert_eq!(agent.status().await, Status::Idle);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_discards_queued_steering_no_rollover() {
        let dir = TempDir::new().unwrap();
        let (barrier, mut inflight) = Entry::barrier();
        let agent = start(session_in(&dir), FakeLlm::script(vec![barrier]));
        let mut rx = agent.subscribe();

        agent.submit("slow work").await.unwrap();
        recv_match(&mut rx, is_turn_started).await;

        // The Turn parks in complete forever (we never release it).
        let _inflight = inflight.recv().await.expect("parked");
        agent.steer("never mind this").await.unwrap();
        agent.cancel().await;

        recv_match(&mut rx, |e| matches!(e, Event::TurnCancelled)).await;
        refute_match(&mut rx, is_turn_started).await;
        assert_eq!(agent.status().await, Status::Idle);

        // The discarded text never entered the Conversation.
        let conv = agent.conversation().await;
        assert!(!conv.messages.iter().any(|m| m.content.iter().any(|b| {
            matches!(b, ContentBlock::Text { text } if text.contains("never mind this"))
        })));
    }

    // ---- cancellation -----------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_mid_turn_emits_turn_cancelled_and_records_the_cancellation() {
        let dir = TempDir::new().unwrap();
        let (barrier, mut inflight) = Entry::barrier();
        let agent = start(session_in(&dir), FakeLlm::script(vec![barrier]));
        let mut rx = agent.subscribe();

        agent.submit("do something slow").await.unwrap();
        let _inflight = inflight.recv().await.expect("parked in llm");
        agent.cancel().await;

        recv_match(&mut rx, |e| matches!(e, Event::TurnCancelled)).await;
        assert_eq!(agent.status().await, Status::Idle);

        let conv = agent.conversation().await;
        let n = conv.messages.len();
        assert_eq!(
            conv.messages[n - 2],
            Message {
                role: Role::User,
                content: vec![ContentBlock::text("do something slow")],
            }
        );
        assert_eq!(
            conv.messages[n - 1],
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text(voice::turn_cancelled_marker())],
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_when_idle_is_a_no_op() {
        let dir = TempDir::new().unwrap();
        let agent = start(session_in(&dir), FakeLlm::script(vec![]));
        agent.cancel().await;
        assert_eq!(agent.status().await, Status::Idle);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_after_a_tool_ran_keeps_the_partial_turn() {
        let dir = TempDir::new().unwrap();
        let (barrier, mut inflight) = Entry::barrier();
        let script = vec![
            Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
            barrier,
        ];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("explore then hang").await.unwrap();

        // The tool ran; only then cancel (its result is on disk/in the conv).
        recv_match(
            &mut rx,
            |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "t1"),
        )
        .await;
        let _inflight = inflight.recv().await.expect("second call parked");
        agent.cancel().await;

        recv_match(&mut rx, |e| matches!(e, Event::TurnCancelled)).await;

        let conv = agent.conversation().await;
        let tail: Vec<_> = conv.messages.iter().rev().take(3).rev().cloned().collect();
        assert!(matches!(&tail[0],
            Message { role: Role::Assistant, content } if matches!(&content[0], ContentBlock::ToolUse { id, .. } if id == "t1")));
        assert!(matches!(&tail[1],
            Message { role: Role::User, content } if matches!(&content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")));
        assert_eq!(
            tail[2],
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text(voice::turn_cancelled_marker())],
            }
        );
    }

    // ---- turn error -------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn llm_error_emits_turn_error_keeps_user_message_and_closes_with_failure_marker() {
        let dir = TempDir::new().unwrap();
        let agent = start(
            session_in(&dir),
            FakeLlm::script(vec![Entry::error("boom")]),
        );
        let mut rx = agent.subscribe();

        agent.submit("hello?").await.unwrap();

        recv_match(
            &mut rx,
            |e| matches!(e, Event::TurnError { reason } if reason == "boom"),
        )
        .await;
        assert_eq!(agent.status().await, Status::Idle);

        let conv = agent.conversation().await;
        let n = conv.messages.len();
        assert_eq!(
            conv.messages[n - 2],
            Message {
                role: Role::User,
                content: vec![ContentBlock::text("hello?")],
            }
        );
        assert_eq!(
            conv.messages[n - 1],
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text(voice::turn_failed_marker())],
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_llm_error_after_a_tool_ran_keeps_the_partial_turn_under_the_failure_marker() {
        let dir = TempDir::new().unwrap();
        let script = vec![
            Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
            Entry::error("boom"),
        ];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("explore then die").await.unwrap();

        recv_match(
            &mut rx,
            |e| matches!(e, Event::TurnError { reason } if reason == "boom"),
        )
        .await;

        let conv = agent.conversation().await;
        let tail: Vec<_> = conv.messages.iter().rev().take(3).rev().cloned().collect();
        assert!(matches!(&tail[0],
            Message { role: Role::Assistant, content } if matches!(&content[0], ContentBlock::ToolUse { id, .. } if id == "t1")));
        assert!(matches!(&tail[1],
            Message { role: Role::User, content } if matches!(&content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")));
        assert_eq!(
            tail[2],
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text(voice::turn_failed_marker())],
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_failing_with_an_llm_error_logs_a_settled_entry_carrying_the_error_reason() {
        let dir = TempDir::new().unwrap();
        let session = session_in(&dir);
        let session_dir = session.session_dir.clone();
        // The error reason must reach the settled log entry verbatim.
        let agent = start(
            session,
            FakeLlm::script(vec![Entry::error("{:llm_error, \"connection refused\"}")]),
        );
        let mut rx = agent.subscribe();

        agent.submit("evaluate this project").await.unwrap();

        recv_match(
            &mut rx,
            |e| matches!(e, Event::TurnError { reason } if reason.contains("connection refused")),
        )
        .await;
        assert_eq!(agent.status().await, Status::Idle);

        let path = log::latest(&session_dir).expect("a log file");
        let content = std::fs::read_to_string(&path).unwrap();
        let settled: Vec<Value> = content
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["e"] == "settled")
            .collect();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0]["outcome"], "failed");
        assert_eq!(settled[0]["stop_reason"], "error");
        assert!(
            settled[0]["reason"]
                .as_str()
                .unwrap()
                .contains("connection refused")
        );
    }

    // ---- session log + resume --------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn a_settled_session_resumes_into_a_new_agent_conversation_rebuilt() {
        let dir = TempDir::new().unwrap();
        let session = session_in(&dir);
        let session_dir = session.session_dir.clone();
        let script = vec![
            Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
            Entry::just(text_end("Nothing here.")),
        ];
        let first = start(session.clone(), FakeLlm::script(script));
        let mut rx = first.subscribe();

        first.submit("look around").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;

        let live = first.conversation().await;
        drop(first);

        let path = log::latest(&session_dir).expect("a log file");
        let resumed = AgentHandle::start(
            StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
                .with_system_prompt("You are a test agent.")
                .with_resume(Resume::Path(path.clone())),
        )
        .expect("resumes");

        assert_eq!(resumed.conversation().await.messages, live.messages);
        let info = resumed.resume_info().await.expect("resume info");
        assert_eq!(info.path, path);
        assert_eq!(info.drift, vec![]);
    }

    // ---- Recovery Turns (Continuation + Handoff) ---------------------------

    // A Session whose every Turn caps on Pass 1, so any unfinished work
    // triggers the Endgame Governor's recovery judgment immediately.
    fn recovery_session(dir: &TempDir, shape: crate::session::RecoveryShape) -> Session {
        let root = dir.path().to_string_lossy().into_owned();
        let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
        Session::build(
            SessionOpts {
                root: Some(root),
                session_dir: Some(session_dir),
                turn_limit: Some(1),
                recovery_shape: Some(shape),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .expect("session builds")
    }

    fn write_tool(id: &str, path: &str) -> Response {
        tool_use_result(id, "write_file", json!({ "path": path, "content": "x" }))
    }

    fn is_recovery_turn(e: &Event) -> bool {
        matches!(e, Event::RecoveryTurn { .. })
    }

    // All user-role text blocks of a conversation, flattened.
    fn user_texts(conv: &Conversation) -> Vec<String> {
        conv.messages
            .iter()
            .filter(|m| m.role == Role::User)
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_capped_unfinished_turn_opens_a_continuation_recovery_turn() {
        let dir = TempDir::new().unwrap();
        let session = recovery_session(&dir, crate::session::RecoveryShape::Continuation);
        let session_dir = session.session_dir.clone();
        let script = vec![
            Entry::just(write_tool("w1", "a.txt")), // Turn 1 caps unverified.
            Entry::just(text_end("recovered and done")), // The Recovery Turn.
        ];
        let agent = start(session, FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("write the file").await.unwrap();

        // The capped Turn settles, then the harness opens the Recovery Turn.
        recv_match(&mut rx, is_turn_finished).await;
        let recovery = recv_match(&mut rx, is_recovery_turn).await;
        let prompt = match &recovery {
            Event::RecoveryTurn { shape, text } => {
                assert_eq!(*shape, crate::session::RecoveryShape::Continuation);
                text.clone()
            }
            _ => unreachable!(),
        };
        assert_eq!(prompt, voice::recovery_prompt(false));
        recv_match(&mut rx, is_turn_started).await;
        recv_match(&mut rx, is_turn_finished).await;
        assert_eq!(agent.status().await, Status::Idle);

        // The Conversation was KEPT: the original prompt and the Voice's
        // recovery prompt both ride it, and the recovery's conclusion closes.
        let conv = agent.conversation().await;
        let texts = user_texts(&conv);
        assert!(texts.iter().any(|t| t == "write the file"));
        assert!(texts.contains(&prompt));
        assert!(conv.messages.iter().any(|m| {
            m.role == Role::Assistant
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if text == "recovered and done"))
        }));

        // The Session Log knows one recovery served this request.
        let path = log::latest(&session_dir).expect("a log file");
        assert_eq!(log::recoveries_used(&path), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_recovery_limit_bounds_one_user_request_and_a_recovery_turn_never_resets_it() {
        let dir = TempDir::new().unwrap();
        let session = recovery_session(&dir, crate::session::RecoveryShape::Continuation);
        // Turn 1 caps unverified -> one recovery; the Recovery Turn ALSO caps
        // unverified, but the request's budget (limit 1) is spent.
        let script = vec![
            Entry::just(write_tool("w1", "a.txt")),
            Entry::just(write_tool("w2", "b.txt")),
        ];
        let agent = start(session, FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("write the files").await.unwrap();

        recv_match(&mut rx, is_turn_finished).await;
        recv_match(&mut rx, is_recovery_turn).await;
        recv_match(&mut rx, is_turn_finished).await;

        // No third Turn: the capped Recovery Turn settles and the Agent idles.
        refute_match(&mut rx, is_recovery_turn).await;
        refute_match(&mut rx, is_turn_started).await;
        assert_eq!(agent.status().await, Status::Idle);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_genuine_user_prompt_resets_the_recovery_count() {
        let dir = TempDir::new().unwrap();
        let session = recovery_session(&dir, crate::session::RecoveryShape::Continuation);
        let script = vec![
            // Request 1: cap -> recovery -> cap (budget spent).
            Entry::just(write_tool("w1", "a.txt")),
            Entry::just(write_tool("w2", "b.txt")),
            // Request 2: cap -> the reset budget grants a fresh recovery.
            Entry::just(write_tool("w3", "c.txt")),
            Entry::just(text_end("second request recovered")),
        ];
        let agent = start(session, FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("first request").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;
        recv_match(&mut rx, is_recovery_turn).await;
        recv_match(&mut rx, is_turn_finished).await;
        assert_eq!(agent.status().await, Status::Idle);

        agent.submit("second request").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;
        recv_match(&mut rx, is_recovery_turn).await;
        recv_match(&mut rx, is_turn_finished).await;
        assert_eq!(agent.status().await, Status::Idle);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_handoff_recovery_seeds_a_fresh_conversation_with_the_mechanical_facts() {
        let dir = TempDir::new().unwrap();
        let session = recovery_session(&dir, crate::session::RecoveryShape::Handoff);
        let session_dir = session.session_dir.clone();
        let script = vec![
            // Turn 1's single Pass: set the Plan and run a failing verification.
            Entry::just(Response {
                content: vec![
                    ContentBlock::tool_use("p1", "plan", json!({ "plan": "Goal: fix. 1. run [ ]" })),
                    ContentBlock::tool_use("r1", "run_command", json!({ "command": "false" })),
                ],
                stop_reason: RStop::ToolUse,
                usage: Usage::default(),
                error: None,
            }),
            // The Handoff's summarize call.
            Entry::just(text_end("## Task\nnarrative-of-dying-turn")),
            // The Recovery Turn over the seeded Conversation.
            Entry::just(text_end("handoff recovered")),
        ];
        let agent = start(session.clone(), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("fix the failing tests").await.unwrap();

        // Approve the failing verification.
        let req = recv_match(&mut rx, |e| matches!(e, Event::ApprovalRequest { .. })).await;
        let id = match req {
            Event::ApprovalRequest { approval_id, .. } => approval_id,
            _ => unreachable!(),
        };
        agent.approve(id, Decision::Approve).await;

        // The verification result the seed must carry verbatim.
        let result = recv_match(
            &mut rx,
            |e| matches!(e, Event::ToolResult { id, .. } if id == "r1"),
        )
        .await;
        let verification = match result {
            Event::ToolResult { content, .. } => content,
            _ => unreachable!(),
        };

        recv_match(&mut rx, is_turn_finished).await;
        let recovery = recv_match(&mut rx, is_recovery_turn).await;
        assert!(matches!(
            &recovery,
            Event::RecoveryTurn { shape, text }
                if *shape == crate::session::RecoveryShape::Handoff
                    && text == voice::recovery_prompt(true)
        ));
        recv_match(&mut rx, is_turn_started).await;
        recv_match(&mut rx, is_turn_finished).await;
        assert_eq!(agent.status().await, Status::Idle);

        // The Conversation was RETIRED: one seed message (task verbatim,
        // narrative, final verification verbatim, recovery prompt) plus the
        // recovery's conclusion.
        let conv = agent.conversation().await;
        assert_eq!(conv.messages.len(), 2);
        let seed = user_texts(&conv).join("\n");
        assert!(seed.contains("fix the failing tests"));
        assert!(seed.contains("narrative-of-dying-turn"));
        assert!(seed.contains(&verification));
        assert!(seed.contains(voice::recovery_prompt(true)));
        assert_eq!(
            conv.messages[1],
            Message::assistant(vec![ContentBlock::text("handoff recovered")])
        );

        // The Plan is harness-owned and survives the retirement verbatim.
        assert_eq!(agent.plan().await.as_deref(), Some("Goal: fix. 1. run [ ]"));

        // The whole session round-trips: a Resume rebuilds the seeded
        // Conversation byte-identically, and the recovery count survives.
        let live = agent.conversation().await;
        drop(agent);
        let path = log::latest(&session_dir).expect("a log file");
        assert_eq!(log::recoveries_used(&path), 1);
        let resumed = AgentHandle::start(
            StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
                .with_system_prompt("You are a test agent.")
                .with_resume(Resume::Path(path)),
        )
        .expect("resumes");
        assert_eq!(resumed.conversation().await.messages, live.messages);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_handoff_summarization_degrades_to_the_mechanical_skeleton() {
        let dir = TempDir::new().unwrap();
        let session = recovery_session(&dir, crate::session::RecoveryShape::Handoff);
        let script = vec![
            Entry::just(write_tool("w1", "a.txt")), // Turn 1 caps unverified.
            Entry::error("summarizer down"),        // The Handoff's LLM call fails.
            Entry::just(text_end("recovered anyway")),
        ];
        let agent = start(session, FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("write the file").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;
        recv_match(&mut rx, is_recovery_turn).await;
        recv_match(&mut rx, is_turn_finished).await;

        // The recovery still happened, on the mechanical skeleton alone.
        let conv = agent.conversation().await;
        assert_eq!(conv.messages.len(), 2);
        let seed = user_texts(&conv).join("\n");
        assert!(seed.contains(voice::handoff_no_narrative()));
        assert!(seed.contains("write the file")); // task verbatim
        assert!(seed.contains(voice::recovery_prompt(false)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_limit_zero_leaves_a_capped_unfinished_turn_alone() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
        let session = Session::build(
            SessionOpts {
                root: Some(root),
                session_dir: Some(session_dir),
                turn_limit: Some(1),
                recovery_limit: Some(0),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .unwrap();
        let agent = start(session, FakeLlm::script(vec![Entry::just(write_tool("w1", "a.txt"))]));
        let mut rx = agent.subscribe();

        agent.submit("write the file").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;
        refute_match(&mut rx, is_recovery_turn).await;
        refute_match(&mut rx, is_turn_started).await;
        assert_eq!(agent.status().await, Status::Idle);
    }

    // ---- riders in the Session Log (Anchors + Endgame prompts) ------------

    // Session facts tuned so riders fire: turn_limit 4 puts the wrap-up
    // warning on Pass 2 and the final-Pass prompt on Pass 3; anchor_interval 2
    // places an Anchor on Pass 2.
    fn rider_session(dir: &TempDir) -> Session {
        let root = dir.path().to_string_lossy().into_owned();
        let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
        Session::build(
            SessionOpts {
                root: Some(root),
                session_dir: Some(session_dir),
                turn_limit: Some(4),
                anchor_interval: Some(2),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .expect("session builds")
    }

    // [`rider_session`] with the Recovery Turn disabled: a Turn that caps
    // with unverified writes now settles as a recovery close (ADR-0028
    // addendum), which has its own coverage - the test on this fixture
    // asserts rider logging and byte-for-byte Resume only.
    fn rider_session_no_recovery(dir: &TempDir) -> Session {
        let root = dir.path().to_string_lossy().into_owned();
        let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
        Session::build(
            SessionOpts {
                root: Some(root),
                session_dir: Some(session_dir),
                turn_limit: Some(4),
                anchor_interval: Some(2),
                recovery_limit: Some(0),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .expect("session builds")
    }

    // Three exploration Passes then a conclusion: crosses the anchor cadence,
    // the Endgame schedule, and (on Pass 3) the Explore Nudge.
    fn exploring_script() -> Vec<Entry> {
        vec![
            Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
            Entry::just(tool_use_result("t2", "list_files", json!({ "path": "." }))),
            Entry::just(tool_use_result("t3", "list_files", json!({ "path": "." }))),
            Entry::just(text_end("done")),
        ]
    }

    // The (entry, tag) shape of a Session Log file, header line skipped.
    fn log_shape(path: &str) -> Vec<(String, Option<String>)> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                (
                    v["e"].as_str().unwrap().to_string(),
                    v.get("tag").and_then(|t| t.as_str()).map(String::from),
                )
            })
            .collect()
    }

    fn shape(pairs: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        pairs
            .iter()
            .map(|(e, t)| (e.to_string(), t.map(String::from)))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn riders_are_logged_in_linear_position_as_they_are_injected() {
        let dir = TempDir::new().unwrap();
        let session = rider_session(&dir);
        let session_dir = session.session_dir.clone();
        let agent = start(session, FakeLlm::script(exploring_script()));
        let mut rx = agent.subscribe();

        agent.submit("look around").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;
        drop(agent);

        let path = log::latest(&session_dir).expect("a log file");
        assert_eq!(
            log_shape(&path),
            shape(&[
                ("user_text", None),
                // Pass 1: an ordinary tool Pass, nothing rides.
                ("assistant_blocks", None),
                ("tool_result", None),
                // Pass 2: the Anchor (interval 2) and the wrap-up warning
                // (2 Passes remaining) ride the results tail, in merge order.
                ("assistant_blocks", None),
                ("tool_result", None),
                ("rider", Some("anchor")),
                ("rider", Some("wrap_up_warning")),
                // Pass 3: the Explore Nudge (3rd exploration Pass) rides
                // before the final-Pass prompt (1 remaining).
                ("assistant_blocks", None),
                ("tool_result", None),
                ("nudge", None),
                ("rider", Some("final_pass")),
                // Pass 4: the tool-less final Pass concludes.
                ("assistant_blocks", None),
                ("settled", None),
            ])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_that_carried_riders_resumes_byte_for_byte() {
        let dir = TempDir::new().unwrap();
        let session = rider_session(&dir);
        let session_dir = session.session_dir.clone();
        let agent = start(session.clone(), FakeLlm::script(exploring_script()));
        let mut rx = agent.subscribe();

        agent.submit("look around").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;

        let live = agent.conversation().await;
        drop(agent);

        let path = log::latest(&session_dir).expect("a log file");
        let resumed = AgentHandle::start(
            StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
                .with_system_prompt("You are a test agent.")
                .with_resume(Resume::Path(path)),
        )
        .expect("resumes");

        // The reconstructed Conversation carries the same bytes the model
        // read live - Anchor and Endgame prompts included.
        assert_eq!(resumed.conversation().await.messages, live.messages);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unverified_write_logs_the_verification_pass_prompt_and_resumes_byte_for_byte() {
        let dir = TempDir::new().unwrap();
        let session = rider_session_no_recovery(&dir);
        let session_dir = session.session_dir.clone();
        // A successful write and no run_command: the Verification Pass prompt
        // subsumes the wrap-up warning at 2 remaining, and the Verify Nudge
        // (a standalone finish Nudge, not a rider) fires on the early finish.
        let script = vec![
            Entry::just(tool_use_result(
                "w1",
                "write_file",
                json!({ "path": "new.txt", "content": "hello" }),
            )),
            Entry::just(tool_use_result("t2", "list_files", json!({ "path": "." }))),
            Entry::just(text_end("not verified yet")),
            Entry::just(text_end("done")),
        ];
        let agent = start(session.clone(), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("write it").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;

        let live = agent.conversation().await;
        drop(agent);

        let path = log::latest(&session_dir).expect("a log file");
        assert_eq!(
            log_shape(&path),
            shape(&[
                ("user_text", None),
                ("assistant_blocks", None),
                ("tool_result", None),
                ("assistant_blocks", None),
                ("tool_result", None),
                ("rider", Some("anchor")),
                ("rider", Some("verification_pass")),
                ("assistant_blocks", None),
                ("nudge", None),
                ("assistant_blocks", None),
                ("settled", None),
            ])
        );

        let resumed = AgentHandle::start(
            StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
                .with_system_prompt("You are a test agent.")
                .with_resume(Resume::Path(path)),
        )
        .expect("resumes");
        assert_eq!(resumed.conversation().await.messages, live.messages);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_plan_survives_a_turn_boundary_and_is_restored_on_resume() {
        let dir = TempDir::new().unwrap();
        let session = session_in(&dir);
        let session_dir = session.session_dir.clone();
        let script = vec![
            Entry::just(tool_use_result(
                "p1",
                "plan",
                json!({ "plan": "Goal: Y. 1. do [ ]" }),
            )),
            Entry::just(text_end("planned")),
        ];
        let first = start(session.clone(), FakeLlm::script(script));
        let mut rx = first.subscribe();

        first.submit("do Y").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;

        assert_eq!(first.plan().await.as_deref(), Some("Goal: Y. 1. do [ ]"));
        drop(first);

        let path = log::latest(&session_dir).expect("a log file");
        let resumed = AgentHandle::start(
            StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
                .with_system_prompt("You are a test agent.")
                .with_resume(Resume::Path(path)),
        )
        .expect("resumes");

        assert_eq!(resumed.plan().await.as_deref(), Some("Goal: Y. 1. do [ ]"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_proactive_compaction_is_written_to_the_session_log_and_round_trips_through_resume() {
        let dir = TempDir::new().unwrap();
        let connection = Connection::new("http://test:4000/v1", "", "test-model", 200);
        let session = Session::build(
            SessionOpts {
                root: Some(dir.path().to_string_lossy().into_owned()),
                session_dir: Some(dir.path().join("sessions").to_string_lossy().into_owned()),
                connection: Some(connection),
                // Tuned so THREE small Turns cross the Compaction Target and
                // two do not: the tool-spec overhead rides the estimate, so
                // this number tracks the registry (web_fetch, ADR-0024, moved
                // it from 4000; run_command's pipefail description moved it
                // from 4200; the no-invented-line-numbers Voice rule moved it
                // from 4230).
                context_budget: Some(4320),
                eviction_slack: Some(0.3),
                compaction_keep: Some(0.1),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .expect("session builds");
        let session_dir = session.session_dir.clone();

        // Adaptation of baud's mid-test `Baud.FakeLLM.script(...)` re-scripting:
        // the Rust FakeLlm is per-instance with a fixed queue (ADR-0020), so all
        // entries ride ONE script up front - three small Turns to build history
        // past the compaction target, then the proactive summarization call
        // (popped FIRST on the next submit) and that Turn's own reply.
        let reply = "word ".repeat(250);
        let entries = vec![
            Entry::just(text_end(&format!("{reply} 1"))),
            Entry::just(text_end(&format!("{reply} 2"))),
            Entry::just(text_end(&format!("{reply} 3"))),
            Entry::just(text_end("[Compaction narrative] work summarized")),
            Entry::just(text_end("continuing")),
        ];
        let agent = start_voiced(session.clone(), FakeLlm::script(entries));
        let mut rx = agent.subscribe();
        for n in 1..=3 {
            agent.submit(format!("step {n}")).await.unwrap();
            recv_match(&mut rx, is_turn_finished).await;
        }
        // The next submit trips proactive compaction before its own reply.
        agent.submit("keep going").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;

        // The proactive Compaction replaced old messages: the head is a summary.
        let live = agent.conversation().await;
        let head_text: String = live.messages[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(head_text.contains("Compaction narrative"));
        drop(agent);

        let path = log::latest(&session_dir).expect("a log file");
        let content = std::fs::read_to_string(&path).unwrap();
        let compacted: Vec<Value> = content
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["e"] == "compacted")
            .collect();
        assert_eq!(compacted.len(), 1);
        assert!(
            compacted[0]["summary"]
                .as_str()
                .unwrap()
                .contains("Compaction narrative")
        );
        assert_eq!(compacted[0]["original_task"], "step 1");

        // Resume folds to the COMPACTED view, not the raw pre-compaction msgs.
        let resumed = AgentHandle::start(
            StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
                .with_system_prompt("You are a test agent.")
                .with_resume(Resume::Path(path)),
        )
        .expect("resumes");
        let resumed_msgs = resumed.conversation().await.messages;
        let resumed_head: String = resumed_msgs[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(resumed_head.contains("Compaction narrative"));
        assert!(resumed_head.contains("step 1"));
        assert!(!resumed_msgs.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "step 1"))
        }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_from_a_different_project_root_fails_init_loudly() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
        let session = Session::build(
            SessionOpts {
                root: Some(dir.path().to_string_lossy().into_owned()),
                session_dir: Some(session_dir.clone()),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .unwrap();
        let mut log = Log::open(&session).unwrap();
        log.append(LogEntry::UserText("hi".into()));
        let path = log.path.clone();
        drop(log);

        let other = Session::build(
            SessionOpts {
                root: Some(dir.path().join("other").to_string_lossy().into_owned()),
                session_dir: Some(session_dir),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .unwrap();

        let result = AgentHandle::start(
            StartOpts::new(other, Arc::new(FakeLlm::script(vec![])))
                .with_system_prompt("You are a test agent.")
                .with_resume(Resume::Path(path)),
        );
        assert!(matches!(result, Err(StartError::ResumeRootMismatch(_))));
    }

    // ---- subscriber pruning ----------------------------------------------

    // Adaptation of baud's "a dead subscriber is pruned and does not break later
    // turns": in tokio a dropped broadcast Receiver auto-cleans on the next
    // send, so there is no monitor/DOWN to model. We DROP a Receiver (the tokio
    // analog of the subscriber process dying), then run a full Turn and assert a
    // live subscriber still gets every event and the Agent stays healthy.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_subscriber_is_pruned_and_does_not_break_later_turns() {
        let dir = TempDir::new().unwrap();
        let agent = start(
            session_in(&dir),
            FakeLlm::script(vec![Entry::response(
                vec![Delta::Text("ok".into())],
                text_end("ok"),
            )]),
        );

        // A subscriber that immediately goes away.
        let dead = agent.subscribe();
        drop(dead);

        let mut rx = agent.subscribe();
        agent.submit("still alive?").await.unwrap();
        recv_match(&mut rx, is_turn_finished).await;
        assert_eq!(agent.status().await, Status::Idle);
    }

    // ---- streaming responsiveness ----------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn tool_use_during_streaming_steer_then_unblock_no_crash() {
        let dir = TempDir::new().unwrap();
        let (barrier, mut inflight) = Entry::barrier();
        let script = vec![barrier, Entry::just(text_end("done"))];
        let agent = start(session_in(&dir), FakeLlm::script(script));
        let mut rx = agent.subscribe();

        agent.submit("test streaming").await.unwrap();

        // The first model call parks in-flight (mid-Turn). The first Pass's
        // MessageStart has already gone out; steer NOW - the Turn is running but
        // has not reached its drain point - then release into a tool_use.
        // `steer().await` round-trips through the Agent, so the text is queued
        // before the tool batch runs and the drain delivers it (this removes
        // baud's implicit scheduler race while preserving the observable
        // behavior: steering issued mid-Turn, delivered after the tool batch, no
        // crash).
        let InFlight { release, .. } = inflight.recv().await.expect("blocked in llm");
        assert_eq!(agent.status().await, Status::Running);
        recv_match(&mut rx, |e| matches!(e, Event::MessageStart { pass: 1 })).await;
        agent.steer("more data").await.unwrap();
        recv_match(&mut rx, |e| matches!(e, Event::SteeringQueued { .. })).await;

        release
            .send(Release {
                deltas: vec![
                    Delta::Text("Thinking".into()),
                    Delta::Text(" carefully".into()),
                ],
                response: tool_use_result("t1", "list_files", json!({ "path": "." })),
            })
            .ok();

        // The parked call's deltas flush now (streaming), then the tool batch
        // runs and the drain delivers the queued Steering.
        recv_match(&mut rx, |e| matches!(e, Event::MessageUpdate { .. })).await;
        recv_match(&mut rx, |e| matches!(e, Event::SteeringDelivered { .. })).await;
        recv_match(&mut rx, is_turn_finished).await;
        assert_eq!(agent.status().await, Status::Idle);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_during_streaming_does_not_crash() {
        let dir = TempDir::new().unwrap();
        let (barrier, mut inflight) = Entry::barrier();
        let agent = start(session_in(&dir), FakeLlm::script(vec![barrier]));
        let mut rx = agent.subscribe();

        agent.submit("cancel me").await.unwrap();

        let _inflight = inflight.recv().await.expect("blocked in llm");
        agent.cancel().await;
        // The barrier drops its release when the test ends; the parked call is
        // aborted at the await.

        recv_match(&mut rx, |e| matches!(e, Event::TurnCancelled)).await;
        assert_eq!(agent.status().await, Status::Idle);
    }
}
