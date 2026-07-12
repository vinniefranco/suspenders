//! UI Components - the SINGLE mapping from the semantic display vocabulary
//! (ADR-0008) to ratatui `Style`/`Color`, plus the render helpers the frontend
//! draws with.
//!
//! This is the one place semantics become terminal colors: [`LineStyle`] →
//! color for a Block's lines, [`PressureLevel`] → color/emphasis for the status
//! bar. Plugins and the Transcript core never touch ratatui; they speak the
//! vocabulary and this module renders it. Everything here is pure presentation
//! of [`TranscriptItem`]s - no state, no IO. Only this module and [`crate::ui`]
//! `use ratatui` / `use crossterm` (ADR-0019 invariant).

use std::sync::OnceLock;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::ui::composer::{self, ComposerLayout};
use crate::ui::markdown::{self, MdLine, MdStyle};
use crate::ui::picker::Picker;
use crate::ui::transcript::{
    LineStyle, PressureLevel, Status, StyledLine, Transcript, TranscriptItem,
};
use crate::ui::viewport::Viewport;

// ---------------------------------------------------------------------------
// The single semantic → color mapping (ADR-0008).
// ---------------------------------------------------------------------------

/// The ONE mapping from a semantic [`LineStyle`] to a ratatui [`Style`]
/// (ADR-0008). Plugins produce styles; this turns them into colors.
pub fn line_style(style: LineStyle) -> Style {
    match style {
        LineStyle::Added => Style::default().fg(Color::Green),
        LineStyle::Removed => Style::default().fg(Color::Red),
        LineStyle::Context => Style::default().fg(Color::DarkGray),
        LineStyle::Emphasis => Style::default().add_modifier(Modifier::BOLD),
        LineStyle::Muted => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        LineStyle::Default => Style::default(),
    }
}

/// The ONE mapping from a semantic markdown [`MdStyle`] to a ratatui [`Style`]
/// (ADR-0008's move, applied to assistant markdown): [`markdown::to_lines`]
/// speaks semantics; this is where they become colors.
pub fn md_style(style: MdStyle) -> Style {
    match style {
        MdStyle::Plain => Style::default(),
        MdStyle::Bold => Style::default().add_modifier(Modifier::BOLD),
        MdStyle::Italic => Style::default().add_modifier(Modifier::ITALIC),
        MdStyle::BoldItalic => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        MdStyle::Code => Style::default().fg(Color::Yellow),
        MdStyle::CodeBlock => Style::default().fg(Color::Rgb(185, 215, 180)).bg(CODE_BG),
        MdStyle::Heading => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        MdStyle::Bullet => Style::default().fg(Color::Cyan),
        MdStyle::Quote => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        MdStyle::Link => Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::UNDERLINED),
    }
}

/// The ONE mapping from the semantic [`PressureLevel`] (ADR-0008) to the
/// tokens segment's style: `Ok` reads muted, `Elevated` warns, `Critical`
/// alarms. Segment form (fg ON a bg) because the status bar is a powerline of
/// colored blocks - the semantics are unchanged, only the presentation moved
/// from colored text to colored blocks.
pub fn pressure_style(level: PressureLevel) -> Style {
    match level {
        PressureLevel::Critical => Style::default()
            .fg(Color::Black)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
        PressureLevel::Elevated => Style::default().fg(Color::Black).bg(Color::Yellow),
        PressureLevel::Ok => Style::default().fg(Color::Gray).bg(SEGMENT_DARK_BG),
    }
}

/// The ONE mapping from a [`SegmentKind`] to its powerline segment style
/// (ADR-0008: this is the only place segment semantics become colors). Every
/// segment style carries a bg - the powerline separators are drawn from the
/// adjacent segments' bgs ([`segment_bg`]).
pub fn segment_style(kind: SegmentKind) -> Style {
    match kind {
        SegmentKind::ModeIdle | SegmentKind::Position => Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD),
        SegmentKind::ModeRunning => Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        SegmentKind::Connection => Style::default()
            .fg(Color::Rgb(150, 160, 185))
            .bg(Color::Rgb(52, 58, 82)),
        SegmentKind::Thinking => Style::default().fg(Color::DarkGray).bg(SEGMENT_DARK_BG),
        // Tokens keep the single PressureLevel mapping - segment_style only
        // routes to it, it does not restate the colors.
        SegmentKind::Tokens(level) => pressure_style(level),
    }
}

/// A segment's background - what the powerline separator glyphs blend with.
fn segment_bg(kind: SegmentKind) -> Color {
    segment_style(kind).bg.unwrap_or(BAR_BG)
}

// ---------------------------------------------------------------------------
// Render helpers.
// ---------------------------------------------------------------------------

/// Renders the whole frame: the transcript viewport, the status bar, the
/// Composer, and - when an Approval is pending - the modal on top. The
/// [`Viewport`] holds the pure scroll state; the returned `(total_lines,
/// height)` is the geometry the viewport was measured/drawn at, which the
/// adapter stores for the scroll effects that execute between draws.
///
/// The Composer GROWS with its draft: its height is the wrapped row count
/// (hard newlines and width-wrapping both), capped by
/// [`composer::max_visible_rows`] so a tall draft never starves the
/// transcript viewport - which is expected to shrink as the Composer grows.
/// The wrap math runs at the exact width the Composer is drawn at (the frame
/// minus the 2-cell gutter), so the measured cursor cell is the drawn one.
pub fn render(
    frame: &mut Frame,
    t: &Transcript,
    base_url: &str,
    spinner: u64,
    viewport: &Viewport,
    cache: &mut RenderCache,
) -> (usize, usize) {
    let area = frame.area();
    let layout = composer::layout(
        &t.input_value,
        t.input_cursor,
        area.width.saturating_sub(2) as usize,
    );
    let composer_height = layout
        .rows
        .len()
        .min(composer::max_visible_rows(area.height as usize));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // transcript viewport
            Constraint::Length(1), // status bar
            Constraint::Length(composer_height as u16), // composer (grows with the draft)
        ])
        .split(area);

    // The viewport renders FIRST: the status bar's position segment reads the
    // measured geometry (and the Viewport's clamped top) from this frame, not
    // a stale one.
    let geometry = render_viewport(frame, chunks[0], t, viewport, cache);
    render_status_bar(frame, chunks[1], t, base_url, spinner, viewport, geometry);
    render_composer(frame, chunks[2], t, &layout);

    if let Some(pending) = &t.pending_approval {
        render_approval_modal(frame, area, &pending.command);
    }
    geometry
}

