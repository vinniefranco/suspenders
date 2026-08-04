use super::*;

// ---- ContentBlock media variants (ADR-0068) ----

#[test]
fn image_block_round_trips_through_the_tagged_serde_shape() {
    // The Session-Log projection (ADR-0010) round-trips a ContentBlock as the
    // `#[serde(tag = "type", rename_all = "snake_case")]` shape, so the new
    // media variant must serialize to `{type:"image",mime,data}` and back.
    let block = ContentBlock::image("image/png", "AAAA");
    let value = serde_json::to_value(&block).unwrap();
    assert_eq!(
        value,
        serde_json::json!({ "type": "image", "mime": "image/png", "data": "AAAA" })
    );
    let back: ContentBlock = serde_json::from_value(value).unwrap();
    assert_eq!(back, block);
}

#[test]
fn document_block_round_trips_through_the_tagged_serde_shape() {
    let block = ContentBlock::document("application/pdf", "BBBB");
    let value = serde_json::to_value(&block).unwrap();
    assert_eq!(
        value,
        serde_json::json!({ "type": "document", "mime": "application/pdf", "data": "BBBB" })
    );
    let back: ContentBlock = serde_json::from_value(value).unwrap();
    assert_eq!(back, block);
}

#[test]
fn media_placeholder_mirrors_the_result_blocks_text_convention() {
    // The projection mirrors `result_blocks_text` exactly: `[image: <mime>]`
    // and `[document: <mime>]`; every non-media block is `None`.
    assert_eq!(
        ContentBlock::image("image/png", "AAAA").media_placeholder(),
        Some("[image: image/png]".to_string())
    );
    assert_eq!(
        ContentBlock::document("application/pdf", "BBBB").media_placeholder(),
        Some("[document: application/pdf]".to_string())
    );
    assert_eq!(ContentBlock::text("hi").media_placeholder(), None);
}

// ---- UserPrompt (ADR-0068) ----

#[test]
fn user_prompt_from_string_is_one_text_block() {
    // The ergonomic widening: a bare String becomes a single Text block, so
    // `agent.submit("foo")` keeps compiling and a text-only prompt is unchanged.
    let prompt = UserPrompt::from("do the thing".to_string());
    assert_eq!(prompt.blocks(), &[ContentBlock::text("do the thing")]);
    assert!(prompt.is_plain_text());
}

#[test]
fn user_prompt_from_str_matches_from_string() {
    assert_eq!(UserPrompt::from("hi"), UserPrompt::from("hi".to_string()));
}

#[test]
fn user_prompt_text_projection_of_plain_text_is_the_text() {
    // A pure-text prompt projects to exactly its text: the byte-identical display
    // string the transcript line / history ring / user_prompt_submit hook read.
    assert_eq!(UserPrompt::from("hello world").text(), "hello world");
}

#[test]
fn user_prompt_text_projection_renders_media_as_placeholders() {
    // A media prompt projects Text verbatim and each media block as its
    // `[image: <mime>]` / `[document: <mime>]` placeholder, mirroring
    // `result_blocks_text`.
    let prompt = UserPrompt::from_blocks(vec![
        ContentBlock::text("look at "),
        ContentBlock::image("image/png", "AAAA"),
        ContentBlock::text(" and "),
        ContentBlock::document("application/pdf", "BBBB"),
    ]);
    assert_eq!(
        prompt.text(),
        "look at [image: image/png] and [document: application/pdf]"
    );
    assert!(!prompt.is_plain_text());
}

#[test]
fn user_prompt_display_text_omits_media_placeholders() {
    // BUG 4: the USER-FACING display text is the Text blocks only, with media
    // OMITTED (no `[image: <mime>]`). This is the clean typed text a transcript /
    // steering line shows - contrast `text()`, which keeps the wire projection.
    let prompt = UserPrompt::from_blocks(vec![
        ContentBlock::text("look at @shot.png"),
        ContentBlock::image("image/png", "AAAA"),
    ]);
    assert_eq!(prompt.display_text(), "look at @shot.png");
    // The wire projection still carries the placeholder (unchanged).
    assert_eq!(prompt.text(), "look at @shot.png[image: image/png]");
}

#[test]
fn user_prompt_display_text_of_plain_text_matches_text() {
    // A pure-text prompt: display and wire projections are byte-identical.
    let prompt = UserPrompt::from("hello world");
    assert_eq!(prompt.display_text(), prompt.text());
    assert_eq!(prompt.display_text(), "hello world");
}

#[test]
fn user_prompt_into_blocks_yields_the_ordered_list() {
    let blocks = vec![
        ContentBlock::text("a"),
        ContentBlock::image("image/png", "AAAA"),
    ];
    assert_eq!(
        UserPrompt::from_blocks(blocks.clone()).into_blocks(),
        blocks
    );
}

#[test]
fn user_prompt_is_plain_text_only_for_a_lone_text_block() {
    // A lone Text block is plain; an empty prompt, a media block, or Text+media
    // is not - the discriminant `start_run` uses to choose UserText vs
    // UserContent in the Session Log.
    assert!(!UserPrompt::from_blocks(vec![]).is_plain_text());
    assert!(
        !UserPrompt::from_blocks(vec![ContentBlock::image("image/png", "AAAA")]).is_plain_text()
    );
    assert!(
        !UserPrompt::from_blocks(vec![ContentBlock::text("a"), ContentBlock::text("b"),])
            .is_plain_text()
    );
}

// ---- context_floor/1 ----

#[test]
fn context_floor_sums_all_four_figures() {
    let usage = Usage {
        input_tokens: Some(200),
        output_tokens: Some(300),
        cache_read_input_tokens: Some(90_000),
        cache_creation_input_tokens: Some(1_500),
    };
    assert_eq!(usage.context_floor(), Some(92_000));
}

#[test]
fn context_floor_counts_absent_figures_as_zero() {
    assert_eq!(Usage::with_input_tokens(200).context_floor(), Some(200));
}

#[test]
fn context_floor_is_none_without_input_tokens() {
    // A usage map without input_tokens is no signal, not a zero floor -
    // even when the cache figures are present.
    assert_eq!(Usage::default().context_floor(), None);
    let cache_only = Usage {
        input_tokens: None,
        output_tokens: Some(300),
        cache_read_input_tokens: Some(90_000),
        cache_creation_input_tokens: Some(1_500),
    };
    assert_eq!(cache_only.context_floor(), None);
}
