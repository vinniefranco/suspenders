//! `enter_plan_mode({userRequested?})`: switches the session into the read-only
//! Plan mode (faithful to qwen enterPlanMode.ts). The model reaches for this only
//! after the user explicitly asks for plan mode or confirms they want it -
//! entering plan mode is a privilege reduction, so it needs no confirmation.
//!
//! The state mutation (the atomic mode-flip + prePlanMode tracking + the YOLO
//! guard + idempotency) lives Agent-side, reached through the
//! [`crate::tool::caps::PlanMode`] capability: state mutation is the single
//! owner's (ADR-0017), and keeping the YOLO guard there makes reading-the-mode and
//! flipping-it one atomic step rather than a tool-side check that could race a
//! concurrent cycle. The tool words the returned [`EnterPlanOutcome`] VERBATIM as
//! qwen's `execute` does.
//!
//! qwen's `execute` also reveals the deferred `exit_plan_mode` tool so the model
//! can call it directly. In suspenders `exit_plan_mode` carries `always_load`
//! (qwen's always-declared flag, #5210), so its schema is already on every
//! request's wire list - a reveal would be a no-op under that flag (qwen's
//! `getFunctionDeclarations` includes an always-loaded tool regardless of reveal),
//! so this tool does no reveal; the always-declared flag is the whole mechanism.
//!
//! `Kind::Think` (qwen enterPlanMode.ts `Kind.Think`) and ALWAYS-VISIBLE (never
//! deferred, qwen `shouldDefer: false`) so an explicit plan-mode request works.

use crate::tool::caps::EnterPlanOutcome;
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

pub struct EnterPlanMode;

/// The tool description, VERBATIM from qwen enterPlanMode.ts
/// (`enterPlanModeToolDescription`).
const DESCRIPTION: &str = "Use this tool only after the user explicitly asks to switch into plan mode or confirms they want plan mode. Entering plan mode is a privilege reduction, so it does not require user confirmation at execution time.

## When to Use This Tool
Use this tool when the user has opted into plan mode for a task that should be read-only while the plan is formed, such as multi-file changes, design choices, or ambiguous requirements.

## When NOT to Use This Tool
Do not use this tool just because a task involves planning, is complex, or requires investigation. In the current mode, you can still think, inspect files, ask clarifying questions, and present a plan without switching modes.

## Important
If plan mode seems helpful but the user has not asked for it, ask first. Do NOT use this tool if the user has explicitly asked you not to use plan mode.";

/// The VERBATIM qwen no-op guidance when a model-initiated entry from YOLO is
/// blocked (enterPlanMode.ts `execute`, the YOLO branch `llmContent`).
const STAYED_YOLO_MESSAGE: &str = "Plan mode was not entered: the session is in YOLO mode, which the user explicitly chose for low-friction execution. Continue investigating and presenting your plan in the current mode without switching. If the user explicitly asked for plan mode in this turn, retry this tool call with userRequested: true.";

#[async_trait::async_trait]
impl Tool for EnterPlanMode {
    // Think (qwen enterPlanMode.ts `Kind.Think`): a plan-lifecycle tool, allowed
    // in plan mode - it changes the mode, it does not mutate the workspace.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Think
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "enter_plan_mode".into(),
            description: DESCRIPTION.into(),
            // Schema VERBATIM from qwen enterPlanMode.ts
            // (`enterPlanModeToolSchemaData.parametersJsonSchema`): a single
            // optional `userRequested` boolean, additionalProperties false.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "userRequested": {
                        "type": "boolean",
                        "description": "Set to true ONLY when the user explicitly asked for plan mode in this turn, or explicitly confirmed they want it. Leave unset (or false) when you are deciding to plan on your own without the user asking. In YOLO mode, an explicit user request will not take effect unless this is true."
                    }
                },
                // qwen enterPlanMode.ts omits `required` (userRequested is
                // optional). suspenders' Anthropic-tool-format invariant expects a
                // `required` array on every spec, so this is an explicit empty one -
                // semantically identical (no required fields), the single forced
                // adaptation over qwen's schema here.
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // `userRequested` defaults to false (qwen's `!this.params.userRequested`):
        // a model-initiated entry unless the model explicitly flags a user request.
        let user_requested = input
            .get("userRequested")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // The Plan-Mode effect (ADR-0067): ask the Agent to enter plan mode. The
        // Agent runs the YOLO guard / idempotency / prePlanMode logic atomically
        // and returns the outcome; a subagent's degraded capability returns
        // `SubagentBlocked`.
        match ctx.caps.plan_mode.enter(user_requested).await {
            EnterPlanOutcome::Entered { reminder } => {
                // qwen's enterPlanMode.ts reveals the deferred `exit_plan_mode`
                // tool here so the model can call it directly. In suspenders
                // `exit_plan_mode` carries `always_load` (qwen's always-declared
                // flag, #5210), so its schema is ALREADY on every request's wire
                // list - `getFunctionDeclarations` includes an `always_load` tool
                // regardless of reveal, so qwen's reveal is a no-op under that flag.
                // No reveal is needed here; the always-declared flag is the whole
                // mechanism. The plan-mode system reminder is qwen's `llmContent`.
                Ok(reminder)
            }
            // The VERBATIM YOLO no-op guidance (enterPlanMode.ts).
            EnterPlanOutcome::StayedYolo => Ok(STAYED_YOLO_MESSAGE.to_string()),
            // The VERBATIM subagent block result (subagent-plan-tool-policy.ts).
            EnterPlanOutcome::SubagentBlocked => Ok(
                crate::tools::plan_lifecycle::subagent_block_message("enter_plan_mode"),
            ),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/tools/enter_plan_mode.rs"]
mod tests;
