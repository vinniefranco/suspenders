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
//! The Run's [`deps::AgentDeps`] sends [`RunMsg`]s over the SAME `mpsc`
//! the public Commands use, so the single owner serializes them and Event order
//! is the owner's order. Fire-and-forget effects (`emit`, `checkpoint`,
//! `set_plan`, `compacted`) are plain sends; `drain_steering` and
//! `request_approval` carry a `oneshot` reply - the Agent owns the Steering
//! queue (a dead Run cannot hand back its mailbox) and consults the Standing
//! Approvals when relaying (an auto-approve emits `approval_auto` and answers
//! the reply immediately; the Run cannot tell the difference).

use std::collections::HashMap;
use std::sync::Arc;

/// The broadcast channel capacity for the Agent's Event stream. Large enough
/// to absorb a burst of events between subscriber polls without dropping.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::approvals::{ApprovalId, ApprovalMode, Approvals, Decide, Decision, Request};
use crate::compaction::Compaction;
use crate::content::{ContentBlock, Provenance};
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::model::Model;
use crate::llm::{Llm, ProviderModels};
use crate::run::loop_::{Outcome as LoopOutcome, RunOpts};
use crate::run::settlement::{Reason, Rollover, Settlement};
use crate::session::Session;
use crate::session::log::{self, Entry as LogEntry, Log, ResumeError};
use crate::tool::caps::SubagentResult;
use crate::voice;

pub mod background;
pub mod background_shell;
mod capabilities;
mod deps;
mod init;
mod mcp_ops;
mod model_ops;
mod settle;
use background::BackgroundTask;
use background_shell::{BackgroundShell, ShellOutcome};
use deps::{AgentDeps, RequestSettings, RunSpawn};
use mcp_ops::{mcp_authenticate, mcp_clear_auth, mcp_reconnect, mcp_set_enabled};
use model_ops::{apply_enriched_model, spawn_list_models, swap_active_model};
use settle::{LoopOrDown, settle_event_to_event, to_settlement_outcome};
// The Session-stable tool-set rebuild lives with the live MCP ops that share it;
// `init_agent` reaches it as `crate::agent::rebuild_session_tools`.
pub(crate) use mcp_ops::rebuild_session_tools;

#[cfg(test)]
#[path = "../tests/agent.rs"]
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

    #[cfg(test)]
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

