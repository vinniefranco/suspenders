//! Run finish - how a Run ends when the model stops calling tools (carved
//! from the Run Loop port of baud's `Baud.Turn.Loop`). Deliberately NOT named
//! "settlement": Run Settlement (CONTEXT.md, [`super::settlement`]) is how an
//! already-ended Run enters the Conversation; this module is the ending
//! itself.
//!
//! [`finish`] handles a Pass without (executable) Tool Calls. Usually the Run
//! ends there, but the finish-settlement arbiter ([`super::governor`],
//! ADR-0026) may intervene instead: close the Run on the run-limit marker
//! (ADR-0015's tool-insistence rule) or send the model back for one more Pass
//! with a stand-alone Nudge - the strict Verify-failed > Verify > Empty
//! precedence lives in [`governor::settle_finish`], not here. This module
//! keeps the effects: appending blocks, announcing the Nudge, closing.
//!
//! The marker algebra: an empty close gets the empty-response marker (or the
//! truncation marker on max_tokens), a parroted empty-response marker counts
//! as empty, and the Run Limit / stopped / failed markers keep roles
//! alternating when the Loop closes a Run itself. The LLM error algebra
//! ([`fail`]): partial text survives, unanswered tool_use blocks are dropped,
//! and the failed marker closes the Run.

use crate::content::ContentBlock;
use crate::conversation::Conversation;
use crate::event::{Event, VoicedTag};
use crate::llm::response::{Response, StopReason};
use crate::session::log;
use crate::run::deps::RunDeps;
use crate::run::governor::{self, FinishIntervention};
use crate::run::loop_::{Flow, LoopState, Outcome, OutcomeStop};
use crate::voice;

pub(super) fn close<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    marker: &str,
    stop_reason: log::StopReason,
) -> Outcome {
    conversation.add_assistant_blocks(vec![ContentBlock::text(marker)]);
    state.deps.checkpoint(&conversation);
    Outcome::Ok(conversation, OutcomeStop::Reason(stop_reason))
}

// The close-and-open-a-Recovery-Run Intervention's close half: like
// [`close`], but the outcome carries the Endgame Governor's directive out to
// the Agent, which executes the opening (CONTEXT.md: Recovery Run). One
// author for both recovery closes: `closing` is the run-limit marker at the
// tool-answering cap and on the tool-insistent reply (roles keep
// alternating; the insistent markup never enters), or the model's own
// final-Pass reply on the text settle (ADR-0028 addendum). `provenance` is
// the captured Model's when `closing` is that reply, `None` when it is the
// Voice's marker (ADR-0037: Provenance marks what the model produced).
pub(super) fn close_recover<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    closing: Vec<ContentBlock>,
    provenance: Option<crate::content::Provenance>,
    stop_reason: log::StopReason,
    recovery: governor::endgame::Recovery,
) -> Outcome {
    match provenance {
        Some(p) => conversation.add_assistant_response(closing, p),
        None => conversation.add_assistant_blocks(closing),
    };
    state.deps.checkpoint(&conversation);
    Outcome::Recover(conversation, stop_reason, recovery)
}

pub(super) fn close_custom<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    marker: &str,
    stop_reason: String,
) -> Outcome {
    conversation.add_assistant_blocks(vec![ContentBlock::text(marker)]);
    state.deps.checkpoint(&conversation);
    Outcome::Ok(conversation, OutcomeStop::Custom(stop_reason))
}

// The LLM error algebra: text survives; unanswered tool_use blocks are dropped;
// the failed marker closes the Run so roles keep alternating.
pub(super) fn fail<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    response: Response,
) -> Outcome {
    let mut blocks: Vec<ContentBlock> = response
        .content
        .iter()
        .filter(|b| !b.is_tool_use())
        .cloned()
        .collect();
    blocks.push(ContentBlock::text(voice::run_failed_marker()));
    // The partial text is the model's, so the message carries its Provenance
    // (the appended marker rides the same message, as the fold's does).
    conversation.add_assistant_response(blocks, state.deps.provenance());
    state.deps.checkpoint(&conversation);
    let reason = response.error.unwrap_or_default();
    Outcome::Failed(reason, conversation)
}

