//! The tx-backed Tool Capability adapters the Agent fulfils (F1/P2a/P4b,
//! ADR-0055, ADR-0057, ADR-0063). Each is a handle over the Agent's mpsc that a
//! Run's [`crate::run::Capture`] carries into the Tool
//! [`crate::tool::caps::Capabilities`]: the [`AgentApprover`] relays an Approval,
//! the [`AgentQuestioner`] relays a question, the [`AgentSubagentSpawner`]
//! drives the foreground DIRECT spawn plus the two tx-backed background legs, and
//! the [`AgentBackgroundShellSpawner`] relays the two tx-backed background-shell
//! legs (the Agent owns the detached process lifecycle, Phase 9, ADR-0063).
//! Built by [`super::deps::AgentDeps::capture`] (the adapter that owns the
//! channel), they live beside the deps rather than inside it - the deps keeps the
//! `RunDeps` port; these keep the tool-facing capability seams.

use tokio::sync::{mpsc, oneshot};

use crate::agent::{Msg, RunMsg};
use crate::event::Event;

/// The tx-backed [`Approver`](crate::tool::caps::Approver): the Agent's
/// fulfilment of the tool-initiated Approval seam (F1, ADR-0055). It relays an
/// Approval over the same mpsc the [`super::deps::AgentDeps::request_approval`]
/// batch gate uses and awaits the same reply oneshot - so a tool-initiated
/// Approval and a gate Approval reach the Agent identically. Built by
/// [`super::deps::AgentDeps::capture`] and carried on the Run's
/// [`crate::run::Capture`] into the Tool [`crate::tool::caps::Capabilities`].
///
/// Why kept without a consumer yet: P1b wires this into the Capabilities carrier
/// to prove the whole seam with a live wire, but no tool consumes the Approver in
/// P1b - the batch gate still drives approval through [`crate::run::deps::RunDeps::request_approval`].
/// The two share the exact request path; a later phase collapses the duplication
/// once a tool initiates its own Approval.
pub(super) struct AgentApprover {
    pub(super) tx: mpsc::UnboundedSender<Msg>,
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

/// The tx-backed [`Questioner`](crate::tool::caps::Questioner): the Agent's
/// fulfilment of the tool-initiated Question seam (P2a, ADR-0057). It mints a
/// question id, relays an `AskQuestion` over the same mpsc the Approver uses, and
/// awaits the reply oneshot - so the tool call parks until the user answers.
/// Unlike the Approver there is NO Standing-Approval / auto path: every question
/// opens a modal (the Agent's `ask_question` handler is unconditionally the
/// pending leg). Built by [`super::deps::AgentDeps::capture`] and carried on the
/// Run's [`crate::run::Capture`] into the Tool
/// [`crate::tool::caps::Capabilities`].
///
/// On a closed/dropped channel (a dead Agent, a cancelled Run) it returns the
/// VERBATIM decline string, the safe answer the tool folds into its content -
/// symmetric with [`AgentApprover`] returning `false`.
pub(super) struct AgentQuestioner {
    pub(super) tx: mpsc::UnboundedSender<Msg>,
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
        let answers = rx
            .await
            .unwrap_or_else(|_| Err(DECLINE_MESSAGE.to_string()));
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

/// The tx-backed [`PlanMode`](crate::tool::caps::PlanMode): the Agent's
/// fulfilment of the plan-lifecycle seam (ADR-0067). `enter` relays an
/// `EnterPlanMode` and awaits the outcome the Agent's handler resolves (the
/// atomic YOLO-guard / idempotent / prePlanMode logic); `request_exit` relays a
/// `RequestPlanExit`, and the Agent mints a dialog id, broadcasts `PlanRequest`,
/// and parks the reply until a `Command::AnswerPlan` resolves it. Built by
/// [`super::deps::AgentDeps::capture`] and carried on the Run's
/// [`crate::run::Capture`] into the Tool [`crate::tool::caps::Capabilities`].
///
/// A child Run (subagent) carries the degraded
/// [`crate::tool::caps::SubagentPlanMode`] instead - the subagent block - so a
/// plan-lifecycle call there never reaches this handle. On a closed/dropped
/// channel (a dead Agent, a cancelled Run) both legs return the subagent-blocked
/// outcome, the safe answer the tool folds into its content - the same posture
/// [`AgentQuestioner`] takes with its decline string.
pub(super) struct AgentPlanMode {
    pub(super) tx: mpsc::UnboundedSender<Msg>,
}

#[async_trait::async_trait]
impl crate::tool::caps::PlanMode for AgentPlanMode {
    async fn enter(&self, user_requested: bool) -> crate::tool::caps::EnterPlanOutcome {
        // Relay the entry and await the outcome the Agent's handler resolves. A
        // closed channel is the safe subagent-blocked answer (the tool folds it).
        let tx = self.tx.clone();
        let (reply, rx) = oneshot::channel();
        if tx
            .send(Msg::Run(RunMsg::EnterPlanMode {
                user_requested,
                reply,
            }))
            .is_err()
        {
            return crate::tool::caps::EnterPlanOutcome::SubagentBlocked;
        }
        rx.await
            .unwrap_or(crate::tool::caps::EnterPlanOutcome::SubagentBlocked)
    }

