//! The Endgame: how a Turn ends at its Turn Limit, as one pure decision
//! module (CONTEXT.md: Turn Limit; ADR-0015, ADR-0016).
//!
//! The endgame is mechanical because small models comply with mechanics, not
//! requests (the lesson learned at every scale: the Explore Nudge's
//! classifier, the Scout's forced report Pass, the tool-less final Pass). Its
//! schedule, counted in Passes remaining before the Turn Limit:
//!
//!   * **2 remaining** - the tail rider warns: the one-shot wrap-up warning,
//!     or the Verification Pass prompt in its place when writes are unverified
//!     (the prompt subsumes the warning: verify now, the final Pass concludes).
//!   * **1 remaining** - the Verification Pass (ADR-0016) when writes are
//!     unverified: the request offers run_command ONLY, so a capped Turn cannot
//!     end unverified for lack of opportunity. The tail rider is the final-Pass
//!     prompt either way: tools are about to be withdrawn.
//!   * **0 remaining (the final Pass)** - no tools offered (ADR-0015): the only
//!     move left is the conclusion. A reply that still insists on tools - as
//!     real tool_use blocks or as serialized markup in plain text - closes on
//!     the turn-limit marker instead of passing as a conclusion.
//!
//! Every query is a pure function over the Pass position and the Nudge state;
//! `crate::turn::loop_` owns when to ask and applies the answers, and
//! `crate::voice` owns the wording the answers carry.

use crate::content::ContentBlock;
use crate::session::log::StopReason;
use crate::tool::ToolSpec;
use crate::turn::nudges::Nudges;
use crate::voice;

/// The Voice text riding a tool-results tail, tagged with the Transcript event
/// announcing it (baud's `tail_rider` type). `None` when no rider is due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailRider {
    VerificationPass(String),
    WrapUpWarning(String),
    FinalPass(String),
    None,
}

/// The Tool specs this Pass's request offers: none on the final Pass
/// (ADR-0015), run_command only on the Verification Pass (ADR-0016 - one Pass
/// before the limit with unverified writes), the full registry otherwise.
///
/// The unverified state cannot drift between the Verification Pass prompt (the
/// previous Pass's tail) and this request - no tool runs in between. A path
/// that never carried the prompt (a verify-nudged finish) still narrows here;
/// the nudge's own wording is the prompt in that path.
pub fn tools(pass: u64, turn_limit: u64, nudges: &Nudges) -> Vec<ToolSpec> {
    if final_pass(pass, turn_limit) {
        Vec::new()
    } else if verification_pass(pass, turn_limit, nudges) {
        crate::tools::verification_specs()
    } else {
        crate::tools::specs()
    }
}

/// The Voice text riding this Pass's tool-results tail, or [`TailRider::None`].
/// One rider per position; each is one-shot by construction (the Pass counter
/// equals its threshold once).
///
/// Without the warning, open-ended tasks burn every Pass exploring and settle
/// `turn_limit` with no answer delivered; the warning alone is ignored
/// (observed live 2/2), so the final-Pass prompt precedes the mechanical
/// ending.
pub fn tail_rider(pass: u64, turn_limit: u64, nudges: &Nudges) -> TailRider {
    let remaining = turn_limit.saturating_sub(pass);
    if remaining == 2 && nudges.unverified_writes() {
        TailRider::VerificationPass(voice::verification_pass_prompt().to_string())
    } else if remaining == 2 {
        TailRider::WrapUpWarning(voice::wrap_up_warning(2))
    } else if remaining == 1 {
        TailRider::FinalPass(voice::final_pass_prompt().to_string())
    } else {
        TailRider::None
    }
}

/// Is this the Turn's last permitted Pass (or past it)?
pub fn final_pass(pass: u64, turn_limit: u64) -> bool {
    pass >= turn_limit
}

/// May a finish Nudge send the model back for one more Pass? False at the Turn
/// Limit - the limit bounds every Nudge (CONTEXT.md).
pub fn can_loop(pass: u64, turn_limit: u64) -> bool {
    pass < turn_limit
}

/// The stop reason for a Turn closing at its limit: `TurnLimitStuck` when the
/// Turn has been stuck in a recent failure loop ([`Nudges::stuck`]),
/// `TurnLimit` otherwise - so Settlement and the UI can distinguish "ran out of
/// turns productively" from "ran out of turns while stuck."
pub fn limit_stop_reason(nudges: &Nudges) -> StopReason {
    if nudges.stuck() {
        StopReason::TurnLimitStuck
    } else {
        StopReason::TurnLimit
    }
}

/// Tool insistence as text (seen live TWICE on the forced final Pass): the
/// visible text carries a serialized tool call, in the markup Qwen emits when
/// the request offers no tools to parse against - once leading the response,
/// once after a one-sentence preamble. Detection is line-anchored: a line that
/// IS markup means the model is still trying to work; the markup string
/// appearing inline in prose is still a conclusion. A final-Pass reply carrying
/// such a line closes on the turn-limit marker path, and the markup never
/// enters the Conversation (kept, it would prime later Turns to emit more).
pub fn tool_insistent_text(blocks: &[ContentBlock]) -> bool {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .flat_map(|text| text.split('\n'))
        .any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("<tool_call") || trimmed.starts_with("<function=")
        })
}