/// The transcript viewport: the message list, oldest first, plus any in-flight
/// streaming Thinking/text, scrolled to the [`Viewport`]'s clamped top offset
/// and overlaid with a scrollbar when the content overflows. Returns the
/// measured geometry `(total wrapped lines, viewport height)`.
///
/// Per-frame cost is O(visible), not O(session): settled items' lines and
/// wrapped counts come from the [`RenderCache`] (built once per item, per
/// width), the total comes from summing the cached counts, and only the items
/// intersecting the visible window ([`visible_window`]) are handed to the
/// `Paragraph` - with a scroll offset RELATIVE to that slice. Measuring and
/// drawing still agree exactly: each item was measured with the same
/// `Wrap { trim: false }` at the same width it is drawn at, and ratatui wraps
/// each `Line` independently, so per-item counts sum to the whole.
pub fn render_viewport(
    frame: &mut Frame,
    area: Rect,
    t: &Transcript,
    viewport: &Viewport,
    cache: &mut RenderCache,
) -> (usize, usize) {
    // The rightmost column is ALWAYS the scrollbar gutter, occupied or not:
    // reserving it only when the scrollbar shows would make the wrap width
    // depend on the line count and the line count on the wrap width.
    let text_area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    cache.sync(t, text_area.width);

    // The live streaming snapshot renders below the settled items: the
    // one-line thinking indicator (rebuilt each frame - one Line is cheap)
    // and the streaming markdown (cached - see [`RenderCache::sync`]).
    let thinking = t.streaming_thinking();
    let thinking_lines: Vec<Line<'static>> = if thinking.is_empty() {
        vec![]
    } else {
        vec![Line::styled(
            format!("🧠 thinking… ({} chars)", thinking.chars().count()),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]
    };

    // One (lines, wrapped-count) entry per window "item": every settled
    // message, then the streaming tail - a single indexing shared by the
    // window selection and the slice assembly below.
    let mut item_lines: Vec<&[Line<'static>]> =
        cache.items.iter().map(|item| item.lines.as_slice()).collect();
    let mut counts: Vec<usize> = cache.items.iter().map(|item| item.wrapped).collect();
    if !thinking_lines.is_empty() {
        counts.push(wrapped_count(thinking_lines.clone(), text_area.width));
        item_lines.push(&thinking_lines);
    }
    if let Some(streaming) = &cache.streaming {
        counts.push(streaming.wrapped);
        item_lines.push(&streaming.lines);
    }

    let total_lines: usize = counts.iter().sum();
    let height = area.height as usize;
    let top = viewport.top_offset(total_lines, height);
    let (range, offset) = visible_window(&counts, top, height);
    let visible: Vec<Line> = item_lines[range]
        .iter()
        .flat_map(|lines| lines.iter().cloned())
        .collect();
    let paragraph = Paragraph::new(visible).wrap(Wrap { trim: false });
    // The pure window math speaks usize; saturate only here, at the ratatui
    // boundary. The relative offset is bounded by ONE item's wrapped rows
    // (the item straddling the window top), never the session's.
    let scroll = u16::try_from(offset).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll, 0)), text_area);

    if total_lines > height {
        let mut state = ScrollbarState::new(total_lines)
            .position(top)
            .viewport_content_length(height);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut state,
        );
    }
    (total_lines, height)
}

// ---------------------------------------------------------------------------
// The per-item render cache + the visible-window math.
//
// WHY: rebuilding every settled item's lines (markdown parse + syntect
// highlight) and re-wrapping the whole session on EVERY frame pegged a core
// while scrolling and made typing expensive - each keystroke only changes the
// Composer, each wheel tick only a scroll offset. Settled items never change
// content (the pure core only appends, and bumps `messages_revision` on its
// one structural edit), so their lines and wrapped counts are built once and
// reused; the frame then renders only the items intersecting the window.
// ---------------------------------------------------------------------------

/// Per-item render state for the transcript viewport, owned by the adapter's
/// run loop and threaded through [`render`]. Holds ratatui [`Line`]s, so it
/// lives HERE, not in the pure modules (ADR-0019).
pub struct RenderCache {
    /// The text width everything below was built/measured at.
    width: u16,
    /// The Ctrl-T state the settled lines were built with (it changes every
    /// Thinking item's lines, so a flip clears the cache wholesale).
    thinking_expanded: bool,
    /// The core's `messages_revision` the entries were built at: while it
    /// holds still, `messages` only appends and the cache extends; when it
    /// moves (a structural edit), the cache rebuilds from scratch.
    revision: u64,
    /// One entry per settled `Transcript::messages` item, same order.
    items: Vec<CachedItem>,
    /// The in-flight streaming markdown, keyed on its char length: within one
    /// message the snapshot only grows, so the length is a cheap monotonic
    /// key that changes exactly when the text does. Cleared between messages
    /// (empty streaming text) so a new message can never collide with a stale
    /// entry of the same length.
    streaming: Option<CachedStreaming>,
}

/// One settled item's built lines and its wrapped row count at the cache's
/// width - the numbers [`visible_window`] does its prefix-sum math over.
struct CachedItem {
    lines: Vec<Line<'static>>,
    wrapped: usize,
}

/// The cached streaming-markdown tail (see [`RenderCache::streaming`]).
struct CachedStreaming {
    char_len: usize,
    lines: Vec<Line<'static>>,
    wrapped: usize,
}

impl RenderCache {
    pub fn new() -> Self {
        RenderCache {
            width: 0,
            thinking_expanded: false,
            revision: 0,
            items: Vec::new(),
            streaming: None,
        }
    }

    /// Brings the cache up to date with the Transcript at `width`: clears
    /// wholesale when a key input changed (width, Ctrl-T, a structural
    /// `messages` edit), then builds entries for the newly appended items
    /// only - the steady-state cost of a frame is zero rebuilt items.
    fn sync(&mut self, t: &Transcript, width: u16) {
        if self.width != width
            || self.thinking_expanded != t.thinking_expanded
            || self.revision != t.messages_revision
            || self.items.len() > t.messages.len()
        {
            self.items.clear();
            self.streaming = None;
            self.width = width;
            self.thinking_expanded = t.thinking_expanded;
            self.revision = t.messages_revision;
        }
        for item in &t.messages[self.items.len()..] {
            let lines = message_lines(item, t.thinking_expanded);
            let wrapped = wrapped_count(lines.clone(), width);
            self.items.push(CachedItem { lines, wrapped });
        }
        self.sync_streaming(&t.streaming_text(), width);
    }

