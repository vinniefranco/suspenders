//! The cross-Provider transform pass (ADR-0037, CONTEXT.md: Provenance).
//!
//! At request-shaping, each assistant message's Provenance is compared to the
//! target Model. Same Provider id AND Model id: verbatim replay. Anything
//! else - a different model, or unknown Provenance - is normalized so the
//! history satisfies the target Api:
//!
//! - Tool-call identifiers are rewritten to the target Api's alphabet and
//!   length limits, deterministically, with the matching Tool Result ids
//!   rewritten in the same pass (two passes over the messages: build the id
//!   map, then apply it everywhere).
//! - Orphaned Tool Calls - a `tool_use` with no matching `tool_result`
//!   anywhere after it - are answered with a synthetic error Tool Result in
//!   the Voice (the ADR-0004/0009 machinery, relocated here for histories
//!   crossing Providers; Settlement already prevents most orphans).
//!
//! Thinking never enters the Conversation in Suspenders, so there is no
//! thinking/signature handling here - the hardest part of pi's handoff is
//! deleted outright.
//!
//! Runs in ONE place: the Dispatcher's `complete`, before routing to the
//! adapter, so both adapters get it for free (deliberately unlike pi, which
//! calls it per-adapter). Pure functions, no I/O.

use std::collections::{HashMap, HashSet};

use crate::content::{
    ContentBlock, Message, Provenance, ResultBlock, unsupported_modality_placeholder,
};
use crate::llm::LlmRequest;
use crate::llm::model::{Api, Model};
use crate::voice;

/// The id length cap of the anthropic-messages Api (alphabet `[a-zA-Z0-9_-]`).
const ANTHROPIC_MAX_ID: usize = 64;
/// The id length cap of the openai-completions Api (same alphabet, capped
/// shorter: 40 chars).
const OPENAI_MAX_ID: usize = 40;

fn max_id_len(api: Api) -> usize {
    match api {
        Api::AnthropicMessages => ANTHROPIC_MAX_ID,
        Api::OpenaiCompletions => OPENAI_MAX_ID,
    }
}

/// [`normalize`] over a whole request: clones the request with its messages
/// normalized for `target`. The Dispatcher's one call site.
pub fn normalize_request(request: &LlmRequest, target: &Model) -> LlmRequest {
    let mut normalized = request.clone();
    normalized.messages = normalize(std::mem::take(&mut normalized.messages), target);
    normalized
}

/// The media-degrade pass (ADR-0059): replaces every Tool Result media block the
/// target Model cannot accept with the VERBATIM unsupported-modality placeholder
/// (a Text block). The cross-Model-history safety net - a request may carry media
/// a previous, capable Model produced, so this fires at wire-build time for the
/// Model actually receiving the request. A Model that supports the modality keeps
/// its media untouched. Pure; runs after [`normalize_request`] in the Dispatcher.
pub fn degrade_unsupported_media(mut request: LlmRequest, model: &Model) -> LlmRequest {
    for message in &mut request.messages {
        for block in &mut message.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                degrade_blocks(content, model);
            }
        }
    }
    request
}

// Rewrites a Tool Result's block list in place: an Image block degrades unless
// the Model accepts image; a Document (PDF) degrades unless it accepts pdf. The
// placeholder names the mime as the display name (the wire has no filename).
fn degrade_blocks(blocks: &mut [ResultBlock], model: &Model) {
    let modalities = model.input_modalities;
    for block in blocks.iter_mut() {
        let placeholder = match block {
            ResultBlock::Image { mime, .. } if !modalities.image => {
                Some(unsupported_modality_placeholder("image", mime))
            }
            ResultBlock::Document { mime, .. } if !modalities.pdf => {
                Some(unsupported_modality_placeholder("pdf", mime))
            }
            _ => None,
        };
        if let Some(text) = placeholder {
            *block = ResultBlock::text(text);
        }
    }
}

/// Normalizes `messages` for the target Model. Assistant messages whose
/// Provenance matches the target (Provider id AND Model id equal) pass
/// through verbatim; the rest get their tool-call ids rewritten to the
/// target Api's rules (matching Tool Result ids included) and their orphaned
/// Tool Calls answered in the Voice. Deterministic and pure.
pub fn normalize(messages: Vec<Message>, target: &Model) -> Vec<Message> {
    let target_provenance = target.provenance();
    let id_map = build_id_map(&messages, &target_provenance, max_id_len(target.api));
    let messages = apply_id_map(messages, &target_provenance, &id_map);
    answer_orphans(messages, &target_provenance)
}

// Does this assistant message replay verbatim? Only an exact Provenance match
// does; unknown Provenance is a mismatch (the message cannot prove it came
// from the target).
fn matches_target(message: &Message, target: &Provenance) -> bool {
    message.role == crate::content::Role::Assistant && message.provenance.as_ref() == Some(target)
}

// A mismatched assistant message whose tool ids must satisfy the target Api.
fn needs_rewrite(message: &Message, target: &Provenance) -> bool {
    message.role == crate::content::Role::Assistant && !matches_target(message, target)
}

