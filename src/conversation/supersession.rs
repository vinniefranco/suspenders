//! Supersession (CONTEXT.md): the rule that classifies Conversation content
//! as dead. Two classifiers, both mechanical - what they classify is correct
//! or incorrect, never a judgment call:
//!
//! * A successful write's input body is dead once its result lands: the file
//!   on disk holds the result. A FAILED edit's input is NOT dead - the model
//!   may need to see what it tried against the error - until a later
//!   successful write to the same file supersedes the attempt chain.
//! * A run_command or read_file Tool Call identical to a LATER call in the
//!   same Turn - full `(name, input)` equality, the same identity the
//!   duplicate Governor uses - leaves its older result dead. The newest
//!   result always survives verbatim.
//!
//! The recency guard is symmetric: the last two tool-result-bearing user
//! messages AND the assistant tool_use blocks paired to their results are
//! untouchable, so classification never reaches into the exchanges the model
//! is actively working against.

use serde_json::Value;

use crate::content::{ContentBlock, Message, Role};
use crate::voice;

// The write Tools whose landed input the file on disk supersedes. Mirrors the
// duplicate Governor's list; kept local so the Conversation core carries no
// turn dependency.
const WRITE_TOOLS: &[&str] = &["edit_file", "write_file"];

/// One dead block: where it sits and which husk replaces it at wave time.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Dead {
    /// A write's input body - the file on disk holds the result.
    WriteInput {
        msg_index: usize,
        block_index: usize,
        path: Option<String>,
    },
    /// A Tool Result superseded by a later identical call in the same Turn.
    Result {
        msg_index: usize,
        block_index: usize,
        marker: &'static str,
    },
}

impl Dead {
    pub(super) fn position(&self) -> (usize, usize) {
        match self {
            Dead::WriteInput {
                msg_index,
                block_index,
                ..
            }
            | Dead::Result {
                msg_index,
                block_index,
                ..
            } => (*msg_index, *block_index),
        }
    }
}

/// Every dead block outside the recency guard, in Conversation order.
pub(super) fn dead_blocks(messages: &[Message]) -> Vec<Dead> {
    let calls = collect_calls(messages);
    let guard = RecencyGuard::over(messages);

    let mut dead: Vec<Dead> = Vec::new();
    dead.extend(dead_write_inputs(&calls, &guard));
    dead.extend(superseded_results(&calls, &guard));
    dead.sort_by_key(Dead::position);
    dead
}

/// The Dead Mass measure (CONTEXT.md: Dead Mass): the estimated chars a wave
/// would reclaim from the given dead blocks.
pub(super) fn dead_chars(messages: &[Message], dead: &[Dead]) -> u64 {
    dead.iter()
        .map(|d| {
            let (msg_index, block_index) = d.position();
            super::block_chars(&messages[msg_index].content[block_index]) as u64
        })
        .sum()
}

// One Tool Call joined to its landed Tool Result, positioned for oldest-first
// ordering and Turn scoping.
struct Call<'a> {
    msg_index: usize,
    block_index: usize,
    turn: usize,
    id: &'a str,
    name: &'a str,
    input: &'a Value,
    result: Option<Landed<'a>>,
}

struct Landed<'a> {
    msg_index: usize,
    block_index: usize,
    is_error: bool,
    content: &'a str,
}

impl Call<'_> {
    fn position(&self) -> (usize, usize) {
        (self.msg_index, self.block_index)
    }
}

fn collect_calls(messages: &[Message]) -> Vec<Call<'_>> {
    let mut calls: Vec<Call<'_>> = Vec::new();
    // Turn boundary: a user message opening with text (the same predicate
    // Compaction's cutoff uses). A standalone rider can read as a boundary;
    // that only narrows same-Turn supersession, never widens it.
    let mut turn = 0usize;
    for (msg_index, message) in messages.iter().enumerate() {
        if message.role == Role::User
            && matches!(message.content.first(), Some(ContentBlock::Text { .. }))
        {
            turn += 1;
        }
        for (block_index, block) in message.content.iter().enumerate() {
            if let ContentBlock::ToolUse { id, name, input } = block {
                calls.push(Call {
                    msg_index,
                    block_index,
                    turn,
                    id,
                    name,
                    input,
                    result: None,
                });
            }
        }
    }
    for (msg_index, message) in messages.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
                && let Some(call) = calls.iter_mut().find(|c| c.id == tool_use_id)
            {
                call.result = Some(Landed {
                    msg_index,
                    block_index,
                    is_error: *is_error,
                    content,
                });
            }
        }
    }
    calls
}

