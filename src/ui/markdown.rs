//! Markdown — the pure fold from markdown source to semantic terminal lines.
//!
//! Assistant text arrives as raw markdown; this module renders it into
//! [`MdLine`]s of [`MdSpan`]s carrying SEMANTIC styles ([`MdStyle`]), never
//! colors — the one markdown-style → ratatui mapping lives in
//! [`crate::ui::components`] (`md_style`), the same move as ADR-0008's
//! `LineStyle`. Pure data in/out like [`crate::ui::transcript`]: no ratatui,
//! no state, no IO, and [`to_lines`] never panics — any input, including
//! partial mid-stream markdown, produces reasonable lines.

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

/// Semantic inline/block style of one span. The color mapping lives in components.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdStyle {
    Plain,
    Bold,
    Italic,
    BoldItalic,
    Code,
    CodeBlock,
    Heading,
    Bullet,
    Quote,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdSpan {
    pub text: String,
    pub style: MdStyle,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MdLine {
    pub spans: Vec<MdSpan>,
    /// `Some(lang)` on every line inside a code block — the fence's info
    /// string, lowercased and cut at the first word (```` ```Rust,ignore ````
    /// → `Some("rust")`). A bare ```` ``` ```` fence and indented code blocks
    /// carry `Some("")` (a code block with no language). `None` on every
    /// non-code line. Still semantic — WHAT language, never a color; the
    /// highlighting lives in components.rs (ADR-0008).
    pub code_lang: Option<String>,
}

/// Renders markdown source into terminal lines. NEVER panics; any input
/// (including partial, mid-stream markdown) produces reasonable lines.
pub fn to_lines(text: &str) -> Vec<MdLine> {
    let mut fold = Fold::default();
    for event in Parser::new(text) {
        fold.event(event);
    }
    fold.finish()
}

/// `\r` stripped, tabs → two spaces (terminal rows must not carry either).
fn normalize(text: &str) -> String {
    text.replace('\r', "").replace('\t', "  ")
}

/// A fence's language from its info string: lowercased, cut at the first
/// whitespace or comma (```` ```Rust,ignore ```` → `"rust"`). Empty for a
/// bare ```` ``` ````.
fn fence_lang(info: &str) -> String {
    info.trim()
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// Inline emphasis markers, resolved by a style stack (bold inside italic →
/// [`MdStyle::BoldItalic`]).
#[derive(PartialEq)]
enum Marker {
    Bold,
    Italic,
}

/// An in-flight `[text](url)` link: the spans render as [`MdStyle::Link`]
/// while we accumulate the plain text to compare against the url at the end.
struct LinkCtx {
    url: String,
    text: String,
}

/// The fold from parser events to [`MdLine`]s: `current` is the line being
/// built, the stacks track the enclosing containers (quote depth, list
/// nesting, emphasis, links) that decide each span's semantic style and each
/// new line's prefix.
#[derive(Default)]
struct Fold {
    lines: Vec<MdLine>,
    current: Vec<MdSpan>,
    emphasis: Vec<Marker>,
    links: Vec<LinkCtx>,
    in_heading: bool,
    quote_depth: usize,
    in_code_block: bool,
    code_buffer: String,
    /// The open code block's language (see [`MdLine::code_lang`]).
    code_lang: String,
    /// One entry per open list; `Some(n)` = ordered, carrying the NEXT number.
    list_stack: Vec<Option<u64>>,
    /// One entry per open list item: the column its text starts at, so
    /// continuation lines indent to align under the text.
    cont_stack: Vec<usize>,
}

impl Fold {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                let style = if self.in_heading {
                    MdStyle::Heading
                } else {
                    MdStyle::Code
                };
                let code = normalize(&code);
                if let Some(link) = self.links.last_mut() {
                    link.text.push_str(&code);
                }
                self.push_span(code, style);
            }
            Event::SoftBreak | Event::HardBreak => self.break_line(),
            // A thematic break degrades to a plain rule glyph line.
            Event::Rule => {
                self.block_boundary();
                self.push_span("───".to_string(), MdStyle::Plain);
                self.flush();
            }
            // Raw HTML degrades to its literal text — never dropped silently.
            Event::Html(html) | Event::InlineHtml(html) => self.text(&html),
            // Math/footnotes/tasklists are OFF (Options::empty()), but degrade
            // to their literal text if they ever arrive.
            Event::InlineMath(math) | Event::DisplayMath(math) => self.text(&math),
            Event::FootnoteReference(name) => {
                self.push_span(format!("[^{name}]"), MdStyle::Plain);
            }
            Event::TaskListMarker(done) => {
                let marker = if done { "[x] " } else { "[ ] " };
                self.push_span(marker.to_string(), MdStyle::Bullet);
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                // Top-level paragraphs get blank-line separation; a paragraph
                // inside a list item continues the bullet's line (first) or
                // starts a continuation-indented one (later).
                if self.list_stack.is_empty() && self.quote_depth == 0 {
                    self.block_boundary();
                }
                if self.current.is_empty() {
                    self.push_prefix();
                }
            }
            Tag::Heading { .. } => {
                self.block_boundary();
                self.in_heading = true;
                if self.current.is_empty() {
                    self.push_prefix();
                }
            }
            Tag::BlockQuote(_) => {
                self.block_boundary();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.block_boundary();
                self.in_code_block = true;
                self.code_buffer.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(info) => fence_lang(&info),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Tag::List(start) => {
                self.block_boundary();
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let glyph = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let glyph = format!("{n}. ");
                        *n += 1;
                        glyph
                    }
                    _ => "• ".to_string(),
                };
                self.cont_stack.push(indent.chars().count() + glyph.chars().count());
                for _ in 0..self.quote_depth {
                    self.current.push(MdSpan {
                        text: "▎ ".to_string(),
                        style: MdStyle::Quote,
                    });
                }
                self.current.push(MdSpan {
                    text: format!("{indent}{glyph}"),
                    style: MdStyle::Bullet,
                });
            }
            Tag::Emphasis => self.emphasis.push(Marker::Italic),
            Tag::Strong => self.emphasis.push(Marker::Bold),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links.push(LinkCtx {
                    url: dest_url.to_string(),
                    text: String::new(),
                });
            }
            // Tables/footnotes/definition lists/metadata are OFF by default;
            // if they ever arrive, their Text events flow through as plain
            // content — structure degrades, content survives.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.in_heading = false;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::List(_) => {
                self.flush();
                self.list_stack.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.cont_stack.pop();
            }
            TagEnd::Emphasis | TagEnd::Strong => {
                self.emphasis.pop();
            }
            TagEnd::Link | TagEnd::Image => {
                if let Some(link) = self.links.pop() {
                    // The url shows only when it adds information beyond the
                    // link text (so `[x](url)` gets ` (url)`, autolinks don't).
                    if !link.url.is_empty() && link.url != link.text {
                        let style = self.inline_style();
                        self.push_span(format!(" ({})", link.url), style);
                    }
                }
            }
            _ => {}
        }
    }

    /// Inline text: inside a code block it accumulates verbatim; elsewhere it
    /// becomes spans in the current style, splitting on any embedded newline.
    fn text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_buffer.push_str(text);
            return;
        }
        let text = normalize(text);
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.break_line();
            }
            if part.is_empty() {
                continue;
            }
            if let Some(link) = self.links.last_mut() {
                link.text.push_str(part);
            }
            let style = self.inline_style();
            self.push_span(part.to_string(), style);
        }
    }

    /// The semantic style the enclosing context gives a text span: heading and
    /// link swallow inline emphasis; a plain span inside a quote reads Quote.
    fn inline_style(&self) -> MdStyle {
        if self.in_heading {
            return MdStyle::Heading;
        }
        if !self.links.is_empty() {
            return MdStyle::Link;
        }
        let bold = self.emphasis.contains(&Marker::Bold);
        let italic = self.emphasis.contains(&Marker::Italic);
        match (bold, italic) {
            (true, true) => MdStyle::BoldItalic,
            (true, false) => MdStyle::Bold,
            (false, true) => MdStyle::Italic,
            (false, false) if self.quote_depth > 0 => MdStyle::Quote,
            (false, false) => MdStyle::Plain,
        }
    }

    /// Appends a span to the current line, merging into the previous span when
    /// the style matches (keeps lines readable: "Title bold" not two chunks).
    fn push_span(&mut self, text: String, style: MdStyle) {
        if let Some(last) = self.current.last_mut()
            && last.style == style
        {
            last.text.push_str(&text);
            return;
        }
        self.current.push(MdSpan { text, style });
    }

    /// Ends the current visual line (SoftBreak/HardBreak) and starts the next
    /// with the container prefix, so hard line breaks in the source survive.
    fn break_line(&mut self) {
        self.flush();
        self.push_prefix();
    }

    /// The prefix every fresh line inside a container starts with: one `▎ `
    /// per quote level, then continuation indent aligning under a list item's
    /// text (the bullet line itself is built by `Start(Item)` instead).
    fn push_prefix(&mut self) {
        for _ in 0..self.quote_depth {
            self.current.push(MdSpan {
                text: "▎ ".to_string(),
                style: MdStyle::Quote,
            });
        }
        if let Some(&width) = self.cont_stack.last() {
            self.push_span(" ".repeat(width), MdStyle::Plain);
        }
    }

    /// Flushes any pending spans, then separates top-level blocks with one
    /// empty line so paragraphs/lists/headings/code blocks stay visually apart.
    fn block_boundary(&mut self) {
        self.flush();
        if self.list_stack.is_empty() && self.quote_depth == 0 && !self.lines.is_empty() {
            self.lines.push(MdLine::default());
        }
    }

    /// Emits the buffered code block: each source line is its own MdLine with
    /// a single CodeBlock span, contents verbatim — internal blank lines
    /// survive as empty CodeBlock-span lines so the block reads as a unit.
    /// Every line carries the fence's language ([`MdLine::code_lang`]).
    fn end_code_block(&mut self) {
        self.in_code_block = false;
        let lang = std::mem::take(&mut self.code_lang);
        let buffer = normalize(&std::mem::take(&mut self.code_buffer));
        let buffer = buffer.strip_suffix('\n').unwrap_or(&buffer);
        if buffer.is_empty() {
            return;
        }
        for line in buffer.split('\n') {
            self.lines.push(MdLine {
                spans: vec![MdSpan {
                    text: line.to_string(),
                    style: MdStyle::CodeBlock,
                }],
                code_lang: Some(lang.clone()),
            });
        }
    }

    fn flush(&mut self) {
        if !self.current.is_empty() {
            let spans = std::mem::take(&mut self.current);
            self.lines.push(MdLine {
                spans,
                code_lang: None,
            });
        }
    }

    fn finish(mut self) -> Vec<MdLine> {
        // Defensive: pulldown-cmark closes open blocks at EOF, but if a code
        // buffer were ever left open (mid-stream input), its text still lands.
        if self.in_code_block {
            self.end_code_block();
        }
        self.flush();
        self.lines
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(
            flats,
            vec!["Title", "", "para", "", "• item", "", "code"]
        );
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
                line(vec![span("• ", MdStyle::Bullet), span("first", MdStyle::Plain)]),
                line(vec![span("• ", MdStyle::Bullet), span("second", MdStyle::Plain)]),
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
            assert_eq!(l.code_lang, None, "non-code line {:?} carries a lang", flat(l));
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
}
