//! The Endgame Governor: the mechanical schedule by which a Run ends at its
//! Run Limit (CONTEXT.md: Endgame, Governor; ADR-0015, ADR-0016, ADR-0026).
//!
//! * **Trigger**: the Pass position against the Run Limit - Passes remaining -
//!   plus the Ledger's unverified-writes fact (a principled cross-read:
//!   verification state is a Ledger fact, never a sibling Governor's state).
//! * **Interventions**: uniquely, this Governor speaks at all three moments of
//!   a Pass - it narrows the offered Tools at the request-shaping moment
//!   ([`narrowed_tools`]), rides the results tail at the answering moment
//!   ([`tail_rider`]), and closes the Run on the run-limit marker at the
//!   finish settlement ([`final_pass`], [`tool_insistent_text`],
//!   [`limit_stop_reason`]).
//! * **Setpoints**: the recovery pair ([`RecoverySetpoints`]) - `recovery_limit`
//!   (at most N Recovery Runs per user request, `0` disables the mechanic) and
//!   `recovery_shape` (Handoff or Continuation). The schedule itself carries
//!   none: its offsets (2, 1, 0 Passes remaining) ARE the mechanics, and the
//!   Run Limit is a Session fact read from the Ledger's Pass position.
//!
//! The Recovery Run (CONTEXT.md): when this Governor closes a Run at its
//! Run Limit and the Ledger says the work is demonstrably unfinished
//! (unverified writes, or a Dangling Failure - a command string whose most
//! recent run this Run failed), it issues the
//! close-and-open-a-Recovery-Run Intervention instead of the plain close
//! ([`recovery`]) - evidence: 12 of 15 hard f5 runs died AT the cap, several
//! one honest debugging run from green (LOG.md cycles 005-006). The Agent
//! executes the Intervention; this Governor only judges.
//!
//! The endgame is mechanical because small models comply with mechanics, not
//! requests (the lesson learned at every scale: the Explore Nudge's
//! classifier, the Scout's forced report Pass, the tool-less final Pass). Its
//! schedule, counted in Passes remaining before the Run Limit:
//!
//!   * **2 remaining** - the tail rider warns: the one-shot wrap-up warning,
//!     or the Verification Pass prompt in its place when writes are unverified
//!     (the prompt subsumes the warning: verify now, the final Pass concludes).
//!   * **1 remaining** - the Verification Pass (ADR-0016) when writes are
//!     unverified: the request offers run_command ONLY, so a capped Run cannot
//!     end unverified for lack of opportunity. The tail rider is the final-Pass
//!     prompt either way: tools are about to be withdrawn.
//!   * **0 remaining (the final Pass)** - no tools offered (ADR-0015): the only
//!     move left is the conclusion. A reply that still insists on tools - as
//!     real tool_use blocks or as serialized markup in plain text - closes on
//!     the run-limit marker instead of passing as a conclusion.
//!
//! Every query is a pure function over the Pass position and the Run
//! Ledger's facts (ADR-0026: Governors read the Ledger and judge); the
//! arbiter in [`super`] owns when to ask, the firing sites in
//! `crate::run::loop_` apply the answers, and `crate::voice` owns the
//! wording the answers carry.

use crate::content::ContentBlock;
use crate::session::RecoveryShape;
use crate::session::log::StopReason;
use crate::tool::ToolSpec;
use crate::run::governor::failure;
use crate::run::governor::ledger::Ledger;
use crate::voice;

/// The Endgame Governor's recovery Setpoints (CONTEXT.md: Setpoint -
/// resolved by the Session once at launch and fed to the Governor that owns
/// them). Defaults mirror the shipped config: one Recovery Run per user
/// request, Handoff-shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySetpoints {
    /// At most this many Recovery Runs per user request; `0` disables the
    /// mechanic entirely.
    pub limit: u64,
    /// Which arm a Recovery Run takes.
    pub shape: RecoveryShape,
}

impl Default for RecoverySetpoints {
    fn default() -> Self {
        RecoverySetpoints {
            limit: 1,
            shape: RecoveryShape::Handoff,
        }
    }
}