    /// Re-parses the streaming markdown only when its char length moved
    /// (monotonic within a message - see the field doc); drops the entry when
    /// streaming ended so the next message starts from nothing.
    fn sync_streaming(&mut self, text: &str, width: u16) {
        if text.is_empty() {
            self.streaming = None;
            return;
        }
        let char_len = text.chars().count();
        if self.streaming.as_ref().is_some_and(|s| s.char_len == char_len) {
            return;
        }
        let lines = markdown_lines(text);
        let wrapped = wrapped_count(lines.clone(), width);
        self.streaming = Some(CachedStreaming {
            char_len,
            lines,
            wrapped,
        });
    }
}

impl Default for RenderCache {
    fn default() -> Self {
        RenderCache::new()
    }
}

/// The rows `lines` wrap to at `width`, measured by a throwaway `Paragraph`
/// with the SAME `Wrap { trim: false }` the viewport draws with - the window
/// math is only correct if measuring and drawing agree exactly.
fn wrapped_count(lines: Vec<Line<'static>>, width: u16) -> usize {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// The visible window over per-item wrapped row counts, pure: given the
/// clamped top offset and the viewport height, the range of items whose rows
/// intersect `[top, top + height)` and the scroll offset RELATIVE to the
/// range's first row. Rendering only this slice (scrolled by the relative
/// offset) draws exactly what rendering everything (scrolled by `top`) would.
fn visible_window(counts: &[usize], top: usize, height: usize) -> (std::ops::Range<usize>, usize) {
    // Walk to the first item whose rows reach past `top` (prefix sums).
    let mut start = counts.len();
    let mut before = 0;
    for (i, &count) in counts.iter().enumerate() {
        if before + count > top {
            start = i;
            break;
        }
        before += count;
    }
    // `top` beyond the content (degenerate; the caller clamps) selects nothing.
    let offset = top.saturating_sub(before);
    // Extend until the slice covers the window's bottom row (or runs out).
    let mut end = start;
    let mut covered = 0;
    while end < counts.len() && covered < offset + height {
        covered += counts[end];
        end += 1;
    }
    (start..end, offset)
}

/// The lines one Transcript item renders as. `Block` is the semantic display
/// vocabulary (ADR-0008): a titled block whose lines take their color from
/// [`line_style`]. `thinking_expanded` (Ctrl-T, the core's
/// `Transcript::thinking_expanded`) picks the collapsed one-liner or the full
/// text for settled `Thinking` items.
fn message_lines(item: &TranscriptItem, thinking_expanded: bool) -> Vec<Line<'static>> {
    match item {
        // User prompts: the "› " gutter on the first row, continuation rows
        // aligned under it. Multi-line input renders as multiple rows.
        TranscriptItem::User { text } => {
            let gutter = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
            text_rows(text)
                .into_iter()
                .enumerate()
                .map(|(i, row)| {
                    let prefix = if i == 0 { "› " } else { "  " };
                    Line::from(vec![Span::styled(prefix, gutter), Span::raw(row)])
                })
                .collect()
        }
        // Assistant text is markdown: the pure ui::markdown fold produces
        // semantic lines and [`md_style`] turns them into colors here.
        // Width-wrapping is left to the viewport Paragraph's Wrap.
        TranscriptItem::Assistant { text } => markdown_lines(text),
        // Settled Thinking: collapsed is the one-line form; expanded (Ctrl-T)
        // is a header row then the full text, all in the same dim italic. The
        // in-flight "🧠 thinking… (N chars)" streaming indicator is rendered by
        // the viewport and is unaffected by the toggle.
        TranscriptItem::Thinking { text } => {
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            if thinking_expanded {
                let mut out = vec![Line::styled("🧠 thought:", style)];
                out.extend(text_rows(text).into_iter().map(|row| Line::styled(row, style)));
                out
            } else {
                vec![Line::styled(
                    format!("🧠 thought: {}", first_line(text)),
                    style,
                )]
            }
        }
        TranscriptItem::ToolCall { name, summary } => vec![Line::styled(
            format!("⚙ {}", join_summary(name, summary)),
            Style::default().fg(Color::Yellow),
        )],
        TranscriptItem::ToolResult {
            name,
            summary,
            is_error: false,
        } => vec![Line::styled(
            format!("⚙ {name} → {summary}"),
            Style::default().fg(Color::Yellow),
        )],
        TranscriptItem::ToolResult {
            name,
            summary,
            is_error: true,
        } => vec![Line::styled(
            format!("⚙ {name} ✗ {summary}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )],
        TranscriptItem::Block { title, lines } => {
            let mut out = vec![Line::styled(
                format!("⚙ {title}"),
                Style::default().fg(Color::Yellow),
            )];
            out.extend(lines.iter().map(block_line));
            out
        }
        TranscriptItem::Info { text } => {
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            text_rows(text)
                .into_iter()
                .map(|row| Line::styled(row, style))
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Code-fence syntax highlighting (presentation, so it lives HERE - ADR-0008:
// markdown.rs carries only the semantic fact, the fence's language).
// ---------------------------------------------------------------------------

/// The code block background every code line keeps, highlighted or not.
const CODE_BG: Color = Color::Rgb(25, 25, 35);

/// The bundled syntax definitions + the one theme we color code with. Lazy:
/// headless runs that never render pay nothing for `load_defaults`.
struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();

fn highlighter() -> &'static Highlighter {
    HIGHLIGHTER.get_or_init(|| Highlighter {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        theme: ThemeSet::load_defaults().themes["base16-ocean.dark"].clone(),
    })
}

/// One highlighted fragment: the `(r, g, b)` foreground and the text it colors.
type CodeFragment = ((u8, u8, u8), String);

/// Highlights one code block: per input line, the [`CodeFragment`]s syntect
/// colors it with - pure data in/out, no ratatui types. `None` when `lang`
/// resolves to no bundled syntax (caller falls back to the plain
/// [`MdStyle::CodeBlock`] rendering). Parse state carries across the lines, so
/// multi-line constructs (block comments, raw strings) color correctly.
fn highlight_code(lines: &[&str], lang: &str) -> Option<Vec<Vec<CodeFragment>>> {
    let hl = highlighter();
    // `find_syntax_by_token` matches the syntax name ("rust", "python") AND
    // file extensions ("rs", "py"), case-insensitively - the widest net for
    // fence tags.
    let syntax = hl.syntaxes.find_syntax_by_token(lang)?;
    let mut state = HighlightLines::new(syntax, &hl.theme);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        // The newlines-variant SyntaxSet expects each line `\n`-terminated.
        let with_newline = format!("{line}\n");
        let ranges = state.highlight_line(&with_newline, &hl.syntaxes).ok()?;
        let mut fragments = Vec::new();
        for (style, text) in ranges {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            let fg = style.foreground;
            fragments.push(((fg.r, fg.g, fg.b), text.to_string()));
        }
        out.push(fragments);
    }
    Some(out)
}

/// Renders assistant markdown into ratatui lines: one `Line` per [`MdLine`],
/// each span styled by the single [`md_style`] mapping; an empty MdLine (block
/// separation) becomes a blank row. Consecutive code lines sharing a non-empty
/// `code_lang` are highlighted as one block via [`highlight_code`] - syntect
/// fg over OUR code background; blocks with no/unknown language fall back to
/// the plain CodeBlock style.
fn markdown_lines(text: &str) -> Vec<Line<'static>> {
    let md_lines = markdown::to_lines(text);
    let mut out = Vec::with_capacity(md_lines.len());
    let mut i = 0;
    while i < md_lines.len() {
        let lang = match md_lines[i].code_lang.as_deref() {
            Some(lang) if !lang.is_empty() => lang.to_string(),
            _ => {
                out.push(plain_md_line(&md_lines[i]));
                i += 1;
                continue;
            }
        };
        let mut end = i;
        while end < md_lines.len() && md_lines[end].code_lang.as_deref() == Some(lang.as_str()) {
            end += 1;
        }
        let block = &md_lines[i..end];
        let texts: Vec<String> = block.iter().map(md_line_text).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        match highlight_code(&refs, &lang) {
            Some(highlighted) => {
                for (fragments, text) in highlighted.into_iter().zip(&texts) {
                    if fragments.is_empty() {
                        // Blank (or all-whitespace) code line: keep the same
                        // bg treatment the plain path gives it.
                        out.push(Line::from(Span::styled(
                            text.clone(),
                            md_style(MdStyle::CodeBlock),
                        )));
                    } else {
                        out.push(Line::from(
                            fragments
                                .into_iter()
                                .map(|((r, g, b), text)| {
                                    Span::styled(
                                        text,
                                        Style::default().fg(Color::Rgb(r, g, b)).bg(CODE_BG),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        ));
                    }
                }
            }
            // Unknown language: the existing plain CodeBlock rendering.
            None => out.extend(block.iter().map(plain_md_line)),
        }
        i = end;
    }
    out
}

/// One [`MdLine`] rendered the plain way: each span through the single
/// [`md_style`] mapping.
fn plain_md_line(line: &MdLine) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|span| Span::styled(span.text.clone(), md_style(span.style)))
            .collect::<Vec<_>>(),
    )
}

/// One MdLine's concatenated text (code lines carry a single span, but this
/// stays correct regardless).
fn md_line_text(line: &MdLine) -> String {
    line.spans.iter().map(|s| s.text.as_str()).collect()
}

/// Splits multi-line text into one string per source row: ratatui does not break
/// a single `Line` on an embedded '\n', so multi-line messages must become
/// multiple `Line`s or they collapse into one blob. Tabs become two spaces; `\r`
/// is stripped; empty lines survive as blank rows. Width-wrapping is the
/// Paragraph's job (`Wrap`), so this only handles hard line breaks.
fn text_rows(text: &str) -> Vec<String> {
    text.replace('\r', "")
        .split('\n')
        .map(|row| row.replace('\t', "  "))
        .collect()
}

fn block_line(line: &StyledLine) -> Line<'static> {
    let text = if line.text.is_empty() {
        " ".to_string()
    } else {
        line.text.replace('\t', "  ")
    };
    Line::styled(text, line_style(line.style))
}

/// The running-spinner animation frames (braille), advanced by the adapter's
/// animation tick while a Turn is running.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ---------------------------------------------------------------------------
// The powerline status bar.
// ---------------------------------------------------------------------------

/// The status bar's base background - what the middle gap and the outermost
/// separators fade into.
const BAR_BG: Color = Color::Rgb(30, 30, 40);

/// The shared dark bg of the low-emphasis right-side segments (thinking,
/// tokens at `Ok` pressure).
const SEGMENT_DARK_BG: Color = Color::Rgb(40, 44, 58);

/// Powerline separators (Nerd Font): right-pointing after left-side segments,
/// left-pointing before right-side segments. Drawn fg = the segment's bg over
/// bg = the neighbor's bg - the standard powerline triangle technique.
const SEP_RIGHT: &str = "\u{e0b0}"; //
const SEP_LEFT: &str = "\u{e0b2}"; //

/// The semantic kind of one status bar segment. The pure assembly
/// ([`status_segments`]) speaks kinds; [`segment_style`] is the single place
/// they become colors (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Agent idle - calm green mode block.
    ModeIdle,
    /// Agent running - yellow mode block with the animated spinner.
    ModeRunning,
    /// `suspenders · <base_url>` - the brand + endpoint, lowest priority.
    Connection,
    /// The Ctrl-T thinking-expansion state (`▾`/`▸`). Always visible so the
    /// toggle has feedback even when no Thinking items are on screen.
    Thinking,
    /// The `~Ntok / budget` estimate, colored by its [`PressureLevel`].
    Tokens(PressureLevel),
    /// The viewport scroll position (`Bot`/`Top`/`NN%`) - the bold accent.
    Position,
}

/// One assembled segment: its text (padding included) and its kind.
type Segment = (String, SegmentKind);

/// The bottom status bar, powerline style: left segments (mode, connection)
/// fading into the base bg, right segments (thinking, tokens, position)
/// growing out of it, each block joined by triangle separators. `geometry` is
/// the `(total_lines, height)` the viewport was measured at THIS frame - the
/// position segment must agree with what is actually drawn above it.
pub fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    t: &Transcript,
    base_url: &str,
    spinner: u64,
    viewport: &Viewport,
    geometry: (usize, usize),
) {
    let (total_lines, height) = geometry;
    let position = scroll_position_label(
        viewport.top_offset(total_lines, height),
        total_lines,
        height,
    );
    let (left, right) = status_segments(
        area.width as usize,
        t.status,
        spinner,
        base_url,
        t.thinking_expanded,
        tokens_label(t.token_estimate, t.context_budget).map(|label| (label, t.pressure_level)),
        position,
    );

    let mut spans: Vec<Span> = Vec::new();
    for (i, (text, kind)) in left.iter().enumerate() {
        spans.push(Span::styled(text.clone(), segment_style(*kind)));
        // The separator wears THIS segment's bg over the NEXT one's (the base
        // bg after the last segment) - that is what draws the triangle.
        let next_bg = left.get(i + 1).map(|(_, k)| segment_bg(*k)).unwrap_or(BAR_BG);
        spans.push(Span::styled(
            SEP_RIGHT,
            Style::default().fg(segment_bg(*kind)).bg(next_bg),
        ));
    }
    let gap = (area.width as usize).saturating_sub(segments_width(&left, &right));
    spans.push(Span::styled(" ".repeat(gap), Style::default().bg(BAR_BG)));
    let mut prev_bg = BAR_BG;
    for (text, kind) in &right {
        spans.push(Span::styled(
            SEP_LEFT,
            Style::default().fg(segment_bg(*kind)).bg(prev_bg),
        ));
        spans.push(Span::styled(text.clone(), segment_style(*kind)));
        prev_bg = segment_bg(*kind);
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(BAR_BG));
    frame.render_widget(bar, area);
}

/// Assembles the status bar's segments, pure: `(left, right)`, already
/// fitted to `width` by [`fit_segments`]. The thinking segment reads the
/// core's `thinking_expanded` directly - Ctrl-T flips it on the next draw,
/// which is the whole point (the toggle used to be invisible when no Thinking
/// items were on screen). `tokens` is `None` when no estimate exists yet.
fn status_segments(
    width: usize,
    status: Status,
    spinner: u64,
    base_url: &str,
    thinking_expanded: bool,
    tokens: Option<(String, PressureLevel)>,
    position: String,
) -> (Vec<Segment>, Vec<Segment>) {
    // While running, the animated braille spinner lives inside the mode
    // block; the frame counter comes from the adapter's animation tick.
    let mode = match status {
        Status::Idle => (" IDLE ".to_string(), SegmentKind::ModeIdle),
        Status::Running => (
            format!(" {} RUNNING ", SPINNER[(spinner as usize) % SPINNER.len()]),
            SegmentKind::ModeRunning,
        ),
    };
    let left = vec![
        mode,
        (
            format!(" suspenders · {base_url} "),
            SegmentKind::Connection,
        ),
    ];

    let marker = if thinking_expanded { "▾" } else { "▸" };
    let mut right = vec![(format!(" {marker} thinking "), SegmentKind::Thinking)];
    if let Some((label, level)) = tokens {
        right.push((format!(" {label} "), SegmentKind::Tokens(level)));
    }
    right.push((format!(" {position} "), SegmentKind::Position));

    fit_segments(left, right, width)
}

/// Drops segments until the bar fits `width`, lowest-value first: connection,
/// then thinking, then tokens - mode and position survive longest. Simple
/// width arithmetic on purpose; a partially-truncated segment would garble
/// the powerline blocks.
fn fit_segments(
    mut left: Vec<Segment>,
    mut right: Vec<Segment>,
    width: usize,
) -> (Vec<Segment>, Vec<Segment>) {
    let drop_order: [fn(SegmentKind) -> bool; 3] = [
        |k| k == SegmentKind::Connection,
        |k| k == SegmentKind::Thinking,
        |k| matches!(k, SegmentKind::Tokens(_)),
    ];
    for dropped in drop_order {
        if segments_width(&left, &right) <= width {
            break;
        }
        left.retain(|(_, kind)| !dropped(*kind));
        right.retain(|(_, kind)| !dropped(*kind));
    }
    (left, right)
}

/// The columns the segments occupy: their text plus one separator glyph per
/// segment (left segments each trail one, right segments each lead with one).
fn segments_width(left: &[Segment], right: &[Segment]) -> usize {
    let text: usize = left
        .iter()
        .chain(right)
        .map(|(text, _)| text.chars().count())
        .sum();
    text + left.len() + right.len()
}

/// The position segment's label, vim-ruler style: `Bot` at the tail, `Top` at
/// the top of overflowing content, otherwise the percentage of the scroll
/// range. Content that FITS the viewport is `Bot`, not `Top`: the tail is
/// visible, which is what a pinned reader cares about - and it keeps the
/// label stable as a fresh session grows past one page.
fn scroll_position_label(top: usize, total_lines: usize, height: usize) -> String {
    let max_top = total_lines.saturating_sub(height);
    if top >= max_top {
        // Also covers max_top == 0 (content fits, or empty/degenerate
        // geometry) - no division by zero below.
        "Bot".to_string()
    } else if top == 0 {
        "Top".to_string()
    } else {
        format!("{}%", top * 100 / max_top)
    }
}

/// The Composer: the draft, pre-wrapped by the pure [`composer::layout`]
/// (char-based, so the cursor cell below is exact - `Paragraph`'s word-wrap
/// points can't be queried). The FIRST row keeps the "› " gutter; every
/// continuation row - hard-newline and wrapped alike - indents 2 spaces to
/// align under it, mirroring how submitted multi-line User prompts render.
///
/// When the draft is taller than the box, the Composer scrolls internally
/// ([`composer::first_visible_row`]) so the cursor row stays visible, near
/// the bottom like a terminal. The REAL terminal cursor is placed at the
/// cursor's cell - except while the Approval modal owns the keyboard, when a
/// blinking composer cursor would misstate where keys go.
pub fn render_composer(frame: &mut Frame, area: Rect, t: &Transcript, layout: &ComposerLayout) {
    let visible = area.height as usize;
    if visible == 0 || area.width < 2 {
        return;
    }
    let top = composer::first_visible_row(layout.cursor_row, visible);
    let gutter = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let lines: Vec<Line> = layout
        .rows
        .iter()
        .enumerate()
        .skip(top)
        .take(visible)
        .map(|(i, row)| {
            let prefix = if i == 0 { "› " } else { "  " };
            Line::from(vec![Span::styled(prefix, gutter), Span::raw(row.clone())])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);

    if t.pending_approval.is_none() {
        // `cursor_col < width` by the layout contract, so the cell is always
        // inside the Composer's rect; `top <= cursor_row` by construction.
        frame.set_cursor_position((
            area.x + 2 + layout.cursor_col as u16,
            area.y + (layout.cursor_row - top) as u16,
        ));
    }
}

/// The Approval modal for a run_command Tool Call: `y` approves, `n` denies,
/// `a` approves-always. Key handling lives in the Transcript core; this draws it.
pub fn render_approval_modal(frame: &mut Frame, area: Rect, command: &str) {
    let width = (command.chars().count() as u16 + 8).max(44).min(area.width.saturating_sub(4));
    let height = 8u16.min(area.height.saturating_sub(2));
    let modal = centered_rect(width, height, area);

    frame.render_widget(Clear, modal);
    let block = Block::default().title("Approval").borders(Borders::ALL);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let body = Paragraph::new(vec![
        Line::styled("Run command?", Style::default().add_modifier(Modifier::BOLD)),
        Line::styled(command.to_string(), Style::default().fg(Color::Yellow)),
        Line::raw(""),
        Line::from(vec![
            Span::styled("[y]es", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" / "),
            Span::styled("[n]o", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" / "),
            Span::styled("[a]lways", Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
        ]),
    ])
    .wrap(Wrap { trim: false });
    frame.render_widget(body, inner);
}

/// The `--resume` Session Picker: a centered bordered list, one row per
/// Session (`stamp  label`), the cursor row reversed+bold, and a dim key-hint
/// footer. Key handling lives in the pure [`Picker`] core; this only draws.
pub fn render_picker(frame: &mut Frame, picker: &Picker) {
    const FOOTER: &str = "↑/↓ select · Enter resume · Esc fresh session · q quit";

    let area = frame.area();
    let content_width = picker
        .entries
        .iter()
        .map(|e| e.stamp.chars().count() + 2 + e.label.chars().count())
        .chain(std::iter::once(FOOTER.chars().count()))
        .max()
        .unwrap_or(0) as u16;
    // rows + footer + borders; both dimensions capped to the terminal.
    let width = (content_width + 4).max(44).min(area.width.saturating_sub(2));
    let height = (picker.entries.len() as u16 + 3).min(area.height.saturating_sub(2));
    let modal = centered_rect(width, height, area);

    frame.render_widget(Clear, modal);
    let block = Block::default()
        .title(" resume a session ")
        .borders(Borders::ALL);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let mut lines: Vec<Line> = picker
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let style = if i == picker.cursor {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::styled(format!("{}  {}", entry.stamp, entry.label), style)
        })
        .collect();
    lines.push(Line::styled(
        FOOTER,
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// The `~Ntok / budget` label, or `None` when either number is missing.
fn tokens_label(estimate: Option<u64>, budget: Option<u64>) -> Option<String> {
    match (estimate, budget) {
        (Some(estimate), Some(budget)) => Some(format!("~{estimate}tok / {budget}")),
        _ => None,
    }
}

fn join_summary(name: &str, summary: &str) -> String {
    if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name} {summary}")
    }
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or("")
}

/// A centered `width`×`height` rect inside `area`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The color of the first fragment whose text contains `needle`.
    fn color_of(lines: &[Vec<CodeFragment>], needle: &str) -> (u8, u8, u8) {
        lines
            .iter()
            .flatten()
            .find(|(_, text)| text.contains(needle))
            .unwrap_or_else(|| panic!("no fragment containing {needle:?}"))
            .0
    }

    #[test]
    fn highlight_code_colors_keywords_differently_from_string_literals() {
        // Syntect fragments the literal (quotes vs contents); the contents
        // fragment is what must differ from the `fn` keyword.
        let lines = highlight_code(&["fn main() { let s = \"hi\"; }"], "rust").unwrap();
        assert_ne!(color_of(&lines, "fn"), color_of(&lines, "hi"));
    }

    #[test]
    fn highlight_code_resolves_extension_tokens_too() {
        // `find_syntax_by_token` matches extensions, not just names.
        assert!(highlight_code(&["let x = 1;"], "rs").is_some());
        assert!(highlight_code(&["x = 1"], "py").is_some());
    }

    #[test]
    fn highlight_code_returns_none_for_an_unknown_lang() {
        assert_eq!(highlight_code(&["whatever"], "notareallanguage"), None);
    }

    #[test]
    fn highlight_code_on_empty_input_is_some_empty() {
        assert_eq!(highlight_code(&[], "rust"), Some(vec![]));
    }

    #[test]
    fn highlight_code_blank_line_yields_no_fragments() {
        let lines = highlight_code(&["let a = 1;", "", "let b = 2;"], "rust").unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].is_empty());
        assert!(!lines[0].is_empty() && !lines[2].is_empty());
    }

    #[test]
    fn highlight_code_carries_parse_state_across_lines() {
        // A block comment opened on line 1 must color line 2 as comment, not code.
        let lines = highlight_code(&["/* comment", "still comment */", "let x = 1;"], "rust")
            .unwrap();
        let comment = color_of(&lines[..1], "comment");
        assert_eq!(color_of(&lines[1..2], "still comment"), comment);
        assert_ne!(color_of(&lines[2..], "let"), comment);
    }

    #[test]
    fn highlight_code_preserves_the_line_text_verbatim() {
        let source = "fn add(a: u32, b: u32) -> u32 { a + b }";
        let lines = highlight_code(&[source], "rust").unwrap();
        let joined: String = lines[0].iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(joined, source);
    }

    // -----------------------------------------------------------------------
    // The scroll-position label.
    // -----------------------------------------------------------------------

    #[test]
    fn scroll_position_label_is_bot_at_the_tail() {
        // top == max_top (100 - 20 = 80): the tail is on screen.
        assert_eq!(scroll_position_label(80, 100, 20), "Bot");
    }

    #[test]
    fn scroll_position_label_is_top_at_the_top_of_overflowing_content() {
        assert_eq!(scroll_position_label(0, 100, 20), "Top");
    }

    #[test]
    fn scroll_position_label_is_the_percentage_of_the_scroll_range() {
        // max_top = 80; vim-ruler style: 0% at the top, 100% at the tail.
        assert_eq!(scroll_position_label(40, 100, 20), "50%");
        assert_eq!(scroll_position_label(8, 100, 20), "10%");
        assert_eq!(scroll_position_label(79, 100, 20), "98%");
    }

    #[test]
    fn scroll_position_label_shows_bot_when_the_content_fits() {
        // The tail is visible, so a pinned reader sees `Bot` - and the label
        // does not flap Top→Bot as a fresh session grows past one page.
        assert_eq!(scroll_position_label(0, 5, 20), "Bot");
        assert_eq!(scroll_position_label(0, 20, 20), "Bot");
        assert_eq!(scroll_position_label(0, 0, 20), "Bot");
    }

    #[test]
    fn scroll_position_label_survives_zero_heights() {
        // Degenerate geometry (zero-height viewport) must not divide by zero.
        assert_eq!(scroll_position_label(0, 0, 0), "Bot");
        assert_eq!(scroll_position_label(100, 100, 0), "Bot");
        assert_eq!(scroll_position_label(0, 100, 0), "Top");
        assert_eq!(scroll_position_label(50, 100, 0), "50%");
    }

    // -----------------------------------------------------------------------
    // The powerline segment assembly.
    // -----------------------------------------------------------------------

    /// Assembles at `width` with everything present: running, tokens known.
    fn segments_at(width: usize) -> (Vec<Segment>, Vec<Segment>) {
        status_segments(
            width,
            Status::Running,
            0,
            "http://localhost:8080",
            false,
            Some(("~1200tok / 32000".to_string(), PressureLevel::Ok)),
            "Bot".to_string(),
        )
    }

    fn kinds(segments: &[Segment]) -> Vec<SegmentKind> {
        segments.iter().map(|(_, kind)| *kind).collect()
    }

    #[test]
    fn a_wide_bar_keeps_every_segment_in_order() {
        let (left, right) = segments_at(200);
        assert_eq!(
            kinds(&left),
            vec![SegmentKind::ModeRunning, SegmentKind::Connection]
        );
        assert_eq!(
            kinds(&right),
            vec![
                SegmentKind::Thinking,
                SegmentKind::Tokens(PressureLevel::Ok),
                SegmentKind::Position,
            ]
        );
        assert!(segments_width(&left, &right) <= 200);
    }

    #[test]
    fn a_narrow_bar_drops_the_connection_segment_first() {
        let (left, right) = segments_at(60);
        assert_eq!(kinds(&left), vec![SegmentKind::ModeRunning]);
        assert_eq!(
            kinds(&right),
            vec![
                SegmentKind::Thinking,
                SegmentKind::Tokens(PressureLevel::Ok),
                SegmentKind::Position,
            ]
        );
    }

    #[test]
    fn a_narrower_bar_drops_thinking_then_tokens() {
        let (left, right) = segments_at(40);
        assert_eq!(kinds(&left), vec![SegmentKind::ModeRunning]);
        assert_eq!(
            kinds(&right),
            vec![SegmentKind::Tokens(PressureLevel::Ok), SegmentKind::Position]
        );

        let (left, right) = segments_at(20);
        assert_eq!(kinds(&left), vec![SegmentKind::ModeRunning]);
        assert_eq!(kinds(&right), vec![SegmentKind::Position]);
    }

    #[test]
    fn mode_and_position_survive_even_when_nothing_fits() {
        // Dropping stops at the last two; a sub-minimal width never panics.
        let (left, right) = segments_at(1);
        assert_eq!(kinds(&left), vec![SegmentKind::ModeRunning]);
        assert_eq!(kinds(&right), vec![SegmentKind::Position]);
    }

    #[test]
    fn the_thinking_segment_flips_its_marker_with_ctrl_t_state() {
        let thinking = |expanded: bool| {
            let (_, right) = status_segments(
                200,
                Status::Idle,
                0,
                "http://localhost:8080",
                expanded,
                None,
                "Bot".to_string(),
            );
            right
                .iter()
                .find(|(_, kind)| *kind == SegmentKind::Thinking)
                .expect("thinking segment is always assembled")
                .0
                .clone()
        };
        assert_eq!(thinking(true), " ▾ thinking ");
        assert_eq!(thinking(false), " ▸ thinking ");
    }

    #[test]
    fn the_tokens_segment_is_absent_until_an_estimate_exists() {
        let (_, right) = status_segments(
            200,
            Status::Idle,
            0,
            "http://localhost:8080",
            false,
            None,
            "Bot".to_string(),
        );
        assert_eq!(
            kinds(&right),
            vec![SegmentKind::Thinking, SegmentKind::Position]
        );
    }

    #[test]
    fn the_running_mode_segment_carries_the_spinner_frame() {
        let mode = |spinner: u64| {
            status_segments(200, Status::Running, spinner, "u", false, None, "Bot".into())
                .0
                .remove(0)
                .0
        };
        assert_eq!(mode(0), format!(" {} RUNNING ", SPINNER[0]));
        assert_eq!(mode(1), format!(" {} RUNNING ", SPINNER[1]));
        // The counter wraps around the frame set.
        assert_eq!(mode(SPINNER.len() as u64), format!(" {} RUNNING ", SPINNER[0]));
    }

    #[test]
    fn every_segment_style_carries_a_bg_for_the_separators() {
        // The powerline triangles are fg-over-neighbor-bg; a segment without
        // a bg would render a hole in the bar.
        for kind in [
            SegmentKind::ModeIdle,
            SegmentKind::ModeRunning,
            SegmentKind::Connection,
            SegmentKind::Thinking,
            SegmentKind::Tokens(PressureLevel::Ok),
            SegmentKind::Tokens(PressureLevel::Elevated),
            SegmentKind::Tokens(PressureLevel::Critical),
            SegmentKind::Position,
        ] {
            assert!(segment_style(kind).bg.is_some(), "{kind:?} has no bg");
        }
    }

    // -----------------------------------------------------------------------
    // The visible-window math (per-item wrapped counts → slice + offset).
    // -----------------------------------------------------------------------

    #[test]
    fn visible_window_at_offset_zero_starts_at_the_first_item() {
        // Items of 3+4+5 rows, window of 6: items 0 and 1 cover rows 0..7.
        assert_eq!(visible_window(&[3, 4, 5], 0, 6), (0..2, 0));
    }

    #[test]
    fn visible_window_at_the_tail_reaches_the_last_item() {
        // total 12, height 5 → clamped top 7: row 7 is item 2's first row.
        assert_eq!(visible_window(&[3, 4, 5], 7, 5), (2..3, 0));
    }

    #[test]
    fn visible_window_keeps_an_item_straddling_the_top_boundary() {
        // top 5 lands inside item 1 (rows 3..7): the slice starts there and
        // the offset is relative to ITS first row, not the session's.
        assert_eq!(visible_window(&[3, 4, 5], 5, 4), (1..3, 2));
    }

    #[test]
    fn visible_window_inside_a_single_huge_item_selects_just_it() {
        assert_eq!(visible_window(&[1000], 500, 20), (0..1, 500));
    }

    #[test]
    fn visible_window_of_an_empty_transcript_is_empty() {
        assert_eq!(visible_window(&[], 0, 20), (0..0, 0));
    }

    #[test]
    fn visible_window_taller_than_the_content_takes_everything() {
        assert_eq!(visible_window(&[3, 4, 5], 0, 100), (0..3, 0));
    }

    #[test]
    fn visible_window_survives_an_offset_past_the_content() {
        // The caller clamps `top`; a degenerate overshoot selects nothing
        // rather than panicking or underflowing.
        assert_eq!(visible_window(&[3, 4], 50, 10), (2..2, 43));
    }

    // -----------------------------------------------------------------------
    // The render cache (sync + the streaming tail's monotonic key).
    // -----------------------------------------------------------------------

    use crate::ui::transcript::TranscriptOpts;

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn fresh_transcript() -> Transcript {
        Transcript::new(TranscriptOpts::default())
    }

    #[test]
    fn cache_sync_builds_one_entry_per_settled_item_with_its_wrapped_count() {
        let mut t = fresh_transcript();
        t.messages.push(TranscriptItem::User {
            // At width 10 the word-wrapper puts the unbreakable 16-char word
            // below the "› " gutter row and splits it: 3 rows.
            text: "0123456789012345".to_string(),
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, 10);
        assert_eq!(cache.items.len(), 2);
        assert_eq!(cache.items[1].wrapped, 3);
    }

    #[test]
    fn cache_sync_extends_for_appends_without_rebuilding_settled_entries() {
        let mut t = fresh_transcript();
        t.messages.push(TranscriptItem::Info {
            text: "first".to_string(),
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, 80);

        // Mutating a settled item WITHOUT bumping the revision is outside the
        // core's contract (it only appends); the cache must not have paid to
        // notice - the stale entry proves nothing was rebuilt.
        t.messages[1] = TranscriptItem::Info {
            text: "mutated".to_string(),
        };
        t.messages.push(TranscriptItem::Info {
            text: "appended".to_string(),
        });
        cache.sync(&t, 80);
        assert_eq!(cache.items.len(), 3);
        assert_eq!(line_text(&cache.items[1].lines[0]), "first");
        assert_eq!(line_text(&cache.items[2].lines[0]), "appended");
    }

    #[test]
    fn cache_sync_rebuilds_when_the_messages_revision_moves() {
        let mut t = fresh_transcript();
        t.messages.push(TranscriptItem::Info {
            text: "first".to_string(),
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, 80);

        // A structural edit (SteeringDelivered's remove) bumps the revision:
        // the cache rebuilds and sees the new content.
        t.messages[1] = TranscriptItem::Info {
            text: "replaced".to_string(),
        };
        t.messages_revision += 1;
        cache.sync(&t, 80);
        assert_eq!(cache.items.len(), 2);
        assert_eq!(line_text(&cache.items[1].lines[0]), "replaced");
    }

    #[test]
    fn cache_sync_rebuilds_when_the_width_changes() {
        let mut t = fresh_transcript();
        t.messages.push(TranscriptItem::User {
            text: "0123456789012345".to_string(),
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, 80);
        assert_eq!(cache.items[1].wrapped, 1);
        cache.sync(&t, 10); // resize: every wrapped count is stale
        assert!(cache.items[1].wrapped > 1);
    }

    #[test]
    fn cache_sync_rebuilds_when_the_thinking_toggle_flips() {
        let mut t = fresh_transcript();
        t.messages.push(TranscriptItem::Thinking {
            text: "line one\nline two".to_string(),
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, 80);
        assert_eq!(cache.items[1].lines.len(), 1); // collapsed one-liner
        t.thinking_expanded = true;
        cache.sync(&t, 80);
        assert_eq!(cache.items[1].lines.len(), 3); // header + both rows
    }

    #[test]
    fn streaming_cache_reparses_only_when_the_char_length_moves() {
        let mut cache = RenderCache::new();
        cache.sync_streaming("hello", 80);
        assert_eq!(line_text(&cache.streaming.as_ref().unwrap().lines[0]), "hello");

        // Same length, different text: the monotonic-key contract - within a
        // message the snapshot only GROWS, so an equal length means unchanged
        // and the cached lines are reused as-is.
        cache.sync_streaming("world", 80);
        assert_eq!(line_text(&cache.streaming.as_ref().unwrap().lines[0]), "hello");

        // Growth re-parses; the end of streaming clears, so the next
        // message can never collide with a stale entry of the same length.
        cache.sync_streaming("hello more", 80);
        assert_eq!(line_text(&cache.streaming.as_ref().unwrap().lines[0]), "hello more");
        cache.sync_streaming("", 80);
        assert!(cache.streaming.is_none());
    }

    #[test]
    fn per_item_wrapped_counts_sum_to_the_whole_paragraph_measure() {
        // The windowed render's geometry is the SUM of per-item measures; it
        // is only the same total the old whole-paragraph measure produced if
        // ratatui wraps each `Line` independently. Guard that assumption.
        let items = [
            TranscriptItem::User {
                text: "a user prompt long enough to wrap at a narrow width".to_string(),
            },
            TranscriptItem::Assistant {
                text: "some *markdown* with a fairly long paragraph in it\n\n- and\n- a list"
                    .to_string(),
            },
            TranscriptItem::Info {
                text: "an info line".to_string(),
            },
        ];
        for width in [10u16, 24, 80] {
            let per_item: usize = items
                .iter()
                .map(|item| wrapped_count(message_lines(item, false), width))
                .sum();
            let whole: Vec<Line> = items
                .iter()
                .flat_map(|item| message_lines(item, false))
                .collect();
            assert_eq!(per_item, wrapped_count(whole, width), "width {width}");
        }
    }
}
