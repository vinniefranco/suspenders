//! Turn finish — how a Turn ends when the model stops calling tools (carved
//! from the Turn Loop port of baud's `Baud.Turn.Loop`). Deliberately NOT named
//! "settlement": Turn Settlement (CONTEXT.md, [`super::settlement`]) is how an
//! already-ended Turn enters the Conversation; this module is the ending
//! itself.
//!
//! [`finish`] handles a Pass without (executable) Tool Calls. Usually the Turn
//! ends there, but the finish-settlement arbiter ([`super::governor`],
//! ADR-0026) may intervene instead: close the Turn on the turn-limit marker
//! (ADR-0015's tool-insistence rule) or send the model back for one more Pass
//! with a stand-alone Nudge — the strict Verify-failed > Verify > Empty
//! precedence lives in [`governor::settle_finish`], not here. This module
//! keeps the effects: appending blocks, announcing the Nudge, closing.
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
use crate::turn::governor::{self, FinishIntervention};
use crate::turn::loop_::{Flow, LoopState, Outcome, OutcomeStop};
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

// The close-and-open-a-Recovery-Turn Intervention's close half: like
// [`close`], but the outcome carries the Endgame Governor's directive out to
// the Agent, which executes the opening (CONTEXT.md: Recovery Turn). One
// author for both recovery closes: `closing` is the turn-limit marker at the
// tool-answering cap and on the tool-insistent reply (roles keep
// alternating; the insistent markup never enters), or the model's own
// final-Pass reply on the text settle (ADR-0028 addendum).
pub(super) fn close_recover<D: TurnDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    closing: Vec<ContentBlock>,
    stop_reason: log::StopReason,
    recovery: governor::endgame::Recovery,
) -> Outcome {
    conversation.add_assistant_blocks(closing);
    state.deps.checkpoint(&conversation);
    Outcome::Recover(conversation, stop_reason, recovery)
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
        .filter(|b| !b.is_tool_use())
        .cloned()
        .collect();
    blocks.push(ContentBlock::text(voice::turn_failed_marker()));
    conversation.add_assistant_blocks(blocks);
    state.deps.checkpoint(&conversation);
    let reason = response.error.unwrap_or_default();
    Outcome::Failed(reason, conversation)
}

// The model stopped without (executable) Tool Calls. The finish-settlement
// arbiter (ADR-0026) decides how the finish settles; this site translates:
// a Close appends the turn-limit marker (the reply — ADR-0015's insistent
// markup — never enters the Conversation), a CloseRecover appends the marker
// or — `keep_reply`, the final-Pass text settle — the reply itself before
// carrying the recovery directive out, a Standalone Nudge appends the reply
// and then the user-role Nudge for one more Pass, and no Intervention
// concludes the Turn on the reply. Any tool_use block in this branch is
// unanswered and is dropped.
pub(super) fn finish<D: TurnDeps>(
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
            voice::turn_limit_marker(),
            reason,
        )),
        Some(FinishIntervention::CloseRecover {
            reason,
            recovery,
            keep_reply,
        }) => {
            let closing = if keep_reply {
                close_blocks(&blocks, &stop_reason)
            } else {
                vec![ContentBlock::text(voice::turn_limit_marker())]
            };
            Flow::Done(close_recover(
                state,
                conversation,
                closing,
                reason,
                recovery,
            ))
        }
        Some(FinishIntervention::Standalone { tag, text }) => {
            conversation.add_assistant_blocks(close_blocks(&blocks, &stop_reason));
            nudge_finish(state, conversation, &text, tag)
        }
        None => {
            conversation.add_assistant_blocks(close_blocks(&blocks, &stop_reason));
            Flow::Done(Outcome::Ok(conversation, outcome_stop_of(&closed)))
        }
    }
}

// Shared finish-Nudge mechanic: append the user-role Nudge, announce it, count
// it as a normal Pass against the Turn Limit, loop.
fn nudge_finish<D: TurnDeps>(
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
