//! Run boundaries (CONTEXT.md: Run): the one rule that locates where a Run
//! begins in a Conversation slice. A Run is one user request and everything
//! the Agent does to answer it; a Conversation is a sequence of Runs. The
//! Run starts at a run-start user message - role User with a Text first
//! block. A standalone rider (text appended after tool results) is NOT a
//! boundary: its first block is a ToolResult, so a new user request always
//! opens with the request text itself.
//!
//! This rule has one owner so it cannot drift: Compaction snaps its cutoff
//! back to the nearest run-start ([`run_start_indices`]). Change the rule here
//! and Compaction follows.

use crate::content::{ContentBlock, Message, Role};

/// Does this message open a Run? True for a run-start user message - role
/// User whose first content block is Text (the user's request). False for a
/// standalone rider (a User message whose first block is a ToolResult), an
/// assistant message, and an empty message.
pub fn is_run_start(message: &Message) -> bool {
    message.role == Role::User && matches!(message.content.first(), Some(ContentBlock::Text { .. }))
}

/// The message indices at which Runs begin, in order. Each index is a
/// run-start user message ([`is_run_start`]); the count is the number of
/// Runs the slice contains.
pub fn run_start_indices(messages: &[Message]) -> impl Iterator<Item = usize> + '_ {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_run_start(m))
        .map(|(idx, _)| idx)
}

#[cfg(test)]
#[path = "../../tests/conversation/run_boundary.rs"]
mod tests;
