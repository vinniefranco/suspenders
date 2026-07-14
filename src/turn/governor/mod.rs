//! Governor — the per-moment arbiter over the closed Intervention set
//! (CONTEXT.md: Governor, Intervention; ADR-0026).
//!
//! A Governor is a tunable rule that watches the Pass cycle and intervenes to
//! keep the model on course; it acts only through an **Intervention**, one of
//! the eight closed actions: replace a Tool Result, annotate one, stand alone
//! as a user message, ride the results tail, narrow the offered Tools, silence
//! Thinking for a Pass, close the Turn on a marker, or close the Turn and open
//! a Recovery Turn. The set is deliberately closed: a new Governor is routine,
//! a new KIND of Intervention is a visible design decision (ADR-0026 — do not
//! generalize these enums away; the eighth variant, the Endgame Governor's
//! close-and-recover, deliberately touched the firing sites).
//!
//! Every Intervention belongs to exactly one of the three moments of a Pass,
//! and precedence is decided within a moment, never across moments
//! (CONTEXT.md). The moments are three distinct types, so an Intervention
//! cannot fire at the wrong moment by construction:
//!
//!   * **shaping the request** — [`RequestIntervention`], consulted once per
//!     Pass by [`shape_request`] as the request is built;
//!   * **answering a Tool Call** — [`AnswerIntervention`]. One moment, three
//!     consultation points: [`answer_sent`] before a call executes (Governors
//!     judge what the model SENT — CONTEXT.md), [`answer_read`] after it
//!     executes (what the model will READ), and [`answer_tail`] once per batch
//!     for the results tail — the same user message, so still this moment;
//!   * **settling a finish** — [`FinishIntervention`]. One moment, two
//!     consultation points, because a Turn finishes two ways:
//!     [`settle_capped`] after a tool-answering Pass at the Turn Limit, and
//!     [`settle_finish`] when the model stops calling tools.
//!
//! Facts live in the Turn [`Ledger`](ledger) ("The Ledger holds facts, never
//! opinions or setpoints" — CONTEXT.md), written once by the loop at the
//! firing sites and READ here. Each Governor lives in its own child module
//! (ADR-0022: modules mirror the domain tree) — [`duplicate`], [`failure`],
//! [`explore`], [`verify`], [`empty`], [`anchor`], [`endgame`] — owning its
//! private trigger state, its opinion predicates, and its Setpoints
//! (declared with defaults; the Session's resolved knobs feed the Governors
//! that carry them). The loop threads their state as one [`Governors`]
//! value, and no Governor reads a sibling's state — cross-cutting needs go
//! through the Ledger (CONTEXT.md). What lives HERE is precedence: each
//! entry point is the one readable function where its moment's order is
//! decided. The firing sites (`loop_`, `batch`, `finish`) keep every effect —
//! building requests, executing tools, appending messages — and decide
//! nothing heuristic inline.

pub mod anchor;
pub mod duplicate;
pub mod empty;
pub mod endgame;
pub mod explore;
pub mod failure;
pub mod ledger;
pub mod verify;

use serde_json::Value;

use crate::content::ContentBlock;
use crate::event::VoicedTag;
use crate::llm::response::StopReason;
use crate::session::log;
use crate::tool::ToolSpec;
use crate::turn::governor::endgame::TailRider;
use crate::turn::governor::ledger::{Ledger, ToolResult};
use crate::voice;

/// Interventions at the request-shaping moment.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestIntervention {
    /// Narrow the offered Tools: the Endgame schedule withdraws them all on
    /// the final Pass (ADR-0015) or narrows to run_command on the
    /// Verification Pass (ADR-0016).
    NarrowTools(Vec<ToolSpec>),
    /// Silence Thinking for this Pass: the break-glass no-think rescue after
    /// an Empty-response Nudge.
    SilenceThinking,
}

/// What rides the results tail — the payload of
/// [`AnswerIntervention::RideTail`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rider {
    /// Suspenders-voiced text, announced with its Transcript tag: the Explore
    /// Nudge, or the Endgame's rider (wrap-up warning / Verification Pass
    /// prompt / final-Pass prompt).
    Voiced { tag: VoicedTag, text: String },
    /// The Anchor (CONTEXT.md): its placement rides the same seam
    /// ([`Conversation::inject_anchor`](crate::conversation::Conversation::inject_anchor)
    /// merges into the trailing user message), but its content is the Plan's —
    /// the model's voice, never authored here — so the Plan text never rides
    /// with the Intervention. The one exception is `stale_line`: the anchor
    /// Governor's Voiced stale-plan line, appended BELOW the Anchor when the
    /// Plan has sat unchanged past its Setpoint while writes landed
    /// (PROPOSALS.md #4); it rides every Anchor placed while the condition
    /// holds.
    Anchor { stale_line: Option<String> },
}

/// Interventions at the Tool Call answering moment.
#[derive(Debug, Clone, PartialEq)]
pub enum AnswerIntervention {
    /// Replace the Tool Result: the call never executes and the model reads
    /// this content instead (identical Tool Call repeated). It reads as an
    /// error so the model re-plans rather than trusting a stale echo.
    ReplaceResult { content: String, is_error: bool },
    /// Annotate the Tool Result: the model reads this content — the real
    /// result carrying the consecutive-failure step-back suffix.
    AnnotateResult(String),
    /// Ride the results tail: merge into the trailing tool-results user
    /// message, where a small model actually attends.
    RideTail(Rider),
}