/// The user's answer to a pending question (ADR-0057): `Ok` is one
/// `(question_index, answer_value)` per answered question, `Err` the VERBATIM
/// decline/degraded string the tool returns as its content. Named so the
/// question reply oneshot and the [`Command::AnswerQuestion`] payload read the
/// same shape (clippy: this Result-of-Vec-of-tuple is otherwise "very complex").
pub type QuestionAnswers = Result<Vec<(usize, String)>, String>;

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
    /// A question request (ADR-0057, `ask_user_question`): the Agent broadcasts
    /// `question_request` (opening the modal) and holds the reply until
    /// `answer_question` arrives. Unlike an Approval there is NO auto/standing
    /// path - every question opens a modal. The reply carries the user's picks
    /// (`Ok`) or the decline/degraded string (`Err`).
    AskQuestion {
        id: String,
        questions: Vec<crate::tool::caps::Question>,
        reply: oneshot::Sender<QuestionAnswers>,
    },
    /// A completed Compaction from the Run task: log the `{:compacted, ...}`
    /// entry and update the accumulated state (ADR-0012).
    Compacted {
        new_state: Compaction,
        skip_count: u64,
        tokens_before: u64,
    },
    /// A background-subagent launch (P4b/4c, ADR-0063): the `agent` tool asked
    /// the Agent (which owns the registry) to launch a detached child Run. The
    /// Agent mints the id, spawns the child, registers it, and replies the id -
    /// the tool does NOT park (the whole point of a background launch).
    SpawnBackground {
        request: crate::tool::caps::SubagentRequest,
        description: String,
        reply: oneshot::Sender<String>,
    },
    /// A background child settled (P4b/4c, ADR-0063): the detached child Run's
    /// watcher posts its result back so the Agent updates the registry entry and
    /// queues the `<task-notification>` for the next Run to drain. A `Stopped`
    /// entry (a `task_stop` already cancelled it) drops the result.
    BackgroundDone { id: String, result: SubagentResult },
    /// A background-subagent stop request (P4b/4d, ADR-0063): `task_stop` asked
    /// the Agent to cancel a running background agent. The Agent aborts the
    /// child, sets the entry `Stopped`, queues the `was cancelled` notification
    /// synchronously, and replies the VERBATIM qwen wording (found/not-running/
    /// not-found).
    StopBackground {
        id: String,
        reply: oneshot::Sender<String>,
    },
    /// A background-shell launch (Phase 9, ADR-0063): `run_command` with
    /// `is_background: true` asked the Agent (which owns the process lifecycle) to
    /// spawn a detached shell. The Agent mints the id, spawns the child, registers
    /// it, and replies the id - the tool does NOT park (the whole point of a
    /// background launch).
    SpawnBackgroundShell {
        command: String,
        cwd: String,
        reply: oneshot::Sender<String>,
    },
    /// A background shell settled (Phase 9, ADR-0063): the detached watcher task
    /// posts the child's [`background_shell::ShellOutcome`] back so the Agent
    /// updates the registry entry and queues the `<task-notification>`. A
    /// `Cancelled` entry (a `task_stop` already killpg'd it) drops the outcome.
    BackgroundShellDone { id: String, outcome: ShellOutcome },
    /// A background-shell stop request (Phase 9, ADR-0063): the shell-only leg of
    /// the [`crate::tool::caps::BackgroundShellSpawner`] seam. The dual-registry
    /// resolution `task_stop` drives lives on [`RunMsg::StopBackground`]; this
    /// variant is the direct shell-registry stop the bg-shells capability relays.
    StopBackgroundShell {
        id: String,
        reply: oneshot::Sender<String>,
    },
    /// The Loop's notification drain point (P4b, ADR-0063): return all queued
    /// background `<task-notification>` texts and empty the queue. Mirrors
    /// [`RunMsg::DrainSteering`] - the parallel channel, not steering.
    DrainNotifications(oneshot::Sender<Vec<String>>),
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
    /// Rotate the Approval mode one step in the Shift+Tab cycle (ADR-0050): the
    /// pure `Approvals` fold cycles and the Agent broadcasts the new mode so the
    /// Screen mirror updates. Session-scoped, not per-Run - it applies whether
    /// or not a Run is in flight. The reply carries the NEW mode so the caller
    /// can set the Screen mirror directly from the authoritative fold result,
    /// not from the lossy broadcast (P0: broadcast `Lagged` must not desync the
    /// footer AutoAcceptIndicator).
    CycleApprovalMode(oneshot::Sender<ApprovalMode>),
    /// Resolve a pending question (ADR-0057): `Ok(answers)` carries the user's
    /// `(question_index, answer_value)` picks; `Err(string)` is the decline the
    /// tool returns as its content. The Agent forwards it to the parked tool
    /// call's reply oneshot. Mirrors [`Command::Approve`] but with no Standing-
    /// Approval fold - the reply map is the whole mechanic.
    AnswerQuestion(String, QuestionAnswers, oneshot::Sender<()>),
    /// The `/mcp` dialog's read model (ADR-0065 Phase C): a clone of the manager's
    /// per-server [`McpServerView`]s, server-name-sorted. Read-only, so it never
    /// touches the tool set; the dialog polls it after every live op.
    McpViews(oneshot::Sender<Vec<crate::mcp::McpServerView>>),
    /// Re-attach one MCP server (ADR-0065 Phase C, the dialog's Reconnect action):
    /// re-run its per-server attach and rebuild `session_tools` from the manager's
    /// current adapters, so the NEXT Run sees the fresh tools. Always `Ok(())` for
    /// a known server (a failed re-attach lands as a Disconnected view + a launch
    /// notice, fail-open); an unknown server is a no-op that still replies `Ok`.
    McpReconnect(String, oneshot::Sender<Result<(), String>>),
    /// Enable or disable one MCP server (ADR-0065 Phase C, the dialog's
    /// Enable/Disable action): persist the scope's `mcp.excluded` list, update the
    /// Session + the manager, and rebuild `session_tools`. An `Err(reason)` when
    /// the scope cannot be written (or the server is Extension-sourced, which qwen
    /// cannot toggle); the bool is the DESIRED enabled state (`false` = disable).
    McpSetEnabled(String, bool, oneshot::Sender<Result<(), String>>),
    /// Authenticate one MCP server via OAuth (ADR-0065 Phase D, the dialog's
    /// Authenticate action): run the browser flow for the server, store the token,
    /// re-attach the server (so its tools re-discover under the fresh Bearer), and
    /// rebuild `session_tools`. Progress lines (the copy-the-URL hint + the auth
    /// URL) stream back over the Agent's `events` broadcast as
    /// [`Event::McpAuthProgress`] while it runs. An `Err(reason)` when the server
    /// is unknown, carries no OAuth config, or the flow failed.
    McpAuthenticate(String, oneshot::Sender<Result<(), String>>),
    /// Clear one MCP server's stored OAuth token (ADR-0065 Phase D, qwen
    /// `handleClearAuth`): delete the credential, disconnect the server (dropping
    /// its tools), and rebuild `session_tools`. An `Err(reason)` when the token
    /// store cannot be written; an unknown server still clears any stray token.
    McpClearAuth(String, oneshot::Sender<Result<(), String>>),
    Cancel(oneshot::Sender<()>),
    Status(oneshot::Sender<Status>),
    Conversation(oneshot::Sender<Conversation>),
    Plan(oneshot::Sender<Option<String>>),
    ResumeInfoQuery(oneshot::Sender<Option<ResumeInfo>>),
    /// The discovered skill manager (ADR-0058), a clone of the shared `Arc` the
    /// Agent discovered at launch. Read-only, so it rides the sync handler: the
    /// UI reads it once at mount to build the `/<name>` slash-command layer
    /// (ADR-0032/0058) and again to resolve a committed `/<skill>` to its
    /// submit-prompt body. Cheap - the `Arc` clones, the manager is not copied.
    Skills(oneshot::Sender<Arc<crate::skills::SkillManager>>),
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
    /// The off-actor window enrichment for a `/model` swap finished (ADR-0037):
    /// the Model rebuilt on the server's live window, folded back onto the
    /// Active Model when it is still the pick that spawned the enrichment.
    EnrichedModel(Model),
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
        // yields the messages, the Transcript-facing `ResumeInfo`, AND the last
        // logged Plan - no re-reading the log for the Plan.
        let (resumed_messages, resume_info, plan) = maybe_resume(resume, &session)?;

        // The Active Model lives here as mutable Agent state (ADR-0033,
        // CONTEXT.md: Active Model), seeded from the Session's launch-resolved
        // Model. Each Run is spawned with a snapshot of THIS Model, so a
        // `SetModel` between Runs lands on the next Run and an in-flight
        // Run finishes on the Model it captured.
        let model = session.model.clone();

        // Every Session gets a fresh log; a Resume seeds it with the folded
        // messages verbatim so the new file alone rebuilds the Conversation.
        let mut log = Log::open(&session).ok();
        if let Some(ref mut log) = log {
            for message in &resumed_messages {
                log.append(LogEntry::Message(message.clone()));
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let (events, _rx0) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        // Session cost metering (ADR-0037): every model call this Session
        // makes - Run Passes, Scouts, Compaction - flows through this one
        // Arc, so a single decorator prices them all against
        // each call's captured Model. The running total rides the Agent's own
        // mpsc like every Run event, so Event order stays the single owner's;
        // it is display-side only and never enters the Session Log.
        let llm: Arc<dyn Llm> = {
            let tx = tx.clone();
            Arc::new(crate::llm::metered::Metered::new(llm, move |total| {
                let _ = tx.send(Msg::Run(RunMsg::Emit(Event::session_cost(total))));
            }))
        };

        // The rest of the Session's assembly - MCP connect, the
        // built-ins+MCP tool set, the tool-spec overhead, and the Deferred Tools
        // system-prompt section - must see the MCP tools, so it moves into the
        // async init below (`init_agent`). It is `async` (the connect awaits per
        // server), so it cannot run in this sync `start`; the raw pieces travel
        // there in [`AgentInit`].
        let init = init::AgentInit {
            session,
            model,
            llm,
            system_prompt,
            resumed_messages,
            log,
            resume_info,
            plan,
            events: events.clone(),
            self_tx: tx.clone(),
        };

        tokio::spawn(async move {
            let state = init::init_agent(init).await;
            run_agent(state, rx).await;
        });

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
    /// caller marking "(current)". A dead Agent (the process is shutting down,
    /// every handle about to drop) answers the empty string rather than
    /// panicking, matching the graceful degradation `status`/`plan` take.
    pub async fn active_model(&self) -> String {
        self.query(Command::ActiveModel).await.unwrap_or_default()
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

    /// The `/mcp` dialog's read model (ADR-0065 Phase C): the manager's per-server
    /// [`McpServerView`]s, server-name-sorted. Cloned off the actor, so the dialog
    /// re-polls it after every live op; a dead Agent answers an empty list.
    pub async fn mcp_views(&self) -> Vec<crate::mcp::McpServerView> {
        self.query(Command::McpViews).await.unwrap_or_default()
    }

    /// The discovered [`crate::skills::SkillManager`] (ADR-0058), for the
    /// `/<name>` slash-command layer (ADR-0032): the UI reads it once at mount
    /// to build every discovered skill's slash descriptor, and again to resolve
    /// a committed `/<skill>` to its submit-prompt body. A dead Agent answers a
    /// fresh empty manager (no skills), so the caller never unwraps.
    pub async fn skills(&self) -> Arc<crate::skills::SkillManager> {
        self.query(Command::Skills)
            .await
            .unwrap_or_else(|| Arc::new(crate::skills::SkillManager::default()))
    }

    /// Reconnects one MCP server (ADR-0065 Phase C, the dialog's Reconnect action):
    /// re-attach it and rebuild the Session tool set so the NEXT Run sees its
    /// fresh tools; an in-flight Run is unaffected (the same rule as `set_model`).
    /// A failed re-attach is fail-open (a Disconnected view), so this answers
    /// `Ok(())` for any known server; a dead Agent answers `Err`.
    pub async fn mcp_reconnect(&self, name: impl Into<String>) -> Result<(), String> {
        let name = name.into();
        self.query(move |reply| Command::McpReconnect(name, reply))
            .await
            .unwrap_or_else(|| Err("agent unavailable".to_string()))
    }

    /// Enables or disables one MCP server (ADR-0065 Phase C, the dialog's
    /// Enable/Disable action): persist the `mcp.excluded` change to the server's
    /// scope config, update the manager, and rebuild the Session tool set for the
    /// next Run. `enabled` is the desired state (`false` disables). An `Err`
    /// naming the reason when the scope cannot be written or the server is
    /// Extension-sourced; a dead Agent answers `Err` too.
    pub async fn mcp_set_enabled(
        &self,
        name: impl Into<String>,
        enabled: bool,
    ) -> Result<(), String> {
        let name = name.into();
        self.query(move |reply| Command::McpSetEnabled(name, enabled, reply))
            .await
            .unwrap_or_else(|| Err("agent unavailable".to_string()))
    }

    /// Authenticates one MCP server via OAuth (ADR-0065 Phase D, the dialog's
    /// Authenticate action): run the browser flow, store the token, re-attach the
    /// server so its tools re-discover under the fresh Bearer, and rebuild the
    /// Session tool set for the next Run. Progress lines stream back as
    /// [`Event::McpAuthProgress`] over the events broadcast while it runs; the
    /// `Result` settles when the flow finishes. An `Err` names the reason (unknown
    /// server, no OAuth config, or a flow failure); a dead Agent answers `Err`.
    pub async fn mcp_authenticate(&self, name: impl Into<String>) -> Result<(), String> {
        let name = name.into();
        self.query(move |reply| Command::McpAuthenticate(name, reply))
            .await
            .unwrap_or_else(|| Err("agent unavailable".to_string()))
    }

    /// Clears one MCP server's stored OAuth token (ADR-0065 Phase D, qwen
    /// `handleClearAuth`): delete the credential, disconnect the server (dropping
    /// its tools), and rebuild the Session tool set. An `Err` when the token store
    /// cannot be written; a dead Agent answers `Err`.
    pub async fn mcp_clear_auth(&self, name: impl Into<String>) -> Result<(), String> {
        let name = name.into();
        self.query(move |reply| Command::McpClearAuth(name, reply))
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

    /// Resolves a pending question (ADR-0057, `ask_user_question`): `Ok(answers)`
    /// carries the user's `(question_index, answer_value)` picks, `Err(string)`
    /// the decline. The Agent forwards it to the parked tool call's reply oneshot.
    /// Mirrors [`AgentHandle::approve`]; a dead Agent silently drops it.
    pub async fn answer_question(&self, question_id: impl Into<String>, answers: QuestionAnswers) {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Msg::Command(Command::AnswerQuestion(
            question_id.into(),
            answers,
            reply,
        )));
        let _ = rx.await;
    }

    /// Rotates the Approval mode one step (ADR-0050, the Shift+Tab cycle): the
    /// Agent's pure `Approvals` fold cycles and broadcasts the new mode. Applies
    /// whether or not a Run is in flight (Session-scoped). RETURNS the new mode
    /// so the caller can set the Screen mirror directly from this authoritative
    /// result rather than depending on the lossy `ApprovalModeChanged` broadcast
    /// (P0: a broadcast `Lagged` must never desync the footer indicator). The
    /// broadcast still fires for any other subscribers. Falls back to `Default`
    /// only if the actor is gone (reply dropped), which is the safest mode.
    pub async fn cycle_approval_mode(&self) -> ApprovalMode {
        let (reply, rx) = oneshot::channel();
        let _ = self
            .tx
            .send(Msg::Command(Command::CycleApprovalMode(reply)));
        rx.await.unwrap_or(ApprovalMode::Default)
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

    /// The current Conversation (baud's `conversation/1`). A dead Agent (the
    /// process is shutting down) answers an empty Conversation rather than
    /// panicking, the same graceful degradation `active_model`/`status` take.
    pub async fn conversation(&self) -> Conversation {
        self.query(Command::Conversation)
            .await
            .unwrap_or_else(empty_conversation)
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

/// The empty Conversation a dead-Agent [`AgentHandle::conversation`] degrades to:
/// no system prompt, zero budget knobs. Never sent to the model - only read for
/// its (zero) token estimate and (empty) message list by a shutting-down caller.
fn empty_conversation() -> Conversation {
    Conversation::new("", crate::conversation::ConversationOpts::default())
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
    // The per-question reply channels (ADR-0057), keyed by the question id the
    // tool minted: the tool call parks awaiting this oneshot; `answer_question`
    // removes and answers it. No Standing-Approval analog - every question opens
    // a modal, so there is no auto/fold path, just this map.
    question_replies: HashMap<String, oneshot::Sender<QuestionAnswers>>,
    steering: Vec<String>,
    compaction: Compaction,
    // A clone of the mpsc sender, handed to the Run's AgentDeps so the Run
    // talks back over the same channel, and used to post the Run's outcome.
    self_tx: mpsc::UnboundedSender<Msg>,
    // The attached MCP servers (F8, ADR-0056): connected once in `init_agent`,
    // held for the Session's lifetime so the transports stay alive. Read only
    // for `failures()` (a startup notice); the tools themselves live on
    // `session_tools`.
    #[allow(dead_code)]
    mcp: crate::mcp::manager::McpManager,
    // The Session-stable tool set (F8, ADR-0056): built-ins plus discovered MCP
    // tools, built once in `init_agent` and REBUILT by the live `/mcp` ops
    // (ADR-0065 Phase C, `rebuild_session_tools`) after a reconnect/enable/disable.
    // Threaded into each Run's Capture (via AgentDeps) so every Run's registry
    // shares it; the swap lands on the NEXT Run's capture, the in-flight Run keeps
    // its own (the same rule as a `SetModel` swap).
    session_tools: Arc<[Box<dyn crate::tool::Tool>]>,
    // The disk-skill manager (ADR-0058): discovered once in `init_agent`. The one
    // `skill` tool holds a clone for its dynamic `<available_skills>` catalog; the
    // Agent keeps this handle so the live `/mcp` ops can REBUILD `session_tools`
    // (which must re-mint that same skill tool) without re-discovering skills
    // (ADR-0065 Phase C).
    skill_manager: Arc<crate::skills::SkillManager>,
    // The hook subsystem (ADR-0066): the standing `config.json` hooks resolved
    // once in `init_agent`, held so every Run fires the same standing set.
    // Threaded into each Run's Capture (via AgentDeps) so `batch.rs` can fire the
    // tool-dispatch + permission hooks. Skill-hook REGISTRATION (session scope) is
    // Phase 4; this holds only the standing source today, reachable by every Run.
    hook_manager: Arc<crate::hooks::HookManager>,
    // The subagent definitions (P4/F4, ADR-0061): the built-in registry, built
    // once in `init_agent`. Held by the `agent` tool (on `session_tools`) for its
    // dynamic schema/description AND threaded into each Run's Capture (via
    // AgentDeps) so the Run's DirectSubagentSpawner resolves a def by name.
    subagents: Arc<crate::subagents::SubagentRegistry>,
    // The background subagent registry (P4b/4c, ADR-0063): the detached child
    // Runs the Agent is tracking, keyed by the minted task id. Single-owner
    // state (ADR-0017): a launch inserts, a settlement/stop mutates, an
    // actor-loop exit aborts them all. The `agent` tool's background launch and
    // `task_stop` reach it ONLY through the mpsc (SpawnBackground/StopBackground),
    // so the map never leaves this task.
    background: HashMap<String, BackgroundTask>,
    // The queued background `<task-notification>` envelopes (P4b, ADR-0063): a
    // settling/cancelled child pushes one here; the next Run's Loop drains them
    // (DrainNotifications) and merges each into the next request's tool-results
    // user message. The queue survives idle drains, so a notification that lands
    // between Runs still reaches the model on the next Run.
    notifications: Vec<String>,
    // The monotonic per-Session background task counter (ADR-0063): the `n` in
    // `{subagent_type}-{n}`, so ids never collide within a Session.
    background_counter: u64,
    // The background shell registry (Phase 9, ADR-0063): the detached subprocesses
    // the Agent is tracking, keyed by the minted shell id (`bg_{n}`). Single-owner
    // state (ADR-0017) like `background`: a launch inserts, a settlement/stop
    // mutates, an actor-loop exit killpg's + aborts them all. `run_command`'s
    // background branch and `task_stop` reach it ONLY through the mpsc.
    background_shells: HashMap<String, BackgroundShell>,
    // The monotonic per-Session background shell counter (Phase 9, ADR-0063): the
    // `n` in `bg_{n}`, so shell ids never collide within a Session.
    background_shell_counter: u64,
}

async fn run_agent(mut state: AgentState, mut rx: mpsc::UnboundedReceiver<Msg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            // The live `/mcp` ops (ADR-0065 Phase C) re-attach a server and mutate
            // the Agent's tool set, so they await INLINE on the actor loop (like a
            // Run's own awaits, ADR-0017's single owner): the mutation is
            // exclusive, and each is bounded by the per-server connect timeout the
            // same as launch. The read-only `McpViews` rides the sync handler.
            Msg::Command(Command::McpReconnect(name, reply)) => {
                mcp_reconnect(&mut state, name).await;
                let _ = reply.send(Ok(()));
            }
            Msg::Command(Command::McpSetEnabled(name, enabled, reply)) => {
                let result = mcp_set_enabled(&mut state, name, enabled).await;
                let _ = reply.send(result);
            }
            Msg::Command(Command::McpAuthenticate(name, reply)) => {
                let result = mcp_authenticate(&mut state, name).await;
                let _ = reply.send(result);
            }
            Msg::Command(Command::McpClearAuth(name, reply)) => {
                let result = mcp_clear_auth(&mut state, name).await;
                let _ = reply.send(result);
            }
            Msg::Command(cmd) => handle_command(&mut state, cmd),
            Msg::Run(run) => handle_run(&mut state, run),
            Msg::Settle(outcome) => settle(&mut state, LoopOrDown::Loop(outcome)),
            Msg::RunDown(reason) => settle(&mut state, LoopOrDown::Down(reason)),
            Msg::EnrichedModel(model) => apply_enriched_model(&mut state, model),
        }
    }
    // The actor loop ended (every handle dropped): the Session is over, so any
    // still-running background child (subagent OR shell) must not outlive it
    // (P4b/Phase 9, ADR-0063).
    state.abort_all_background();
    state.abort_all_background_shells();

    // The SessionEnd hooks (Phase 3b, ADR-0066): fired ONCE here at shutdown,
    // through the same firing facade a Run uses. Observational (a SessionEnd hook
    // cannot steer a session that is already ending). `exit` is qwen's SessionEnd
    // reason for the process ending. Fail-open: no hooks, or a runner failure,
    // is a no-op.
    fire_session_end(&state).await;
}

// Builds the Agent's lifecycle-hook firing facade (Phase 3b, ADR-0066) over the
// standing manager + the Session's Llm/Model/Root - the same
// [`crate::run::hooks::Hooks`] a Run builds, so the Agent fires session/
// notification events through the identical capability set (command/http/prompt)
// without threading hook plumbing into the UI. Built on demand (it borrows the
// state), never held, so it never outlives an `&AgentState` borrow.
fn agent_hooks(state: &AgentState) -> crate::run::hooks::Hooks<'_> {
    let transcript = transcript_path(state);
    let session_id = crate::run::hooks::session_id_from_log_path(&transcript);
    crate::run::hooks::Hooks::new(
        state.hook_manager.as_ref(),
        state.llm.as_ref(),
        &state.model,
        state.session.root.clone(),
        session_id,
        transcript,
    )
}

// The Session Log's JSONL path (H1, ADR-0010/0066): the running transcript the hook
// payloads report as `transcript_path`. Empty when the Agent opened no log (the log
// open failed) - the fail-open base-identity fallback.
fn transcript_path(state: &AgentState) -> String {
    state
        .log
        .as_ref()
        .map(|log| log.path.clone())
        .unwrap_or_default()
}

// Fires the SessionEnd hooks at Agent shutdown (Phase 3b, ADR-0066).
async fn fire_session_end(state: &AgentState) {
    agent_hooks(state).session_end("exit").await;
}

// Fires the Notification hooks (Phase 3b, ADR-0066) for the "agent is waiting"
// moment - the same ask-request broadcast that drives the terminal notification
// (an Approval or a Question opening a modal). Spawned DETACHED off the actor loop
// (over owned Arc/Model/Root clones the built `Hooks` borrows inside the task) so a
// slow command/http/prompt hook never stalls the single owner while it holds the
// user waiting; observational, so nothing flows back. A Session with no
// Notification hooks spawns nothing (the resolved list is empty and the fire is a
// no-op) - cheap enough to skip the emptiness pre-check.
fn fire_notification(state: &AgentState, message: String) {
    let manager = Arc::clone(&state.hook_manager);
    let llm = Arc::clone(&state.llm);
    let model = state.model.clone();
    let root = state.session.root.clone();
    let transcript = transcript_path(state);
    let session_id = crate::run::hooks::session_id_from_log_path(&transcript);
    tokio::spawn(async move {
        let hooks = crate::run::hooks::Hooks::new(
            manager.as_ref(),
            llm.as_ref(),
            &model,
            root,
            session_id,
            transcript,
        );
        hooks.notification(&message).await;
    });
}

// The public-Command dispatcher: a flat table that routes each Command to its
// own small handler (the Command pattern), so no arm carries inline branching
// and the dispatch stays cyclomatically trivial. Each handler owns the
// state-mutation-plus-reply for one Command; the reads that are a single
// `reply.send(state.field.clone())` route through `reply_query` so the read arms
// share one shape. The four MCP live-op arms are defensive: `run_agent`
// intercepts those ahead of this sync handler (they await), so reaching one
// means a future dispatch change regressed - answer the Err rather than panic.
fn handle_command(state: &mut AgentState, cmd: Command) {
    match cmd {
        Command::Submit(prompt, reply) => handle_submit(state, prompt, reply),
        Command::Steer(text, reply) => handle_steer(state, text, reply),
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
        Command::CycleApprovalMode(reply) => {
            let _ = reply.send(cycle_approval_mode(state));
        }
        Command::AnswerQuestion(id, answers, reply) => {
            answer_question(state, id, answers);
            let _ = reply.send(());
        }
        Command::McpViews(reply) => {
            let _ = reply.send(state.mcp.views());
        }
        Command::McpReconnect(_, reply) => {
            let _ = reply.send(mcp_not_dispatched("reconnect"));
        }
        Command::McpSetEnabled(_, _, reply) => {
            let _ = reply.send(mcp_not_dispatched("enable/disable"));
        }
        Command::McpAuthenticate(_, reply) => {
            let _ = reply.send(mcp_not_dispatched("authenticate"));
        }
        Command::McpClearAuth(_, reply) => {
            let _ = reply.send(mcp_not_dispatched("clear-auth"));
        }
        Command::Cancel(reply) => handle_cancel(state, reply),
        Command::Status(reply) => {
            let _ = reply.send(current_status(state));
        }
        Command::Conversation(reply) => {
            let _ = reply.send(state.conversation.clone());
        }
        Command::Plan(reply) => {
            let _ = reply.send(state.plan.clone());
        }
        Command::ResumeInfoQuery(reply) => {
            let _ = reply.send(state.resume_info.clone());
        }
        Command::Skills(reply) => {
            let _ = reply.send(Arc::clone(&state.skill_manager));
        }
    }
}

// Submit starts a Run when idle; a running Run is Busy (baud's submit guard).
fn handle_submit(state: &mut AgentState, prompt: String, reply: oneshot::Sender<Result<(), Busy>>) {
    if state.task.is_some() {
        let _ = reply.send(Err(Busy));
    } else {
        start_run(state, prompt);
        let _ = reply.send(Ok(()));
    }
}

// Steer queues text onto the running Run; no Run is Idle (baud's steer guard).
fn handle_steer(state: &mut AgentState, text: String, reply: oneshot::Sender<Result<(), Idle>>) {
    if state.task.is_some() {
        state.steering.push(text.clone());
        broadcast(state, Event::steering_queued(text));
        let _ = reply.send(Ok(()));
    } else {
        let _ = reply.send(Err(Idle));
    }
}

// Cancel aborts the running Run and notes the cancellation on the Settlement so
// the following abort settles as cancelled; a no-op when idle (baud's cancel).
fn handle_cancel(state: &mut AgentState, reply: oneshot::Sender<()>) {
    if let Some(abort) = &state.task {
        abort.abort();
        state.cancel_flag = true;
        state.settlement = std::mem::take(&mut state.settlement).note_cancelled();
    }
    let _ = reply.send(());
}

// The Agent's status derived from whether a Run task is in flight (baud's
// `status`).
fn current_status(state: &AgentState) -> Status {
    if state.task.is_some() {
        Status::Running
    } else {
        Status::Idle
    }
}

// The defensive Err a live-MCP arm answers when it reaches the sync handler:
// `run_agent` intercepts those ahead of here (they await), so this only fires on
// a dispatch regression - the one place the four arms' wording is authored.
fn mcp_not_dispatched(op: &str) -> Result<(), String> {
    Err(format!("MCP {op} was not dispatched"))
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
        RunMsg::AskQuestion {
            id,
            questions,
            reply,
        } => {
            ask_question(state, id, questions, reply);
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
        RunMsg::SpawnBackground {
            request,
            description,
            reply,
        } => {
            let id = state.spawn_background(request, description);
            let _ = reply.send(id);
        }
        RunMsg::BackgroundDone { id, result } => {
            state.background_done(id, result);
        }
        RunMsg::StopBackground { id, reply } => {
            // Dual-registry (Phase 9, ADR-0063): `task_stop` names one id space, so
            // try the subagent registry, then the shell registry, then synthesize
            // the VERBATIM not-found ONCE. Both `stop_background` handlers return an
            // `Option` so the fall-through is a `None`, not a string sniff.
            let wording = state
                .stop_background(id.clone())
                .or_else(|| state.stop_background_shell(id.clone()))
                .unwrap_or_else(|| format!("Error: No background task found with ID \"{id}\"."));
            let _ = reply.send(wording);
        }
        RunMsg::SpawnBackgroundShell {
            command,
            cwd,
            reply,
        } => {
            let id = state.spawn_background_shell(command, cwd);
            let _ = reply.send(id);
        }
        RunMsg::BackgroundShellDone { id, outcome } => {
            state.background_shell_done(id, outcome);
        }
        RunMsg::StopBackgroundShell { id, reply } => {
            let wording = state
                .stop_background_shell(id.clone())
                .unwrap_or_else(|| format!("Error: No background task found with ID \"{id}\"."));
            let _ = reply.send(wording);
        }
        RunMsg::DrainNotifications(reply) => {
            let drained = std::mem::take(&mut state.notifications);
            let _ = reply.send(drained);
        }
    }
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
            // The Notification seam (Phase 3b, ADR-0066): an ask opening a modal is
            // the "agent is waiting" moment, so fire the Notification hooks with the
            // command the user is being asked to approve.
            fire_notification(state, format!("Approval requested: {command}"));
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

// A question request (ADR-0057, `ask_user_question`): broadcast
// `question_request` to open the modal and hold the reply oneshot under the
// question id until `answer_question` arrives. Unlike `request_approval` there is
// NO Standing-Approval / auto path - every question opens a modal - so this is
// unconditionally the "pending" leg: insert the reply, broadcast the request.
fn ask_question(
    state: &mut AgentState,
    id: String,
    questions: Vec<crate::tool::caps::Question>,
    reply: oneshot::Sender<QuestionAnswers>,
) {
    state.question_replies.insert(id.clone(), reply);
    // The Notification seam (Phase 3b, ADR-0066): a question opening a modal is the
    // "agent is waiting" moment, so fire the Notification hooks. The question text
    // is the salient waiting content.
    let message = questions
        .first()
        .map(|q| format!("Question: {}", q.question))
        .unwrap_or_else(|| "Agent is waiting for input".to_string());
    fire_notification(state, message);
    broadcast(state, Event::question_request(id, questions));
}

// Resolve a pending question (ADR-0057): remove the parked reply and forward the
// user's answer to it. A stale/duplicate `answer_question` (no matching entry)
// is dropped, mirroring `approve`'s `Decide::Ignore`. The tool emits
// `question_resolved` after it reads the reply, like the Approver emits
// `approval_resolved`.
fn answer_question(state: &mut AgentState, id: String, answers: QuestionAnswers) {
    if let Some(reply) = state.question_replies.remove(&id) {
        let _ = reply.send(answers);
    }
}

// The Shift+Tab Approval-mode cycle (ADR-0050): the pure `Approvals` fold
// rotates the mode and the Agent broadcasts the new mode so every subscriber
// (the Screen mirror, hence the footer indicator) sees it. Session-scoped, so
// no Run needs to be running - `Yolo` then auto-approves the NEXT gated Call
// via `request` without a modal.
fn cycle_approval_mode(state: &mut AgentState) -> ApprovalMode {
    let (approvals, mode) = std::mem::take(&mut state.approvals).cycle_mode();
    state.approvals = approvals;
    broadcast(state, Event::approval_mode_changed(mode));
    mode
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
// log_event). Steering and Voice-authored tail markers ride as user-role
// entries.
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

// ---- Run start (submit + Rollover) -----------------------------------------

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
    let deps = AgentDeps::new(RunSpawn {
        tx: state.self_tx.clone(),
        llm: Arc::clone(&state.llm),
        model: state.model.clone(),
        settings: RequestSettings {
            temperature: state.session.temperature,
            thinking_budget: state.session.thinking_budget,
            tool_call_style: state.session.tool_call_style,
        },
        compaction: state.compaction.clone(),
        session_tools: Arc::clone(&state.session_tools),
        subagents: Arc::clone(&state.subagents),
        session: state.session.clone(),
        hooks: Arc::clone(&state.hook_manager),
        skills: Arc::clone(&state.skill_manager),
        transcript_path: transcript_path(state),
    });
    let conversation = state.conversation.clone();
    let session = state.session.clone();
    let opts = run_opts(state, state.compaction.original_task.clone());

    let run = tokio::spawn(async move {
        let capture = deps.capture();
        crate::run::run(conversation, session, capture, deps, opts).await
    });
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
    state.question_replies.clear();
    state.cancel_flag = false;
    state.run_provenance = state.model.provenance();
    // The captured Model's budget figures land at Run start (ADR-0037): the
    // Context Budget from its window (config may cap it), the reply reserve
    // from its output cap clamped to leave a live window. The Run task clones
    // this Conversation, so an
    // in-flight Run keeps the figures it captured, and a switch to a smaller
    // window lands here as ordinary pressure on the next Run.
    state.conversation.context_budget = state.session.context_budget_for(&state.model);
    state.conversation.max_tokens_reserve = state.session.reply_reserve_for(&state.model);
}

fn run_opts(state: &AgentState, original_task: Option<String>) -> RunOpts {
    RunOpts {
        plan: state.plan.clone(),
        original_task,
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

fn mint_run_ref() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("turn-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

// ---- Settlement ------------------------------------------------------------

fn settle(state: &mut AgentState, outcome: LoopOrDown) {
    // If there is no running Run, this is a stale outcome (e.g. the watcher for
    // an already-settled Run); ignore it.
    if state.task.is_none() {
        return;
    }

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
    state.question_replies.clear();
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

    // Rolled-over Steering is the user's voice continuing the same request:
    // start a fresh Run over it.
    match resolution.rollover {
        Rollover::Submit(prompt) => start_run(state, prompt),
        Rollover::None => {}
    }
}

// ---- Resume ----------------------------------------------------------------

/// What a Resume yields: the folded Conversation messages, the Transcript-facing
/// [`ResumeInfo`], and the last logged Plan (held outside the Conversation).
type Resumed = (
    Vec<crate::content::Message>,
    Option<ResumeInfo>,
    Option<String>,
);

/// The governance fact a Resume restores alongside the Conversation: the last
/// logged Plan (held outside the Conversation), computed in the single fold, so
/// it never belongs on the Transcript-facing [`ResumeInfo`] - the Agent threads
/// it privately.
fn maybe_resume(resume: Option<Resume>, session: &Session) -> Result<Resumed, StartError> {
    let path = match resume {
        None => return Ok((Vec::new(), None, None)),
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
            r.plan,
        )),
        Err(ResumeError::RootMismatch) => Err(StartError::ResumeRootMismatch(path)),
        Err(e) => Err(StartError::ResumeFailed(format!(
            "cannot resume {path}: {e:?}"
        ))),
    }
}