    async fn request_exit(&self, plan: String) -> crate::tool::caps::PlanExitOutcome {
        // Relay the exit request; the Agent mints the dialog id, broadcasts the
        // `PlanRequest` (opening the modal), snapshots the revision, and parks the
        // reply until the user answers. No timeout - the user decides. Unlike the
        // Questioner (whose capability mints the id and so emits `question_resolved`
        // itself), the plan dialog id is Agent-minted, so `plan_resolved` is emitted
        // Agent-side in `answer_plan` where the id is known.
        let tx = self.tx.clone();
        let (reply, rx) = oneshot::channel();
        if tx
            .send(Msg::Run(RunMsg::RequestPlanExit { plan, reply }))
            .is_err()
        {
            return crate::tool::caps::PlanExitOutcome::SubagentBlocked;
        }
        rx.await
            .unwrap_or(crate::tool::caps::PlanExitOutcome::SubagentBlocked)
    }
}

/// The VERBATIM qwen `task_stop` not-found wording, the one template both
/// background-stop capability legs fall back to on a closed channel (a dead
/// Agent). Owned here so the two relays share one copy of the sentence.
fn not_found(id: &str) -> String {
    format!("Error: No background task found with ID \"{id}\".")
}

/// The two-channel [`SubagentSpawner`](crate::tool::caps::SubagentSpawner) the
/// Agent builds (P4b, ADR-0063). Its FOREGROUND `spawn` is the DIRECT
/// (Llm-boundary, non-mpsc) path - it delegates to the held
/// [`crate::run::subagent::DirectSubagentSpawner`], driving a child Run inline (a
/// foreground subagent touches no Agent/Conversation state). Its BACKGROUND
/// `spawn_background`/`stop_background` are tx-backed: a detached child Run and
/// the task registry are Agent-owned mutable state (ADR-0017), so they relay a
/// `SpawnBackground`/`StopBackground` over the same mpsc the Approver/Questioner
/// use and await the reply. Two channels, one terminus - the foreground straight
/// to the Llm, the background through the Agent that owns the registry.
///
/// Built by [`super::deps::AgentDeps::capture`] and carried on the Run's
/// [`crate::run::Capture`] into the Tool [`crate::tool::caps::Capabilities`],
/// replacing the bare `DirectSubagentSpawner` P4a put there.
pub(super) struct AgentSubagentSpawner {
    /// The foreground DIRECT spawner: `spawn` delegates to it unchanged.
    pub(super) direct: crate::run::subagent::DirectSubagentSpawner,
    /// The Agent's mpsc: `spawn_background`/`stop_background` relay over it.
    pub(super) tx: mpsc::UnboundedSender<Msg>,
}

#[async_trait::async_trait]
impl crate::tool::caps::SubagentSpawner for AgentSubagentSpawner {
    async fn spawn(
        &self,
        request: crate::tool::caps::SubagentRequest,
    ) -> Result<crate::tool::caps::SubagentResult, String> {
        // Foreground: the DIRECT path, unchanged - drive the child Run inline off
        // the captured Llm (the held DirectSubagentSpawner holds the handles).
        self.direct.spawn(request).await
    }

