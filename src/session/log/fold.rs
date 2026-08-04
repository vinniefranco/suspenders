//! The Resume fold: a decoded [`Entry`] stream folded into Conversation
//! [`Message`]s (ADR-0010), carved out of [`super`] so the parent module keeps
//! the wire codec + file lifecycle and this one owns "reconstruct the
//! Conversation".
//!
//! The fold mirrors the Loop's close rules by construction: answered tool_use
//! blocks are kept (ADR-0009), unanswered ones dropped (ADR-0004), a Compaction
//! entry collapses everything before it into the reconstructed summary, and a
//! log that ends mid-Run settles as failed. [`compose_summary`] is the single
//! source of the reconstructed summary text (the Compaction module reuses it),
//! so it stays `pub` and is re-exported from [`super`].

use crate::content::{ContentBlock, Message, Provenance, Role};
use crate::voice::{self, FileOps};

use super::{Entry, Settled, StopReason};

// The open tool batch: the last assistant_blocks (with the Provenance it was
// logged under) and the results/steering that followed it, pending until the
// batch closes - mirroring how the Loop builds the live Conversation.
struct Batch {
    blocks: Vec<ContentBlock>,
    provenance: Option<Provenance>,
    results: Vec<ContentBlock>,
    steering: Vec<String>,
}

pub(super) fn fold(entries: &[Entry]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    let mut batch: Option<Batch> = None;

    for entry in entries {
        fold_entry(entry, &mut messages, &mut batch);
    }

    // A log whose last entry is a settlement (or a Resume seed, written only at
    // open) is complete; anything else died mid-Run and settles as failed.
    match entries.last() {
        Some(Entry::Settled { .. }) | Some(Entry::Message(_)) => messages,
        _ => {
            if messages.is_empty() && batch.is_none() {
                Vec::new()
            } else {
                flush(&mut messages, batch);
                close_with(&mut messages, voice::Marker::RunFailed.text());
                messages
            }
        }
    }
}

fn fold_entry(entry: &Entry, messages: &mut Vec<Message>, batch: &mut Option<Batch>) {
    match entry {
        Entry::UserText(text) => {
            flush(messages, batch.take());
            messages.push(user_message(vec![text_block(text)]));
        }
        Entry::UserContent(blocks) => {
            flush(messages, batch.take());
            messages.push(user_message(blocks.clone()));
        }
        Entry::Message(message) => {
            flush(messages, batch.take());
            messages.push(message.clone());
        }
        // The Plan is held outside the Conversation, so it never becomes a
        // message and never disturbs an open tool batch.
        Entry::Plan(_) => {}
        // A malformed-tool-call re-draw (ADR-0030) is silent to the model's
        // Conversation: the failed draw produced nothing to keep, so the entry
        // is forensic only and never becomes a message or disturbs an open
        // batch - the re-issued request lands as the next assistant_blocks.
        Entry::Retry { .. } => {}
        Entry::AssistantBlocks { blocks, provenance } => {
            flush(messages, batch.take());
            *batch = Some(Batch {
                blocks: blocks.clone(),
                provenance: provenance.clone(),
                results: Vec::new(),
                steering: Vec::new(),
            });
        }
        Entry::ToolResult(block) => {
            // A stray tool_result with no open batch: corrupt tail; ignore.
            if let Some(b) = batch {
                b.results.push(block.clone());
            }
        }
        Entry::Steering(text) => {
            if let Some(b) = batch {
                b.steering.push(text.clone());
            }
        }
        Entry::Compacted {
            summary,
            file_ops,
            original_task,
            ..
        } => {
            // Compaction replaces everything folded before this point with the
            // reconstructed summary; reappend the harness-owned mechanical
            // facts so the message matches the live one.
            let composed = compose_summary(summary, original_task.as_deref(), file_ops);
            messages.clear();
            *batch = None;
            messages.push(user_message(vec![voice::summary_block(&composed)]));
        }
        Entry::Settled {
            outcome,
            stop_reason,
            ..
        } => {
            if let Some(open) = batch.take() {
                let stop = settle_stop(*outcome, *stop_reason);
                flush_batch(messages, open, stop);
            }
            close_settled(messages, *outcome, *stop_reason);
        }
    }
}