/// The close-and-recover directive: the payload of
/// [`super::FinishIntervention::CloseRecover`], carried out of the Run to the
/// Agent (which executes the Intervention - opening the next Run, or seeding
/// the fresh Conversation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    /// The arm to take, from the shape Setpoint.
    pub shape: RecoveryShape,
    /// Why the work is unfinished: `true` when a verification is failing (a
    /// Dangling Failure - a command string whose most recent run this Run
    /// failed), `false` when writes went unverified - the fact the Voice's
    /// recovery prompt is parameterized with.
    pub verification_failing: bool,
    /// The Dangling Failure's command string, so the Handoff seed can carry
    /// that command's OWN failing result verbatim - the command the recovery
    /// prompt names, never merely the last command run (ADR-0028 addendum
    /// 2026-07-14). `None` on an unverified-writes-only recovery (nothing
    /// dangles), which keeps the pre-existing last-command seed.
    pub failing_command: Option<String>,
}

/// The recovery judgment, consulted only when this Governor is already
/// closing the Run at its Run Limit: `Some` when the Ledger says the work
/// is demonstrably unfinished (unverified writes, or a Dangling Failure) and
/// the request's recovery budget is not spent. The failing arm is
/// dangling-failure-based, not last-command-only - a red full-suite run
/// followed by a green filtered rerun (observed live) must not read as
/// green. The dangling-failure arm additionally requires that a write landed
/// this Run: a failing command during pure exploration is not unfinished
/// implementation, so a read-only Run draws no recovery (ADR-0028 addendum
/// 2026-07-14). A capped Run that settled green gets no recovery; `limit` 0
/// disables the mechanic.
pub fn recovery(setpoints: &RecoverySetpoints, ledger: &Ledger) -> Option<Recovery> {
    let unfinished =
        ledger.unverified_writes() || (ledger.dangling_failure() && ledger.wrote_this_run());
    if unfinished && ledger.recoveries_used() < setpoints.limit {
        Some(Recovery {
            shape: setpoints.shape,
            verification_failing: ledger.dangling_failure(),
            failing_command: ledger.dangling_command().map(str::to_string),
        })
    } else {
        None
    }
}

/// The Voice text riding a tool-results tail, tagged with the Transcript event
/// announcing it (baud's `tail_rider` type). `None` when no rider is due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailRider {
    VerificationPass(String),
    WrapUpWarning(String),
    FinalPass(String),
    None,
}