/// The Governors' private trigger state and resolved Setpoints, threaded
/// through the Turn loop as one value beside the [`Ledger`] — one field per
/// Governor that carries state or a Session-fed Setpoint ([`failure`] and
/// [`endgame`] carry neither: they are pure reads over the Ledger), each
/// field's internals opaque outside its own module, so no Governor reads a
/// sibling's state (CONTEXT.md: Turn Ledger). The arbiter's entry points
/// below consult the fields; the loop's firing sites feed the Pass-cycle
/// bookkeeping through the methods here.
#[derive(Debug, Clone)]
pub struct Governors {
    anchor: anchor::Anchor,
    duplicate: duplicate::Duplicate,
    empty: empty::Empty,
    endgame: endgame::RecoverySetpoints,
    explore: explore::Explore,
    verify: verify::Verify,
}

impl Governors {
    /// Resolves the Governors for one Turn: the unexposed Setpoints are their
    /// defaults, and the Session's resolved knobs feed the Governors that
    /// carry them (`anchor_interval` and `plan_stale_after` feed the anchor
    /// Governor, `no_think_rescue` the empty Governor, and
    /// [`with_recovery`](Governors::with_recovery) the endgame Governor —
    /// CONTEXT.md: "the Session resolves them once at launch").
    pub fn new(anchor_interval: u64, plan_stale_after: u64, no_think_rescue: bool) -> Self {
        Governors {
            anchor: anchor::Anchor::new(anchor::Setpoints {
                interval: anchor_interval,
                plan_stale_after,
            }),
            duplicate: duplicate::Duplicate::new(),
            empty: empty::Empty::new(empty::Setpoints { no_think_rescue }),
            endgame: endgame::RecoverySetpoints::default(),
            explore: explore::Explore::new(),
            verify: verify::Verify::new(),
        }
    }

    /// Feeds the endgame Governor its Session-resolved recovery Setpoints
    /// (the defaults otherwise mirror the shipped config).
    pub fn with_recovery(mut self, recovery: endgame::RecoverySetpoints) -> Self {
        self.endgame = recovery;
        self
    }

    /// A Tool Call batch closed: the duplicate Governor's freshness memory
    /// advances (this response's still-fresh calls become the next Pass's
    /// duplicate memory).
    pub fn next_pass(&mut self) {
        self.duplicate.next_pass();
    }

    /// A Pass's Tool Calls, recorded for the finish Nudges' re-arming rule:
    /// a Pass that made at least one Tool Call is progress.
    pub fn note_progress(&mut self, calls: &[(String, Value)]) {
        self.verify.note_progress(calls);
        self.empty.note_progress(calls);
    }
}

/// Interventions at the finish-settlement moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishIntervention {
    /// Close the Turn on the turn-limit marker with this stop reason.
    Close(log::StopReason),
    /// Close the Turn AND open a Recovery Turn (CONTEXT.md: the eighth
    /// Intervention): issued by the Endgame Governor when a Turn caps with
    /// demonstrably unfinished work and the request's recovery budget is not
    /// spent. The Agent executes the opening.
    CloseRecover {
        reason: log::StopReason,
        recovery: endgame::Recovery,
        /// Whether the model's reply enters the Conversation before the
        /// close. True on the final-Pass text settle — the reply is the
        /// model's genuine wrap-up (the tools were withdrawn, ADR-0015), so
        /// Handoff compaction-seeding and Continuation both read it. False
        /// at the tool-answering cap and on the tool-insistent close, where
        /// the turn-limit marker stands in for the reply.
        keep_reply: bool,
    },
    /// Stand alone as a user message — a finish Nudge; the model gets one
    /// more Pass to act on it.
    Standalone { tag: VoicedTag, text: String },
}

/// The request-shaping moment, consulted once per Pass as the request is
/// built. Absence of an Intervention means the full Tool registry rides and
/// Thinking stays on.
///
/// The two Interventions shape disjoint parts of the request (which Tools
/// ride, whether Thinking rides), so they never contend; the narrowing order
/// itself (final Pass over Verification Pass) is the Endgame schedule in
/// [`endgame::narrowed_tools`].
///
/// Borrows the empty Governor's rescue state mutably: consulting this moment
/// consumes the one-Pass rescue arm, so the Pass after a rescued one reverts
/// — unless the rescue has gone sticky (second empty of the Turn).
pub fn shape_request(ledger: &Ledger, governors: &mut Governors) -> Vec<RequestIntervention> {
    let mut interventions = Vec::new();

    // The Endgame Governor's narrowing (ADR-0015/0016), answered directly:
    // outside the schedule it is `None` and no Intervention issues — the
    // firing site's full registry rides.
    if let Some(tools) = endgame::narrowed_tools(ledger.pass(), ledger.turn_limit(), ledger) {
        interventions.push(RequestIntervention::NarrowTools(tools));
    }

    if governors.empty.rescue_armed() {
        interventions.push(RequestIntervention::SilenceThinking);
    }
    governors.empty.consume_rescue();

    interventions
}

