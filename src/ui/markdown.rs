//! Markdown - the pure fold from markdown source to semantic terminal lines.
//!
//! Assistant text arrives as raw markdown; this module renders it into
//! [`MdLine`]s of [`MdSpan`]s carrying SEMANTIC styles ([`MdStyle`]), never
//! colors - the one markdown-style → ratatui mapping lives in
//! [`crate::ui::components`] (`md_style`), the same move as ADR-0008's
//! diff-side → color mapping. Pure data in/out like [`crate::ui::transcript`]: no ratatui,
//! no state, no IO, and [`to_lines`] never panics - any input, including
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
    /// `Some(lang)` on every line inside a code block - the fence's info
    /// string, lowercased and cut at the first word (```` ```Rust,ignore ````
    /// → `Some("rust")`). A bare ```` ``` ```` fence and indented code blocks
    /// carry `Some("")` (a code block with no language). `None` on every
    /// non-code line. Still semantic - WHAT language, never a color; the
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
            // Raw HTML degrades to its literal text - never dropped silently.
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
                self.cont_stack
                    .push(indent.chars().count() + glyph.chars().count());
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
            // content - structure degrades, content survives.
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
    /// a single CodeBlock span, contents verbatim - internal blank lines
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
#[path = "../../tests/unit/ui/markdown.rs"]
mod tests;