// The model stopped without (executable) Tool Calls. The finish-settlement
// arbiter (ADR-0026) decides how the finish settles; this site translates:
// a Close appends the run-limit marker (the reply - ADR-0015's insistent
// markup - never enters the Conversation), a CloseRecover appends the marker
// or - `keep_reply`, the final-Pass text settle - the reply itself before
// carrying the recovery directive out, a Standalone Nudge appends the reply
// and then the user-role Nudge for one more Pass, and no Intervention
// concludes the Run on the reply. Any tool_use block in this branch is
// unanswered and is dropped.
pub(super) fn finish<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    blocks: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> Flow {
    let closed = close_stop_reason(&stop_reason);

    match governor::settle_finish(&state.ledger, &mut state.governors, &blocks, &closed) {
        Some(FinishIntervention::Close(reason)) => Flow::Done(close(
            state,
            conversation,
            voice::run_limit_marker(),
            reason,
        )),
        Some(FinishIntervention::CloseRecover {
            reason,
            recovery,
            keep_reply,
        }) => {
            let (closing, provenance) = if keep_reply {
                (
                    close_blocks(&blocks, &stop_reason),
                    Some(state.deps.provenance()),
                )
            } else {
                (vec![ContentBlock::text(voice::run_limit_marker())], None)
            };
            Flow::Done(close_recover(
                state,
                conversation,
                closing,
                provenance,
                reason,
                recovery,
            ))
        }
        Some(FinishIntervention::Standalone { tag, text }) => {
            let provenance = state.deps.provenance();
            conversation.add_assistant_response(close_blocks(&blocks, &stop_reason), provenance);
            nudge_finish(state, conversation, &text, tag)
        }
        None => {
            let provenance = state.deps.provenance();
            conversation.add_assistant_response(close_blocks(&blocks, &stop_reason), provenance);
            Flow::Done(Outcome::Ok(conversation, outcome_stop_of(&closed)))
        }
    }
}

// Shared finish-Nudge mechanic: append the user-role Nudge, announce it, count
// it as a normal Pass against the Run Limit, loop.
fn nudge_finish<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    nudge: &str,
    tag: VoicedTag,
) -> Flow {
    conversation.add_user_text(nudge);
    state.emitter.emit(Event::voiced(tag, nudge));
    state.ledger.advance_pass();
    Flow::Continue(conversation)
}

fn close_blocks(blocks: &[ContentBlock], stop_reason: &StopReason) -> Vec<ContentBlock> {
    let kept: Vec<ContentBlock> = blocks
        .iter()
        .filter(|b| !b.is_tool_use())
        .cloned()
        .collect();
    if kept.is_empty() {
        if *stop_reason == StopReason::MaxTokens {
            vec![ContentBlock::text(voice::truncation_marker())]
        } else {
            vec![ContentBlock::text(voice::empty_response_marker())]
        }
    } else {
        kept
    }
}

// A phantom :tool_use stop ends the Run like a normal completion.
fn close_stop_reason(stop_reason: &StopReason) -> StopReason {
    match stop_reason {
        StopReason::ToolUse => StopReason::EndTurn,
        other => other.clone(),
    }
}

// Maps a (closed) LLM stop reason to the outcome's terminal reason.
fn outcome_stop_of(stop_reason: &StopReason) -> OutcomeStop {
    let reason = match stop_reason {
        StopReason::EndTurn | StopReason::ToolUse => log::StopReason::EndTurn,
        StopReason::MaxTokens => log::StopReason::MaxTokens,
        StopReason::StopSequence => log::StopReason::StopSequence,
        StopReason::Error => log::StopReason::Error,
        StopReason::Unknown => log::StopReason::Unknown,
    };
    OutcomeStop::Reason(reason)
}
