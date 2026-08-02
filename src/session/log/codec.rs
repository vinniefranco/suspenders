//! The Session Log wire codec (ADR-0010): [`Entry`] <-> JSON with the `"e"`
//! discriminator and baud's field names, carved out of [`super`] so the parent
//! keeps the entry vocabulary + file lifecycle + Resume fold and this owns
//! encode/decode. A human can grep/diff the log, the load-bearing thesis of
//! ADR-0010.
//!
//! [`Entry::to_json`] and [`Entry::from_json`] are `pub(super)` (the file
//! lifecycle and the fold reach them); [`decode_line`] is too (the picker and
//! the resume fold parse header + entry lines through it). Everything else -
//! the per-kind parsers and the field/provenance/role helpers - is private to
//! this codec.

use crate::content::{ContentBlock, Message, Provenance, Role};
use crate::voice::FileOps;

use super::{Entry, Settled, StopReason};

impl Entry {
    pub(super) fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Entry::UserText(text) => json!({"e": "user_text", "text": text}),
            Entry::Steering(text) => json!({"e": "steering", "text": text}),
            Entry::Plan(text) => json!({"e": "plan", "text": text}),
            Entry::AssistantBlocks { blocks, provenance } => {
                let mut value = json!({"e": "assistant_blocks", "blocks": blocks});
                write_provenance(&mut value, provenance.as_ref());
                value
            }
            Entry::ToolResult(block) => json!({"e": "tool_result", "block": block}),
            Entry::Message(message) => {
                let mut value = json!({
                    "e": "message",
                    "role": role_str(message.role),
                    "content": message.content,
                });
                write_provenance(&mut value, message.provenance.as_ref());
                value
            }
            Entry::Settled {
                outcome,
                stop_reason,
                reason,
            } => json!({
                "e": "settled",
                "outcome": outcome.as_str(),
                "stop_reason": stop_reason.as_str(),
                "reason": reason,
            }),
            Entry::Compacted {
                summary,
                skip_count,
                tokens_before,
                file_ops,
                original_task,
            } => json!({
                "e": "compacted",
                "summary": summary,
                "skip_count": skip_count,
                "tokens_before": tokens_before,
                "read_files": file_ops.read_files,
                "modified_files": file_ops.modified_files,
                "original_task": original_task,
            }),
            Entry::Retry {
                error,
                attempt,
                budget,
            } => json!({
                "e": "retry",
                "error": error,
                "attempt": attempt,
                "budget": budget,
            }),
        }
    }

    // Decode a JSON object into an entry. `None` means "valid JSON but not a
    // valid entry shape" - the fold stops there, like a torn line.
    pub(super) fn from_json(m: &serde_json::Value) -> Option<Entry> {
        let e = m.get("e")?.as_str()?;
        match e {
            "user_text" => Some(Entry::UserText(string_field(m, "text")?)),
            "steering" => Some(Entry::Steering(string_field(m, "text")?)),
            "plan" => Some(Entry::Plan(string_field(m, "text")?)),
            "assistant_blocks" => parse_assistant_blocks(m),
            "tool_result" => parse_tool_result(m),
            "message" => parse_message(m),
            "settled" => parse_settled(m),
            "compacted" => parse_compacted(m),
            "retry" => parse_retry(m),
            _ => None,
        }
    }
}

// Per-kind entry parsers. Each returns `None` on a shape mismatch - the same
// torn-line tolerance `from_json` carries to the fold.

fn parse_assistant_blocks(m: &serde_json::Value) -> Option<Entry> {
    let blocks = decode_blocks(m.get("blocks")?)?;
    Some(Entry::AssistantBlocks {
        blocks,
        provenance: read_provenance(m),
    })
}

fn parse_tool_result(m: &serde_json::Value) -> Option<Entry> {
    let block: ContentBlock = serde_json::from_value(m.get("block")?.clone()).ok()?;
    Some(Entry::ToolResult(block))
}

fn parse_message(m: &serde_json::Value) -> Option<Entry> {
    let role = decode_role(m.get("role")?.as_str()?)?;
    let content = decode_blocks(m.get("content")?)?;
    Some(Entry::Message(Message {
        role,
        content,
        provenance: read_provenance(m),
    }))
}

// The Provenance codec, shared by the assistant_blocks and message entries:
// two flat keys beside the entry's own. Absent keys decode as `None`
// (unknown Provenance) with the same optional-field tolerance the settled
// entry's `reason` takes - the transform treats unknown as a mismatch, so a
// missing stamp degrades to normalization, never to a torn line.
fn write_provenance(value: &mut serde_json::Value, provenance: Option<&Provenance>) {
    if let (Some(p), Some(obj)) = (provenance, value.as_object_mut()) {
        obj.insert("provider".into(), p.provider.clone().into());
        obj.insert("model".into(), p.model.clone().into());
    }
}

fn read_provenance(m: &serde_json::Value) -> Option<Provenance> {
    Some(Provenance::new(
        m.get("provider")?.as_str()?,
        m.get("model")?.as_str()?,
    ))
}

fn parse_settled(m: &serde_json::Value) -> Option<Entry> {
    let outcome = Settled::from_str(m.get("outcome")?.as_str()?)?;
    let stop_reason = StopReason::from_str(m.get("stop_reason")?.as_str()?);
    // The old 3-element form has no "reason" key; it decodes as
    // None, the same tolerance the compacted entry took.
    let reason = m
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(Entry::Settled {
        outcome,
        stop_reason,
        reason,
    })
}

fn parse_compacted(m: &serde_json::Value) -> Option<Entry> {
    let file_ops = FileOps {
        read_files: decode_str_list(m.get("read_files")),
        modified_files: decode_str_list(m.get("modified_files")),
    };
    Some(Entry::Compacted {
        summary: string_field(m, "summary").unwrap_or_default(),
        skip_count: m.get("skip_count").and_then(|v| v.as_u64()).unwrap_or(0),
        tokens_before: m.get("tokens_before").and_then(|v| v.as_u64()).unwrap_or(0),
        file_ops,
        original_task: m
            .get("original_task")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

fn parse_retry(m: &serde_json::Value) -> Option<Entry> {
    Some(Entry::Retry {
        error: string_field(m, "error")?,
        attempt: m.get("attempt")?.as_u64()?,
        budget: m.get("budget")?.as_u64()?,
    })
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn decode_role(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn string_field(m: &serde_json::Value, key: &str) -> Option<String> {
    Some(m.get(key)?.as_str()?.to_string())
}

fn decode_blocks(v: &serde_json::Value) -> Option<Vec<ContentBlock>> {
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        out.push(serde_json::from_value(item.clone()).ok()?);
    }
    Some(out)
}

fn decode_str_list(v: Option<&serde_json::Value>) -> Vec<String> {
    match v.and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect(),
        None => Vec::new(),
    }
}

// A log line parsed to a JSON object, or `None` for a non-object / torn line
// (the header and every entry line pass through here before decode).
pub(super) fn decode_line(line: &str) -> Option<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) if v.is_object() => Some(v),
        _ => None,
    }
}