// Pass 1: the old-id -> new-id map over every mismatched assistant message's
// tool_use blocks. Ids already valid for the target keep themselves (map
// entries mapping to self are harmless); occupied ids - matched messages'
// verbatim ids plus every id already assigned - never collide with a rewrite.
fn build_id_map(
    messages: &[Message],
    target: &Provenance,
    max_len: usize,
) -> HashMap<String, String> {
    let mut occupied: HashSet<String> = messages
        .iter()
        .filter(|m| matches_target(m, target))
        .flat_map(tool_use_ids)
        .map(str::to_string)
        .collect();

    let mut map = HashMap::new();
    for message in messages.iter().filter(|m| needs_rewrite(m, target)) {
        for id in tool_use_ids(message) {
            if map.contains_key(id) {
                continue;
            }
            let rewritten = rewrite_id(id, max_len, &occupied);
            occupied.insert(rewritten.clone());
            map.insert(id.to_string(), rewritten);
        }
    }
    map
}

fn tool_use_ids(message: &Message) -> impl Iterator<Item = &str> {
    message.content.iter().filter_map(|b| match b {
        ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
        _ => None,
    })
}

// One id, rewritten to the target Api's alphabet and length cap. The sanitized
// id keeps itself when it fits and is free; overflow or a collision takes a
// short-hash suffix derived from the ORIGINAL id, so the rewrite is
// deterministic across passes and sessions.
fn rewrite_id(id: &str, max_len: usize, occupied: &HashSet<String>) -> String {
    let sanitized = sanitize(id);
    if sanitized.len() <= max_len && !occupied.contains(&sanitized) {
        return sanitized;
    }
    let suffix = format!("_{:08x}", short_hash(id) as u32);
    let head: String = sanitized.chars().take(max_len - suffix.len()).collect();
    format!("{head}{suffix}")
}

// The shared id alphabet of both Apis: `[a-zA-Z0-9_-]`. Anything else
// becomes `_`.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// FNV-1a 64-bit offset basis (the starting hash value).
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime (the multiplier applied after each XOR).
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

// FNV-1a 64, inlined: a stable hash (std's DefaultHasher is not guaranteed
// stable across releases, and the rewrite must be deterministic across
// sessions so a resumed history renormalizes to the same ids).
fn short_hash(s: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// Pass 2: apply the map everywhere - tool_use ids in mismatched assistant
// messages, tool_result ids in EVERY message (the results answering a
// rewritten call live in user messages). Matched assistant messages replay
// verbatim: their ids are never in the map.
fn apply_id_map(
    messages: Vec<Message>,
    target: &Provenance,
    id_map: &HashMap<String, String>,
) -> Vec<Message> {
    if id_map.is_empty() {
        return messages;
    }
    messages
        .into_iter()
        .map(|mut message| {
            let rewrite_uses = needs_rewrite(&message, target);
            for block in &mut message.content {
                remap_block(block, id_map, rewrite_uses);
            }
            message
        })
        .collect()
}

fn remap_block(block: &mut ContentBlock, id_map: &HashMap<String, String>, rewrite_uses: bool) {
    match block {
        ContentBlock::ToolUse { id, .. } if rewrite_uses => {
            if let Some(new) = id_map.get(id) {
                *id = new.clone();
            }
        }
        ContentBlock::ToolResult { tool_use_id, .. } => {
            if let Some(new) = id_map.get(tool_use_id) {
                *tool_use_id = new.clone();
            }
        }
        _ => {}
    }
}

// The transform's own orphan guarantee: every tool_use in a MISMATCHED
// assistant message must have a tool_result somewhere after it, or the next
// request is rejected by strict servers. Orphans are answered with the
// Voice's synthetic error result, inserted into the following user message
// (after its existing Tool Results, before any text riders) or as a fresh
// user message when none follows.
fn answer_orphans(mut messages: Vec<Message>, target: &Provenance) -> Vec<Message> {
    let mut index = 0;
    while index < messages.len() {
        if needs_rewrite(&messages[index], target) {
            let orphans = orphan_ids(&messages, index);
            if !orphans.is_empty() {
                insert_answers(&mut messages, index, &orphans);
            }
        }
        index += 1;
    }
    messages
}

// The tool_use ids of `messages[index]` with no matching tool_result in any
// later message, in call order.
fn orphan_ids(messages: &[Message], index: usize) -> Vec<String> {
    let answered: HashSet<&str> = messages[index + 1..]
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    tool_use_ids(&messages[index])
        .filter(|id| !answered.contains(id))
        .map(str::to_string)
        .collect()
}

// Insert the synthetic error results for `orphans` right where their batch's
// results belong: into the user message following `index` (after its existing
// Tool Results), or as a new user message when the assistant message has no
// following user message.
fn insert_answers(messages: &mut Vec<Message>, index: usize, orphans: &[String]) {
    let answers = orphans
        .iter()
        .map(|id| ContentBlock::tool_result(id, voice::orphaned_call_answer(), true));
    match messages.get_mut(index + 1) {
        Some(next) if next.role == crate::content::Role::User => {
            let at = next
                .content
                .iter()
                .rposition(|b| matches!(b, ContentBlock::ToolResult { .. }))
                .map(|i| i + 1)
                .unwrap_or(0);
            next.content.splice(at..at, answers);
        }
        _ => messages.insert(index + 1, Message::user(answers.collect())),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/llm/transform.rs"]
mod tests;