/// The Tool Call answering moment, before a call executes: judge what the
/// model SENT. A call identical to a still-fresh one from the previous Pass
/// draws a replacement Tool Result instead of a rerun. Only
/// [`AnswerIntervention::ReplaceResult`] (or nothing) issues here.
pub fn answer_sent(governors: &Governors, name: &str, input: &Value) -> Option<AnswerIntervention> {
    if governors.duplicate.is_duplicate(name, input) {
        Some(AnswerIntervention::ReplaceResult {
            content: voice::duplicate_call_nudge().to_string(),
            is_error: true,
        })
    } else {
        None
    }
}

/// The Tool Call answering moment, after a call executes (or was replaced):
/// judge what the model will READ. The outcome's facts are already on the
/// Ledger — the firing site records them ([`Ledger::record_result`]) before
/// consulting, replaced results included. This consultation folds the outcome
/// into the duplicate Governor's fresh-set trigger state and, from the third
/// consecutive failure of one Tool onward, annotates the result with the
/// step-back suffix. Only [`AnswerIntervention::AnnotateResult`] (or nothing)
/// issues here.
pub fn answer_read(
    ledger: &Ledger,
    governors: &mut Governors,
    name: &str,
    input: &Value,
    result: &ToolResult,
) -> Option<AnswerIntervention> {
    governors
        .duplicate
        .note_answered(name, input, result.is_error);
    failure::annotation(ledger, name, result.content).map(AnswerIntervention::AnnotateResult)
}

/// The Tool Call answering moment, once per batch: what rides the results
/// tail — the trailing tool-results user message the model reads next. The
/// Pass's carried calls and position are Ledger facts. Precedence within this
/// consultation is the merge ORDER (every due rider rides; none subsumes
/// another):
///
///   1. the Explore Nudge — every 3rd consecutive exploration Pass,
///   2. the Anchor — every `anchor_interval` Passes, and the first Pass after
///      a Compaction (the anchor Governor's cadence), carrying the Voiced
///      stale-plan line whenever the anchor Governor's stale-plan opinion
///      holds ([`anchor::Anchor::stale_plan`]),
///   3. the Endgame's rider (wrap-up warning / Verification Pass prompt /
///      final-Pass prompt) — last, nearest the model's attention.
pub fn answer_tail(ledger: &Ledger, governors: &mut Governors) -> Vec<AnswerIntervention> {
    let mut interventions = Vec::new();

    if governors.explore.note_pass_calls(ledger.pass_calls()) {
        interventions.push(AnswerIntervention::RideTail(Rider::Voiced {
            tag: VoicedTag::ExploreNudge,
            text: voice::explore_nudge().to_string(),
        }));
    }

    if governors.anchor.due(ledger) {
        let stale_line = governors
            .anchor
            .stale_plan(ledger)
            .map(voice::stale_plan_line);
        interventions.push(AnswerIntervention::RideTail(Rider::Anchor { stale_line }));
    }

    if let Some((tag, text)) = endgame_rider(ledger) {
        interventions.push(AnswerIntervention::RideTail(Rider::Voiced { tag, text }));
    }

    interventions
}

/// The finish-settlement moment, consulted after a tool-answering Pass: at
/// the Turn Limit the Turn closes on the marker even though the model is
/// still asking for Tools (CONTEXT.md: a Turn ends at its Turn Limit). The
/// close is plain, or — when the Ledger says the work is demonstrably
/// unfinished and the endgame Governor's recovery budget allows — the
/// close-and-open-a-Recovery-Turn Intervention; the stop reason distinguishes
/// a stuck Turn from a productive one either way.
pub fn settle_capped(ledger: &Ledger, governors: &Governors) -> Option<FinishIntervention> {
    if endgame::final_pass(ledger.pass(), ledger.turn_limit()) {
        Some(limit_close(ledger, governors))
    } else {
        None
    }
}

// The Endgame Governor's close at the Turn Limit: plain, or with a Recovery
// Turn when its recovery judgment fires. One author for both settle paths.
fn limit_close(ledger: &Ledger, governors: &Governors) -> FinishIntervention {
    let reason = endgame::limit_stop_reason(ledger);
    match endgame::recovery(&governors.endgame, ledger) {
        Some(recovery) => FinishIntervention::CloseRecover {
            reason,
            recovery,
            keep_reply: false,
        },
        None => FinishIntervention::Close(reason),
    }
}

