//! Shared wording for the two plan-lifecycle tools (`enter_plan_mode`,
//! `exit_plan_mode`, ADR-0067): the VERBATIM qwen subagent-block message both
//! return when called inside a subagent. Ported from qwen's
//! `subagent-plan-tool-policy.ts` (`getSubagentPlanToolUnavailableMessage`).
//!
//! In suspenders the block is enforced by the DEGRADED
//! [`crate::tool::caps::SubagentPlanMode`] capability a child Run carries (not a
//! per-tool context check as in qwen): the capability returns the
//! `SubagentBlocked` outcome and the tool words it with this shared string, so
//! the two tools cannot drift apart.

/// The VERBATIM qwen subagent-block message (subagent-plan-tool-policy.ts
/// `getSubagentPlanToolUnavailableMessage`): only the tool name is interpolated,
/// the rest is qwen verbatim. Returned by `enter_plan_mode`/`exit_plan_mode` when
/// the caller is a subagent (the degraded [`crate::tool::caps::SubagentPlanMode`]
/// capability answers `SubagentBlocked`).
pub fn subagent_block_message(tool_name: &str) -> String {
    format!(
        "{tool_name} is not available inside subagents or team agents. Plan mode \
is owned by the caller/main session; return your plan, findings, or constraints \
to the caller in your normal response instead of entering or exiting plan mode."
    )
}
