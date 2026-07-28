//! Run finish - how a Run ends when the model stops calling tools (carved
//! from the Run Loop). Deliberately NOT named "settlement": Run Settlement
//! (CONTEXT.md, [`super::settlement`]) is how an already-ended Run enters the
//! Conversation; this module is the ending itself.
//!
//! [`finish`] handles a Pass without (executable) Tool Calls: the model's
//! reply concludes the Run. The marker algebra: an empty close gets the
//! empty-response marker (or the truncation marker on max_tokens), and the Run
//! Limit / stopped / failed markers keep roles alternating when the Loop closes
//! a Run itself. The LLM error algebra ([`fail`]): partial text survives,
//! unanswered tool_use blocks are dropped, and the failed marker closes the
//! Run.

use crate::content::ContentBlock;
use crate::conversation::Conversation;
use crate::llm::response::{Response, StopReason};
use crate::run::deps::RunDeps;
use crate::run::loop_::{Flow, LoopState, Outcome, OutcomeStop};
use crate::session::log;
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

// The model stopped without (executable) Tool Calls: the reply concludes the
// Run. The reply is stamped with the Run's captured Provenance (ADR-0037); an
// empty reply gets the marker instead (`close_blocks`). Any tool_use block in
// this branch is unanswered and is dropped.
pub(super) fn finish<D: RunDeps>(
    state: &mut LoopState<'_, D>,
    mut conversation: Conversation,
    blocks: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> Flow {
    let closed = close_stop_reason(&stop_reason);
    let provenance = state.deps.provenance();
    conversation.add_assistant_response(close_blocks(&blocks, &stop_reason), provenance);
    Flow::Done(Outcome::Ok(conversation, outcome_stop_of(&closed)))
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