// The Verification Pass (ADR-0016): exactly one Pass before the limit, with
// successful writes and no run_command since.
fn verification_pass(pass: u64, turn_limit: u64, nudges: &Nudges) -> bool {
    pass == turn_limit - 1 && nudges.unverified_writes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::turn::nudges::{Nudges, ToolResult};
    use serde_json::json;

    // One successful edit_file leaves the Turn with unverified writes.
    fn unverified() -> Nudges {
        let mut nudges = Nudges::new();
        nudges.note_result(
            "edit_file",
            &json!({ "path": "a.ex" }),
            &ToolResult {
                content: "edited a.ex",
                is_error: false,
            },
        );
        nudges
    }

    // ---- tools/3 ----

    #[test]
    fn the_final_pass_offers_no_tools() {
        assert_eq!(tools(25, 25, &Nudges::new()), Vec::new());
        assert_eq!(tools(26, 25, &Nudges::new()), Vec::new());
    }

    #[test]
    fn the_verification_pass_offers_run_command_only() {
        let specs = tools(24, 25, &unverified());
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "run_command");
    }

    #[test]
    fn one_before_the_limit_with_verified_writes_offers_the_full_registry() {
        assert_eq!(tools(24, 25, &Nudges::new()), crate::tools::specs());
    }

    #[test]
    fn any_earlier_pass_offers_the_full_registry_unverified_or_not() {
        assert_eq!(tools(1, 25, &unverified()), crate::tools::specs());
        assert_eq!(tools(23, 25, &unverified()), crate::tools::specs());
    }

    // ---- tail_rider/3 ----

    #[test]
    fn two_remaining_the_wrap_up_warning() {
        assert_eq!(
            tail_rider(23, 25, &Nudges::new()),
            TailRider::WrapUpWarning(voice::wrap_up_warning(2))
        );
    }

    #[test]
    fn two_remaining_with_unverified_writes_the_verification_pass_prompt_subsumes_it() {
        assert_eq!(
            tail_rider(23, 25, &unverified()),
            TailRider::VerificationPass(voice::verification_pass_prompt().to_string())
        );
    }

    #[test]
    fn one_remaining_the_final_pass_prompt_unverified_or_not() {
        assert_eq!(
            tail_rider(24, 25, &Nudges::new()),
            TailRider::FinalPass(voice::final_pass_prompt().to_string())
        );
        assert!(matches!(
            tail_rider(24, 25, &unverified()),
            TailRider::FinalPass(_)
        ));
    }

    #[test]
    fn outside_the_endgame_none() {
        assert_eq!(tail_rider(1, 25, &Nudges::new()), TailRider::None);
        assert_eq!(tail_rider(22, 25, &unverified()), TailRider::None);
        assert_eq!(tail_rider(25, 25, &Nudges::new()), TailRider::None);
    }

    // ---- final_pass?/2 and can_loop?/2 ----

    #[test]
    fn the_boundary() {
        assert!(!final_pass(24, 25));
        assert!(final_pass(25, 25));
        assert!(final_pass(26, 25));

        assert!(can_loop(24, 25));
        assert!(!can_loop(25, 25));
    }

    // ---- limit_stop_reason/1 ----

    #[test]
    fn distinguishes_a_stuck_turn_from_a_productive_one() {
        assert_eq!(limit_stop_reason(&Nudges::new()), StopReason::TurnLimit);

        let mut stuck = Nudges::new();
        for _ in 0..3 {
            stuck.note_result(
                "grep",
                &json!({ "pattern": "x" }),
                &ToolResult {
                    content: "boom",
                    is_error: true,
                },
            );
        }

        assert_eq!(limit_stop_reason(&stuck), StopReason::TurnLimitStuck);
    }

    // ---- tool_insistent_text?/1 ----

    #[test]
    fn a_line_that_is_tool_markup_means_the_model_is_still_working() {
        assert!(tool_insistent_text(&[ContentBlock::text(
            "<tool_call>{...}</tool_call>"
        )]));

        assert!(tool_insistent_text(&[ContentBlock::text(
            "I need to update the file:\n<function=edit_file>"
        )]));
    }

    #[test]
    fn markup_inline_in_prose_is_still_a_conclusion() {
        assert!(!tool_insistent_text(&[ContentBlock::text(
            "The response format uses <tool_call> markers internally."
        )]));
    }

    #[test]
    fn plain_conclusions_and_empty_blocks_pass() {
        assert!(!tool_insistent_text(&[ContentBlock::text(
            "All tests pass; done."
        )]));
        assert!(!tool_insistent_text(&[]));
    }
}
