//! `exit_plan_mode({plan, originalRequest?, researchSummary?})`: presents the
//! finished plan and raises the plan-confirmation dialog (faithful to qwen
//! exitPlanMode.ts). The model reaches for this when it is in plan mode, has
//! finished planning the implementation of a coding task, and is ready to code.
//!
//! The plan-confirmation round-trip (raise the dialog, await the user's target
//! mode, flip the mode + save the plan on a proceed, stale-guard) lives Agent-side
//! through the [`crate::tool::caps::PlanMode`] capability - the mode is
//! Agent-owned state (ADR-0017). The tool validates the plan, calls the
//! capability, and words the returned [`PlanExitOutcome`] VERBATIM as qwen's
//! `execute` does.
//!
//! `originalRequest`/`researchSummary` are kept in the schema for wire-fidelity
//! but are unused: they feed qwen's OMITTED plan-required-teammate (team) path,
//! which suspenders does not have (ADR-0067). qwen's deprecated `resolutionSummary`
//! is OMITTED. Empty `plan` is rejected in `run` (suspenders' schema `validate` is
//! shallow), like ask_user_question re-implements its shape rules.
//!
//! `Kind::Think` (qwen exitPlanMode.ts `Kind.Think`) and DEFERRED + always-declared
//! (qwen `shouldDefer: true` + the always-declared flag): its schema is
//! discoverable AND revealed by `enter_plan_mode`, so once plan mode tells the
//! model to call it, it is on the wire list.

use crate::tool::caps::PlanExitOutcome;
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

pub struct ExitPlanMode;

/// The tool description, VERBATIM from qwen exitPlanMode.ts
/// (`exitPlanModeToolDescription`).
const DESCRIPTION: &str = "Use this tool when you are in plan mode and have finished presenting your plan and are ready to code. This will prompt the user to exit plan mode.

## When to Use This Tool
IMPORTANT: Only use this tool when the task requires planning the implementation steps of a task that requires writing code. For research tasks where you're gathering information, searching files, reading files or in general trying to understand the codebase - do NOT use this tool.

## Before Using This Tool
Ensure your plan is complete and unambiguous:
- If you have unresolved questions about requirements or approach, use AskUserQuestion first (in earlier phases)
- The plan parameter MUST contain your actual plan content — empty strings will be rejected
- Once your plan is finalized, use THIS tool to request approval

**Important:** Do NOT use AskUserQuestion to ask \"Is this plan okay?\" or \"Should I proceed?\" - that's exactly what THIS tool does. ExitPlanMode inherently requests user approval of your plan.

## Examples
1. Initial task: \"Search for and understand the implementation of vim mode in the codebase\" - Do not use the exit plan mode tool because you are not planning the implementation steps of a task.
2. Initial task: \"Help me implement yank mode for vim\" - Use the exit plan mode tool after you have finished planning the implementation steps of the task.
3. Initial task: \"Add a new feature to handle user authentication\" - If unsure about auth method (OAuth, JWT, etc.), use AskUserQuestion first, then use exit plan mode tool after clarifying the approach.
";

/// The VERBATIM qwen empty-plan rejection (exitPlanMode.ts `validateToolParams`).
const EMPTY_PLAN_MESSAGE: &str = "Parameter \"plan\" must be a non-empty string.";

/// The VERBATIM qwen approved result (exitPlanMode.ts `execute`, the success
/// return's `llmContent`).
const APPROVED_MESSAGE: &str =
    "User approved. You can now start coding. Start with updating your todo list if applicable.";

/// The VERBATIM qwen cancel result (exitPlanMode.ts `execute`, the no-approval
/// `noActionResult`).
const CANCEL_MESSAGE: &str = "Plan execution was not approved. Remaining in plan mode.";

/// The VERBATIM qwen stale result (exitPlanMode.ts `execute`, the
/// changed-revision `noActionResult`).
const STALE_MESSAGE: &str =
    "Plan approval is stale because the approval mode changed. No action was taken.";