// The reconstructed compaction summary: the model's narrative plus the
// harness-owned mechanical facts. compose_summary lives here; the Compaction
// module reuses this exact composition (re-exported from `super`).
pub fn compose_summary(narrative: &str, original_task: Option<&str>, file_ops: &FileOps) -> String {
    format!(
        "{narrative}\n{}",
        voice::compaction_facts(original_task, file_ops)
    )
}

fn settle_stop(outcome: Settled, stop_reason: StopReason) -> StopReason {
    match outcome {
        Settled::Completed => stop_reason,
        _ => StopReason::Error, // stand-in for baud's `:failed` batch-close marker path
    }
}

fn flush(messages: &mut Vec<Message>, batch: Option<Batch>) {
    if let Some(batch) = batch {
        flush_batch(messages, batch, StopReason::EndTurn);
    }
}

// Close an open batch the way the Loop would have: keep tool_use blocks a
// result answered (ADR-0009 error answers included), drop the rest (ADR-0004),
// and never leave an empty assistant message.
fn flush_batch(messages: &mut Vec<Message>, batch: Batch, stop: StopReason) {
    let answered: std::collections::HashSet<&str> = batch
        .results
        .iter()
        .filter_map(|r| match r {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();

    let mut kept: Vec<ContentBlock> = batch
        .blocks
        .iter()
        .filter(|b| match b {
            ContentBlock::ToolUse { id, .. } => answered.contains(id.as_str()),
            _ => true,
        })
        .cloned()
        .collect();

    if kept.is_empty() {
        let marker = if stop == StopReason::MaxTokens {
            voice::Marker::Truncation
        } else {
            voice::Marker::EmptyResponse
        };
        kept = vec![text_block(marker.text())];
    }

    // The batch re-enters under the Provenance it was logged with, so a
    // resumed history normalizes at request-shaping exactly as the live one
    // would (ADR-0037).
    messages.push(Message {
        role: Role::Assistant,
        content: kept,
        provenance: batch.provenance,
    });

    let mut content = batch.results;
    content.extend(batch.steering.iter().map(|s| text_block(s)));
    if !content.is_empty() {
        messages.push(user_message(content));
    }
}

// A settled Run that ended on a user-role message (Run Limit, stop hook)
// closed with a marker live; restore it so roles keep alternating.
fn close_settled(messages: &mut Vec<Message>, outcome: Settled, stop_reason: StopReason) {
    match outcome {
        // A completed Run that ended on a user-role message (Run Limit, stop
        // hook) needs a fresh assistant marker so roles keep alternating; one
        // that ended on an assistant message needs nothing.
        Settled::Completed => {
            if matches!(messages.last(), Some(m) if m.role == Role::User) {
                let marker = voice::Marker::completing(stop_reason);
                messages.push(Message::assistant(vec![text_block(marker.text())]));
            }
        }
        Settled::Failed => close_with(messages, voice::Marker::RunFailed.text()),
        Settled::Cancelled => close_with(messages, voice::Marker::RunCancelled.text()),
    }
}

// Mirror the live fail path: the marker rides the trailing assistant message
// (the Loop appends kept text and marker as ONE message); a user-role tail gets
// a fresh assistant message, as Settlement does.
fn close_with(messages: &mut Vec<Message>, marker: &str) {
    let marker_block = text_block(marker);
    match messages.last_mut() {
        Some(last) if last.role == Role::Assistant => {
            if last.content.last() != Some(&marker_block) {
                last.content.push(marker_block);
            }
        }
        _ => messages.push(Message::assistant(vec![marker_block])),
    }
}

fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::user(content)
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_string(),
    }
}