    async fn spawn_background(
        &self,
        request: crate::tool::caps::SubagentRequest,
        description: String,
    ) -> Result<String, String> {
        // Background: relay a SpawnBackground to the Agent (it mints the id,
        // spawns the detached child, registers it) and await the id it replies.
        // A closed channel is the degraded launch failure (the tool folds it).
        let tx = self.tx.clone();
        let (reply, rx) = oneshot::channel();
        if tx
            .send(Msg::Run(RunMsg::SpawnBackground {
                request,
                description,
                reply,
            }))
            .is_err()
        {
            return Err("background subagents are unavailable in this environment".into());
        }
        rx.await
            .map_err(|_| "background subagents are unavailable in this environment".into())
    }

    async fn stop_background(&self, id: String) -> Result<String, String> {
        // Relay a StopBackground to the Agent and await the VERBATIM wording it
        // replies (found/not-running/not-found). This is the DUAL-REGISTRY leg the
        // Agent resolves (subagent -> shell -> not-found), so `task_stop` reaches
        // both registries through this one relay. A closed channel yields the
        // not-found wording (the safe answer the tool returns as content).
        let tx = self.tx.clone();
        let (reply, rx) = oneshot::channel();
        if tx
            .send(Msg::Run(RunMsg::StopBackground {
                id: id.clone(),
                reply,
            }))
            .is_err()
        {
            return Ok(not_found(&id));
        }
        Ok(rx.await.unwrap_or_else(|_| not_found(&id)))
    }
}

/// The tx-backed [`BackgroundShellSpawner`](crate::tool::caps::BackgroundShellSpawner):
/// the Agent's fulfilment of the tool-initiated background-shell seam (Phase 9,
/// ADR-0063). The Agent owns the detached process lifecycle + the shell registry
/// (both Agent-owned mutable state, ADR-0017), so both legs are tx-backed: they
/// relay a `SpawnBackgroundShell`/`StopBackgroundShell` over the same mpsc the
/// Approver/Questioner use and await the reply. Built by
/// [`super::deps::AgentDeps::capture`] and carried on the Run's
/// [`crate::run::Capture`] into the Tool [`crate::tool::caps::Capabilities`].
pub(super) struct AgentBackgroundShellSpawner {
    pub(super) tx: mpsc::UnboundedSender<Msg>,
}

#[async_trait::async_trait]
impl crate::tool::caps::BackgroundShellSpawner for AgentBackgroundShellSpawner {
    async fn spawn_background(&self, command: String, cwd: String) -> Result<String, String> {
        // Relay a SpawnBackgroundShell to the Agent (it mints the id, spawns the
        // detached child, registers it) and await the id it replies. A closed
        // channel is the degraded launch failure (the tool folds it).
        let tx = self.tx.clone();
        let (reply, rx) = oneshot::channel();
        if tx
            .send(Msg::Run(RunMsg::SpawnBackgroundShell {
                command,
                cwd,
                reply,
            }))
            .is_err()
        {
            return Err("background shells are unavailable in this environment".into());
        }
        rx.await
            .map_err(|_| "background shells are unavailable in this environment".into())
    }

    async fn stop_background(&self, id: String) -> Result<String, String> {
        // The shell-only stop leg (the dual-registry resolution `task_stop` drives
        // lives on `StopBackground`). Relay a `StopBackgroundShell` and await the
        // VERBATIM wording. A closed channel yields the not-found wording.
        let tx = self.tx.clone();
        let (reply, rx) = oneshot::channel();
        if tx
            .send(Msg::Run(RunMsg::StopBackgroundShell {
                id: id.clone(),
                reply,
            }))
            .is_err()
        {
            return Ok(not_found(&id));
        }
        Ok(rx.await.unwrap_or_else(|_| not_found(&id)))
    }
}