#[async_trait::async_trait]
impl Tool for ExitPlanMode {
    // Think (qwen exitPlanMode.ts `Kind.Think`): a plan-lifecycle tool, allowed
    // in plan mode - it raises the exit dialog, it does not mutate the workspace.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Think
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "exit_plan_mode".into(),
            description: DESCRIPTION.into(),
            // Schema VERBATIM from qwen exitPlanMode.ts
            // (`exitPlanModeToolSchemaData.parametersJsonSchema`): required `plan`,
            // optional `originalRequest`/`researchSummary` (the OMITTED team path -
            // kept for wire-fidelity), additionalProperties false. qwen's
            // deprecated `resolutionSummary` is omitted.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "string",
                        "description": "The plan you came up with, that you want to run by the user for approval. Supports markdown. The plan should be pretty concise. Must contain your actual plan content — empty strings will be rejected."
                    },
                    "originalRequest": {
                        "type": "string",
                        "description": "The original user request that prompted this plan. Restate it faithfully for a plan-required teammate leader."
                    },
                    "researchSummary": {
                        "type": "string",
                        "description": "A brief summary of the investigation and key findings gathered during plan mode for a plan-required teammate leader."
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
        }
    }

    fn should_defer(&self) -> bool {
        // Deferred (qwen `shouldDefer: true`): not on the base wire list. Plan mode
        // reveals it (enter_plan_mode) so the model can call it once planning.
        true
    }

    fn always_load(&self) -> bool {
        // Always-declared (qwen's always-declared flag, issue #5210): plan mode
        // tells the model to call exit_plan_mode directly, so its schema stays
        // discoverable even without a reveal.
        true
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // TOOL-LEVEL validation (qwen `validateToolParams`): the plan must be a
        // non-empty (trimmed) string. suspenders' schema `validate` only checks
        // required/type, so the emptiness rule is re-implemented here VERBATIM.
        let plan = match input.get("plan") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            _ => return Err(EMPTY_PLAN_MESSAGE.to_string()),
        };

        // The Plan-Mode effect (ADR-0067): raise the plan-confirmation dialog and
        // await the user's resolution. Outside plan mode the Agent returns
        // `NotInPlanMode` without a modal; in plan mode it opens the dialog and
        // parks until `answer_plan`. A subagent's degraded capability returns
        // `SubagentBlocked`.
        match ctx.caps.plan_mode.request_exit(plan).await {
            // The VERBATIM approved / cancel / stale results (exitPlanMode.ts).
            PlanExitOutcome::Approved => Ok(APPROVED_MESSAGE.to_string()),
            PlanExitOutcome::Cancel => Ok(CANCEL_MESSAGE.to_string()),
            PlanExitOutcome::Stale => Ok(STALE_MESSAGE.to_string()),
            // The VERBATIM outside-plan guidance (exitPlanMode.ts
            // `outsidePlanGuidanceMessage`), the current mode interpolated as qwen
            // does. Returned as a guidance error (qwen's `errorResult`), not a deny.
            PlanExitOutcome::NotInPlanMode { current_mode } => {
                Err(outside_plan_guidance(&current_mode))
            }
            // The VERBATIM subagent block result (subagent-plan-tool-policy.ts).
            PlanExitOutcome::SubagentBlocked => Ok(
                crate::tools::plan_lifecycle::subagent_block_message("exit_plan_mode"),
            ),
        }
    }
}

/// The VERBATIM qwen outside-plan guidance (exitPlanMode.ts
/// `outsidePlanGuidanceMessage`): the current mode's wire string interpolated
/// exactly as qwen's `${currentMode}`.
fn outside_plan_guidance(current_mode: &str) -> String {
    format!(
        "You are not in plan mode (current mode: {current_mode}). \
The user may have manually switched modes via Shift+Tab or /approval-mode. \
Do not call exit_plan_mode again. Continue working in the current mode."
    )
}

#[cfg(test)]
#[path = "../../tests/tools/exit_plan_mode.rs"]
mod tests;
