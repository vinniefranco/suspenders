use super::*;

fn span(text: &str, style: MdStyle) -> MdSpan {
    MdSpan {
        text: text.to_string(),
        style,
    }
}

fn line(spans: Vec<MdSpan>) -> MdLine {
    MdLine {
        spans,
        ..Default::default()
    }
}

/// One code-block line: a single CodeBlock span carrying the fence's lang.
fn code(text: &str, lang: &str) -> MdLine {
    MdLine {
        spans: vec![span(text, MdStyle::CodeBlock)],
        code_lang: Some(lang.to_string()),
    }
}

/// The rendered lines as `(text, style)` rows for compact assertions.
fn rows(text: &str) -> Vec<Vec<(String, MdStyle)>> {
    to_lines(text)
        .into_iter()
        .map(|l| l.spans.into_iter().map(|s| (s.text, s.style)).collect())
        .collect()
}

/// One line's concatenated text, for content-survival assertions.
fn flat(l: &MdLine) -> String {
    l.spans.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn empty_input_renders_no_lines() {
    assert_eq!(to_lines(""), Vec::<MdLine>::new());
}

#[test]
fn plain_text_passes_through_as_plain_lines_split_on_newline() {
    assert_eq!(
        to_lines("hello\nworld"),
        vec![
            line(vec![span("hello", MdStyle::Plain)]),
            line(vec![span("world", MdStyle::Plain)]),
        ]
    );
}

#[test]
fn softbreak_starts_a_new_line_without_a_blank_between() {
    // Single '\n' in the source = SoftBreak = the author's hard line break.
    let lines = to_lines("first row\nsecond row");
    assert_eq!(lines.len(), 2);
    assert_eq!(flat(&lines[0]), "first row");
    assert_eq!(flat(&lines[1]), "second row");
}

#[test]
fn hardbreak_starts_a_new_line() {
    let lines = to_lines("first  \nsecond");
    assert_eq!(lines.len(), 2);
    assert_eq!(flat(&lines[0]), "first");
    assert_eq!(flat(&lines[1]), "second");
}

#[test]
fn paragraphs_are_separated_by_one_empty_line() {
    assert_eq!(
        to_lines("one\n\ntwo"),
        vec![
            line(vec![span("one", MdStyle::Plain)]),
            MdLine::default(),
            line(vec![span("two", MdStyle::Plain)]),
        ]
    );
}

#[test]
fn multi_paragraph_document_separates_every_top_level_block() {
    let lines = to_lines("# Title\n\npara\n\n- item\n\n```\ncode\n```");
    let flats: Vec<String> = lines.iter().map(flat).collect();
    assert_eq!(flats, vec!["Title", "", "para", "", "• item", "", "code"]);
}

#[test]
fn heading_renders_one_line_all_heading_style_without_markers() {
    let lines = to_lines("## Section **bold** `code`");
    assert_eq!(lines.len(), 1);
    assert!(lines[0].spans.iter().all(|s| s.style == MdStyle::Heading));
    assert_eq!(flat(&lines[0]), "Section bold code");
}

#[test]
fn deep_heading_levels_also_render_as_heading() {
    assert_eq!(
        to_lines("###### deep"),
        vec![line(vec![span("deep", MdStyle::Heading)])]
    );
}

#[test]
fn bold_italic_and_code_get_their_styles() {
    assert_eq!(
        rows("a **b** *i* `c`"),
        vec![vec![
            ("a ".to_string(), MdStyle::Plain),
            ("b".to_string(), MdStyle::Bold),
            (" ".to_string(), MdStyle::Plain),
            ("i".to_string(), MdStyle::Italic),
            (" ".to_string(), MdStyle::Plain),
            ("c".to_string(), MdStyle::Code),
        ]]
    );
}

#[test]
fn bold_italic_nesting_resolves_to_bold_italic() {
    // ***x*** and **a *b* c** both nest via the style stack.
    let lines = to_lines("***x***");
    assert_eq!(lines[0].spans, vec![span("x", MdStyle::BoldItalic)]);

    let lines = to_lines("**a *b* c**");
    assert_eq!(
        lines[0].spans,
        vec![
            span("a ", MdStyle::Bold),
            span("b", MdStyle::BoldItalic),
            span(" c", MdStyle::Bold),
        ]
    );
}

#[test]
fn fenced_code_block_lines_are_verbatim_single_codeblock_spans() {
    let lines = to_lines("```rust\nlet x = 1;\nlet y = 2;\n```");
    assert_eq!(
        lines,
        vec![code("let x = 1;", "rust"), code("let y = 2;", "rust")]
    );
}

#[test]
fn code_block_internal_blank_lines_survive_as_empty_codeblock_spans() {
    let lines = to_lines("```\na\n\nb\n```");
    assert_eq!(lines, vec![code("a", ""), code("", ""), code("b", "")]);
}

#[test]
fn indented_code_block_renders_as_codeblock_lines() {
    let lines = to_lines("para\n\n    indented code\n    second line");
    assert_eq!(
        lines,
        vec![
            line(vec![span("para", MdStyle::Plain)]),
            MdLine::default(),
            code("indented code", ""),
            code("second line", ""),
        ]
    );
}

#[test]
fn unclosed_code_fence_renders_remaining_text_as_codeblock() {
    // Mid-stream markdown: the fence never closes; nothing is lost.
    let lines = to_lines("intro\n\n```rust\nlet x =");
    assert_eq!(
        lines,
        vec![
            line(vec![span("intro", MdStyle::Plain)]),
            MdLine::default(),
            code("let x =", "rust"),
        ]
    );
}

#[test]
fn unordered_list_gets_bullet_glyph_spans() {
    assert_eq!(
        to_lines("- first\n- second"),
        vec![
            line(vec![
                span("• ", MdStyle::Bullet),
                span("first", MdStyle::Plain)
            ]),
            line(vec![
                span("• ", MdStyle::Bullet),
                span("second", MdStyle::Plain)
            ]),
        ]
    );
}

#[test]
fn nested_lists_indent_two_spaces_per_level_inside_the_bullet_span() {
    let lines = to_lines("- outer\n  - inner\n    - deepest");
    assert_eq!(lines[0].spans[0], span("• ", MdStyle::Bullet));
    assert_eq!(lines[1].spans[0], span("  • ", MdStyle::Bullet));
    assert_eq!(lines[2].spans[0], span("    • ", MdStyle::Bullet));
}

#[test]
fn ordered_list_respects_the_start_number() {
    let lines = to_lines("3. third\n4. fourth");
    assert_eq!(lines[0].spans[0], span("3. ", MdStyle::Bullet));
    assert_eq!(lines[1].spans[0], span("4. ", MdStyle::Bullet));
}

#[test]
fn ordered_list_counts_up_from_one() {
    let lines = to_lines("1. a\n2. b\n3. c");
    let bullets: Vec<&str> = lines.iter().map(|l| l.spans[0].text.as_str()).collect();
    assert_eq!(bullets, vec!["1. ", "2. ", "3. "]);
}

#[test]
fn multi_line_item_continuation_aligns_under_the_text() {
    let lines = to_lines("- first line\nsecond line");
    assert_eq!(flat(&lines[0]), "• first line");
    assert_eq!(flat(&lines[1]), "  second line");

    let lines = to_lines("1. first line\nsecond line");
    assert_eq!(flat(&lines[0]), "1. first line");
    assert_eq!(flat(&lines[1]), "   second line");
}

#[test]
fn blockquote_prefixes_and_plain_text_reads_quote_style() {
    assert_eq!(
        to_lines("> quoted words"),
        vec![line(vec![span("▎ quoted words", MdStyle::Quote)])]
    );
}

#[test]
fn bold_inside_a_quote_keeps_its_own_style() {
    let lines = to_lines("> plain **strong**");
    assert_eq!(
        lines[0].spans,
        vec![
            span("▎ plain ", MdStyle::Quote),
            span("strong", MdStyle::Bold),
        ]
    );
}

#[test]
fn multi_line_quote_prefixes_every_line() {
    let lines = to_lines("> one\n> two");
    assert_eq!(flat(&lines[0]), "▎ one");
    assert_eq!(flat(&lines[1]), "▎ two");
    assert_eq!(lines[1].spans[0].style, MdStyle::Quote);
}

#[test]
fn link_with_different_url_appends_the_url_plain() {
    assert_eq!(
        to_lines("see [docs](https://example.com)"),
        vec![line(vec![
            span("see ", MdStyle::Plain),
            span("docs", MdStyle::Link),
            span(" (https://example.com)", MdStyle::Plain),
        ])]
    );
}

#[test]
fn link_whose_text_equals_the_url_gets_no_suffix() {
    assert_eq!(
        to_lines("[https://example.com](https://example.com)"),
        vec![line(vec![span("https://example.com", MdStyle::Link)])]
    );
}

#[test]
fn autolink_renders_the_url_as_link() {
    assert_eq!(
        to_lines("<https://example.com>"),
        vec![line(vec![span("https://example.com", MdStyle::Link)])]
    );
}

#[test]
fn stray_emphasis_markers_lose_no_text_and_never_panic() {
    let lines = to_lines("a ** b and *unclosed");
    let all: String = lines.iter().map(flat).collect::<Vec<_>>().join("\n");
    assert!(all.contains("a ** b"));
    assert!(all.contains("unclosed"));
}

#[test]
fn html_degrades_to_its_literal_text() {
    let all: String = to_lines("before <br> after\n\n<div>\nblock\n</div>")
        .iter()
        .map(flat)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("before"));
    assert!(all.contains("<br>"));
    assert!(all.contains("block"));
}

