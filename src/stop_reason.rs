//! A Run's terminal stop reason - THE canonical run-stop vocabulary.
//!
//! This is a leaf: the one type every layer speaks about why a Run stopped
//! (the loop mints it, Settlement writes it, the Session Log codes it, the
//! settlement event carries it to the UI, Voice reads it to pick a marker).
//! Living at the crate root - not under `session::log` - keeps it a
//! zero-dependency vocabulary type, so a reader like [`crate::voice`] names the
//! reason without importing the Session Log (which would close a dependency
//! cycle back through `session`).
//!
//! The wire-level [`crate::llm::response::StopReason`] is NOT this type: it is
//! the LLM boundary's fact about one response, and it converts into this
//! vocabulary at exactly one seam (the `From` impl beside it). Two other
//! seams reshape stop reasons, both named here so the map stays honest:
//! `close_stop_reason` in [`crate::run`]'s finish module normalizes a phantom
//! `tool_use` stop to `end_turn` WIRE-SIDE, before the `From` embedding, and
//! `settle_stop` in [`crate::session::log`]'s fold remaps canonical to
//! canonical at Resume (a non-completed settled Run reads back as `Error`
//! whatever it logged).

/// A Run's terminal stop reason as it enters the Session Log and the
/// settlement event. Spans the LLM-reported reasons that ride through a
/// completed Run (`end_turn`, `max_tokens`, ...), the Run-Limit reasons
/// the loop mints (`turn_limit` at the max-turns bound, `turn_limit_stuck`
/// when the loop-detector trips), and the arbitrary atom a Hook's
/// `continue:false` names ([`StopReason::Custom`], ADR-0066).
/// `Error`/`Unknown` are the synthetic reasons Settlement writes for
/// failed/cancelled Runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    /// Reachable only by parsing a pre-unification Session Log: the live loop
    /// normalizes a phantom `tool_use` stop to [`StopReason::EndTurn`] before
    /// it ever enters this vocabulary (`close_stop_reason`). Kept so the wire
    /// name stays stable and an old log still parses.
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
    /// The atom a Hook's `continue:false` named (ADR-0066): the one
    /// open-vocabulary reason. It rides the Session Log and the settlement
    /// event verbatim - never degraded to [`StopReason::Unknown`].
    Custom(String),
}

impl StopReason {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::RunLimit => "turn_limit",
            StopReason::RunLimitStuck => "turn_limit_stuck",
            StopReason::Error => "error",
            StopReason::Unknown => "unknown",
            StopReason::Custom(atom) => atom,
        }
    }

    // The fixed names decode to their variants (the pre-Custom wire names stay
    // stable, so an old log still parses); any other non-empty string is a
    // Hook's custom atom - the only writer of novel names - and survives
    // verbatim as `Custom` rather than degrading to `Unknown`.
    //
    // Reserved-atom aliasing: a Hook `stopReason` equal to a fixed wire name
    // (e.g. "turn_limit") logs as that name and re-parses as the fixed variant
    // on Resume, not as `Custom` - accepted, the names collide by construction.
    pub(crate) fn from_str(s: &str) -> StopReason {
        match s {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "turn_limit" => StopReason::RunLimit,
            "turn_limit_stuck" => StopReason::RunLimitStuck,
            "error" => StopReason::Error,
            "unknown" | "" => StopReason::Unknown,
            atom => StopReason::Custom(atom.to_string()),
        }
    }
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
#[path = "../tests/stop_reason.rs"]
mod tests;