// The recency guard, extended symmetrically over both sides of an exchange:
// the last two tool-result-bearing user messages and the tool_use blocks
// their results answer.
struct RecencyGuard {
    result_messages: Vec<usize>,
    call_ids: Vec<String>,
}

impl RecencyGuard {
    fn over(messages: &[Message]) -> Self {
        let result_messages: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.role == Role::User
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .map(|(idx, _)| idx)
            .rev()
            .take(2)
            .collect();
        let call_ids = result_messages
            .iter()
            .flat_map(|&idx| {
                messages[idx].content.iter().filter_map(|b| match b {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                    _ => None,
                })
            })
            .collect();
        RecencyGuard {
            result_messages,
            call_ids,
        }
    }

    fn protects_result(&self, msg_index: usize) -> bool {
        self.result_messages.contains(&msg_index)
    }

    fn protects_call(&self, id: &str) -> bool {
        self.call_ids.iter().any(|c| c == id)
    }
}

fn dead_write_inputs(calls: &[Call<'_>], guard: &RecencyGuard) -> Vec<Dead> {
    calls
        .iter()
        .filter(|call| WRITE_TOOLS.contains(&call.name))
        .filter(|call| !voice::is_write_input_husk(call.input))
        .filter(|call| !guard.protects_call(call.id))
        .filter(|call| write_input_is_dead(call, calls))
        .map(|call| Dead::WriteInput {
            msg_index: call.msg_index,
            block_index: call.block_index,
            path: path_of(call.input),
        })
        .collect()
}

// A landed write's input is dead; a failed attempt's input stays live against
// its error until a later successful write to the same file supersedes the
// attempt chain. (A husked later write still supersedes: its path survives.)
fn write_input_is_dead(call: &Call<'_>, calls: &[Call<'_>]) -> bool {
    match &call.result {
        None => false,
        Some(result) if !result.is_error => true,
        Some(_) => match path_of(call.input) {
            None => false,
            Some(path) => calls.iter().any(|later| {
                later.position() > call.position()
                    && WRITE_TOOLS.contains(&later.name)
                    && later.result.as_ref().is_some_and(|r| !r.is_error)
                    && path_of(later.input).as_deref() == Some(path.as_str())
            }),
        },
    }
}