#[test]
fn carriage_returns_stripped_and_tabs_become_two_spaces() {
    assert_eq!(
        to_lines("a\tb\r\nnext"),
        vec![
            line(vec![span("a  b", MdStyle::Plain)]),
            line(vec![span("next", MdStyle::Plain)]),
        ]
    );
    // Inside a code block too.
    let lines = to_lines("```\n\tindented\r\n```");
    assert_eq!(lines, vec![code("  indented", "")]);
}

#[test]
fn list_and_following_paragraph_are_separated() {
    let lines = to_lines("- item\n\nafter");
    let flats: Vec<String> = lines.iter().map(flat).collect();
    assert_eq!(flats, vec!["• item", "", "after"]);
}

#[test]
fn thematic_break_degrades_to_a_rule_glyph_line() {
    let lines = to_lines("a\n\n---\n\nb");
    let flats: Vec<String> = lines.iter().map(flat).collect();
    assert_eq!(flats, vec!["a", "", "───", "", "b"]);
}

#[test]
fn heading_inside_a_quote_keeps_the_quote_prefix() {
    let lines = to_lines("> # quoted heading");
    assert_eq!(
        lines[0].spans,
        vec![
            span("▎ ", MdStyle::Quote),
            span("quoted heading", MdStyle::Heading),
        ]
    );
}

