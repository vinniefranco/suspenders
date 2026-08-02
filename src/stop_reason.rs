//! A Run's terminal stop reason - shared run-lifecycle vocabulary.
//!
//! This is a leaf: the settlement value every layer speaks (the loop mints it,
//! Settlement writes it, the Session Log codes it, Voice reads it to pick a
//! marker). Living at the crate root - not under `session::log` - keeps it a
//! zero-dependency vocabulary type, so a reader like [`crate::voice`] names the
//! reason without importing the Session Log (which would close a dependency
//! cycle back through `session`).

/// A Run's terminal stop reason as it enters the Session Log and the
/// settlement event. Spans the LLM-reported reasons that ride through a
/// completed Run (`end_turn`, `max_tokens`, ...) and the Run-Limit reasons
/// the loop mints (`turn_limit` at the max-turns bound, `turn_limit_stuck`
/// when the loop-detector trips). `Error`/`Unknown` are the synthetic reasons
/// Settlement writes for failed/cancelled Runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    /// The Run ran out of Passes productively (baud `:turn_limit`).
    RunLimit,
    /// The Run ran out of Passes while stuck in a failure loop.
    RunLimitStuck,
    /// A failed Run's synthetic reason (baud `:error`).
    Error,
    /// A cancelled Run's synthetic reason (baud `:unknown`).
    Unknown,
}

impl StopReason {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::RunLimit => "turn_limit",
            StopReason::RunLimitStuck => "turn_limit_stuck",
            StopReason::Error => "error",
            StopReason::Unknown => "unknown",
        }
    }

    // Unknown strings degrade to `Unknown` rather than minting reasons from
    // disk (baud: `String.to_existing_atom` guarded by a known set).
    pub(crate) fn from_str(s: &str) -> StopReason {
        match s {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "turn_limit" => StopReason::RunLimit,
            "turn_limit_stuck" => StopReason::RunLimitStuck,
            "error" => StopReason::Error,
            _ => StopReason::Unknown,
        }
    }
}
