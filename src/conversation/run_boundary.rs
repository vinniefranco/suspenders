//! Run boundaries (CONTEXT.md: Run): the one rule that locates where a Run
//! begins in a Conversation slice. A Run is one user request and everything
//! the Agent does to answer it; a Conversation is a sequence of Runs. The
//! Run starts at a run-start user message - role User with a Text first
//! block. A standalone rider (text appended after tool results) is NOT a
//! boundary: its first block is a ToolResult, so a new user request always
//! opens with the request text itself.
//!
//! This rule has one owner so its two readers cannot drift: Compaction snaps
//! its cutoff back to the nearest run-start ([`run_start_indices`]), and
//! Supersession counts Runs to scope same-Run duplicate results
//! ([`is_run_start`]). Change the rule here and both follow.

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
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use serde_json::json;

    fn user(blocks: Vec<ContentBlock>) -> Message {
        Message::new(Role::User, blocks)
    }

    fn assistant(blocks: Vec<ContentBlock>) -> Message {
        Message::new(Role::Assistant, blocks)
    }

    #[test]
    fn run_start_user_message_is_a_boundary() {
        assert!(is_run_start(&user(vec![ContentBlock::text(
            "do the thing"
        )])));
    }

    #[test]
    fn standalone_rider_is_not_a_boundary() {
        // Text appended after a tool result: first block is a ToolResult, so a
        // rider never opens a Run even though it is a User message.
        let rider = user(vec![
            ContentBlock::tool_result("t1", "ok", false),
            ContentBlock::text("and also this"),
        ]);
        assert!(!is_run_start(&rider));
    }

    #[test]
    fn tool_result_first_user_message_is_not_a_boundary() {
        let results = user(vec![ContentBlock::tool_result("t1", "ok", false)]);
        assert!(!is_run_start(&results));
    }

    #[test]
    fn assistant_message_is_not_a_boundary() {
        assert!(!is_run_start(&assistant(vec![ContentBlock::text(
            "thinking out loud"
        )])));
        assert!(!is_run_start(&assistant(vec![ContentBlock::tool_use(
            "t1",
            "read_file",
            json!({}),
        )])));
    }

    #[test]
    fn empty_content_is_not_a_boundary() {
        assert!(!is_run_start(&user(vec![])));
        assert!(!is_run_start(&assistant(vec![])));
    }

    #[test]
    fn indices_pick_out_only_run_starts() {
        let messages = vec![
            user(vec![ContentBlock::text("first request")]), // 0: boundary
            assistant(vec![ContentBlock::tool_use("t1", "read_file", json!({}))]),
            user(vec![ContentBlock::tool_result("t1", "ok", false)]), // rider/results, not a boundary
            user(vec![ContentBlock::text("second request")]),         // 3: boundary
            assistant(vec![ContentBlock::text("done")]),
        ];
        let indices: Vec<usize> = run_start_indices(&messages).collect();
        assert_eq!(indices, vec![0, 3]);
    }

    #[test]
    fn indices_over_empty_slice_is_empty() {
        let indices: Vec<usize> = run_start_indices(&[]).collect();
        assert!(indices.is_empty());
    }
}
