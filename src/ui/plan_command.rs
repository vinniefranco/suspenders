//! The `/plan` Slash Command (ADR-0067, qwen `planCommand.ts`) - a faithful port
//! of qwen's built-in `plan` command: enter Plan mode, exit it (restoring the
//! previous mode), or - with a trailing prompt - enter Plan and submit that
//! prompt in one step. Its BRANCH LOGIC is pure and tested ([`decide`]); the I/O
//! (the Agent mode round-trip and the prompt submission) is the thin adapter
//! ([`run`]), mirroring [`super::model_command`]/[`super::theme_command`].
//!
//! ## Faithful to qwen's semantics (planCommand.ts)
//!
//! The command reads the live Approval mode and branches exactly as qwen does:
//! - `/plan exit` when NOT in Plan → the error `Not in plan mode. Use "/plan" to
//!   enter plan mode first.`
//! - `/plan exit` when in Plan → restore the pre-plan mode (the MANUAL-exit path,
//!   so the one-shot manual-exit reminder fires, like Shift+Tab out of Plan) and
//!   the info `Exited plan mode. Previous approval mode restored.`
//! - `/plan` (no args) when NOT in Plan → enter Plan (a user request, so even from
//!   Yolo) and the info `Enabled plan mode. The agent will analyze and plan
//!   without executing tools.`
//! - `/plan <prompt>` when NOT in Plan → enter Plan, THEN submit `<prompt>`.
//! - `/plan` (no args) when ALREADY in Plan → the info `Already in plan mode. Use
//!   "/plan exit" to exit plan mode.`
//! - `/plan <prompt>` when ALREADY in Plan → submit `<prompt>` (no mode change).
//!
//! Every message is byte-verbatim from planCommand.ts. The generic router that
//! reaches this module lives in [`super::command`].

use crate::approvals::ApprovalMode;
use crate::ui::AdapterCtx;

use super::screen::Screen;

/// The command's registered name - what [`super::command::handled`] routes here,
/// minted once beside the module that owns the command.
pub(crate) const NAME: &str = "plan";

// The verbatim qwen messages (planCommand.ts). Held as consts so [`decide`] and
// its tests reference the exact bytes qwen emits, not a re-typed copy.

/// planCommand.ts:45 - `/plan exit` outside Plan mode (an error line).
const NOT_IN_PLAN_MODE: &str = "Not in plan mode. Use \"/plan\" to enter plan mode first.";
/// planCommand.ts:60 - a successful `/plan exit` (an info line).
const EXITED_PLAN_MODE: &str = "Exited plan mode. Previous approval mode restored.";
/// planCommand.ts:85-87 - `/plan` (no args) entering Plan (an info line).
const ENABLED_PLAN_MODE: &str =
    "Enabled plan mode. The agent will analyze and plan without executing tools.";
/// planCommand.ts:102 - `/plan` (no args) already in Plan (an info line).
const ALREADY_IN_PLAN_MODE: &str = "Already in plan mode. Use \"/plan exit\" to exit plan mode.";

/// The pure `/plan` decision (qwen planCommand.ts `action`): over the live
/// Approval mode and the trimmed argument string, which of qwen's six branches
/// applies. The impure [`run`] carries out the mode round-trip and the submit;
/// this states WHAT to do so the branch table is unit-testable without an Agent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlanAction {
    /// Emit an info/error line, no mode change, no submit (the four message-only
    /// branches). `is_error` picks the qwen `messageType` (error vs info); the
    /// Transcript renders both as a line today, but the flag keeps the port
    /// faithful and lets a future styled-error read it.
    Message { text: &'static str, is_error: bool },
    /// Restore the pre-plan mode (the `/plan exit` success branch), then emit the
    /// "Exited plan mode" info line.
    Exit,
    /// Enter Plan mode, then emit the "Enabled plan mode" info line (`/plan` with
    /// no args outside Plan).
    Enter,
    /// Enter Plan mode, then submit the carried prompt (`/plan <prompt>` outside
    /// Plan). No info line - the submitted prompt IS the feedback.
    EnterThenSubmit(String),
    /// Submit the carried prompt with no mode change (`/plan <prompt>` while
    /// already in Plan).
    Submit(String),
}