#[test]
fn list_inside_a_quote_prefixes_bullet_lines() {
    let lines = to_lines("> - item");
    assert_eq!(
        lines[0].spans,
        vec![
            span("▎ ", MdStyle::Quote),
            span("• ", MdStyle::Bullet),
            span("item", MdStyle::Quote),
        ]
    );
}

#[test]
fn fence_lang_is_captured_on_every_code_line() {
    let lines = to_lines("```python\nx = 1\ny = 2\n```");
    assert_eq!(lines.len(), 2);
    for l in &lines {
        assert_eq!(l.code_lang.as_deref(), Some("python"));
    }
}

#[test]
fn fence_lang_is_lowercased_and_cut_at_the_first_word() {
    let lines = to_lines("```Rust,ignore\nlet x = 1;\n```");
    assert_eq!(lines[0].code_lang.as_deref(), Some("rust"));

    let lines = to_lines("```JS extra info\nx\n```");
    assert_eq!(lines[0].code_lang.as_deref(), Some("js"));
}

#[test]
fn non_code_lines_carry_no_code_lang() {
    let lines = to_lines("# Title\n\npara\n\n- item\n\n> quote");
    for l in &lines {
        assert_eq!(
            l.code_lang,
            None,
            "non-code line {:?} carries a lang",
            flat(l)
        );
    }
}

#[test]
fn bare_fence_and_indented_block_carry_the_empty_lang() {
    let lines = to_lines("```\ncode\n```");
    assert_eq!(lines[0].code_lang.as_deref(), Some(""));

    let lines = to_lines("para\n\n    indented");
    assert_eq!(lines.last().unwrap().code_lang.as_deref(), Some(""));
}

#[test]
fn unclosed_fence_still_carries_the_lang() {
    // Mid-stream markdown: the fence never closes; the lang still lands.
    let lines = to_lines("```rust\nlet x =");
    assert_eq!(lines, vec![code("let x =", "rust")]);
}

#[test]
fn code_lang_resets_between_blocks() {
    let lines = to_lines("```rust\na\n```\n\ntext\n\n```\nb\n```");
    let langs: Vec<Option<&str>> = lines.iter().map(|l| l.code_lang.as_deref()).collect();
    assert_eq!(langs, vec![Some("rust"), None, None, None, Some("")]);
}

#[test]
fn adversarial_soup_never_panics_or_drops_words() {
    let soup = "# h **b `c\n> * [x](\n```\n未闭合 ** [\n\n- *\t\r*";
    let lines = to_lines(soup);
    let all: String = lines.iter().map(flat).collect::<Vec<_>>().join("\n");
    assert!(all.contains('h'));
    assert!(all.contains("未闭合"));
}
