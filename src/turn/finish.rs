//! Turn finish - how a Turn ends when the model stops calling tools (carved
//! from the Turn Loop port of baud's `Baud.Turn.Loop`). Deliberately NOT named
//! "settlement": Turn Settlement (CONTEXT.md, [`super::settlement`]) is how an
//! already-ended Turn enters the Conversation; this module is the ending
//! itself.
//!
//! [`finish`] handles a Pass without (executable) Tool Calls. Usually the Turn
//! ends there, but a finish Nudge may send the model back for one more Pass -
//! in strict precedence Verify-failed > Verify > Empty (the last verification
//! command failed, files changed but nothing was verified, the reply was
//! empty; ADR-0016 covers the Endgame side of unverified writes), each gated
//! on end_turn, room under the Turn Limit, and its own re-arm bookkeeping in
//! [`Nudges`](super::nudges::Nudges).
//!
//! The marker algebra: an empty close gets the empty-response marker (or the
//! truncation marker on max_tokens), a parroted empty-response marker counts
//! as empty, and the Turn Limit / stopped / failed markers keep roles
//! alternating when the Loop closes a Turn itself. The LLM error algebra
//! ([`fail`]): partial text survives, unanswered tool_use blocks are dropped,
//! and the failed marker closes the Turn.

use crate::content::ContentBlock;
use crate::conversation::Conversation;
use crate::event::{Event, VoicedTag};
use crate::llm::response::{Response, StopReason};
use crate::session::log;
use crate::turn::deps::TurnDeps;
use crate::turn::endgame;
use crate::turn::loop_::{is_tool_use, Flow, LoopState, Outcome, OutcomeStop};
use crate::voice;

pub(super) fn close<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    marker: &str,
    stop_reason: log::StopReason,
) -> Outcome {
    conversation.add_assistant_blocks(vec![ContentBlock::text(marker)]);
    state.deps.checkpoint(&conversation);
    Outcome::Ok(conversation, OutcomeStop::Reason(stop_reason))
}

pub(super) fn close_custom<D: TurnDeps>(
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
// the failed marker closes the Turn so roles keep alternating.
pub(super) fn fail<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    response: Response,
) -> Outcome {
    let mut blocks: Vec<ContentBlock> = response
        .content
        .iter()
        .filter(|b| !is_tool_use(b))
        .cloned()
        .collect();
    blocks.push(ContentBlock::text(voice::turn_failed_marker()));
    conversation.add_assistant_blocks(blocks);
    state.deps.checkpoint(&conversation);
    let reason = response.error.unwrap_or_default();
    Outcome::Failed(reason, conversation)
}

// The model stopped without (executable) Tool Calls. Usually the Turn ends here;
// a finish Nudge may send it back for one more Pass (Verify-failed > Verify >
// Empty). Any tool_use block in this branch is unanswered and is dropped.
pub(super) async fn finish<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    conversation: Conversation,
    blocks: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> Flow {
    // ADR-0015: on the final Pass a tool-insistent TEXT reply settles on the
    // turn-limit marker path, and the markup never enters the Conversation.
    if endgame::final_pass(state.pass, state.session.turn_limit)
        && endgame::tool_insistent_text(&blocks)
    {
        let reason = endgame::limit_stop_reason(&state.nudges);
        return Flow::Done(close(state, conversation, voice::turn_limit_marker(), reason));
    }

    do_finish(state, conversation, blocks, stop_reason).await
}

async fn do_finish<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    blocks: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> Flow {
    conversation.add_assistant_blocks(close_blocks(&blocks, &stop_reason));

    let closed = close_stop_reason(&stop_reason);
    let can_loop = endgame::can_loop(state.pass, state.session.turn_limit);
    let end_turn = closed == StopReason::EndTurn;

    if end_turn && can_loop && state.nudges.verify_failed_nudge() {
        state.nudges.note_verify_failed_nudged();
        return nudge_finish(
            state,
            conversation,
            voice::verify_failed_nudge(),
            VoicedTag::VerifyFailedNudge,
        )
        .await;
    }

    if end_turn && can_loop && state.nudges.verify_nudge() {
        state.nudges.note_verify_nudged();
        return nudge_finish(
            state,
            conversation,
            voice::verify_nudge(),
            VoicedTag::VerifyNudge,
        )
        .await;
    }

    if end_turn && can_loop && empty_content(&blocks) && state.nudges.empty_response_nudge() {
        // Arm the break-glass no-think rescue for the next Pass, gated by the
        // Session knob.
        state.nudges.arm_rescue(state.session.no_think_rescue);
        state.nudges.note_empty_response_nudged();
        return nudge_finish(
            state,
            conversation,
            voice::empty_response_nudge(),
            VoicedTag::EmptyResponseNudge,
        )
        .await;
    }

    Flow::Done(Outcome::Ok(
        conversation,
        outcome_stop_of(&close_stop_reason(&stop_reason)),
    ))
}

// Shared finish-Nudge mechanic: append the user-role Nudge, announce it, count
// it as a normal Pass against the Turn Limit, loop.
async fn nudge_finish<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    nudge: &str,
    tag: VoicedTag,
) -> Flow {
    conversation.add_user_text(nudge);
    state.emitter.emit(Event::voiced(tag, nudge));
    state.pass += 1;
    Flow::Continue(conversation)
}

// An empty response: zero content blocks once tool_use blocks are dropped, OR a
// parroted empty-response marker.
fn empty_content(blocks: &[ContentBlock]) -> bool {
    let kept: Vec<&ContentBlock> = blocks.iter().filter(|b| !is_tool_use(b)).collect();
    if kept.is_empty() {
        true
    } else {
        marker_parrot(&kept)
    }
}

fn marker_parrot(blocks: &[&ContentBlock]) -> bool {
    let all_text = blocks
        .iter()
        .all(|b| matches!(b, ContentBlock::Text { .. }));
    if !all_text {
        return false;
    }
    let joined = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    joined.trim() == voice::empty_response_marker()
}

fn close_blocks(blocks: &[ContentBlock], stop_reason: &StopReason) -> Vec<ContentBlock> {
    let kept: Vec<ContentBlock> = blocks
        .iter()
        .filter(|b| !is_tool_use(b))
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

// A phantom :tool_use stop ends the Turn like a normal completion.
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