fn superseded_results(calls: &[Call<'_>], guard: &RecencyGuard) -> Vec<Dead> {
    calls
        .iter()
        .filter_map(|call| {
            let marker = match call.name {
                "run_command" => voice::superseded_command_marker(),
                "read_file" => voice::superseded_read_marker(),
                _ => return None,
            };
            let result = call.result.as_ref()?;
            if guard.protects_result(result.msg_index)
                || result.content == marker
                || result.content == voice::elision_marker()
            {
                return None;
            }
            let superseded = calls.iter().any(|later| {
                later.position() > call.position()
                    && later.turn == call.turn
                    && later.name == call.name
                    && later.input == call.input
                    && later.result.is_some()
            });
            superseded.then_some(Dead::Result {
                msg_index: result.msg_index,
                block_index: result.block_index,
                marker,
            })
        })
        .collect()
}

fn path_of(input: &Value) -> Option<String> {
    input
        .get("path")
        .and_then(|p| p.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
        ContentBlock::tool_use(id, name, input)
    }

    fn result(id: &str, content: &str) -> ContentBlock {
        ContentBlock::tool_result(id, content, false)
    }

    fn error(id: &str, content: &str) -> ContentBlock {
        ContentBlock::tool_result(id, content, true)
    }

    // One exchange: an assistant tool_use message and its results message.
    fn exchange(call: ContentBlock, res: ContentBlock) -> [Message; 2] {
        [Message::assistant(vec![call]), Message::user(vec![res])]
    }

    // Enough trailing exchanges that everything before them sits outside the
    // recency guard.
    fn guard_tail(from: usize) -> Vec<Message> {
        let mut tail = Vec::new();
        for i in from..from + 2 {
            let id = format!("guard{i}");
            let [a, b] = exchange(
                tool_use(&id, "list_files", json!({"path": format!("d{i}")})),
                result(&id, "listing"),
            );
            tail.push(a);
            tail.push(b);
        }
        tail
    }

    fn conversation(exchanges: Vec<[Message; 2]>) -> Vec<Message> {
        let mut messages = vec![Message::user(vec![ContentBlock::text("go")])];
        for [a, b] in exchanges {
            messages.push(a);
            messages.push(b);
        }
        messages.extend(guard_tail(90));
        messages
    }

    #[test]
    fn a_landed_edit_input_is_dead_with_its_path_preserved() {
        let messages = conversation(vec![exchange(
            tool_use(
                "t1",
                "edit_file",
                json!({"path": "src/lib.rs", "old_str": "a", "new_str": "b"}),
            ),
            result("t1", "ok"),
        )]);

        assert_eq!(
            dead_blocks(&messages),
            vec![Dead::WriteInput {
                msg_index: 1,
                block_index: 0,
                path: Some("src/lib.rs".to_string()),
            }]
        );
    }

    #[test]
    fn a_failed_edit_input_stays_live_against_its_error() {
        let messages = conversation(vec![exchange(
            tool_use(
                "t1",
                "edit_file",
                json!({"path": "src/lib.rs", "old_str": "a", "new_str": "b"}),
            ),
            error("t1", "old_str not found"),
        )]);

        assert_eq!(dead_blocks(&messages), vec![]);
    }

    #[test]
    fn a_later_successful_write_to_the_same_file_supersedes_the_attempt_chain() {
        let messages = conversation(vec![
            exchange(
                tool_use(
                    "t1",
                    "edit_file",
                    json!({"path": "src/lib.rs", "old_str": "a", "new_str": "b"}),
                ),
                error("t1", "old_str not found"),
            ),
            exchange(
                tool_use(
                    "t2",
                    "write_file",
                    json!({"path": "src/lib.rs", "content": "b"}),
                ),
                result("t2", "ok"),
            ),
        ]);

        let dead = dead_blocks(&messages);
        assert_eq!(dead.len(), 2);
        assert_eq!(dead[0].position(), (1, 0)); // the failed attempt
        assert_eq!(dead[1].position(), (3, 0)); // the landed write
    }

    #[test]
    fn a_later_successful_write_to_a_different_file_supersedes_nothing() {
        let messages = conversation(vec![
            exchange(
                tool_use(
                    "t1",
                    "edit_file",
                    json!({"path": "src/lib.rs", "old_str": "a", "new_str": "b"}),
                ),
                error("t1", "old_str not found"),
            ),
            exchange(
                tool_use(
                    "t2",
                    "write_file",
                    json!({"path": "src/other.rs", "content": "b"}),
                ),
                result("t2", "ok"),
            ),
        ]);

        let dead = dead_blocks(&messages);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].position(), (3, 0)); // only the landed write
    }

    #[test]
    fn a_husked_input_is_never_classified_again() {
        let messages = conversation(vec![exchange(
            tool_use("t1", "edit_file", voice::write_input_husk(Some("a.rs"))),
            result("t1", "ok"),
        )]);

        assert_eq!(dead_blocks(&messages), vec![]);
    }

    #[test]
    fn a_repeated_run_command_leaves_older_results_dead_newest_survives() {
        let cmd = json!({"command": "cargo test"});
        let messages = conversation(vec![
            exchange(
                tool_use("t1", "run_command", cmd.clone()),
                error("t1", "FAILED"),
            ),
            exchange(
                tool_use("t2", "run_command", cmd.clone()),
                error("t2", "FAILED"),
            ),
            exchange(
                tool_use("t3", "run_command", cmd.clone()),
                result("t3", "ok"),
            ),
        ]);

        assert_eq!(
            dead_blocks(&messages),
            vec![
                Dead::Result {
                    msg_index: 2,
                    block_index: 0,
                    marker: voice::superseded_command_marker(),
                },
                Dead::Result {
                    msg_index: 4,
                    block_index: 0,
                    marker: voice::superseded_command_marker(),
                },
            ]
        );
    }

    #[test]
    fn read_file_identity_is_the_full_input_a_different_range_does_not_supersede() {
        let messages = conversation(vec![
            exchange(
                tool_use("t1", "read_file", json!({"path": "a.rs", "start_line": 1})),
                result("t1", "head"),
            ),
            exchange(
                tool_use("t2", "read_file", json!({"path": "a.rs", "start_line": 50})),
                result("t2", "tail"),
            ),
        ]);

        assert_eq!(dead_blocks(&messages), vec![]);
    }

    #[test]
    fn an_identical_read_file_supersedes_the_older_result() {
        let read = json!({"path": "a.rs"});
        let messages = conversation(vec![
            exchange(
                tool_use("t1", "read_file", read.clone()),
                result("t1", "v1"),
            ),
            exchange(
                tool_use("t2", "read_file", read.clone()),
                result("t2", "v2"),
            ),
        ]);

        assert_eq!(
            dead_blocks(&messages),
            vec![Dead::Result {
                msg_index: 2,
                block_index: 0,
                marker: voice::superseded_read_marker(),
            }]
        );
    }

    #[test]
    fn an_identical_call_in_a_different_turn_does_not_supersede() {
        let cmd = json!({"command": "cargo test"});
        let mut messages = vec![Message::user(vec![ContentBlock::text("turn one")])];
        let [a, b] = exchange(
            tool_use("t1", "run_command", cmd.clone()),
            result("t1", "r1"),
        );
        messages.push(a);
        messages.push(b);
        // A new Turn: a user message opening with text.
        messages.push(Message::user(vec![ContentBlock::text("turn two")]));
        let [a, b] = exchange(
            tool_use("t2", "run_command", cmd.clone()),
            result("t2", "r2"),
        );
        messages.push(a);
        messages.push(b);
        messages.extend(guard_tail(90));

        assert_eq!(dead_blocks(&messages), vec![]);
    }

    #[test]
    fn the_recency_guard_protects_results_and_their_paired_inputs() {
        // The landed edit and the repeated command both sit inside the last
        // two exchanges: nothing is classified.
        let cmd = json!({"command": "cargo test"});
        let mut messages = vec![Message::user(vec![ContentBlock::text("go")])];
        let [a, b] = exchange(
            tool_use("t1", "run_command", cmd.clone()),
            error("t1", "FAILED"),
        );
        messages.push(a);
        messages.push(b);
        let [a, b] = exchange(
            tool_use(
                "t2",
                "edit_file",
                json!({"path": "a.rs", "old_str": "x", "new_str": "y"}),
            ),
            result("t2", "ok"),
        );
        messages.push(a);
        messages.push(b);

        assert_eq!(dead_blocks(&messages), vec![]);

        // With two more exchanges appended, the guard moves on and both
        // become classifiable - plus t3's older duplicate of the command.
        let [a, b] = exchange(
            tool_use("t3", "run_command", cmd.clone()),
            result("t3", "ok"),
        );
        messages.push(a);
        messages.push(b);
        let [a, b] = exchange(
            tool_use("t4", "list_files", json!({"path": "."})),
            result("t4", "listing"),
        );
        messages.push(a);
        messages.push(b);

        let dead = dead_blocks(&messages);
        assert_eq!(dead.len(), 2);
        assert!(matches!(dead[0], Dead::Result { msg_index: 2, .. }));
        assert!(matches!(dead[1], Dead::WriteInput { msg_index: 3, .. }));
    }

    #[test]
    fn dead_chars_measures_the_blocks_a_wave_would_reclaim() {
        let big_edit = "x".repeat(500);
        let messages = conversation(vec![exchange(
            tool_use(
                "t1",
                "edit_file",
                json!({"path": "a.rs", "old_str": big_edit, "new_str": "y"}),
            ),
            result("t1", "ok"),
        )]);

        let dead = dead_blocks(&messages);
        assert!(dead_chars(&messages, &dead) > 500);
    }
}