/// The finish-settlement moment, consulted when the model stops calling
/// tools. `stop_reason` is the CLOSED reason — a phantom tool_use already
/// mapped to end_turn by the firing site. One consultation, one explicit
/// precedence:
///
///   1. the final-Pass tool-insistence Close (ADR-0015): a reply that still
///      insists on tools as serialized markup closes on the turn-limit marker
///      — it outranks every Nudge because no Pass is left to grant; it is a
///      Turn-Limit close, so the endgame Governor's recovery judgment applies
///      exactly as at [`settle_capped`];
///   2. the final-Pass text-settle recovery (ADR-0028 addendum): ADR-0015
///      withdraws every tool on the final Pass, so a capped Turn nearly
///      always ends here — a plain reply, end_turn — and the recovery
///      judgment applies exactly as at the marker closes. When it fires, the
///      reply (the model's genuine wrap-up) enters the Conversation before
///      the close (`keep_reply`); when it does not (a green settle, or the
///      budget spent), the Turn concludes on the reply as before;
///   3. the Verify-failed Nudge — the last run_command this Turn failed;
///   4. the Verify Nudge — files changed but nothing was verified;
///   5. the Empty-response Nudge — no content, or a parroted empty marker.
///      The strict Verify-failed > Verify > Empty order, each gated on
///      end_turn, room under the Turn Limit ([`endgame::can_loop`] — the
///      limit bounds every Nudge), and its own re-arm bookkeeping;
///   6. nothing — the Turn concludes on the model's reply.
///
/// A firing Nudge updates its trigger bookkeeping here (the once-per-Turn
/// caps, the no-think rescue arm, and the duplicate Governor's memory clear —
/// the finishing response's dropped tool_use blocks never produced results):
/// that is decision state, not effect. The firing site keeps the effects.
pub fn settle_finish(
    ledger: &Ledger,
    governors: &mut Governors,
    blocks: &[ContentBlock],
    stop_reason: &StopReason,
) -> Option<FinishIntervention> {
    if endgame::final_pass(ledger.pass(), ledger.turn_limit())
        && endgame::tool_insistent_text(blocks)
    {
        return Some(limit_close(ledger, governors));
    }

    let end_turn = *stop_reason == StopReason::EndTurn;

    if end_turn
        && endgame::final_pass(ledger.pass(), ledger.turn_limit())
        && let Some(recovery) = endgame::recovery(&governors.endgame, ledger)
    {
        return Some(FinishIntervention::CloseRecover {
            reason: endgame::limit_stop_reason(ledger),
            recovery,
            keep_reply: true,
        });
    }

    if !end_turn || !endgame::can_loop(ledger.pass(), ledger.turn_limit()) {
        return None;
    }

    if governors.verify.verify_failed_nudge(ledger) {
        governors.verify.note_verify_failed_nudged();
        governors.duplicate.note_finish_nudged();
        return Some(standalone(
            VoicedTag::VerifyFailedNudge,
            voice::verify_failed_nudge(),
        ));
    }

    if governors.verify.verify_nudge(ledger) {
        governors.verify.note_verify_nudged();
        governors.duplicate.note_finish_nudged();
        return Some(standalone(VoicedTag::VerifyNudge, voice::verify_nudge()));
    }

    if empty::is_empty_reply(blocks) && governors.empty.due() {
        // The fire arms the break-glass no-think rescue for the next Pass,
        // gated by the empty Governor's setpoint.
        governors.empty.note_fired();
        governors.duplicate.note_finish_nudged();
        return Some(standalone(
            VoicedTag::EmptyResponseNudge,
            voice::empty_response_nudge(),
        ));
    }

    None
}

fn standalone(tag: VoicedTag, text: &str) -> FinishIntervention {
    FinishIntervention::Standalone {
        tag,
        text: text.to_string(),
    }
}

