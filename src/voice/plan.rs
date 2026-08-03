//! The two plan-mode Voice reminders (ADR-0067), VERBATIM from qwen's
//! `core/prompts.ts` (`getPlanModeSystemReminder` ~1112,
//! `getManualPlanExitSystemReminder` ~1172). Suspenders authors no plan-mode
//! wording of its own - these are the exact strings qwen feeds a small model, so
//! it reads the same standing read-only instruction qwen gives it.
//!
//! qwen interpolates its `ToolNames.*` constants into the reminder; suspenders'
//! tool names match qwen's verbatim (`read_file`, `grep_search`, `glob`,
//! `ask_user_question`, `exit_plan_mode`), so the interpolated text is identical.
//! This phase only ships the strings and has `enter_plan_mode` return the first
//! as its result; the per-Pass injection into `shape_request` and the manual-exit
//! one-shot injection are Phase 4 (ADR-0067).

/// The plan-mode system reminder (qwen `getPlanModeSystemReminder`, prompts.ts
/// ~1112), the `planOnly=false` interactive variant - suspenders has no
/// SDK/planOnly mode, so the branch is hardcoded to the "call the exit_plan_mode
/// tool" wording. Returned VERBATIM by `enter_plan_mode` as its result (qwen's
/// `llmContent`); Phase 4 re-injects it into every request while in plan mode.
pub fn plan_mode_reminder() -> &'static str {
    "<system-reminder>\n\
Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run tools classified as state-modifying (including changing configs or making commits), or otherwise make changes to the system. A shell command whose safety cannot be determined may run only after the user explicitly approves that exact invocation once, and only when it is necessary for the investigation. This supersedes any other instructions you have received (for example, to make edits).\n\
\n\
## Iterative Planning Workflow\n\
\n\
You are pair-planning with the user. Explore the code to build context, ask the user questions when you hit decisions you cannot make alone, and refine your plan incrementally.\n\
\n\
### The Loop\n\
\n\
Repeat this cycle until the plan is complete:\n\
\n\
1. **Explore** — Use read-only tools (read_file, grep_search, glob) to read code. Look for existing functions, utilities, and patterns to reuse. For broader or ambiguous tasks, use multiple parallel exploration passes (directly or via agents when appropriate) to understand different parts of the codebase.\n\
2. **Capture findings** — After each discovery, immediately integrate what you learned into your evolving mental model. Do not wait until the end to synthesize.\n\
3. **Ask the user** — When you hit an ambiguity or decision you cannot resolve from code alone, use ask_user_question. Then go back to step 1.\n\
\n\
### First Turn\n\
\n\
Start by quickly scanning a few key files to form an initial understanding of the task scope. Then ask the user your first round of questions if any exist. Do not explore exhaustively before engaging the user.\n\
\n\
### Asking Good Questions\n\
\n\
- Never ask what you could find out by reading the code\n\
- Batch related questions together (use multi-question ask_user_question calls)\n\
- Focus on things only the user can answer: requirements, preferences, tradeoffs, edge case priorities\n\
- Scale depth to the task — a vague feature request needs many rounds; a focused bug fix may need one or none\n\
\n\
### Planning Principles\n\
\n\
- Build a global understanding of how the relevant pieces fit together before deciding on local edits. Do not jump from the first relevant file straight into a plan when the task likely spans multiple files or behaviors.\n\
- Design an implementation approach that fits the existing codebase rather than inventing a parallel pattern.\n\
- Reference existing functions and utilities you found that should be reused, with their file paths.\n\
- Include a verification section describing how to test the changes end-to-end.\n\
\n\
### When a Tool is Blocked by Plan Mode\n\
\n\
If a non-read-only tool is blocked:\n\
- Do NOT retry the blocked tool or repeatedly attempt similar non-read-only tools\n\
- Do NOT use wrappers, quoting tricks, aliases, or obfuscation to make a blocked write look unknown\n\
- Do NOT immediately call exit_plan_mode just to unblock it — continue gathering context with read-only tools first\n\
- Pivot to read-only tools (read_file, grep_search, glob, list_directory, agents) to gather the information the blocked tool would have provided\n\
- Once you have enough context to form a complete plan, call exit_plan_mode\n\
\n\
An exact one-off approval for an unknown shell command approves only that invocation. It does not approve the plan, authorize related commands, or exit Plan mode.\n\
\n\
### When to Converge\n\
\n\
Your plan is ready when you have addressed all ambiguities and it covers: what to change, which files to modify, what existing code to reuse (with file paths), and how to verify the changes. Present your plan by calling the exit_plan_mode tool, which will prompt the user to confirm the plan. Do NOT make any file changes or run any tools that modify the system state in any way until the user has confirmed the plan.\n\
</system-reminder>"
}

/// The one-shot manual-plan-exit reminder (qwen `getManualPlanExitSystemReminder`,
/// prompts.ts ~1172): injected on the first model-bound Pass after Plan mode is
/// left OUTSIDE the approved `exit_plan_mode` flow (a Shift+Tab cycle). VERBATIM,
/// with the current mode's wire string interpolated exactly as qwen's
/// `${currentMode}`. This phase only ships the string; the one-shot injection is
/// Phase 4 (ADR-0067).
pub fn manual_plan_exit_reminder(current_mode: &str) -> String {
    format!(
        "<system-reminder>\n\
The approval mode changed outside the approved exit_plan_mode flow.\n\
The current approval mode is: {current_mode}.\n\
Plan mode is no longer active. This notice supersedes any earlier reminder that Plan mode is active. Do not call exit_plan_mode; no plan approval is pending. Continue under the current mode's permissions and confirmation requirements.\n\
</system-reminder>"
    )
}

#[cfg(test)]
#[path = "../../tests/voice/plan.rs"]
mod tests;
