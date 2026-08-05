//! The Markers (CONTEXT.md: Voice): every bracketed `[...]` string Suspenders
//! authors into the Conversation to close a Run or answer a Tool Call it could
//! not run normally. A small model reads the brackets as "this is the harness
//! talking", not tool output.
//!
//! The fixed-wording markers are one first-class enum, [`Marker`], whose single
//! [`Marker::text`] match is the authoritative wording table: adding a close
//! reason is a variant, and every call site names the reason rather than
//! re-spelling the string. The few markers that interpolate a value stay
//! functions (their wording is parameterized, not fixed).

use crate::stop_reason::StopReason;

/// A fixed-wording Voice marker: one bracketed `[...]` string Suspenders writes
/// into the Conversation to close a Run, or to answer a Tool Call it could not
/// run normally. The variants ARE the reasons; [`text`](Marker::text) is the one
/// authoritative wording table.
///
/// Wording is the highest-leverage tuning surface for small local models
/// (CONTEXT.md), so it lives in exactly one match here - swapping a marker's
/// wording model-by-model touches one arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// Closes a Run that hit its Run Limit.
    RunLimit,
    /// Closes a Run the loop-detector terminated: the model was stuck emitting
    /// the identical Tool Call batch Pass after Pass (a passive circuit breaker -
    /// no steering text is injected, only this close marker).
    LoopStall,
    /// Closes a Run an after-Pass hook stopped.
    RunStopped,
    /// Closes a Run when max_tokens truncation left no usable content.
    Truncation,
    /// Closes a Run whose response carried zero content blocks.
    EmptyResponse,
    /// Closes a cancelled Run (keeps roles alternating).
    RunCancelled,
    /// Closes a failed or crashed Run.
    RunFailed,
    /// Prefixed to an error Tool Result's content on the openai-completions wire
    /// (ADR-0037): that dialect's `role:"tool"` message has no error slot, so the
    /// failure fact rides in-band. The anthropic-messages wire keeps its native
    /// `is_error` field and never carries this marker.
    ToolError,
    /// Tool Result answering a Tool Call left unanswered when the Conversation
    /// crossed Providers (ADR-0037: the ADR-0004/0009 orphan machinery, relocated
    /// to the transform pass). An error answer, like ADR-0009's: the model should
    /// re-issue the call if it still needs the result.
    OrphanedCall,
    /// Tool Result answering a Tool Call from a max_tokens-truncated response
    /// (ADR-0009).
    TruncatedCallReissue,
    /// Tool Result for a run_shell_command Tool Call the user denied.
    CommandDenied,
}

impl Marker {
    /// This marker's authoritative wording - the one place every fixed marker
    /// string lives.
    pub fn text(self) -> &'static str {
        match self {
            Marker::RunLimit => "[turn limit reached - reply to continue]",
            Marker::LoopStall => "[turn ended - the model was repeating itself; reply to continue]",
            Marker::RunStopped => "[turn stopped - reply to continue]",
            Marker::Truncation => "[response truncated by max_tokens]",
            Marker::EmptyResponse => "[empty response]",
            Marker::RunCancelled => "[turn cancelled by user]",
            Marker::RunFailed => "[turn failed]",
            Marker::ToolError => "[tool error]",
            Marker::OrphanedCall => {
                "[this call's result was lost in a model switch - re-issue the call if you still need it]"
            }
            Marker::TruncatedCallReissue => {
                "[response was cut by max_tokens - the call may be incomplete, re-issue it]"
            }
            Marker::CommandDenied => "[command denied by user]",
        }
    }

    /// The marker for a completed Run that must still close a trailing user-role
    /// message: the Run Limit and loop-stall stops name themselves; every other
    /// completion closes as an after-Pass stop. The one authoritative
    /// stop-reason-to-close-marker mapping (was rewritten at each Run-close site).
    pub fn completing(stop_reason: &StopReason) -> Marker {
        match stop_reason {
            StopReason::RunLimit => Marker::RunLimit,
            StopReason::RunLimitStuck => Marker::LoopStall,
            _ => Marker::RunStopped,
        }
    }

    /// Whether `text` is exactly one of the run-close markers the Loop appends as
    /// a marker-only assistant message (ADR-0061): the Run Limit, loop-stall,
    /// stopped, failed, cancelled, truncation, and empty-response closes. Used so
    /// a parent Run never mistakes a subagent's self-authored close for its
    /// answer.
    pub fn is_run_close(text: &str) -> bool {
        [
            Marker::RunLimit,
            Marker::LoopStall,
            Marker::RunStopped,
            Marker::RunFailed,
            Marker::RunCancelled,
            Marker::Truncation,
            Marker::EmptyResponse,
        ]
        .into_iter()
        .any(|m| m.text() == text)
    }
}

/// The user-role nudge injected when the next-speaker check decides the model
/// should keep going after a no-tool-call Pass (ADR-0043, qwen-code's
/// "Please continue." continuation). It enters the Conversation as an ordinary
/// user message (unstamped), prompting the model to take the next action it
/// announced but did not execute. Not a Marker: it carries no bracketed framing
/// because it is an ordinary user turn, not Voice output.
pub fn please_continue() -> &'static str {
    "Please continue."
}

/// Tool Result for a Tool Call whose input JSON did not decode.
pub fn malformed_input(raw: &str) -> String {
    format!("[tool input was not valid JSON - resend as valid JSON] {raw}")
}

/// Result Cap marker for head-only shaping: appended after the kept head of an
/// oversized Tool Result.
pub fn truncated_output(total: usize, kept: usize) -> String {
    format!("\n[truncated: output is {total} chars, showing the first {kept}]")
}

/// Result Cap marker for read_file's line-boundary shaping: appended after the
/// kept lines, naming the exact 0-based `offset` that continues the read (qwen's
/// read_file param). `last_shown` is the last file-absolute line shown (1-based);
/// the next line is `last_shown + 1` (1-based), which is offset `last_shown`
/// (0-based), so the model pages on with `offset: last_shown` and a `limit`.
pub fn truncated_file(last_shown: usize, last_line: usize) -> String {
    format!(
        "\n[truncated at line {last_shown} of {last_line} - continue with read_file offset {last_shown} (0-based) and a limit]"
    )
}

/// Result Cap marker for head+tail shaping (run_shell_command): replaces the middle
/// of an oversized Tool Result.
pub fn omitted_middle(omitted: usize, total: usize) -> String {
    format!("\n[{omitted} of {total} chars omitted from the middle of this output]\n")
}