// The Endgame's tail rider for this Pass position, paired with the Transcript
// tag announcing it.
fn endgame_rider(ledger: &Ledger) -> Option<(VoicedTag, String)> {
    match endgame::tail_rider(ledger.pass(), ledger.turn_limit(), ledger) {
        TailRider::VerificationPass(t) => Some((VoicedTag::VerificationPass, t)),
        TailRider::WrapUpWarning(t) => Some((VoicedTag::WrapUpWarning, t)),
        TailRider::FinalPass(t) => Some((VoicedTag::FinalPass, t)),
        TailRider::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok() -> ToolResult<'static> {
        ToolResult {
            content: "ok",
            is_error: false,
        }
    }

    fn err() -> ToolResult<'static> {
        ToolResult {
            content: "boom",
            is_error: true,
        }
    }

    // A Ledger at a Pass position.
    fn ledger_at(pass: u64, turn_limit: u64) -> Ledger {
        let mut ledger = Ledger::new(turn_limit);
        for _ in 1..pass {
            ledger.advance_pass();
        }
        ledger
    }

    // One successful edit_file leaves the Turn with unverified writes.
    fn unverified_at(pass: u64, turn_limit: u64) -> Ledger {
        let mut ledger = ledger_at(pass, turn_limit);
        ledger.record_result("edit_file", &json!({}), &ok());
        ledger
    }

    // The model wrote, then its verification dangles red: a successful write
    // followed by a failing run_command. The write is the evidence the
    // dangling-failure recovery arm requires (ADR-0028 addendum 2026-07-14).
    fn command_failing_at(pass: u64, turn_limit: u64) -> Ledger {
        let mut ledger = ledger_at(pass, turn_limit);
        ledger.record_result("edit_file", &json!({"path": "a.ex"}), &ok());
        ledger.record_result("run_command", &json!({"command": "cargo test"}), &err());
        ledger
    }

    // ---- shape_request: the request-shaping moment --------------------------

    #[test]
    fn the_final_pass_withdraws_every_tool() {
        assert_eq!(
            shape_request(&ledger_at(25, 25), &mut Governors::new(5, 8, true)),
            vec![RequestIntervention::NarrowTools(Vec::new())]
        );
    }

    #[test]
    fn the_final_pass_outranks_the_verification_pass() {
        // Both positions coincide only past the limit, but unverified writes
        // at the limit must still yield the tool-less final Pass, never the
        // run_command narrowing.
        assert_eq!(
            shape_request(&unverified_at(25, 25), &mut Governors::new(5, 8, true)),
            vec![RequestIntervention::NarrowTools(Vec::new())]
        );
    }

    #[test]
    fn the_verification_pass_narrows_to_run_command() {
        assert_eq!(
            shape_request(&unverified_at(24, 25), &mut Governors::new(5, 8, true)),
            vec![RequestIntervention::NarrowTools(
                crate::tools::verification_specs()
            )]
        );
    }

    #[test]
    fn an_ordinary_pass_issues_nothing() {
        assert_eq!(
            shape_request(&ledger_at(1, 25), &mut Governors::new(5, 8, true)),
            Vec::new()
        );
        assert_eq!(
            shape_request(&ledger_at(23, 25), &mut Governors::new(5, 8, true)),
            Vec::new()
        );
    }

    #[test]
    fn an_armed_rescue_silences_thinking_for_exactly_one_pass() {
        let mut governors = Governors::new(5, 8, true);
        governors.empty.note_fired();

        assert_eq!(
            shape_request(&ledger_at(1, 25), &mut governors),
            vec![RequestIntervention::SilenceThinking]
        );
        // Consulting the moment consumed the one-Pass arm.
        assert_eq!(shape_request(&ledger_at(2, 25), &mut governors), Vec::new());
    }

    #[test]
    fn a_sticky_rescue_keeps_silencing() {
        let mut governors = Governors::new(5, 8, true);
        governors.empty.note_fired();
        governors.empty.note_fired(); // the second empty makes it sticky

        assert_eq!(
            shape_request(&ledger_at(1, 25), &mut governors),
            vec![RequestIntervention::SilenceThinking]
        );
        assert_eq!(
            shape_request(&ledger_at(2, 25), &mut governors),
            vec![RequestIntervention::SilenceThinking]
        );
    }

    // ---- answer_sent / answer_read: the Tool Call answering moment ----------

    // Records the outcome's facts and consults the arbiter, the way the
    // firing site composes them (batch: record once, then judge).
    fn read_outcome(
        ledger: &mut Ledger,
        governors: &mut Governors,
        name: &str,
        input: &Value,
        result: &ToolResult,
    ) -> Option<AnswerIntervention> {
        ledger.record_result(name, input, result);
        answer_read(ledger, governors, name, input, result)
    }

    #[test]
    fn a_still_fresh_repeat_draws_a_replacement() {
        let input = json!({"path": "a.ex"});
        let mut ledger = Ledger::new(25);
        let mut governors = Governors::new(5, 8, true);
        read_outcome(&mut ledger, &mut governors, "read_file", &input, &ok());
        governors.next_pass();

        assert_eq!(
            answer_sent(&governors, "read_file", &input),
            Some(AnswerIntervention::ReplaceResult {
                content: voice::duplicate_call_nudge().to_string(),
                is_error: true,
            })
        );
    }

    #[test]
    fn a_fresh_call_issues_nothing() {
        let governors = Governors::new(5, 8, true);
        assert_eq!(
            answer_sent(&governors, "read_file", &json!({"path": "a.ex"})),
            None
        );
    }

    #[test]
    fn the_third_consecutive_failure_earns_the_annotation() {
        let mut ledger = Ledger::new(25);
        let mut governors = Governors::new(5, 8, true);
        assert_eq!(
            read_outcome(&mut ledger, &mut governors, "read_file", &json!({}), &err()),
            None
        );
        assert_eq!(
            read_outcome(&mut ledger, &mut governors, "read_file", &json!({}), &err()),
            None
        );

        match read_outcome(&mut ledger, &mut governors, "read_file", &json!({}), &err()) {
            Some(AnswerIntervention::AnnotateResult(content)) => {
                assert!(content.contains("boom"));
                assert!(content.contains("3 consecutive read_file failures"));
            }
            other => panic!("expected AnnotateResult, got {other:?}"),
        }
    }

    #[test]
    fn a_success_issues_nothing_and_resets_the_tally() {
        let mut ledger = Ledger::new(25);
        let mut governors = Governors::new(5, 8, true);
        read_outcome(&mut ledger, &mut governors, "read_file", &json!({}), &err());
        read_outcome(&mut ledger, &mut governors, "read_file", &json!({}), &err());
        assert_eq!(
            read_outcome(&mut ledger, &mut governors, "read_file", &json!({}), &ok()),
            None
        );
        // The reset tally means the next failure starts a fresh streak.
        assert_eq!(
            read_outcome(&mut ledger, &mut governors, "read_file", &json!({}), &err()),
            None
        );
    }

    // ---- answer_tail: the results tail ---------------------------------------

    #[test]
    fn every_due_rider_rides_in_merge_order() {
        // Third consecutive exploration Pass + anchor cadence hit + two
        // Passes remaining: all three ride, Explore -> Anchor -> Endgame.
        let mut governors = Governors::new(6, 8, true);
        let read = vec![("read_file".to_string(), json!({"path": "a.ex"}))];
        governors.explore.note_pass_calls(&read);
        governors.explore.note_pass_calls(&read);

        let mut ledger = ledger_at(6, 8);
        ledger.record_pass_calls(read);

        let interventions = answer_tail(&ledger, &mut governors);
        assert_eq!(
            interventions,
            vec![
                AnswerIntervention::RideTail(Rider::Voiced {
                    tag: VoicedTag::ExploreNudge,
                    text: voice::explore_nudge().to_string(),
                }),
                AnswerIntervention::RideTail(Rider::Anchor { stale_line: None }),
                AnswerIntervention::RideTail(Rider::Voiced {
                    tag: VoicedTag::WrapUpWarning,
                    text: voice::wrap_up_warning(2),
                }),
            ]
        );
    }

    #[test]
    fn a_quiet_pass_rides_nothing() {
        let mut ledger = ledger_at(1, 25);
        ledger.record_pass_calls(vec![("edit_file".to_string(), json!({"path": "a.ex"}))]);
        assert_eq!(
            answer_tail(&ledger, &mut Governors::new(0, 8, true)),
            Vec::new()
        );
    }

    #[test]
    fn a_compaction_places_an_anchor_off_interval() {
        let mut ledger = ledger_at(1, 25);
        ledger.note_compacted();
        ledger.record_pass_calls(vec![("edit_file".to_string(), json!({"path": "a.ex"}))]);
        assert_eq!(
            answer_tail(&ledger, &mut Governors::new(999, 8, true)),
            vec![AnswerIntervention::RideTail(Rider::Anchor {
                stale_line: None
            })]
        );
    }

    #[test]
    fn a_stale_plan_line_rides_the_anchor_and_every_later_one() {
        // Plan on Pass 1, a write since, anchor cadence 5: the Anchors at
        // Pass 10 and Pass 15 both carry the line (9 then 14 Passes since),
        // freshly parameterized — never a one-shot.
        let mut governors = Governors::new(5, 8, true);
        let mut ledger = ledger_at(1, 50);
        ledger.note_plan_updated();
        ledger.record_result("edit_file", &json!({}), &ok());
        for _ in 1..10 {
            ledger.advance_pass();
        }

        assert_eq!(
            answer_tail(&ledger, &mut governors),
            vec![AnswerIntervention::RideTail(Rider::Anchor {
                stale_line: Some(voice::stale_plan_line(9)),
            })]
        );

        for _ in 10..15 {
            ledger.advance_pass();
        }
        assert_eq!(
            answer_tail(&ledger, &mut governors),
            vec![AnswerIntervention::RideTail(Rider::Anchor {
                stale_line: Some(voice::stale_plan_line(14)),
            })]
        );
    }

    #[test]
    fn a_fresh_plan_or_a_writeless_stretch_rides_a_bare_anchor() {
        // Same cadence hit, but the Plan is within its threshold — and a
        // stale Plan with zero writes since (pure reading) stays bare too.
        let mut fresh = ledger_at(1, 50);
        fresh.note_plan_updated();
        fresh.record_result("edit_file", &json!({}), &ok());
        for _ in 1..5 {
            fresh.advance_pass();
        }
        assert_eq!(
            answer_tail(&fresh, &mut Governors::new(5, 8, true)),
            vec![AnswerIntervention::RideTail(Rider::Anchor {
                stale_line: None
            })]
        );

        let mut reading = ledger_at(1, 50);
        reading.note_plan_updated();
        for _ in 1..10 {
            reading.advance_pass();
        }
        assert_eq!(
            answer_tail(&reading, &mut Governors::new(5, 8, true)),
            vec![AnswerIntervention::RideTail(Rider::Anchor {
                stale_line: None
            })]
        );
    }

    #[test]
    fn no_plan_never_draws_the_stale_line() {
        // Deep in the Turn with writes but no Plan ever set: the Anchor rides
        // bare (its no-plan fallback already asks for a Plan).
        let mut ledger = ledger_at(1, 50);
        ledger.record_result("edit_file", &json!({}), &ok());
        for _ in 1..20 {
            ledger.advance_pass();
        }
        assert_eq!(
            answer_tail(&ledger, &mut Governors::new(5, 8, true)),
            vec![AnswerIntervention::RideTail(Rider::Anchor {
                stale_line: None
            })]
        );
    }

    // ---- settle_capped: the Turn Limit after a tool-answering Pass ----------

    #[test]
    fn below_the_limit_the_turn_runs_on() {
        assert_eq!(
            settle_capped(&ledger_at(24, 25), &Governors::new(5, 8, true)),
            None
        );
        // Unfinished work below the limit never triggers a recovery either.
        assert_eq!(
            settle_capped(&unverified_at(24, 25), &Governors::new(5, 8, true)),
            None
        );
    }

    #[test]
    fn at_the_limit_a_clean_turn_closes_on_the_marker_no_recovery() {
        assert_eq!(
            settle_capped(&ledger_at(25, 25), &Governors::new(5, 8, true)),
            Some(FinishIntervention::Close(log::StopReason::TurnLimit))
        );
    }

    #[test]
    fn a_stuck_turn_closes_with_the_stuck_reason() {
        let mut stuck = ledger_at(25, 25);
        for _ in 0..3 {
            stuck.record_result("grep", &json!({}), &err());
        }
        assert_eq!(
            settle_capped(&stuck, &Governors::new(5, 8, true)),
            Some(FinishIntervention::Close(log::StopReason::TurnLimitStuck))
        );
    }

    #[test]
    fn a_cap_with_unverified_writes_closes_and_recovers() {
        assert_eq!(
            settle_capped(&unverified_at(25, 25), &Governors::new(5, 8, true)),
            Some(FinishIntervention::CloseRecover {
                reason: log::StopReason::TurnLimit,
                recovery: endgame::Recovery {
                    shape: crate::session::RecoveryShape::Handoff,
                    verification_failing: false,
                    failing_command: None,
                },
                keep_reply: false,
            })
        );
    }

    #[test]
    fn a_cap_with_a_failing_verification_closes_and_recovers() {
        assert_eq!(
            settle_capped(&command_failing_at(25, 25), &Governors::new(5, 8, true)),
            Some(FinishIntervention::CloseRecover {
                reason: log::StopReason::TurnLimit,
                recovery: endgame::Recovery {
                    shape: crate::session::RecoveryShape::Handoff,
                    verification_failing: true,
                    failing_command: Some("cargo test".to_string()),
                },
                keep_reply: false,
            })
        );
    }

    #[test]
    fn a_spent_recovery_budget_closes_plain() {
        let mut ledger = unverified_at(25, 25);
        ledger.note_recoveries_used(1);
        assert_eq!(
            settle_capped(&ledger, &Governors::new(5, 8, true)),
            Some(FinishIntervention::Close(log::StopReason::TurnLimit))
        );
    }

    #[test]
    fn recovery_limit_zero_disables_the_mechanic_at_the_arbiter() {
        let governors = Governors::new(5, 8, true).with_recovery(endgame::RecoverySetpoints {
            limit: 0,
            shape: crate::session::RecoveryShape::Handoff,
        });
        assert_eq!(
            settle_capped(&unverified_at(25, 25), &governors),
            Some(FinishIntervention::Close(log::StopReason::TurnLimit))
        );
    }

    // ---- settle_finish: the strict per-moment precedence ---------------------

    fn text(t: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::text(t)]
    }

    #[test]
    fn the_tool_insistence_close_outranks_every_nudge() {
        // Verify-failed is armed, but on the final Pass a tool-insistent
        // reply closes: no Pass is left to grant the Nudge. It is a
        // Turn-Limit close with a failing verification, so the endgame
        // Governor's recovery rides it.
        let ledger = command_failing_at(3, 3);
        assert_eq!(
            settle_finish(
                &ledger,
                &mut Governors::new(5, 8, true),
                &text("<tool_call>x</tool_call>"),
                &StopReason::EndTurn
            ),
            Some(FinishIntervention::CloseRecover {
                reason: log::StopReason::TurnLimit,
                recovery: endgame::Recovery {
                    shape: crate::session::RecoveryShape::Handoff,
                    verification_failing: true,
                    failing_command: Some("cargo test".to_string()),
                },
                keep_reply: false,
            })
        );
    }

    #[test]
    fn a_clean_tool_insistence_close_recovers_nothing() {
        assert_eq!(
            settle_finish(
                &ledger_at(3, 3),
                &mut Governors::new(5, 8, true),
                &text("<tool_call>x</tool_call>"),
                &StopReason::EndTurn
            ),
            Some(FinishIntervention::Close(log::StopReason::TurnLimit))
        );
    }

    #[test]
    fn verify_failed_speaks_before_verify() {
        // Both armed: a failing run_command, then an unverified write.
        let mut ledger = ledger_at(2, 25);
        ledger.record_result("run_command", &json!({}), &err());
        ledger.record_result("edit_file", &json!({}), &ok());
        let mut governors = Governors::new(5, 8, true);

        assert_eq!(
            settle_finish(&ledger, &mut governors, &text("done"), &StopReason::EndTurn),
            Some(FinishIntervention::Standalone {
                tag: VoicedTag::VerifyFailedNudge,
                text: voice::verify_failed_nudge().to_string(),
            })
        );
        // With Verify-failed spoken (and not re-armed), Verify speaks next.
        ledger.advance_pass();
        assert_eq!(
            settle_finish(&ledger, &mut governors, &text("done"), &StopReason::EndTurn),
            Some(FinishIntervention::Standalone {
                tag: VoicedTag::VerifyNudge,
                text: voice::verify_nudge().to_string(),
            })
        );
        // Both spoken, the reply has content: the Turn concludes.
        ledger.advance_pass();
        assert_eq!(
            settle_finish(&ledger, &mut governors, &text("done"), &StopReason::EndTurn),
            None
        );
    }

    #[test]
    fn verify_speaks_before_empty() {
        // An empty reply with unverified writes draws Verify, not Empty.
        let ledger = unverified_at(2, 25);
        assert_eq!(
            settle_finish(
                &ledger,
                &mut Governors::new(5, 8, true),
                &[],
                &StopReason::EndTurn
            ),
            Some(FinishIntervention::Standalone {
                tag: VoicedTag::VerifyNudge,
                text: voice::verify_nudge().to_string(),
            })
        );
    }

    #[test]
    fn an_empty_reply_draws_the_empty_nudge_and_arms_the_rescue() {
        let mut governors = Governors::new(5, 8, true);
        assert_eq!(
            settle_finish(&ledger_at(2, 25), &mut governors, &[], &StopReason::EndTurn),
            Some(FinishIntervention::Standalone {
                tag: VoicedTag::EmptyResponseNudge,
                text: voice::empty_response_nudge().to_string(),
            })
        );
        assert!(governors.empty.rescue_armed());
    }

    #[test]
    fn the_off_knob_still_nudges_but_never_arms() {
        let mut governors = Governors::new(5, 8, false);
        let settled = settle_finish(&ledger_at(2, 25), &mut governors, &[], &StopReason::EndTurn);
        assert!(matches!(
            settled,
            Some(FinishIntervention::Standalone {
                tag: VoicedTag::EmptyResponseNudge,
                ..
            })
        ));
        assert!(!governors.empty.rescue_armed());
    }

    #[test]
    fn a_parroted_empty_marker_counts_as_empty() {
        let settled = settle_finish(
            &ledger_at(2, 25),
            &mut Governors::new(5, 8, true),
            &text(voice::empty_response_marker()),
            &StopReason::EndTurn,
        );
        assert!(matches!(
            settled,
            Some(FinishIntervention::Standalone {
                tag: VoicedTag::EmptyResponseNudge,
                ..
            })
        ));
    }

    #[test]
    fn prose_containing_the_marker_is_a_conclusion() {
        assert_eq!(
            settle_finish(
                &ledger_at(2, 25),
                &mut Governors::new(5, 8, true),
                &text("I kept hitting [empty response] markers; here is my summary."),
                &StopReason::EndTurn,
            ),
            None
        );
    }

    #[test]
    fn a_non_end_turn_stop_silences_every_nudge() {
        assert_eq!(
            settle_finish(
                &command_failing_at(2, 25),
                &mut Governors::new(5, 8, true),
                &text("partial"),
                &StopReason::MaxTokens
            ),
            None
        );
    }

    #[test]
    fn the_turn_limit_bounds_every_nudge() {
        // At the limit no Pass is left to grant, so an armed gate stays quiet
        // and the Turn concludes on the model's reply (the recovery budget is
        // spent here, so the text-settle recovery stays out of the way).
        let mut ledger = command_failing_at(2, 2);
        ledger.note_recoveries_used(1);
        assert_eq!(
            settle_finish(
                &ledger,
                &mut Governors::new(5, 8, true),
                &text("done"),
                &StopReason::EndTurn
            ),
            None
        );
    }

    // ---- settle_finish: the final-Pass text settle (ADR-0028 addendum) ------

    #[test]
    fn a_final_pass_text_settle_with_a_dangling_failure_closes_and_recovers() {
        // ADR-0015 withdrew the tools, so the capped Turn ends on a plain
        // reply — the recovery judgment applies, and the reply (not the
        // marker) enters the Conversation.
        assert_eq!(
            settle_finish(
                &command_failing_at(3, 3),
                &mut Governors::new(5, 8, true),
                &text("half done; the tests are still red"),
                &StopReason::EndTurn
            ),
            Some(FinishIntervention::CloseRecover {
                reason: log::StopReason::TurnLimit,
                recovery: endgame::Recovery {
                    shape: crate::session::RecoveryShape::Handoff,
                    verification_failing: true,
                    failing_command: Some("cargo test".to_string()),
                },
                keep_reply: true,
            })
        );
    }

    #[test]
    fn a_final_pass_text_settle_with_unverified_writes_closes_and_recovers() {
        assert_eq!(
            settle_finish(
                &unverified_at(3, 3),
                &mut Governors::new(5, 8, true),
                &text("edited but never ran the tests"),
                &StopReason::EndTurn
            ),
            Some(FinishIntervention::CloseRecover {
                reason: log::StopReason::TurnLimit,
                recovery: endgame::Recovery {
                    shape: crate::session::RecoveryShape::Handoff,
                    verification_failing: false,
                    failing_command: None,
                },
                keep_reply: true,
            })
        );
    }

    #[test]
    fn a_green_final_pass_text_settle_concludes_on_the_reply() {
        assert_eq!(
            settle_finish(
                &ledger_at(3, 3),
                &mut Governors::new(5, 8, true),
                &text("all green; done"),
                &StopReason::EndTurn
            ),
            None
        );
    }

    #[test]
    fn a_spent_budget_concludes_the_text_settle_plain() {
        let mut ledger = command_failing_at(3, 3);
        ledger.note_recoveries_used(1);
        assert_eq!(
            settle_finish(
                &ledger,
                &mut Governors::new(5, 8, true),
                &text("still red, out of recoveries"),
                &StopReason::EndTurn
            ),
            None
        );
    }

    #[test]
    fn a_non_end_turn_final_pass_settle_never_recovers() {
        assert_eq!(
            settle_finish(
                &command_failing_at(3, 3),
                &mut Governors::new(5, 8, true),
                &text("cut off mid-"),
                &StopReason::MaxTokens
            ),
            None
        );
    }
}