/// The Endgame's narrowing of the offered Tools, answered directly: `Some` of
/// no specs on the final Pass (ADR-0015), `Some` of run_command only on the
/// Verification Pass (ADR-0016 - one Pass before the limit with unverified
/// writes), and `None` outside the schedule - no narrowing, the full registry
/// rides (which is the firing site's default, never built here).
///
/// The unverified state cannot drift between the Verification Pass prompt (the
/// previous Pass's tail) and this request - no tool runs in between. A path
/// that never carried the prompt (a verify-nudged finish) still narrows here;
/// the nudge's own wording is the prompt in that path.
pub fn narrowed_tools(pass: u64, run_limit: u64, ledger: &Ledger) -> Option<Vec<ToolSpec>> {
    if final_pass(pass, run_limit) {
        Some(Vec::new())
    } else if verification_pass(pass, run_limit, ledger) {
        Some(crate::tools::verification_specs())
    } else {
        None
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
pub fn tail_rider(pass: u64, run_limit: u64, ledger: &Ledger) -> TailRider {
    let remaining = run_limit.saturating_sub(pass);
    if remaining == 2 && ledger.unverified_writes() {
        TailRider::VerificationPass(voice::verification_pass_prompt().to_string())
    } else if remaining == 2 {
        TailRider::WrapUpWarning(voice::wrap_up_warning(2))
    } else if remaining == 1 {
        TailRider::FinalPass(voice::final_pass_prompt().to_string())
    } else {
        TailRider::None
    }
}

/// Is this the Run's last permitted Pass (or past it)?
pub fn final_pass(pass: u64, run_limit: u64) -> bool {
    pass >= run_limit
}

/// May a finish Nudge send the model back for one more Pass? False at the Run
/// Limit - the limit bounds every Nudge (CONTEXT.md).
pub fn can_loop(pass: u64, run_limit: u64) -> bool {
    pass < run_limit
}

/// The stop reason for a Run closing at its limit: `RunLimitStuck` when the
/// Run has been stuck in a recent failure loop ([`failure::stuck`] - the
/// failure Governor's one exported predicate over the Ledger's failure
/// tallies; one set of setpoints, two readers - ADR-0026), `RunLimit`
/// otherwise - so Settlement and the UI can distinguish "ran out of runs
/// productively" from "ran out of runs while stuck."
pub fn limit_stop_reason(ledger: &Ledger) -> StopReason {
    if failure::stuck(ledger) {
        StopReason::RunLimitStuck
    } else {
        StopReason::RunLimit
    }
}

/// Tool insistence as text (seen live TWICE on the forced final Pass): the
/// visible text carries a serialized tool call, in the markup Qwen emits when
/// the request offers no tools to parse against - once leading the response,
/// once after a one-sentence preamble. Detection is line-anchored: a line that
/// IS markup means the model is still trying to work; the markup string
/// appearing inline in prose is still a conclusion. A final-Pass reply carrying
/// such a line closes on the run-limit marker path, and the markup never
/// enters the Conversation (kept, it would prime later Runs to emit more).
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
fn verification_pass(pass: u64, run_limit: u64, ledger: &Ledger) -> bool {
    pass == run_limit - 1 && ledger.unverified_writes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::run::governor::ledger::{CallOutcome, ToolResult};
    use serde_json::json;

    // One successful edit_file leaves the Run with unverified writes.
    fn unverified() -> Ledger {
        let mut ledger = Ledger::new(25);
        ledger.record(
            "edit_file",
            &json!({"path": "a.ex"}),
            &ToolResult {
                content: "edited a.ex",
                is_error: false,
            },
            CallOutcome::Ran,
        );
        ledger
    }

    // ---- narrowed_tools/3 ----

    #[test]
    fn the_final_pass_offers_no_tools() {
        assert_eq!(narrowed_tools(25, 25, &Ledger::new(25)), Some(Vec::new()));
        assert_eq!(narrowed_tools(26, 25, &Ledger::new(25)), Some(Vec::new()));
    }

    #[test]
    fn the_verification_pass_offers_run_command_only() {
        let specs = narrowed_tools(24, 25, &unverified()).expect("narrows");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "run_command");
    }

    // "The full registry rides" is now answered directly as no narrowing
    // (`None`), instead of being asserted equal to a freshly built registry.

    #[test]
    fn one_before_the_limit_with_verified_writes_narrows_nothing() {
        assert_eq!(narrowed_tools(24, 25, &Ledger::new(25)), None);
    }

    #[test]
    fn any_earlier_pass_narrows_nothing_unverified_or_not() {
        assert_eq!(narrowed_tools(1, 25, &unverified()), None);
        assert_eq!(narrowed_tools(23, 25, &unverified()), None);
    }

    // ---- tail_rider/3 ----

    #[test]
    fn two_remaining_the_wrap_up_warning() {
        assert_eq!(
            tail_rider(23, 25, &Ledger::new(25)),
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
            tail_rider(24, 25, &Ledger::new(25)),
            TailRider::FinalPass(voice::final_pass_prompt().to_string())
        );
        assert!(matches!(
            tail_rider(24, 25, &unverified()),
            TailRider::FinalPass(_)
        ));
    }

    #[test]
    fn outside_the_endgame_none() {
        assert_eq!(tail_rider(1, 25, &Ledger::new(25)), TailRider::None);
        assert_eq!(tail_rider(22, 25, &unverified()), TailRider::None);
        assert_eq!(tail_rider(25, 25, &Ledger::new(25)), TailRider::None);
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
    fn distinguishes_a_stuck_run_from_a_productive_one() {
        assert_eq!(limit_stop_reason(&Ledger::new(25)), StopReason::RunLimit);

        let mut stuck = Ledger::new(25);
        for _ in 0..3 {
            stuck.record(
                "grep",
                &json!({}),
                &ToolResult {
                    content: "boom",
                    is_error: true,
                },
                CallOutcome::Ran,
            );
        }

        assert_eq!(limit_stop_reason(&stuck), StopReason::RunLimitStuck);
    }

    // ---- recovery/2 ----

    // The model wrote, then its verification dangles red: a successful edit
    // followed by a failing run_command. The write is the evidence the
    // dangling-failure arm requires (ADR-0028 addendum 2026-07-14).
    fn command_failing() -> Ledger {
        let mut ledger = Ledger::new(25);
        ledger.record(
            "edit_file",
            &json!({"path": "a.ex"}),
            &ToolResult {
                content: "edited a.ex",
                is_error: false,
            },
            CallOutcome::Ran,
        );
        ledger.record(
            "run_command",
            &json!({"command": "cargo test"}),
            &ToolResult {
                content: "exit 1",
                is_error: true,
            },
            CallOutcome::Ran,
        );
        ledger
    }

    #[test]
    fn unverified_writes_draw_a_recovery() {
        assert_eq!(
            recovery(&RecoverySetpoints::default(), &unverified()),
            Some(Recovery {
                shape: RecoveryShape::Handoff,
                verification_failing: false,
                failing_command: None,
            })
        );
    }

    #[test]
    fn a_failing_verification_draws_a_recovery_naming_the_failure() {
        // The model wrote, then its verification dangles red (see
        // `command_failing`): the dangling-failure arm fires, naming the
        // failing command so the Handoff seed can carry its own result.
        assert_eq!(
            recovery(&RecoverySetpoints::default(), &command_failing()),
            Some(Recovery {
                shape: RecoveryShape::Handoff,
                verification_failing: true,
                failing_command: Some("cargo test".to_string()),
            })
        );
    }

    #[test]
    fn a_green_filtered_rerun_does_not_launder_the_failure() {
        // Observed live: a red full-suite run, then `cargo test one_test`
        // green. The full suite's failure dangles, so the judgment still
        // fires, naming the failure.
        let mut ledger = command_failing();
        ledger.record(
            "run_command",
            &json!({"command": "cargo test one_test"}),
            &ToolResult {
                content: "ok",
                is_error: false,
            },
            CallOutcome::Ran,
        );

        assert_eq!(
            recovery(&RecoverySetpoints::default(), &ledger),
            Some(Recovery {
                shape: RecoveryShape::Handoff,
                verification_failing: true,
                failing_command: Some("cargo test".to_string()),
            })
        );
    }

    #[test]
    fn a_dangling_failure_with_no_writes_draws_no_recovery() {
        // The exact bug (session 20260714-174034): a read-only task ran a
        // failing command (a `head`-truncated pipe reporting a spurious 101),
        // never wrote, and settled green at the cap. With zero writes the
        // dangling-failure arm must NOT fire - that is exploration, not
        // unfinished implementation.
        let mut ledger = Ledger::new(25);
        ledger.record(
            "run_command",
            &json!({"command": "cargo test --lib 2>&1 | head -200"}),
            &ToolResult {
                content: "exit 101",
                is_error: true,
            },
            CallOutcome::Ran,
        );
        assert!(ledger.dangling_failure());
        assert!(!ledger.wrote_this_run());
        assert_eq!(recovery(&RecoverySetpoints::default(), &ledger), None);
    }

    #[test]
    fn a_clean_cap_gets_no_recovery() {
        assert_eq!(
            recovery(&RecoverySetpoints::default(), &Ledger::new(25)),
            None
        );
    }

    #[test]
    fn the_shape_setpoint_picks_the_arm() {
        let setpoints = RecoverySetpoints {
            limit: 1,
            shape: RecoveryShape::Continuation,
        };
        assert_eq!(
            recovery(&setpoints, &unverified()).map(|r| r.shape),
            Some(RecoveryShape::Continuation)
        );
    }

    #[test]
    fn the_limit_bounds_recoveries_per_user_request() {
        let mut spent = unverified();
        spent.note_recoveries_used(1);
        assert_eq!(recovery(&RecoverySetpoints::default(), &spent), None);

        let mut room = unverified();
        room.note_recoveries_used(1);
        let setpoints = RecoverySetpoints {
            limit: 2,
            shape: RecoveryShape::Handoff,
        };
        assert!(recovery(&setpoints, &room).is_some());
    }

    #[test]
    fn limit_zero_disables_the_mechanic() {
        let setpoints = RecoverySetpoints {
            limit: 0,
            shape: RecoveryShape::Handoff,
        };
        assert_eq!(recovery(&setpoints, &unverified()), None);
        assert_eq!(recovery(&setpoints, &command_failing()), None);
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