/// The pure branch table (qwen planCommand.ts `action`, lines 37-104): trims the
/// args, reads the current mode, and returns which [`PlanAction`] applies. `rest`
/// is the raw draft remainder past `/plan` (`slash::parse(...).rest`); `None` and
/// an all-whitespace string both mean "no args" (qwen's `args.trim()`).
fn decide(current: ApprovalMode, rest: Option<&str>) -> PlanAction {
    let trimmed = rest.unwrap_or("").trim();
    let in_plan = current == ApprovalMode::Plan;

    if trimmed == "exit" {
        return if in_plan {
            PlanAction::Exit
        } else {
            PlanAction::Message {
                text: NOT_IN_PLAN_MODE,
                is_error: true,
            }
        };
    }

    if !in_plan {
        return if trimmed.is_empty() {
            PlanAction::Enter
        } else {
            PlanAction::EnterThenSubmit(trimmed.to_string())
        };
    }

    // Already in plan mode.
    if trimmed.is_empty() {
        PlanAction::Message {
            text: ALREADY_IN_PLAN_MODE,
            is_error: false,
        }
    } else {
        PlanAction::Submit(trimmed.to_string())
    }
}

/// Runs a committed `/plan` (ADR-0067, qwen planCommand.ts). Reads the live
/// Approval mode from the Screen mirror - the authoritative adapter-side copy the
/// Agent writes on every mode change (the same mirror the footer indicator reads;
/// `cycle_approval_mode`/`enter`/`exit` all set it from the authoritative fold
/// result, and the `ApprovalModeChanged` broadcast keeps it fresh for model-driven
/// changes), so no extra read round-trip is needed. Carries out the pure
/// [`decide`]'s action: an enter/exit round-trips the Agent and updates the mirror
/// from the authoritative returned mode (the P0 pattern, so a lossy broadcast
/// can never desync the footer). Returns the Screen plus an OPTIONAL prompt to
/// submit - the adapter ([`super::super::run_command_effect`]) feeds it through
/// the same submit path a `/<skill>` uses, so `/plan <prompt>` fires the run loop
/// and UserPromptSubmit hooks identically.
pub(super) async fn run(
    screen: Screen,
    ctx: &AdapterCtx<'_>,
    rest: Option<&str>,
) -> (Screen, Option<String>) {
    match decide(screen.approval_mode(), rest) {
        PlanAction::Message { text, .. } => (screen.info(text).0, None),
        PlanAction::Exit => {
            // The MANUAL-exit path (`from_approved_plan_exit = false`): leaving
            // Plan queues the one-shot manual-exit reminder, exactly like Shift+Tab
            // out of Plan (ADR-0067). Update the mirror from the restored mode.
            let mut screen = screen;
            let mode = ctx.agent.exit_plan_mode().await;
            screen.mirror_approval_mode(mode);
            (screen.info(EXITED_PLAN_MODE).0, None)
        }
        PlanAction::Enter => {
            let mut screen = screen;
            let mode = ctx.agent.enter_plan_mode().await;
            screen.mirror_approval_mode(mode);
            (screen.info(ENABLED_PLAN_MODE).0, None)
        }
        PlanAction::EnterThenSubmit(prompt) => {
            let mut screen = screen;
            let mode = ctx.agent.enter_plan_mode().await;
            screen.mirror_approval_mode(mode);
            (screen, Some(prompt))
        }
        PlanAction::Submit(prompt) => (screen, Some(prompt)),
    }
}

#[cfg(test)]
#[path = "../../tests/ui/plan_command.rs"]
mod tests;
