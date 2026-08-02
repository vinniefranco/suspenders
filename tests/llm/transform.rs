
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
            if crate::content::result_blocks_text(content) == voice::Marker::OrphanedCall.text()
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
                && crate::content::result_blocks_text(content) == voice::Marker::OrphanedCall.text()
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
