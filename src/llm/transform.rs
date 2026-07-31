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

// FNV-1a 64, inlined: a stable hash (std's DefaultHasher is not guaranteed
// stable across releases, and the rewrite must be deterministic across
// sessions so a resumed history renormalizes to the same ids).
fn short_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
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
mod tests {
    use super::*;
    use crate::content::{Message, Role};
    use serde_json::json;

    fn target(api: Api) -> Model {
        Model::new("anthropic", "claude-fable-5", api, 200_000, 8_000)
    }

    fn own(model: &Model) -> Provenance {
        model.provenance()
    }

    fn other() -> Provenance {
        Provenance::new("lmstudio", "qwen3.6-27b")
    }

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock::tool_use(id, "read_file", json!({"path": "a.rs"}))
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::tool_result(id, "ok", false)
    }

    fn result_ids(message: &Message) -> Vec<&str> {
        message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect()
    }

    fn use_ids(message: &Message) -> Vec<&str> {
        tool_use_ids(message).collect()
    }

    // ---- verbatim replay ----

    #[test]
    fn a_matching_provenance_passes_verbatim_even_with_ids_the_api_rejects() {
        let model = target(Api::OpenaiCompletions);
        let wild_id = "x".repeat(80);
        let messages = vec![
            Message::user(vec![ContentBlock::text("go")]),
            Message::assistant_from(vec![tool_use(&wild_id)], own(&model)),
            Message::user(vec![tool_result(&wild_id)]),
        ];
        assert_eq!(normalize(messages.clone(), &model), messages);
    }

    #[test]
    fn user_messages_and_matching_text_replies_pass_untouched() {
        let model = target(Api::AnthropicMessages);
        let messages = vec![
            Message::user(vec![ContentBlock::text("hello")]),
            Message::assistant_from(vec![ContentBlock::text("hi")], own(&model)),
        ];
        assert_eq!(normalize(messages.clone(), &model), messages);
    }

    // ---- mismatch detection ----

    #[test]
    fn a_different_model_id_at_the_same_provider_is_a_mismatch() {
        let model = target(Api::AnthropicMessages);
        let sibling = Provenance::new("anthropic", "claude-haiku-4");
        let bad_id = "call!1";
        let messages = vec![
            Message::assistant_from(vec![tool_use(bad_id)], sibling),
            Message::user(vec![tool_result(bad_id)]),
        ];
        let out = normalize(messages, &model);
        assert_eq!(use_ids(&out[0]), vec!["call_1"]);
        assert_eq!(result_ids(&out[1]), vec!["call_1"]);
    }

    #[test]
    fn unknown_provenance_is_treated_as_a_mismatch() {
        let model = target(Api::AnthropicMessages);
        let bad_id = "call:1";
        let messages = vec![
            Message::assistant(vec![tool_use(bad_id)]),
            Message::user(vec![tool_result(bad_id)]),
        ];
        let out = normalize(messages, &model);
        assert_eq!(use_ids(&out[0]), vec!["call_1"]);
        assert_eq!(result_ids(&out[1]), vec!["call_1"]);
    }

    // ---- id rewriting ----

    #[test]
    fn a_mismatched_valid_id_keeps_itself_for_prefix_stability() {
        let model = target(Api::AnthropicMessages);
        let messages = vec![
            Message::assistant_from(vec![tool_use("toolu_abc-123")], other()),
            Message::user(vec![tool_result("toolu_abc-123")]),
        ];
        let out = normalize(messages.clone(), &model);
        assert_eq!(out, messages, "a conforming id is not rewritten");
    }

    #[test]
    fn invalid_chars_sanitize_to_underscores_and_results_follow_in_the_same_pass() {
        let model = target(Api::AnthropicMessages);
        let messages = vec![
            Message::assistant_from(vec![tool_use("call.1:a/b")], other()),
            Message::user(vec![tool_result("call.1:a/b"), ContentBlock::text("rider")]),
        ];
        let out = normalize(messages, &model);
        assert_eq!(use_ids(&out[0]), vec!["call_1_a_b"]);
        assert_eq!(result_ids(&out[1]), vec!["call_1_a_b"]);
        // The rider text is untouched.
        assert!(matches!(&out[1].content[1], ContentBlock::Text { text } if text == "rider"));
    }

    #[test]
    fn anthropic_overflow_caps_at_64_with_a_short_hash_suffix() {
        let model = target(Api::AnthropicMessages);
        let long = "a".repeat(100);
        let messages = vec![
            Message::assistant_from(vec![tool_use(&long)], other()),
            Message::user(vec![tool_result(&long)]),
        ];
        let out = normalize(messages, &model);
        let id = use_ids(&out[0])[0].to_string();
        assert_eq!(id.len(), 64);
        assert!(id.starts_with(&"a".repeat(55)));
        assert!(id.as_bytes()[55] == b'_');
        assert_eq!(result_ids(&out[1]), vec![id.as_str()]);
    }

    #[test]
    fn openai_overflow_caps_at_40_with_a_short_hash_suffix() {
        let model = target(Api::OpenaiCompletions);
        let long = format!("toolu_{}", "b".repeat(60));
        let messages = vec![
            Message::assistant_from(vec![tool_use(&long)], other()),
            Message::user(vec![tool_result(&long)]),
        ];
        let out = normalize(messages, &model);
        let id = use_ids(&out[0])[0].to_string();
        assert_eq!(id.len(), 40);
        assert!(id.starts_with("toolu_b"));
        assert_eq!(result_ids(&out[1]), vec![id.as_str()]);
    }

    #[test]
    fn the_rewrite_is_deterministic_across_calls() {
        let model = target(Api::OpenaiCompletions);
        let long = "c".repeat(90);
        let messages = vec![
            Message::assistant_from(vec![tool_use(&long)], other()),
            Message::user(vec![tool_result(&long)]),
        ];
        let once = normalize(messages.clone(), &model);
        let twice = normalize(messages, &model);
        assert_eq!(once, twice);
        // And idempotent: normalizing the normalized history changes nothing.
        assert_eq!(normalize(once.clone(), &model), once);
    }

    #[test]
    fn colliding_sanitizations_get_distinct_ids() {
        let model = target(Api::AnthropicMessages);
        // Both sanitize to "call_1".
        let messages = vec![
            Message::assistant_from(vec![tool_use("call.1")], other()),
            Message::user(vec![tool_result("call.1")]),
            Message::assistant_from(vec![tool_use("call:1")], other()),
            Message::user(vec![tool_result("call:1")]),
        ];
        let out = normalize(messages, &model);
        let first = use_ids(&out[0])[0].to_string();
        let second = use_ids(&out[2])[0].to_string();
        assert_eq!(first, "call_1");
        assert_ne!(second, first);
        assert!(second.starts_with("call_1_"));
        assert_eq!(result_ids(&out[1]), vec![first.as_str()]);
        assert_eq!(result_ids(&out[3]), vec![second.as_str()]);
    }

    #[test]
    fn a_rewrite_never_collides_with_a_matched_messages_verbatim_id() {
        let model = target(Api::AnthropicMessages);
        // The matched message legitimately owns "call_1"; the mismatched
        // "call.1" must sanitize AROUND it.
        let messages = vec![
            Message::assistant_from(vec![tool_use("call_1")], own(&model)),
            Message::user(vec![tool_result("call_1")]),
            Message::assistant_from(vec![tool_use("call.1")], other()),
            Message::user(vec![tool_result("call.1")]),
        ];
        let out = normalize(messages, &model);
        assert_eq!(use_ids(&out[0]), vec!["call_1"], "verbatim replay");
        assert_eq!(result_ids(&out[1]), vec!["call_1"]);
        let rewritten = use_ids(&out[2])[0].to_string();
        assert_ne!(rewritten, "call_1");
        assert_eq!(result_ids(&out[3]), vec![rewritten.as_str()]);
    }

    // ---- orphaned Tool Calls ----

    #[test]
    fn an_orphaned_call_in_a_mismatched_message_is_answered_in_the_voice() {
        let model = target(Api::AnthropicMessages);
        let messages = vec![
            Message::user(vec![ContentBlock::text("go")]),
            Message::assistant_from(vec![tool_use("t1"), tool_use("t2")], other()),
            Message::user(vec![tool_result("t1"), ContentBlock::text("steering")]),
            Message::assistant_from(vec![ContentBlock::text("done")], other()),
        ];
        let out = normalize(messages, &model);
        assert_eq!(out.len(), 4);
        // t2's synthetic answer lands AFTER t1's real result, BEFORE the rider.
        assert_eq!(result_ids(&out[2]), vec!["t1", "t2"]);
        assert!(matches!(
            &out[2].content[1],
            ContentBlock::ToolResult { content, is_error: true, .. }
                if crate::content::result_blocks_text(content) == voice::orphaned_call_answer()
        ));
        assert!(matches!(&out[2].content[2], ContentBlock::Text { text } if text == "steering"));
    }

    #[test]
    fn a_trailing_orphaned_call_gets_a_fresh_user_message() {
        let model = target(Api::AnthropicMessages);
        let messages = vec![
            Message::user(vec![ContentBlock::text("go")]),
            Message::assistant_from(vec![tool_use("t1")], other()),
        ];
        let out = normalize(messages, &model);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].role, Role::User);
        assert!(matches!(
            &out[2].content[0],
            ContentBlock::ToolResult { tool_use_id, is_error: true, content }
                if tool_use_id == "t1"
                    && crate::content::result_blocks_text(content) == voice::orphaned_call_answer()
        ));
    }

    #[test]
    fn an_orphan_answered_anywhere_later_is_not_an_orphan() {
        let model = target(Api::AnthropicMessages);
        // The result lands two messages later (unusual but answered).
        let messages = vec![
            Message::assistant_from(vec![tool_use("t1")], other()),
            Message::user(vec![ContentBlock::text("interjection")]),
            Message::user(vec![tool_result("t1")]),
        ];
        let out = normalize(messages.clone(), &model);
        assert_eq!(out, messages);
    }

    #[test]
    fn a_matched_messages_orphan_is_left_alone() {
        // The transform's guarantee is for histories crossing Providers;
        // matched history replays verbatim, orphans included.
        let model = target(Api::AnthropicMessages);
        let messages = vec![
            Message::assistant_from(vec![tool_use("t1")], own(&model)),
            Message::user(vec![ContentBlock::text("cancelled")]),
        ];
        assert_eq!(normalize(messages.clone(), &model), messages);
    }

    #[test]
    fn an_orphan_with_a_rewritten_id_is_answered_under_the_new_id() {
        let model = target(Api::AnthropicMessages);
        let messages = vec![Message::assistant_from(vec![tool_use("t:1")], other())];
        let out = normalize(messages, &model);
        assert_eq!(out.len(), 2);
        assert_eq!(use_ids(&out[0]), vec!["t_1"]);
        assert_eq!(result_ids(&out[1]), vec!["t_1"]);
    }

    // ---- degrade_unsupported_media (ADR-0059) ----

    fn image_result(id: &str) -> ContentBlock {
        ContentBlock::tool_result_blocks(
            id,
            vec![ResultBlock::Image {
                mime: "image/png".into(),
                data: "AAAA".into(),
            }],
            false,
        )
    }

    fn result_blocks(message: &Message) -> &[ResultBlock] {
        match &message.content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            other => panic!("expected a ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn an_unsupported_image_degrades_to_the_verbatim_placeholder() {
        // The target Model accepts text only (all-false modalities), so the image
        // becomes the VERBATIM qwen unsupported-modality message.
        let model = target(Api::AnthropicMessages);
        let request = LlmRequest::new("s", vec![Message::user(vec![image_result("t1")])], vec![]);
        let out = degrade_unsupported_media(request, &model);
        match &result_blocks(&out.messages[0])[0] {
            ResultBlock::Text { text } => assert_eq!(
                text,
                "[Unsupported image file: \"image/png\". This model does not \
support image input. The read_file tool cannot process this type of file \
either. To handle this file, try using skills if applicable, or any tools \
installed at system wide, or let the user know you cannot process this type of \
file.]"
            ),
            other => panic!("expected a degraded Text block, got {other:?}"),
        }
    }

    #[test]
    fn a_supported_modality_keeps_its_media_untouched() {
        let mut model = target(Api::AnthropicMessages);
        model.input_modalities = crate::content::Modalities {
            image: true,
            pdf: false,
        };
        let request = LlmRequest::new("s", vec![Message::user(vec![image_result("t1")])], vec![]);
        let out = degrade_unsupported_media(request, &model);
        assert!(matches!(
            &result_blocks(&out.messages[0])[0],
            ResultBlock::Image { .. }
        ));
    }

    #[test]
    fn an_unsupported_pdf_degrades_but_a_supported_pdf_stays() {
        let pdf = |id: &str| {
            ContentBlock::tool_result_blocks(
                id,
                vec![ResultBlock::Document {
                    mime: "application/pdf".into(),
                    data: "BBBB".into(),
                }],
                false,
            )
        };

        let text_only = target(Api::AnthropicMessages);
        let request = LlmRequest::new("s", vec![Message::user(vec![pdf("t1")])], vec![]);
        let out = degrade_unsupported_media(request, &text_only);
        match &result_blocks(&out.messages[0])[0] {
            ResultBlock::Text { text } => assert!(text.starts_with(
                "[Unsupported pdf file: \"application/pdf\". This model does not support pdf input."
            )),
            other => panic!("expected a degraded Text block, got {other:?}"),
        }

        let mut pdf_model = target(Api::AnthropicMessages);
        pdf_model.input_modalities = crate::content::Modalities {
            image: false,
            pdf: true,
        };
        let request = LlmRequest::new("s", vec![Message::user(vec![pdf("t1")])], vec![]);
        let out = degrade_unsupported_media(request, &pdf_model);
        assert!(matches!(
            &result_blocks(&out.messages[0])[0],
            ResultBlock::Document { .. }
        ));
    }

    // ---- normalize_request ----

    #[test]
    fn normalize_request_rewrites_messages_and_keeps_the_rest() {
        let model = target(Api::AnthropicMessages);
        let request = LlmRequest::new(
            "sys",
            vec![
                Message::assistant(vec![tool_use("t.1")]),
                Message::user(vec![tool_result("t.1")]),
            ],
            vec![],
        )
        .with_no_think(true)
        .with_temperature(Some(0.2));
        let out = normalize_request(&request, &model);
        assert_eq!(out.system, "sys");
        assert!(out.no_think);
        assert_eq!(out.temperature, Some(0.2));
        assert_eq!(use_ids(&out.messages[0]), vec!["t_1"]);
        assert_eq!(result_ids(&out.messages[1]), vec!["t_1"]);
    }
}
