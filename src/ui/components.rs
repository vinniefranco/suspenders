//! UI Components - the SINGLE mapping from the semantic display vocabulary
//! (ADR-0008) to ratatui `Style`/`Color`, plus the render helpers the frontend
//! draws with.
//!
//! This is the one place semantics become terminal colors: [`LineStyle`] →
//! color for a Block's lines, [`PressureLevel`] → color/emphasis for the status
//! bar. Plugins and the Screen core never touch ratatui; they speak the
//! vocabulary and this module renders it. Everything here is pure presentation
//! of [`TranscriptItem`]s - no state, no IO. Only this module and [`crate::ui`]
//! `use ratatui` / `use crossterm` (ADR-0019 invariant).

use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use syntect::easy::HighlightLines;
use syntect::parsing::SyntaxSet;

use thousands::Separable;

use crate::ui::composer::{self, ComposerLayout, OverlayStatus, OverlayView};
use crate::ui::markdown::{self, MdLine, MdStyle};
use crate::ui::picker::Picker;
use crate::ui::screen::{PressureLevel, Screen, Status};
use crate::ui::selector::SelectorRow;
use crate::ui::slash;
use crate::ui::theme::{self, Theme};
use crate::ui::transcript::{LineStyle, StyledLine, TranscriptItem};
use crate::ui::viewport::Viewport;

// ---------------------------------------------------------------------------
// The single semantic → color mapping (ADR-0008), colored by the active
// Theme (ADR-0038): every mapping reads its color from a slot; the
// attributes (bold/italic/underline) are meaning and stay fixed here.
// ---------------------------------------------------------------------------

/// The one [`theme::Color`] → ratatui translation, at the presentation
/// boundary: `ui::theme` never imports ratatui (ADR-0019 invariant), so the
/// terminal type appears only here.
fn tui_color(color: theme::Color) -> Color {
    match color {
        theme::Color::Black => Color::Black,
        theme::Color::Red => Color::Red,
        theme::Color::Green => Color::Green,
        theme::Color::Yellow => Color::Yellow,
        theme::Color::Blue => Color::Blue,
        theme::Color::Magenta => Color::Magenta,
        theme::Color::Cyan => Color::Cyan,
        theme::Color::Gray => Color::Gray,
        theme::Color::DarkGray => Color::DarkGray,
        theme::Color::LightRed => Color::LightRed,
        theme::Color::LightGreen => Color::LightGreen,
        theme::Color::LightYellow => Color::LightYellow,
        theme::Color::LightBlue => Color::LightBlue,
        theme::Color::LightMagenta => Color::LightMagenta,
        theme::Color::LightCyan => Color::LightCyan,
        theme::Color::White => Color::White,
        theme::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The ONE mapping from a semantic [`LineStyle`] to a ratatui [`Style`]
/// (ADR-0008). Plugins produce styles; this turns them into the active
/// Theme's colors.
pub fn line_style(style: LineStyle, theme: &Theme) -> Style {
    match style {
        LineStyle::Added => Style::default().fg(tui_color(theme.added)),
        LineStyle::Removed => Style::default().fg(tui_color(theme.removed)),
        LineStyle::Context => Style::default().fg(tui_color(theme.context)),
        LineStyle::Emphasis => Style::default().add_modifier(Modifier::BOLD),
        LineStyle::Muted => Style::default()
            .fg(tui_color(theme.muted))
            .add_modifier(Modifier::ITALIC),
        LineStyle::Default => Style::default(),
    }
}

/// The ONE mapping from a semantic markdown [`MdStyle`] to a ratatui [`Style`]
/// (ADR-0008's move, applied to assistant markdown): [`markdown::to_lines`]
/// speaks semantics; this is where they become the active Theme's colors.
pub fn md_style(style: MdStyle, theme: &Theme) -> Style {
    match style {
        MdStyle::Plain => Style::default(),
        MdStyle::Bold => Style::default().add_modifier(Modifier::BOLD),
        MdStyle::Italic => Style::default().add_modifier(Modifier::ITALIC),
        MdStyle::BoldItalic => Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC),
        MdStyle::Code => Style::default().fg(tui_color(theme.code)),
        MdStyle::CodeBlock => Style::default()
            .fg(tui_color(theme.code_block))
            .bg(tui_color(theme.code_block_bg)),
        MdStyle::Heading => Style::default()
            .fg(tui_color(theme.heading))
            .add_modifier(Modifier::BOLD),
        MdStyle::Bullet => Style::default().fg(tui_color(theme.bullet)),
        MdStyle::Quote => Style::default()
            .fg(tui_color(theme.quote))
            .add_modifier(Modifier::ITALIC),
        MdStyle::Link => Style::default()
            .fg(tui_color(theme.link))
            .add_modifier(Modifier::UNDERLINED),
    }
}

/// The ONE mapping from the semantic [`PressureLevel`] (ADR-0008) to the
/// tokens segment's style: `Ok` reads muted, `Elevated` warns, `Critical`
/// alarms. Segment form (fg ON a bg) because the status bar is a powerline of
/// colored blocks - the semantics are unchanged, only the presentation moved
/// from colored text to colored blocks.
pub fn pressure_style(level: PressureLevel, theme: &Theme) -> Style {
    match level {
        PressureLevel::Critical => Style::default()
            .fg(tui_color(theme.pressure_critical_fg))
            .bg(tui_color(theme.pressure_critical_bg))
            .add_modifier(Modifier::BOLD),
        PressureLevel::Elevated => Style::default()
            .fg(tui_color(theme.pressure_elevated_fg))
            .bg(tui_color(theme.pressure_elevated_bg)),
        PressureLevel::Ok => Style::default()
            .fg(tui_color(theme.pressure_ok_fg))
            .bg(tui_color(theme.segment_muted_bg)),
    }
}

/// The ONE mapping from a [`SegmentKind`] to its powerline segment style
/// (ADR-0008: this is the only place segment semantics become colors). Every
/// segment style carries a bg - the powerline separators are drawn from the
/// adjacent segments' bgs ([`segment_bg`]).
pub fn segment_style(kind: SegmentKind, theme: &Theme) -> Style {
    match kind {
        SegmentKind::ModeIdle | SegmentKind::Position => Style::default()
            .fg(tui_color(theme.segment_idle_fg))
            .bg(tui_color(theme.segment_idle_bg))
            .add_modifier(Modifier::BOLD),
        SegmentKind::ModeRunning => Style::default()
            .fg(tui_color(theme.segment_running_fg))
            .bg(tui_color(theme.segment_running_bg))
            .add_modifier(Modifier::BOLD),
        // Model + Connection are the two connection facts, styled identically.
        SegmentKind::Connection | SegmentKind::Model => Style::default()
            .fg(tui_color(theme.segment_model_fg))
            .bg(tui_color(theme.segment_model_bg)),
        // Thinking + Tools are the two detail-on-demand toggles, styled alike.
        SegmentKind::Thinking | SegmentKind::Tools => Style::default()
            .fg(tui_color(theme.segment_toggle_fg))
            .bg(tui_color(theme.segment_muted_bg)),
        // Cost is a quiet figure: the same muted read as tokens at Ok
        // pressure, without the pressure routing (cost carries no level).
        SegmentKind::Cost => Style::default()
            .fg(tui_color(theme.segment_cost_fg))
            .bg(tui_color(theme.segment_muted_bg)),
        // Tokens keep the single PressureLevel mapping - segment_style only
        // routes to it, it does not restate the colors.
        SegmentKind::Tokens(level) => pressure_style(level, theme),
    }
}

/// A segment's background - what the powerline separator glyphs blend with.
fn segment_bg(kind: SegmentKind, theme: &Theme) -> Color {
    segment_style(kind, theme)
        .bg
        .unwrap_or_else(|| tui_color(theme.bar_bg))
}

// ---------------------------------------------------------------------------
// Render helpers.
// ---------------------------------------------------------------------------

/// The two connection facts the status bar shows (ADR-0033): the fixed endpoint
/// and the mutable Active Model. Both are adapter-carried - the pure Screen
/// core stays command-agnostic and holds neither. The adapter OWNS them as a
/// [`ConnectionFacts`]; this is the borrowed form the render path takes, so
/// both elements are always name-addressed, never a position-coupled pair.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionView<'a> {
    /// The Session's fixed `base_url`.
    pub base_url: &'a str,
    /// The Agent's Active Model, refreshed by the adapter after any batch that
    /// could change it (a `/model` pick).
    pub model: &'a str,
}

/// The adapter's owned copy of the two connection facts - the endpoint (a fixed
/// Session fact) and the Active Model (mutable Agent state the adapter refreshes
/// after a `/model` pick). Named fields, never a `(String, String)` pair, so the
/// two can't be swapped at a call site. Borrowed into a [`ConnectionView`] at the
/// render boundary via [`ConnectionFacts::view`].
#[derive(Debug, Clone)]
pub struct ConnectionFacts {
    pub base_url: String,
    pub model: String,
}

impl ConnectionFacts {
    /// The borrowed [`ConnectionView`] the render path takes.
    pub fn view(&self) -> ConnectionView<'_> {
        ConnectionView {
            base_url: &self.base_url,
            model: &self.model,
        }
    }
}

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
    t: &Screen,
    conn: ConnectionView,
    spinner: u64,
    viewport: &Viewport,
    cache: &mut RenderCache,
    theme: &Theme,
) -> (usize, usize) {
    let area = frame.area();
    // The Composer's one render window (ADR-0034): the draft, the char-index
    // cursor, and the open overlay, read together.
    let composer_view = t.composer().view();
    let layout = composer::layout(
        composer_view.draft,
        composer_view.cursor,
        area.width.saturating_sub(2) as usize,
    );
    let composer_height = layout
        .rows
        .len()
        .min(composer::max_visible_rows(area.height as usize));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                         // transcript viewport
            Constraint::Length(1),                      // status bar
            Constraint::Length(composer_height as u16), // composer (grows with the draft)
        ])
        .split(area);

    // The viewport renders FIRST: the status bar's position segment reads the
    // measured geometry (and the Viewport's clamped top) from this frame, not
    // a stale one.
    let geometry = render_viewport(frame, chunks[0], t, viewport, cache, theme);
    render_status_bar(
        frame, chunks[1], t, conn, spinner, viewport, geometry, theme,
    );
    render_composer(frame, chunks[2], t, &layout, theme);

    // The Composer overlay (ADR-0032/0033) floats just above the status bar +
    // Composer - an inline popup, a Composer state, not a modal. Drawn after
    // the Composer so it sits on top; skipped entirely when none is open.
    if let Some(overlay) = composer_view.overlay {
        render_composer_popup(frame, chunks[1].y, area, &overlay, theme);
    }

    if let Some(pending) = &t.pending_approval {
        render_approval_modal(frame, area, &pending.command, theme);
    }
    geometry
}

/// The inline Composer overlay popup (ADR-0032/0033): a compact bordered list
/// anchored just above `anchor_y` (the status bar's row), listing the current
/// [`OverlayView`]'s rows with the highlighted one reversed and any hint
/// dimmed. The `Selector`'s `Loading`/`Failed` states draw a single status
/// line instead of rows. Inline and height-bounded - never the full screen:
/// the overlay is a Composer state, not a modal.
fn render_composer_popup(
    frame: &mut Frame,
    anchor_y: u16,
    area: Rect,
    view: &OverlayView,
    theme: &Theme,
) {
    // The lines the popup body holds, plus the title.
    let (title, lines): (String, Vec<Line>) = match view {
        OverlayView::Menu { rows, highlight } => {
            ("commands".into(), popup_rows(rows, *highlight, theme))
        }
        OverlayView::Selector {
            command,
            status,
            rows,
            highlight,
        } => {
            // The popup titles itself after the command's own values -
            // "models" for /model, "themes" for /theme - from the registry
            // descriptor's `list_title` (grammar belongs to the registry, not
            // the painter); an unregistered name falls back to the raw name.
            let title = slash::lookup(command)
                .map(|c| c.list_title.to_string())
                .unwrap_or_else(|| command.clone());
            match status {
                OverlayStatus::Loading => {
                    let line = Line::styled(
                        format!("loading {title}…"),
                        Style::default()
                            .fg(tui_color(theme.muted))
                            .add_modifier(Modifier::ITALIC),
                    );
                    (title, vec![line])
                }
                OverlayStatus::Failed(msg) => (
                    title,
                    vec![Line::styled(
                        format!("failed: {msg}"),
                        Style::default()
                            .fg(tui_color(theme.error))
                            .add_modifier(Modifier::BOLD),
                    )],
                ),
                OverlayStatus::Ready => (title, popup_rows(rows, *highlight, theme)),
            }
        }
    };

    // Body rows + top/bottom border, capped so a long list never eats the
    // screen; width caps to the terminal.
    let body_rows = lines.len().max(1) as u16;
    let height = (body_rows + 2).min(POPUP_MAX_ROWS + 2).min(area.height);
    let width = area.width.saturating_sub(2).max(1);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = anchor_y.saturating_sub(height);
    let popup = Rect {
        x,
        y,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tui_color(theme.popup_border)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Scroll the highlighted row into view when the list overflows the box.
    let visible = inner.height as usize;
    let highlight = match view {
        OverlayView::Menu { highlight, .. } => *highlight,
        OverlayView::Selector { highlight, .. } => *highlight,
    };
    let top = composer::first_visible_row(highlight, visible.max(1));
    let shown: Vec<Line> = lines.into_iter().skip(top).take(visible).collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

/// The most body rows the Slash popup shows before it scrolls internally - keeps
/// the overlay compact even against a long model list.
const POPUP_MAX_ROWS: u16 = 8;

/// One `Line` per [`SelectorRow`]: the label, then the hint dimmed; the
/// highlighted row is reversed so it reads as the selection. A non-selectable
/// row (a Provider group header, an "unavailable" note) draws dim bold and is
/// never reversed - the Selector's cursor cannot land on it.
fn popup_rows(rows: &[SelectorRow], highlight: usize, theme: &Theme) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return vec![Line::styled(
            "no matches",
            Style::default()
                .fg(tui_color(theme.muted))
                .add_modifier(Modifier::ITALIC),
        )];
    }
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let label_style = if row.selectable {
                Style::default()
            } else {
                Style::default()
                    .fg(tui_color(theme.muted))
                    .add_modifier(Modifier::BOLD)
            };
            let mut spans = vec![Span::styled(row.label.clone(), label_style)];
            if let Some(hint) = &row.hint {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    hint.clone(),
                    Style::default()
                        .fg(tui_color(theme.muted))
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            let line = Line::from(spans);
            if i == highlight && row.selectable {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect()
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
    t: &Screen,
    viewport: &Viewport,
    cache: &mut RenderCache,
    theme: &Theme,
) -> (usize, usize) {
    // The rightmost column is ALWAYS the scrollbar gutter, occupied or not:
    // reserving it only when the scrollbar shows would make the wrap width
    // depend on the line count and the line count on the wrap width.
    let text_area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    cache.sync(
        t.transcript(),
        Toggles {
            thinking_expanded: t.thinking_expanded,
            tools_expanded: t.tools_expanded,
        },
        text_area.width,
        theme,
    );

    // The live streaming snapshot renders below the settled items: the
    // one-line thinking indicator (rebuilt each frame - one Line is cheap)
    // and the streaming markdown (cached - see [`RenderCache::sync`]).
    let thinking = t.transcript().streaming_thinking();
    let thinking_lines: Vec<Line<'static>> = if thinking.is_empty() {
        vec![]
    } else {
        vec![Line::styled(
            format!(
                "🧠 thinking… (~{} tokens)",
                crate::conversation::tokens_for_chars(thinking.chars().count() as u64)
            ),
            Style::default()
                .fg(tui_color(theme.thinking))
                .add_modifier(Modifier::ITALIC),
        )]
    };

    // One (lines, wrapped-count) entry per window "item": every settled
    // message, then the streaming tail - a single indexing shared by the
    // window selection and the slice assembly below.
    let mut item_lines: Vec<&[Line<'static>]> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    for (lines, wrapped) in cache.settled() {
        item_lines.push(lines);
        counts.push(wrapped);
    }
    if !thinking_lines.is_empty() {
        counts.push(wrapped_count(thinking_lines.clone(), text_area.width));
        item_lines.push(&thinking_lines);
    }
    if let Some((lines, wrapped)) = cache.streaming_tail() {
        counts.push(wrapped);
        item_lines.push(lines);
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
// content under an unchanged `Transcript::revision` (the store's contract:
// appends never bump, structural edits always do), so their lines and wrapped
// counts are built once and reused; the frame then renders only the items
// intersecting the window.
// ---------------------------------------------------------------------------

/// The two detail-on-demand display toggles the settled lines are built with:
/// Ctrl-T (Thinking) and Ctrl-O (tool Blocks). Carried as named fields - never
/// a position-coupled pair of bools, the same rule as [`ConnectionFacts`] -
/// because two adjacent `bool` parameters swap without a type error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Toggles {
    thinking_expanded: bool,
    tools_expanded: bool,
}

pub use render_cache::RenderCache;

/// The cache is a private child module (the same move ADR-0034 made for the
/// store's streaming snapshot): still in `ui/components`, still ratatui
/// [`Line`]s (ADR-0029 rejects a frame-free extraction). The boundary exists
/// so the fields are genuinely private - the frame path reads through the two
/// accessors - and the extend-vs-rebuild invariant is pinned by unit tests at
/// this seam, next to the state they inspect, not through full-screen renders.
mod render_cache {
    use ratatui::text::Line;

    use super::{Toggles, markdown_lines, message_lines, wrapped_count};
    use crate::ui::theme::{self, Theme};
    use crate::ui::transcript::Transcript;

    /// Per-item render state for the transcript viewport, owned by the
    /// adapter's run loop and threaded through [`super::render`]. Holds
    /// ratatui [`Line`]s, so it lives HERE, not in the pure modules
    /// (ADR-0019).
    pub struct RenderCache {
        /// The text width everything below was built/measured at.
        width: u16,
        /// The [`Toggles`] the settled lines were built with (either flip
        /// changes every affected item's lines, so it clears the cache
        /// wholesale).
        toggles: Toggles,
        /// The [`Theme`] every cached line was colored with. Cached lines
        /// BAKE their colors (styled spans, syntect-highlighted code), so a
        /// theme swap (Stage C's live preview) stales them all: any
        /// difference clears the cache wholesale, exactly like a resize.
        theme: Theme,
        /// The store's [`Transcript::revision`] the entries were built at:
        /// while it holds still, the settled items only extend (the store's
        /// prefix contract) and the cache extends with them; when it moves (a
        /// structural edit), the cache rebuilds from scratch.
        revision: u64,
        /// One entry per settled [`Transcript::items`] item, same order.
        items: Vec<CachedItem>,
        /// The in-flight streaming markdown, keyed on its char length: within
        /// one message the snapshot only grows, so the length is a cheap
        /// monotonic key that changes exactly when the text does. Cleared
        /// between messages (empty streaming text) so a new message can never
        /// collide with a stale entry of the same length.
        streaming: Option<CachedStreaming>,
    }

    /// One settled item's built lines and its wrapped row count at the
    /// cache's width - the numbers [`super::visible_window`] does its
    /// prefix-sum math over.
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
                toggles: Toggles::default(),
                theme: theme::dark().clone(),
                revision: 0,
                items: Vec::new(),
                streaming: None,
            }
        }

        /// The settled entries in [`Transcript::items`] order: each item's
        /// built lines with its wrapped row count at the cache's width.
        pub(super) fn settled(&self) -> impl Iterator<Item = (&[Line<'static>], usize)> {
            self.items
                .iter()
                .map(|item| (item.lines.as_slice(), item.wrapped))
        }

        /// The streaming-markdown tail, if a snapshot is in flight: its lines
        /// with their wrapped row count. Always after every settled entry.
        pub(super) fn streaming_tail(&self) -> Option<(&[Line<'static>], usize)> {
            self.streaming
                .as_ref()
                .map(|s| (s.lines.as_slice(), s.wrapped))
        }

        /// Brings the cache up to date with the Transcript at `width`: clears
        /// wholesale when [`Self::needs_rebuild`] says a key input changed,
        /// then builds entries for the newly appended items only - the
        /// steady-state cost of a frame is zero rebuilt items.
        pub(super) fn sync(&mut self, t: &Transcript, toggles: Toggles, width: u16, theme: &Theme) {
            if self.needs_rebuild(t, toggles, width, theme) {
                self.items.clear();
                self.streaming = None;
                self.width = width;
                self.toggles = toggles;
                self.theme = theme.clone();
                self.revision = t.revision();
            }
            for item in &t.items()[self.items.len()..] {
                let mut lines = message_lines(
                    item,
                    toggles.thinking_expanded,
                    toggles.tools_expanded,
                    theme,
                );
                // One trailing blank row per settled item so turns read as
                // distinct paragraphs rather than one wall. Building it into
                // the cached lines keeps measurement (`wrapped`) and rendering
                // exactly consistent - the viewport window math depends on
                // that agreement.
                lines.push(Line::default());
                let wrapped = wrapped_count(lines.clone(), width);
                self.items.push(CachedItem { lines, wrapped });
            }
            self.sync_streaming(&t.streaming_text(), width, theme);
        }

        /// Whether [`Self::sync`] must clear wholesale instead of extending.
        /// The extend-only fast path is safe because the store guarantees the
        /// settled items are a strict PREFIX of the last read while the
        /// revision holds still (appends never bump, structural edits always
        /// do - see `ui/transcript`); a width or [`Toggles`] change restyles
        /// every settled line, so either clears too. The length check is
        /// cheap defense in kind: a store shorter than the cache (a swapped
        /// Transcript whose revision happens to coincide) cannot extend it.
        fn needs_rebuild(
            &self,
            t: &Transcript,
            toggles: Toggles,
            width: u16,
            theme: &Theme,
        ) -> bool {
            self.width != width
                || self.toggles != toggles
                || self.theme != *theme
                || self.revision != t.revision()
                || self.items.len() > t.items().len()
        }

        /// Re-parses the streaming markdown only when its char length moved
        /// (monotonic within a message - see the field doc); drops the entry
        /// when streaming ended so the next message starts from nothing.
        fn sync_streaming(&mut self, text: &str, width: u16, theme: &Theme) {
            if text.is_empty() {
                self.streaming = None;
                return;
            }
            let char_len = text.chars().count();
            if self
                .streaming
                .as_ref()
                .is_some_and(|s| s.char_len == char_len)
            {
                return;
            }
            let lines = markdown_lines(text, theme);
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

    // The extend-vs-rebuild invariant, pinned at the cache's own seam. These
    // sync against a bare Transcript store (ADR-0034) seeded through its
    // verbs, and they live INSIDE the module because proving "not rebuilt"
    // takes a sentinel planted in the private entries - identity, not
    // equality. Accessor-expressible cache tests stay in the outer module.
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::content::ContentBlock;

        fn line_text(line: &Line<'static>) -> String {
            line.spans.iter().map(|s| s.content.as_ref()).collect()
        }

        fn fresh_transcript() -> Transcript {
            Transcript::new(Vec::new())
        }

        #[test]
        fn cache_sync_extends_for_appends_without_rebuilding_settled_entries() {
            let mut t = fresh_transcript();
            t.info("first");
            let mut cache = RenderCache::new();
            cache.sync(&t, Toggles::default(), 80, theme::dark());

            // Plant a sentinel in the built entry: an append extends the cache
            // without touching settled entries, so the sentinel must survive
            // the next sync - a rebuild would have replaced it with "first".
            cache.items[0].lines[0] = Line::raw("sentinel");
            t.info("appended");
            cache.sync(&t, Toggles::default(), 80, theme::dark());
            assert_eq!(cache.items.len(), 2);
            assert_eq!(line_text(&cache.items[0].lines[0]), "sentinel");
            assert_eq!(line_text(&cache.items[1].lines[0]), "appended");
        }

        #[test]
        fn cache_sync_rebuilds_when_the_revision_moves() {
            let mut t = fresh_transcript();
            t.steering_queued("check");
            let mut cache = RenderCache::new();
            cache.sync(&t, Toggles::default(), 80, theme::dark());
            cache.items[0].lines[0] = Line::raw("sentinel");

            // The delivered steering removes its pending marker - a structural
            // edit that bumps the store's revision - so the cache rebuilds
            // from scratch: the sentinel is gone and the promoted user line is
            // seen.
            t.steering_delivered("check");
            cache.sync(&t, Toggles::default(), 80, theme::dark());
            assert_eq!(cache.items.len(), 1);
            assert_eq!(line_text(&cache.items[0].lines[0]), "› check");
        }

        #[test]
        fn cache_sync_rebuilds_when_the_store_shrinks_below_the_cached_length() {
            // No store verb shrinks without bumping (the prefix contract), so
            // the only way here is a SWAPPED Transcript whose revision happens
            // to coincide - two fresh stores both at revision 0. The length
            // check catches it: the sentinel is gone, wholesale.
            let mut t = fresh_transcript();
            t.info("first");
            t.info("second");
            let mut cache = RenderCache::new();
            cache.sync(&t, Toggles::default(), 80, theme::dark());
            cache.items[0].lines[0] = Line::raw("sentinel");

            let mut shorter = fresh_transcript();
            shorter.info("replacement");
            assert_eq!(t.revision(), shorter.revision());
            cache.sync(&shorter, Toggles::default(), 80, theme::dark());
            assert_eq!(cache.items.len(), 1);
            assert_eq!(line_text(&cache.items[0].lines[0]), "replacement");
        }

        #[test]
        fn the_streaming_tail_is_never_cached_as_a_settled_entry() {
            let mut t = fresh_transcript();
            t.info("settled");
            t.message_start();
            t.message_update(vec![ContentBlock::text("in flight")]);
            let mut cache = RenderCache::new();
            cache.sync(&t, Toggles::default(), 80, theme::dark());
            // The in-flight snapshot lives ONLY in the streaming slot; the
            // settled entries still mirror `Transcript::items` exactly.
            assert_eq!(cache.items.len(), t.items().len());
            assert_eq!(cache.items.len(), 1);
            assert!(cache.streaming.is_some());

            // Settling the message appends without bumping the revision, so
            // the tail arrives as an EXTEND (the sentinel survives) and the
            // streaming slot empties for the next message.
            cache.items[0].lines[0] = Line::raw("sentinel");
            t.message_end(&[ContentBlock::text("in flight")]);
            cache.sync(&t, Toggles::default(), 80, theme::dark());
            assert_eq!(cache.items.len(), 2);
            assert_eq!(line_text(&cache.items[0].lines[0]), "sentinel");
            assert!(cache.streaming.is_none());
        }

        #[test]
        fn streaming_cache_reparses_only_when_the_char_length_moves() {
            let mut cache = RenderCache::new();
            cache.sync_streaming("hello", 80, theme::dark());
            assert_eq!(
                line_text(&cache.streaming.as_ref().unwrap().lines[0]),
                "hello"
            );

            // Same length, different text: the monotonic-key contract - within
            // a message the snapshot only GROWS, so an equal length means
            // unchanged and the cached lines are reused as-is.
            cache.sync_streaming("world", 80, theme::dark());
            assert_eq!(
                line_text(&cache.streaming.as_ref().unwrap().lines[0]),
                "hello"
            );

            // Growth re-parses; the end of streaming clears, so the next
            // message can never collide with a stale entry of the same length.
            cache.sync_streaming("hello more", 80, theme::dark());
            assert_eq!(
                line_text(&cache.streaming.as_ref().unwrap().lines[0]),
                "hello more"
            );
            cache.sync_streaming("", 80, theme::dark());
            assert!(cache.streaming.is_none());
        }
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

/// The backgrounded "machinery" style for tool-call lines: dim DarkGray, NOT
/// italic (italic stays reserved for Thinking/Info so those remain
/// distinguishable). Paired with a two-space indent + "⋯" gutter, it makes
/// tool machinery recede so the conversation owns the foreground.
fn machinery_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.machinery))
}

/// The lines one Transcript item renders as. `Block` is the semantic display
/// vocabulary (ADR-0008): a titled block whose lines take their color from
/// [`line_style`]. `thinking_expanded` (Ctrl-T, the core's
/// `Transcript::thinking_expanded`) picks the collapsed one-liner or the full
/// text for settled `Thinking` items; `tools_expanded` (Ctrl-O, the core's
/// `Transcript::tools_expanded`) does the same for multi-line `Block` bodies -
/// the same detail-on-demand rule applied to the machinery plane.
fn message_lines(
    item: &TranscriptItem,
    thinking_expanded: bool,
    tools_expanded: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // Detail-on-demand collapse (Ctrl-O), keyed on the SEMANTIC fold predicate
    // (Stage 2 review C2 / S1): any item with a `foldable_body` collapses to its
    // `fold_title` one-liner, so the fold rule is NOT gated inside a per-variant
    // match arm - a future non-Block foldable item folds the same way. The
    // affordance is a fixed `· ^O expand`, NOT a line count: a Block's title
    // already carries its `(+A −R)` magnitude, and the body is display-capped
    // upstream, so a raw `lines.len()` would misreport what was elided.
    if !tools_expanded
        && item.foldable_body().is_some()
        && let Some(title) = item.fold_title()
    {
        return vec![Line::styled(
            format!("  ⋯ {title} · ^O expand"),
            machinery_style(theme),
        )];
    }

    match item {
        // User prompts: the "› " gutter on the first row, continuation rows
        // aligned under it. Multi-line input renders as multiple rows.
        TranscriptItem::User { text } => {
            let gutter = Style::default()
                .fg(tui_color(theme.prompt_gutter))
                .add_modifier(Modifier::BOLD);
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
        TranscriptItem::Assistant { text } => markdown_lines(text, theme),
        // Settled Thinking: collapsed is the one-line form; expanded (Ctrl-T)
        // is a header row then the full text, all in the same dim italic. The
        // in-flight "🧠 thinking… (N chars)" streaming indicator is rendered by
        // the viewport and is unaffected by the toggle.
        TranscriptItem::Thinking { text } => {
            let style = Style::default()
                .fg(tui_color(theme.thinking))
                .add_modifier(Modifier::ITALIC);
            if thinking_expanded {
                let mut out = vec![Line::styled("🧠 thought:", style)];
                out.extend(
                    text_rows(text)
                        .into_iter()
                        .map(|row| Line::styled(row, style)),
                );
                out
            } else {
                vec![Line::styled(
                    format!("🧠 thought: {}", first_line(text)),
                    style,
                )]
            }
        }
        // Tool-call machinery recedes into a dim, indented background gutter so
        // the conversation (assistant prose, user text) owns the foreground:
        // DarkGray (not italic - italic stays reserved for Thinking/Info), a
        // two-space indent, and a quiet "⋯" glyph in place of the loud "⚙".
        TranscriptItem::ToolCall { name, summary, .. } => vec![Line::styled(
            format!("  ⋯ {}", join_summary(name, summary)),
            machinery_style(theme),
        )],
        // A merged one-liner (Stage 3): a paired call+result reads
        // `⋯ name  <key_arg> · <result>`; an unpaired result (no live call, so
        // no arg) keeps the older `⋯ name → result` shape.
        TranscriptItem::ToolResult {
            name,
            summary,
            is_error: false,
            key_arg,
        } => vec![Line::styled(
            format!("  ⋯ {}", join_merged(name, key_arg.as_deref(), summary)),
            machinery_style(theme),
        )],
        // Errors are the exception that belongs in the foreground: they keep
        // red + bold and the ⚙ gutter, share the two-space indent, and ALWAYS
        // carry a `✗` failed-marker so they can't be missed (the two-planes
        // design leans on this - red+bold alone is weaker for scanning and
        // colorblind users). The merged `key_arg` is kept so the failing
        // path/command stays visible. The one exception: when the `summary`
        // already begins with a status glyph - a plugin badge like `✗ exit 1`
        // (or `✓`) - the line injects none of its own, so a badge never doubles
        // up its glyph.
        TranscriptItem::ToolResult {
            name,
            summary,
            is_error: true,
            key_arg,
        } => {
            let glyph = if starts_with_status_glyph(summary) {
                ""
            } else {
                "✗ "
            };
            vec![Line::styled(
                format!(
                    "  ⚙ {} {glyph}{summary}",
                    join_arg(name, key_arg.as_deref())
                ),
                Style::default()
                    .fg(tui_color(theme.error))
                    .add_modifier(Modifier::BOLD),
            )]
        }
        // A foldable Block reaches here only EXPANDED (Ctrl-O on) or when it has
        // no foldable body (titleless / empty) - the collapse is handled once at
        // the top of this fn. Expanded: the title line then the body rows, which
        // keep their semantic diff colors (added/removed/context) indented under
        // the gutter.
        TranscriptItem::Block { title, lines } => {
            let mut out = vec![Line::styled(format!("  ⋯ {title}"), machinery_style(theme))];
            // Body rows keep their semantic diff colors (added/removed/context)
            // but sit indented under the gutter.
            out.extend(lines.iter().map(|line| {
                let styled = block_line(line, theme);
                let mut spans = vec![Span::raw("  ")];
                spans.extend(styled.spans);
                Line::from(spans)
            }));
            out
        }
        TranscriptItem::Info { text } => {
            let style = Style::default()
                .fg(tui_color(theme.muted))
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

/// The bundled syntax definitions, lazy: headless runs that never render pay
/// nothing for the load. The syntect themes are NOT here - the theme module
/// owns that set ([`theme::syntax_theme_set`]), so the names its validation
/// accepts and the themes this highlighter draws from are one loaded copy.
static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// One highlighted fragment: the `(r, g, b)` foreground and the text it colors.
type CodeFragment = ((u8, u8, u8), String);

/// Highlights one code block with the named bundled syntect theme (the active
/// Theme's `syntax` slot): per input line, the [`CodeFragment`]s syntect
/// colors it with - pure data in/out, no ratatui types. `None` when `lang`
/// resolves to no bundled syntax (caller falls back to the plain
/// [`MdStyle::CodeBlock`] rendering). Parse state carries across the lines, so
/// multi-line constructs (block comments, raw strings) color correctly.
/// An unknown `syntax` name falls back to `base16-ocean.dark` - theme parsing
/// validates names (ADR-0038), so this is belt-and-suspenders, not a path.
fn highlight_code(
    lines: &[&str],
    lang: &str,
    syntax_theme: &str,
) -> Option<Vec<Vec<CodeFragment>>> {
    let syntaxes = syntaxes();
    // `find_syntax_by_token` matches the syntax name ("rust", "python") AND
    // file extensions ("rs", "py"), case-insensitively - the widest net for
    // fence tags.
    let syntax = syntaxes.find_syntax_by_token(lang)?;
    let themes = &theme::syntax_theme_set().themes;
    let colors = themes
        .get(syntax_theme)
        .unwrap_or(&themes["base16-ocean.dark"]);
    let mut state = HighlightLines::new(syntax, colors);
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        // The newlines-variant SyntaxSet expects each line `\n`-terminated.
        let with_newline = format!("{line}\n");
        let ranges = state.highlight_line(&with_newline, syntaxes).ok()?;
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
fn markdown_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let md_lines = markdown::to_lines(text);
    let mut out = Vec::with_capacity(md_lines.len());
    let mut i = 0;
    while i < md_lines.len() {
        let lang = match md_lines[i].code_lang.as_deref() {
            Some(lang) if !lang.is_empty() => lang.to_string(),
            _ => {
                out.push(plain_md_line(&md_lines[i], theme));
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
        match highlight_code(&refs, &lang, &theme.syntax) {
            Some(highlighted) => {
                for (fragments, text) in highlighted.into_iter().zip(&texts) {
                    if fragments.is_empty() {
                        // Blank (or all-whitespace) code line: keep the same
                        // bg treatment the plain path gives it.
                        out.push(Line::from(Span::styled(
                            text.clone(),
                            md_style(MdStyle::CodeBlock, theme),
                        )));
                    } else {
                        out.push(Line::from(
                            fragments
                                .into_iter()
                                .map(|((r, g, b), text)| {
                                    Span::styled(
                                        text,
                                        Style::default()
                                            .fg(Color::Rgb(r, g, b))
                                            .bg(tui_color(theme.code_block_bg)),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        ));
                    }
                }
            }
            // Unknown language: the existing plain CodeBlock rendering.
            None => out.extend(block.iter().map(|line| plain_md_line(line, theme))),
        }
        i = end;
    }
    out
}

/// One [`MdLine`] rendered the plain way: each span through the single
/// [`md_style`] mapping.
fn plain_md_line(line: &MdLine, theme: &Theme) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|span| Span::styled(span.text.clone(), md_style(span.style, theme)))
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

fn block_line(line: &StyledLine, theme: &Theme) -> Line<'static> {
    let text = if line.text.is_empty() {
        " ".to_string()
    } else {
        line.text.replace('\t', "  ")
    };
    Line::styled(text, line_style(line.style, theme))
}

/// The running-spinner animation frames (braille), advanced by the adapter's
/// animation tick while a Turn is running.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ---------------------------------------------------------------------------
// The powerline status bar.
// ---------------------------------------------------------------------------

/// Powerline separators (Nerd Font): right-pointing after left-side segments,
/// left-pointing before right-side segments. Drawn fg = the segment's bg over
/// bg = the neighbor's bg - the standard powerline triangle technique.
const SEP_RIGHT: &str = "\u{e0b0}"; //
const SEP_LEFT: &str = "\u{e0b2}"; //

/// The Agent's mode as the status bar conveys it - the semantic distinction
/// the leftmost block draws. Carries no spinner frame: the animation glyph is
/// a drawing concern the painter injects, not part of what the bar *means*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeState {
    /// The Agent is idle - no Turn running.
    Idle,
    /// The Agent is running a Turn.
    Running,
}

/// One status bar segment's MEANING, ratatui-free (ADR-0019). The pure
/// assembly ([`status_bar`]) emits these carrying only the display state they
/// convey - no colors (that is [`segment_style`], ADR-0008), no glyphs, no
/// padding, no label formatting (all [`StatusSegment::paint`]'s job). This is
/// the testable seam: the semantics of the bar can be asserted without drawing
/// a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusSegment {
    /// The Agent's mode. Idle vs. Running is the semantic decision; the
    /// spinner frame the running block animates is supplied at paint time.
    Mode(ModeState),
    /// The brand + endpoint the Session connects to.
    Connection {
        /// The model connection's base URL.
        base_url: String,
    },
    /// The Active Model this Session talks to (ADR-0033). Mutable - a `/model`
    /// pick changes it - so the bar shows the one connection fact this feature
    /// makes variable, beside the fixed endpoint.
    Model {
        /// The Active Model identifier.
        model: String,
    },
    /// The Ctrl-T Thinking-expansion state. Carries the boolean meaning; the
    /// `▾`/`▸` marker is chosen by the painter. Always assembled so the toggle
    /// has feedback even when no Thinking items are on screen.
    Thinking {
        /// Whether settled Thinking items are currently expanded.
        expanded: bool,
    },
    /// The Ctrl-O tool-Block-expansion state. Carries the boolean meaning; the
    /// `▾`/`▸` marker is chosen by the painter. Always assembled - the twin of
    /// `Thinking` - so the toggle has feedback even when no Blocks are on
    /// screen.
    Tools {
        /// Whether settled tool Blocks are currently expanded.
        expanded: bool,
    },
    /// The Session's cumulative dollar cost (ADR-0037: Catalog pricing,
    /// surfaced display-side). Carries the pre-formatted label (the pure
    /// [`cost_label`] rule) so the segment stays `Eq`; assembled only when the
    /// total is positive - an unpriced local Session never shows it.
    Cost {
        /// The [`cost_label`]-formatted total, e.g. `$0.42` or `<$0.01`.
        label: String,
    },
    /// Carries the [`PressureLevel`] verbatim so the Critical-renders-red rule
    /// (ADR-0008) is a semantic fact the painter merely routes to a color.
    Tokens {
        /// The token estimate for the Conversation.
        estimate: u64,
        /// How close to the budget the Conversation sits.
        level: PressureLevel,
        /// The LIVE Dead Mass share as an integer percent (from the most recent
        /// ContextPressure), or `None` before any pressure event. When `Some`,
        /// the segment appends a `· N% dead` tail - pre-rounded upstream (the
        /// single rounding rule) and baked into `cells()` like every other
        /// segment fact, never recomputed in the painter.
        dead_mass_pct: Option<u64>,
    },
    /// The viewport scroll position label (`Bot`/`Top`/`NN%`), already derived
    /// from this frame's geometry by [`scroll_position_label`].
    Position {
        /// The vim-ruler style position label.
        label: String,
    },
}

impl StatusSegment {
    /// The painter's [`SegmentKind`] for this segment - the key into
    /// [`segment_style`] (ADR-0008). Pure classification, no ratatui: it just
    /// carries the [`PressureLevel`] through for the Tokens segment so the
    /// single pressure→color mapping (Critical renders red) still decides the
    /// style, now provably fed the right level.
    fn kind(&self) -> SegmentKind {
        match self {
            StatusSegment::Mode(ModeState::Idle) => SegmentKind::ModeIdle,
            StatusSegment::Mode(ModeState::Running) => SegmentKind::ModeRunning,
            StatusSegment::Connection { .. } => SegmentKind::Connection,
            StatusSegment::Model { .. } => SegmentKind::Model,
            StatusSegment::Thinking { .. } => SegmentKind::Thinking,
            StatusSegment::Tools { .. } => SegmentKind::Tools,
            StatusSegment::Cost { .. } => SegmentKind::Cost,
            StatusSegment::Tokens { level, .. } => SegmentKind::Tokens(*level),
            StatusSegment::Position { .. } => SegmentKind::Position,
        }
    }

    /// The columns this segment occupies once painted, ratatui-free. Kept in
    /// lockstep with [`StatusSegment::paint`] so the pure fit policy
    /// ([`StatusBar::fit`]) measures exactly what the painter will draw. The
    /// spinner glyph and `▾`/`▸` marker are each one column, so the width does
    /// not depend on the frame the painter later chooses. Exhaustive so a new
    /// segment kind is a compile error here as well as in the painter.
    fn cells(&self) -> usize {
        match self {
            // " X RUNNING " / " IDLE " - the running spinner glyph is one col.
            StatusSegment::Mode(ModeState::Running) => " X RUNNING ".chars().count(),
            StatusSegment::Mode(ModeState::Idle) => " IDLE ".chars().count(),
            StatusSegment::Connection { base_url } => {
                format!(" suspenders · {base_url} ").chars().count()
            }
            StatusSegment::Model { model } => format!(" model · {model} ").chars().count(),
            // " M thinking " - the marker is one col in either state.
            StatusSegment::Thinking { .. } => " M thinking ".chars().count(),
            // " M tools " - the marker is one col in either state.
            StatusSegment::Tools { .. } => " M tools ".chars().count(),
            StatusSegment::Cost { label } => format!(" {label} ").chars().count(),
            StatusSegment::Tokens {
                estimate,
                dead_mass_pct,
                ..
            } => tokens_label(*estimate, *dead_mass_pct).chars().count(),
            StatusSegment::Position { label } => format!(" {label} ").chars().count(),
        }
    }
}

/// The Tokens segment's display text, the ONE source both [`StatusSegment::cells`]
/// and [`StatusSegment::paint`] draw from so their widths can never drift (the
/// load-bearing fit invariant). `~{estimate} tokens` always (grouped with
/// thousands separators); a `· {N}% dead` tail whenever a live Dead Mass share
/// is known (the percent is
/// pre-rounded upstream through the single rounding rule, so no rounding happens
/// here). The tail shows even at `Some(0)` - a live zero is the meaningful "no
/// dead mass" fact, not an absence.
fn tokens_label(estimate: u64, dead_mass_pct: Option<u64>) -> String {
    let estimate = estimate.separate_with_commas();
    match dead_mass_pct {
        Some(pct) => format!(" ~{estimate} tokens · {pct}% dead "),
        None => format!(" ~{estimate} tokens "),
    }
}

/// The Cost segment's display text (ADR-0037: the Session's cumulative
/// Catalog-priced total, in dollars). Two decimals from a cent up; a flat
/// `<$0.01` below that - a sub-cent figure would render `$0.00` and read as
/// free. Only prices a positive total: the assembly hides the segment
/// entirely at zero, so this never formats one.
pub fn cost_label(total: f64) -> String {
    if total < 0.01 {
        "<$0.01".to_string()
    } else {
        format!("${total:.2}")
    }
}

/// The status bar's assembled MEANING: an ordered left group (mode, then
/// connection) and right group (thinking, tools, tokens, position), already
/// fitted to the terminal width. Pure and ratatui-free - this is what the new
/// colocated tests assert against without drawing a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBar {
    /// Left-anchored segments, highest priority first.
    pub left: Vec<StatusSegment>,
    /// Right-anchored segments, in display order.
    pub right: Vec<StatusSegment>,
}

impl StatusBar {
    /// Drops segments until the bar fits `width`, lowest-value first:
    /// connection, then model, then tools, then thinking, then cost, then
    /// tokens - mode and position survive longest. Connection (the endpoint)
    /// drops BEFORE model: the endpoint is a fixed, knowable fact, while the
    /// model is what the user actively changes via `/model`, so the model
    /// earns the scarcer columns. Tools drops before thinking (both are the
    /// same detail-on-demand class; thinking is the older, more-referenced
    /// affordance). Cost drops before tokens: tokens carry the pressure level
    /// the operator steers by. Which segments to show at a given width is a
    /// SEMANTIC decision, so it lives here in the pure layer; the width
    /// arithmetic reads each segment's own [`StatusSegment::cells`]. Simple on
    /// purpose: a partially-truncated segment would garble the powerline
    /// blocks.
    fn fit(mut self, width: usize) -> StatusBar {
        let drop_order: [fn(&StatusSegment) -> bool; 6] = [
            |s| matches!(s, StatusSegment::Connection { .. }),
            |s| matches!(s, StatusSegment::Model { .. }),
            |s| matches!(s, StatusSegment::Tools { .. }),
            |s| matches!(s, StatusSegment::Thinking { .. }),
            |s| matches!(s, StatusSegment::Cost { .. }),
            |s| matches!(s, StatusSegment::Tokens { .. }),
        ];
        for dropped in drop_order {
            if self.cells() <= width {
                break;
            }
            self.left.retain(|s| !dropped(s));
            self.right.retain(|s| !dropped(s));
        }
        self
    }

    /// The columns the segments occupy: their painted widths plus one
    /// powerline separator glyph per segment (left segments each trail one,
    /// right segments each lead with one).
    fn cells(&self) -> usize {
        let text: usize = self
            .left
            .iter()
            .chain(&self.right)
            .map(StatusSegment::cells)
            .sum();
        text + self.left.len() + self.right.len()
    }
}

/// The token facts the status bar's Tokens segment needs: the `estimate` it
/// draws, the [`PressureLevel`] that colors it, and the live
/// `dead_mass_pct` (an integer percent, pre-rounded through the single rounding
/// rule) from the most recent ContextPressure (`None` before any pressure
/// event). A named struct rather than a tuple so the extra Dead Mass fact
/// rides in cleanly and the `status_bar` arg COUNT stays at 8 (no 9th arg - the
/// Stage 3 review's binding precondition against growing the already-suppressed
/// signature).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenView {
    pub estimate: u64,
    pub level: PressureLevel,
    pub dead_mass_pct: Option<u64>,
}

/// The figures the bar's right side draws beside the toggles: the token facts
/// (`None` before any estimate exists) and the Session's cumulative dollar
/// cost (ADR-0037). A `session_cost` of 0.0 hides the cost segment entirely -
/// an unpriced local Session shows exactly the bar it always did. One struct,
/// like [`TokenView`] before it, so the `status_bar` arg COUNT stays at 8 (the
/// Stage 3 review's binding precondition against growing the
/// already-suppressed signature).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FigureView {
    pub tokens: Option<TokenView>,
    pub session_cost: f64,
}

/// Assembles the status bar's MEANING, pure and ratatui-free (ADR-0019): the
/// ordered semantic segments the bar conveys, fitted to `width`. `figures`
/// carries the token facts (`None` when no estimate exists yet) and the
/// Session cost (segment hidden at zero). No colors, glyphs, or label
/// strings are decided here - that is the painter's job
/// ([`render_status_bar`]) - so every rule this expresses (segment order, the
/// fit/drop policy, which [`PressureLevel`] the tokens segment carries, the
/// tokens-absent-until-estimate and cost-hidden-at-zero rules) is a semantic
/// fact assertable without a frame.
// Each parameter is an independent display FACT the bar renders (status,
// endpoint, model, the two detail-on-demand toggles, figures, position); keeping
// them primitive is exactly what makes the assembly assertable without a
// Transcript or a frame, so we take the extra argument rather than bundle them
// into a struct that would only re-hide those facts behind one opaque type.
#[allow(clippy::too_many_arguments)]
pub fn status_bar(
    width: usize,
    status: Status,
    base_url: &str,
    model: &str,
    thinking_expanded: bool,
    tools_expanded: bool,
    figures: FigureView,
    position: String,
) -> StatusBar {
    let mode = match status {
        Status::Idle => ModeState::Idle,
        Status::Running => ModeState::Running,
    };
    let left = vec![
        StatusSegment::Mode(mode),
        StatusSegment::Connection {
            base_url: base_url.to_string(),
        },
        StatusSegment::Model {
            model: model.to_string(),
        },
    ];

    let mut right = vec![
        StatusSegment::Thinking {
            expanded: thinking_expanded,
        },
        StatusSegment::Tools {
            expanded: tools_expanded,
        },
    ];
    if let Some(TokenView {
        estimate,
        level,
        dead_mass_pct,
    }) = figures.tokens
    {
        right.push(StatusSegment::Tokens {
            estimate,
            level,
            dead_mass_pct,
        });
    }
    // The cost segment exists only once a priced Response landed: at zero the
    // Session has spent nothing meterable and the bar stays as it always was.
    if figures.session_cost > 0.0 {
        right.push(StatusSegment::Cost {
            label: cost_label(figures.session_cost),
        });
    }
    right.push(StatusSegment::Position { label: position });

    StatusBar { left, right }.fit(width)
}

/// The semantic kind of one status bar segment. The painter classifies each
/// [`StatusSegment`] into a kind ([`StatusSegment::kind`]); [`segment_style`]
/// is the single place kinds become colors (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Agent idle - calm green mode block.
    ModeIdle,
    /// Agent running - yellow mode block with the animated spinner.
    ModeRunning,
    /// `suspenders · <base_url>` - the brand + endpoint, lowest priority.
    Connection,
    /// `model · <id>` - the Active Model (ADR-0033), styled like the endpoint
    /// since both are connection facts.
    Model,
    /// The Ctrl-T thinking-expansion state (`▾`/`▸`). Always visible so the
    /// toggle has feedback even when no Thinking items are on screen.
    Thinking,
    /// The Ctrl-O tool-Block-expansion state (`▾`/`▸`). Always visible so the
    /// toggle has feedback even when no Blocks are on screen - the twin of
    /// `Thinking`.
    Tools,
    /// The Session's cumulative dollar cost (ADR-0037) - a quiet figure like
    /// tokens at `Ok` pressure. Present only once a priced Response landed.
    Cost,
    /// The `~N tokens` estimate, colored by its [`PressureLevel`].
    Tokens(PressureLevel),
    /// The viewport scroll position (`Bot`/`Top`/`NN%`) - the bold accent.
    Position,
}

impl StatusSegment {
    /// Paints this segment into its display text (padding included). The ONLY
    /// place the drawing details live: the spinner glyph (chosen from the
    /// adapter's animation `spinner` tick), the `▾`/`▸` Thinking marker, the
    /// `~N tokens` label, and the block padding. Semantics-in,
    /// terminal-text-out - the seam ADR-0019 wants.
    fn paint(&self, spinner: u64) -> String {
        match self {
            // While running, the animated braille spinner lives inside the
            // mode block; the frame counter comes from the adapter's tick.
            StatusSegment::Mode(ModeState::Running) => {
                format!(" {} RUNNING ", SPINNER[(spinner as usize) % SPINNER.len()])
            }
            StatusSegment::Mode(ModeState::Idle) => " IDLE ".to_string(),
            StatusSegment::Connection { base_url } => format!(" suspenders · {base_url} "),
            StatusSegment::Model { model } => format!(" model · {model} "),
            StatusSegment::Thinking { expanded } => {
                let marker = if *expanded { "▾" } else { "▸" };
                format!(" {marker} thinking ")
            }
            StatusSegment::Tools { expanded } => {
                let marker = if *expanded { "▾" } else { "▸" };
                format!(" {marker} tools ")
            }
            StatusSegment::Cost { label } => format!(" {label} "),
            StatusSegment::Tokens {
                estimate,
                dead_mass_pct,
                ..
            } => tokens_label(*estimate, *dead_mass_pct),
            StatusSegment::Position { label } => format!(" {label} "),
        }
    }
}

/// The bottom status bar, powerline style: left segments (mode, connection)
/// fading into the base bg, right segments (thinking, tokens, position)
/// growing out of it, each block joined by triangle separators. `geometry` is
/// the `(total_lines, height)` the viewport was measured at THIS frame - the
/// position segment must agree with what is actually drawn above it.
///
/// A thin painter over the pure [`status_bar`] assembly: the semantics (which
/// segments, in what order, at what [`PressureLevel`]) are decided there; this
/// turns each [`StatusSegment`] into a styled span via [`StatusSegment::paint`]
/// and [`segment_style`].
#[allow(clippy::too_many_arguments)] // one painter input per bar fact, like status_bar above
pub fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    t: &Screen,
    conn: ConnectionView,
    spinner: u64,
    viewport: &Viewport,
    geometry: (usize, usize),
    theme: &Theme,
) {
    let (total_lines, height) = geometry;
    let position = scroll_position_label(
        viewport.top_offset(total_lines, height),
        total_lines,
        height,
    );
    let bar = status_bar(
        area.width as usize,
        t.status,
        conn.base_url,
        conn.model,
        t.thinking_expanded,
        t.tools_expanded,
        FigureView {
            tokens: t.token_estimate.map(|estimate| TokenView {
                estimate,
                level: t.pressure_level,
                dead_mass_pct: t.dead_mass_pct,
            }),
            session_cost: t.session_cost,
        },
        position,
    );

    let bar_bg = tui_color(theme.bar_bg);
    let mut spans: Vec<Span> = Vec::new();
    for (i, segment) in bar.left.iter().enumerate() {
        let kind = segment.kind();
        spans.push(Span::styled(
            segment.paint(spinner),
            segment_style(kind, theme),
        ));
        // The separator wears THIS segment's bg over the NEXT one's (the base
        // bg after the last segment) - that is what draws the triangle.
        let next_bg = bar
            .left
            .get(i + 1)
            .map(|s| segment_bg(s.kind(), theme))
            .unwrap_or(bar_bg);
        spans.push(Span::styled(
            SEP_RIGHT,
            Style::default().fg(segment_bg(kind, theme)).bg(next_bg),
        ));
    }
    let gap = (area.width as usize).saturating_sub(bar.cells());
    spans.push(Span::styled(" ".repeat(gap), Style::default().bg(bar_bg)));
    let mut prev_bg = bar_bg;
    for segment in &bar.right {
        let kind = segment.kind();
        spans.push(Span::styled(
            SEP_LEFT,
            Style::default().fg(segment_bg(kind, theme)).bg(prev_bg),
        ));
        spans.push(Span::styled(
            segment.paint(spinner),
            segment_style(kind, theme),
        ));
        prev_bg = segment_bg(kind, theme);
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(bar_bg));
    frame.render_widget(bar, area);
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
pub fn render_composer(
    frame: &mut Frame,
    area: Rect,
    t: &Screen,
    layout: &ComposerLayout,
    theme: &Theme,
) {
    let visible = area.height as usize;
    if visible == 0 || area.width < 2 {
        return;
    }
    let top = composer::first_visible_row(layout.cursor_row, visible);
    let gutter = Style::default()
        .fg(tui_color(theme.prompt_gutter))
        .add_modifier(Modifier::BOLD);
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
/// `a` approves-always. Key handling lives in the Screen core; this draws it.
/// The accents ride existing slots: the command reads as code, and the
/// yes/no pair takes the added/removed polarity (approve adds the run,
/// deny removes it); always is the link-blue accent.
pub fn render_approval_modal(frame: &mut Frame, area: Rect, command: &str, theme: &Theme) {
    let width = (command.chars().count() as u16 + 8)
        .max(44)
        .min(area.width.saturating_sub(4));
    let height = 8u16.min(area.height.saturating_sub(2));
    let modal = centered_rect(width, height, area);

    frame.render_widget(Clear, modal);
    let block = Block::default().title("Approval").borders(Borders::ALL);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let body = Paragraph::new(vec![
        Line::styled(
            "Run command?",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            command.to_string(),
            Style::default().fg(tui_color(theme.code)),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                "[y]es",
                Style::default()
                    .fg(tui_color(theme.added))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" / "),
            Span::styled(
                "[n]o",
                Style::default()
                    .fg(tui_color(theme.removed))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" / "),
            Span::styled(
                "[a]lways",
                Style::default()
                    .fg(tui_color(theme.link))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ])
    .wrap(Wrap { trim: false });
    frame.render_widget(body, inner);
}

/// The `--resume` Session Picker: a centered bordered list, one row per
/// Session (`stamp  label`), the cursor row reversed+bold, and a dim key-hint
/// footer. Key handling lives in the pure [`Picker`] core; this only draws.
pub fn render_picker(frame: &mut Frame, picker: &Picker, theme: &Theme) {
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
    let width = (content_width + 4)
        .max(44)
        .min(area.width.saturating_sub(2));
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
        Style::default().fg(tui_color(theme.muted)),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn join_summary(name: &str, summary: &str) -> String {
    if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name} {summary}")
    }
}

// Normalizes a `key_arg` for rendering: an absent OR empty arg both read as "no
// arg". The ONE place the display treats emptiness (the source rule lives in the
// core's `key_arg`, but a recovered call summary can still be empty), so both
// join helpers below share it.
fn present_arg(key_arg: Option<&str>) -> Option<&str> {
    key_arg.filter(|a| !a.is_empty())
}

// Whether a Tool Result summary already opens with a status glyph - a plugin
// badge like `✗ exit 1` or `✓ exit 0`. The error line uses this to avoid
// doubling the `✗` it otherwise injects.
fn starts_with_status_glyph(summary: &str) -> bool {
    summary.starts_with('✗') || summary.starts_with('✓')
}

// The name plus its merged `key_arg`, if any: `name  arg` (two spaces set the
// arg off) or bare `name`. Shared by the success and error result lines.
fn join_arg(name: &str, key_arg: Option<&str>) -> String {
    match present_arg(key_arg) {
        Some(arg) => format!("{name}  {arg}"),
        None => name.to_string(),
    }
}

// The merged success one-liner body: `name  arg · result`, dropping to
// `name → result` when there is no arg (an unpaired result).
fn join_merged(name: &str, key_arg: Option<&str>, summary: &str) -> String {
    match present_arg(key_arg) {
        Some(arg) => format!("{name}  {arg} · {summary}"),
        None => format!("{name} → {summary}"),
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
    use crate::ui::transcript::Transcript;

    // -----------------------------------------------------------------------
    // The semantic MdStyle → Style mapping (ADR-0008): one assertion per
    // vocabulary word, pinning the display fact each variant means.
    // -----------------------------------------------------------------------

    #[test]
    fn md_plain_maps_to_the_default_style() {
        assert_eq!(md_style(MdStyle::Plain, theme::dark()), Style::default());
    }

    #[test]
    fn md_bold_maps_to_the_bold_modifier() {
        assert_eq!(
            md_style(MdStyle::Bold, theme::dark()),
            Style::default().add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn md_italic_maps_to_the_italic_modifier() {
        assert_eq!(
            md_style(MdStyle::Italic, theme::dark()),
            Style::default().add_modifier(Modifier::ITALIC)
        );
    }

    #[test]
    fn md_bold_italic_carries_both_modifiers() {
        let style = md_style(MdStyle::BoldItalic, theme::dark());
        assert!(
            style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
    }

    #[test]
    fn md_inline_code_reads_yellow() {
        assert_eq!(
            md_style(MdStyle::Code, theme::dark()).fg,
            Some(Color::Yellow)
        );
    }

    #[test]
    fn md_code_block_carries_the_code_background() {
        // The bg is the block treatment every code row keeps, highlighted or
        // not; the fg is the plain-fallback tint syntect replaces when it can.
        let style = md_style(MdStyle::CodeBlock, theme::dark());
        assert_eq!(style.bg, Some(tui_color(theme::dark().code_block_bg)));
        assert!(matches!(style.fg, Some(Color::Rgb(..))));
    }

    #[test]
    fn md_heading_reads_bold_cyan() {
        let style = md_style(MdStyle::Heading, theme::dark());
        assert_eq!(style.fg, Some(Color::Cyan));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn md_bullet_reads_cyan() {
        assert_eq!(
            md_style(MdStyle::Bullet, theme::dark()).fg,
            Some(Color::Cyan)
        );
    }

    #[test]
    fn md_quote_reads_dim_italic() {
        let style = md_style(MdStyle::Quote, theme::dark());
        assert_eq!(style.fg, Some(Color::DarkGray));
        assert!(style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn md_link_reads_underlined_blue() {
        let style = md_style(MdStyle::Link, theme::dark());
        assert_eq!(style.fg, Some(Color::Blue));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    // -----------------------------------------------------------------------
    // Themes (ADR-0038): the mappings read the active Theme's slots. The dark
    // Theme must render byte-identically to the pre-theme hardcoded palette
    // (the pinning tests below), and a non-default Theme must actually change
    // what the mappings produce.
    // -----------------------------------------------------------------------

    /// A Theme differing from `dark` only in the slots `overrides` states.
    fn themed(overrides: &str) -> Theme {
        theme::SparseTheme::parse(overrides)
            .expect("the test theme parses")
            .over(theme::dark())
    }

    #[test]
    fn theme_colors_translate_one_to_one_to_ratatui() {
        let pairs: [(theme::Color, Color); 17] = [
            (theme::Color::Black, Color::Black),
            (theme::Color::Red, Color::Red),
            (theme::Color::Green, Color::Green),
            (theme::Color::Yellow, Color::Yellow),
            (theme::Color::Blue, Color::Blue),
            (theme::Color::Magenta, Color::Magenta),
            (theme::Color::Cyan, Color::Cyan),
            (theme::Color::Gray, Color::Gray),
            (theme::Color::DarkGray, Color::DarkGray),
            (theme::Color::LightRed, Color::LightRed),
            (theme::Color::LightGreen, Color::LightGreen),
            (theme::Color::LightYellow, Color::LightYellow),
            (theme::Color::LightBlue, Color::LightBlue),
            (theme::Color::LightMagenta, Color::LightMagenta),
            (theme::Color::LightCyan, Color::LightCyan),
            (theme::Color::White, Color::White),
            (theme::Color::Rgb(1, 2, 3), Color::Rgb(1, 2, 3)),
        ];
        for (theme_color, expected) in pairs {
            assert_eq!(tui_color(theme_color), expected, "{theme_color:?}");
        }
    }

    #[test]
    fn dark_line_styles_pin_the_legacy_palette() {
        let t = theme::dark();
        assert_eq!(
            line_style(LineStyle::Added, t),
            Style::default().fg(Color::Green)
        );
        assert_eq!(
            line_style(LineStyle::Removed, t),
            Style::default().fg(Color::Red)
        );
        assert_eq!(
            line_style(LineStyle::Context, t),
            Style::default().fg(Color::DarkGray)
        );
        assert_eq!(
            line_style(LineStyle::Emphasis, t),
            Style::default().add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            line_style(LineStyle::Muted, t),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        );
        assert_eq!(line_style(LineStyle::Default, t), Style::default());
    }

    #[test]
    fn dark_pressure_styles_pin_the_legacy_palette() {
        let t = theme::dark();
        assert_eq!(
            pressure_style(PressureLevel::Critical, t),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            pressure_style(PressureLevel::Elevated, t),
            Style::default().fg(Color::Black).bg(Color::Yellow)
        );
        assert_eq!(
            pressure_style(PressureLevel::Ok, t),
            Style::default().fg(Color::Gray).bg(Color::Rgb(40, 44, 58))
        );
    }

    #[test]
    fn dark_segment_styles_pin_the_legacy_palette() {
        let t = theme::dark();
        let bold = |fg, bg| Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
        let plain = |fg, bg| Style::default().fg(fg).bg(bg);
        assert_eq!(
            segment_style(SegmentKind::ModeIdle, t),
            bold(Color::Black, Color::Green)
        );
        assert_eq!(
            segment_style(SegmentKind::Position, t),
            bold(Color::Black, Color::Green)
        );
        assert_eq!(
            segment_style(SegmentKind::ModeRunning, t),
            bold(Color::Black, Color::Yellow)
        );
        assert_eq!(
            segment_style(SegmentKind::Connection, t),
            plain(Color::Rgb(150, 160, 185), Color::Rgb(52, 58, 82))
        );
        assert_eq!(
            segment_style(SegmentKind::Model, t),
            plain(Color::Rgb(150, 160, 185), Color::Rgb(52, 58, 82))
        );
        assert_eq!(
            segment_style(SegmentKind::Thinking, t),
            plain(Color::DarkGray, Color::Rgb(40, 44, 58))
        );
        assert_eq!(
            segment_style(SegmentKind::Tools, t),
            plain(Color::DarkGray, Color::Rgb(40, 44, 58))
        );
        assert_eq!(
            segment_style(SegmentKind::Cost, t),
            plain(Color::Gray, Color::Rgb(40, 44, 58))
        );
        assert_eq!(
            segment_style(SegmentKind::Tokens(PressureLevel::Critical), t),
            pressure_style(PressureLevel::Critical, t)
        );
        // The ex-constants, now slots: the code, bar, and quiet-segment
        // backgrounds keep their exact legacy values under dark.
        assert_eq!(tui_color(t.code_block_bg), Color::Rgb(25, 25, 35));
        assert_eq!(tui_color(t.bar_bg), Color::Rgb(30, 30, 40));
        assert_eq!(tui_color(t.segment_muted_bg), Color::Rgb(40, 44, 58));
    }

    #[test]
    fn a_non_default_theme_recolors_the_mappings() {
        let t = themed("[colors]\nadded = \"#123456\"\nheading = \"magenta\"\n");
        assert_eq!(
            line_style(LineStyle::Added, &t).fg,
            Some(Color::Rgb(0x12, 0x34, 0x56))
        );
        assert_eq!(md_style(MdStyle::Heading, &t).fg, Some(Color::Magenta));
        // Unstated slots still read the dark floor.
        assert_eq!(line_style(LineStyle::Removed, &t).fg, Some(Color::Red));
    }

    // -----------------------------------------------------------------------
    // markdown_lines: the semantic-MdLine → ratatui-Line rendering, including
    // the code-fence routing (syntect vs. the plain CodeBlock fallback).
    // -----------------------------------------------------------------------

    #[test]
    fn markdown_lines_styles_prose_spans_through_md_style() {
        let lines = markdown_lines("plain **bold** text", theme::dark());
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "plain bold text");
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "bold")
            .expect("the bold span");
        assert_eq!(bold.style, md_style(MdStyle::Bold, theme::dark()));
    }

    #[test]
    fn a_known_language_fence_is_highlighted_over_the_code_background() {
        let lines = markdown_lines("```rust\nlet x = 1;\n```", theme::dark());
        let code = lines
            .iter()
            .find(|l| line_text(l) == "let x = 1;")
            .expect("the code line, text verbatim");
        // Syntect fragments the line; every fragment keeps OUR code bg under
        // its own syntect fg.
        assert!(code.spans.len() > 1, "syntect splits the line");
        for span in &code.spans {
            assert_eq!(span.style.bg, Some(tui_color(theme::dark().code_block_bg)));
            assert!(matches!(span.style.fg, Some(Color::Rgb(..))));
        }
    }

    #[test]
    fn an_unknown_language_fence_falls_back_to_the_plain_code_block_style() {
        let lines = markdown_lines("```notareallanguage\nsome code\n```", theme::dark());
        let code = lines
            .iter()
            .find(|l| line_text(l) == "some code")
            .expect("the code line");
        assert_eq!(code.spans.len(), 1);
        assert_eq!(
            code.spans[0].style,
            md_style(MdStyle::CodeBlock, theme::dark())
        );
    }

    #[test]
    fn a_bare_fence_with_no_language_renders_the_plain_code_block_style() {
        // A bare ``` fence carries `Some("")` - an empty lang must route to
        // the plain path, never to syntect.
        let lines = markdown_lines("```\nunlabeled code\n```", theme::dark());
        let code = lines
            .iter()
            .find(|l| line_text(l) == "unlabeled code")
            .expect("the code line");
        assert_eq!(
            code.spans[0].style,
            md_style(MdStyle::CodeBlock, theme::dark())
        );
    }

    #[test]
    fn a_blank_line_inside_a_highlighted_fence_keeps_the_code_background() {
        let lines = markdown_lines("```rust\nlet a = 1;\n\nlet b = 2;\n```", theme::dark());
        let a_idx = lines
            .iter()
            .position(|l| line_text(l) == "let a = 1;")
            .expect("the first code line");
        // The blank row between the statements yields no syntect fragments,
        // so it takes the plain CodeBlock treatment - same bg, no hole.
        let blank = &lines[a_idx + 1];
        assert_eq!(line_text(blank), "");
        assert_eq!(
            blank.spans[0].style,
            md_style(MdStyle::CodeBlock, theme::dark())
        );
    }

    #[test]
    fn prose_after_a_fence_returns_to_the_plain_path() {
        let lines = markdown_lines("```rust\nlet x = 1;\n```\n\nafter the fence", theme::dark());
        let after = lines
            .iter()
            .find(|l| line_text(l) == "after the fence")
            .expect("the prose line");
        assert_eq!(
            after.spans[0].style,
            md_style(MdStyle::Plain, theme::dark())
        );
    }

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
        let lines = highlight_code(
            &["fn main() { let s = \"hi\"; }"],
            "rust",
            "base16-ocean.dark",
        )
        .unwrap();
        assert_ne!(color_of(&lines, "fn"), color_of(&lines, "hi"));
    }

    #[test]
    fn highlight_code_resolves_extension_tokens_too() {
        // `find_syntax_by_token` matches extensions, not just names.
        assert!(highlight_code(&["let x = 1;"], "rs", "base16-ocean.dark").is_some());
        assert!(highlight_code(&["x = 1"], "py", "base16-ocean.dark").is_some());
    }

    #[test]
    fn highlight_code_returns_none_for_an_unknown_lang() {
        assert_eq!(
            highlight_code(&["whatever"], "notareallanguage", "base16-ocean.dark"),
            None
        );
    }

    #[test]
    fn highlight_code_on_empty_input_is_some_empty() {
        assert_eq!(
            highlight_code(&[], "rust", "base16-ocean.dark"),
            Some(vec![])
        );
    }

    #[test]
    fn highlight_code_blank_line_yields_no_fragments() {
        let lines = highlight_code(
            &["let a = 1;", "", "let b = 2;"],
            "rust",
            "base16-ocean.dark",
        )
        .unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].is_empty());
        assert!(!lines[0].is_empty() && !lines[2].is_empty());
    }

    #[test]
    fn highlight_code_carries_parse_state_across_lines() {
        // A block comment opened on line 1 must color line 2 as comment, not code.
        let lines = highlight_code(
            &["/* comment", "still comment */", "let x = 1;"],
            "rust",
            "base16-ocean.dark",
        )
        .unwrap();
        let comment = color_of(&lines[..1], "comment");
        assert_eq!(color_of(&lines[1..2], "still comment"), comment);
        assert_ne!(color_of(&lines[2..], "let"), comment);
    }

    #[test]
    fn highlight_code_preserves_the_line_text_verbatim() {
        let source = "fn add(a: u32, b: u32) -> u32 { a + b }";
        let lines = highlight_code(&[source], "rust", "base16-ocean.dark").unwrap();
        let joined: String = lines[0].iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(joined, source);
    }

    #[test]
    fn highlighting_follows_the_named_syntax_theme() {
        // The Theme's `syntax` slot picks the syntect theme: the same code
        // colors differently under a dark and a light bundled theme.
        let dark = highlight_code(&["let x = 1;"], "rust", "base16-ocean.dark").unwrap();
        let light = highlight_code(&["let x = 1;"], "rust", "InspiredGitHub").unwrap();
        assert_ne!(dark, light, "two syntax themes color differently");
        // An unknown name falls back to the default rather than panicking
        // (unreachable through Theme parsing, which validates names).
        let fallback = highlight_code(&["let x = 1;"], "rust", "no-such-theme").unwrap();
        assert_eq!(fallback, dark);
    }

    #[test]
    fn markdown_code_highlights_with_the_themes_syntax_slot() {
        // End to end through markdown_lines: a Theme naming a different
        // bundled syntect theme recolors the fence's spans.
        let fence = "```rust\nlet x = 1;\n```";
        let span_fgs = |t: &Theme| -> Vec<Option<Color>> {
            markdown_lines(fence, t)
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.style.fg))
                .collect()
        };
        assert_ne!(
            span_fgs(theme::dark()),
            span_fgs(&themed("syntax = \"InspiredGitHub\"\n"))
        );
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

    /// A [`FigureView`] with no token estimate and a zero (hidden) cost.
    fn no_figures() -> FigureView {
        FigureView {
            tokens: None,
            session_cost: 0.0,
        }
    }

    /// A [`FigureView`] carrying only the token facts, cost hidden.
    fn tokens_only(tokens: TokenView) -> FigureView {
        FigureView {
            tokens: Some(tokens),
            session_cost: 0.0,
        }
    }

    /// Assembles the SEMANTIC bar at `width` with everything present: running,
    /// tokens known at `Ok` pressure, a priced Session total. Returns the pure
    /// [`StatusBar`] - no drawing, no frame.
    fn bar_at(width: usize) -> StatusBar {
        status_bar(
            width,
            Status::Running,
            "http://localhost:8080",
            "qwen/model",
            false,
            false,
            FigureView {
                tokens: Some(TokenView {
                    estimate: 1200,
                    level: PressureLevel::Ok,
                    dead_mass_pct: None,
                }),
                session_cost: 0.42,
            },
            "Bot".to_string(),
        )
    }

    /// The painter's [`SegmentKind`] for each assembled segment - what routes
    /// into [`segment_style`]. Asserting on kinds proves the right meaning
    /// reaches the color mapping without drawing.
    fn kinds(segments: &[StatusSegment]) -> Vec<SegmentKind> {
        segments.iter().map(StatusSegment::kind).collect()
    }

    #[test]
    fn a_wide_bar_keeps_every_segment_in_order() {
        let bar = bar_at(200);
        assert_eq!(
            kinds(&bar.left),
            vec![
                SegmentKind::ModeRunning,
                SegmentKind::Connection,
                SegmentKind::Model,
            ]
        );
        assert_eq!(
            kinds(&bar.right),
            vec![
                SegmentKind::Thinking,
                SegmentKind::Tools,
                SegmentKind::Tokens(PressureLevel::Ok),
                SegmentKind::Cost,
                SegmentKind::Position,
            ]
        );
        assert!(bar.cells() <= 200);
    }

    #[test]
    fn a_narrow_bar_drops_the_connection_then_the_model_segment() {
        // At 70 cols the endpoint drops first (lowest value), then the model -
        // both connection facts leave before mode/position/tokens/cost.
        let bar = bar_at(70);
        assert_eq!(kinds(&bar.left), vec![SegmentKind::ModeRunning]);
        assert_eq!(
            kinds(&bar.right),
            vec![
                SegmentKind::Thinking,
                SegmentKind::Tools,
                SegmentKind::Tokens(PressureLevel::Ok),
                SegmentKind::Cost,
                SegmentKind::Position,
            ]
        );
    }

    #[test]
    fn a_narrower_bar_drops_thinking_then_cost_then_tokens() {
        // At 45 cols both toggles are gone but the figures survive - cost
        // outlives thinking in the drop order.
        let bar = bar_at(45);
        assert_eq!(kinds(&bar.left), vec![SegmentKind::ModeRunning]);
        assert_eq!(
            kinds(&bar.right),
            vec![
                SegmentKind::Tokens(PressureLevel::Ok),
                SegmentKind::Cost,
                SegmentKind::Position,
            ]
        );

        // At 40 cost drops next; tokens survive it (they carry the pressure
        // level the operator steers by).
        let bar = bar_at(40);
        assert_eq!(kinds(&bar.left), vec![SegmentKind::ModeRunning]);
        assert_eq!(
            kinds(&bar.right),
            vec![
                SegmentKind::Tokens(PressureLevel::Ok),
                SegmentKind::Position
            ]
        );

        let bar = bar_at(20);
        assert_eq!(kinds(&bar.left), vec![SegmentKind::ModeRunning]);
        assert_eq!(kinds(&bar.right), vec![SegmentKind::Position]);
    }

    #[test]
    fn mode_and_position_survive_even_when_nothing_fits() {
        // Dropping stops at the last two; a sub-minimal width never panics.
        let bar = bar_at(1);
        assert_eq!(kinds(&bar.left), vec![SegmentKind::ModeRunning]);
        assert_eq!(kinds(&bar.right), vec![SegmentKind::Position]);
    }

    #[test]
    fn the_thinking_segment_carries_the_ctrl_t_state() {
        // The MEANING (expanded true/false) is a semantic fact; the ▾/▸ marker
        // it paints to is a drawing detail asserted separately below.
        let thinking = |expanded: bool| {
            let bar = status_bar(
                200,
                Status::Idle,
                "http://localhost:8080",
                "qwen/model",
                expanded,
                false,
                no_figures(),
                "Bot".to_string(),
            );
            bar.right
                .into_iter()
                .find(|s| matches!(s, StatusSegment::Thinking { .. }))
                .expect("thinking segment is always assembled")
        };
        assert_eq!(thinking(true), StatusSegment::Thinking { expanded: true });
        assert_eq!(thinking(false), StatusSegment::Thinking { expanded: false });
    }

    #[test]
    fn the_thinking_marker_paints_from_its_state() {
        assert_eq!(
            StatusSegment::Thinking { expanded: true }.paint(0),
            " ▾ thinking "
        );
        assert_eq!(
            StatusSegment::Thinking { expanded: false }.paint(0),
            " ▸ thinking "
        );
    }

    #[test]
    fn the_tools_segment_carries_the_ctrl_o_state() {
        // The twin of the thinking segment for the machinery plane: the MEANING
        // (expanded true/false) is the semantic fact; the ▾/▸ marker is a
        // drawing detail asserted separately below.
        let tools = |expanded: bool| {
            let bar = status_bar(
                200,
                Status::Idle,
                "http://localhost:8080",
                "qwen/model",
                false,
                expanded,
                no_figures(),
                "Bot".to_string(),
            );
            bar.right
                .into_iter()
                .find(|s| matches!(s, StatusSegment::Tools { .. }))
                .expect("tools segment is always assembled")
        };
        assert_eq!(tools(true), StatusSegment::Tools { expanded: true });
        assert_eq!(tools(false), StatusSegment::Tools { expanded: false });
    }

    #[test]
    fn the_tools_marker_paints_from_its_state() {
        assert_eq!(
            StatusSegment::Tools { expanded: true }.paint(0),
            " ▾ tools "
        );
        assert_eq!(
            StatusSegment::Tools { expanded: false }.paint(0),
            " ▸ tools "
        );
    }

    #[test]
    fn the_tokens_segment_is_absent_until_an_estimate_exists() {
        let bar = status_bar(
            200,
            Status::Idle,
            "http://localhost:8080",
            "qwen/model",
            false,
            false,
            no_figures(),
            "Bot".to_string(),
        );
        assert_eq!(
            kinds(&bar.right),
            vec![
                SegmentKind::Thinking,
                SegmentKind::Tools,
                SegmentKind::Position
            ]
        );
    }

    // --- the cost segment (ADR-0037: surfacing the priced Session total) ---

    #[test]
    fn cost_label_shows_two_decimals_and_a_sub_cent_floor() {
        // A cent and up: plain two decimals.
        assert_eq!(cost_label(0.42), "$0.42");
        assert_eq!(cost_label(0.01), "$0.01");
        assert_eq!(cost_label(12.3), "$12.30");
        assert_eq!(cost_label(1234.567), "$1234.57");
        // Sub-cent: never "$0.00" - a priced Session must not read as free.
        assert_eq!(cost_label(0.0099), "<$0.01");
        assert_eq!(cost_label(0.0001), "<$0.01");
    }

    #[test]
    fn a_zero_cost_session_shows_no_cost_segment() {
        // The local-only invariant: zero total means the segment is absent
        // entirely, not shown as $0.00 - the bar is exactly the old bar.
        let bar = status_bar(
            200,
            Status::Idle,
            "http://localhost:8080",
            "qwen/model",
            false,
            false,
            tokens_only(TokenView {
                estimate: 1200,
                level: PressureLevel::Ok,
                dead_mass_pct: None,
            }),
            "Bot".to_string(),
        );
        assert!(
            !kinds(&bar.right).contains(&SegmentKind::Cost),
            "zero cost must hide the segment"
        );
    }

    #[test]
    fn a_positive_cost_assembles_the_labelled_segment() {
        let bar = bar_at(200);
        let cost = bar
            .right
            .iter()
            .find(|s| matches!(s, StatusSegment::Cost { .. }))
            .expect("cost segment present once a priced Response landed");
        assert_eq!(
            *cost,
            StatusSegment::Cost {
                label: "$0.42".into()
            }
        );
        assert_eq!(cost.paint(0), " $0.42 ");
        assert_eq!(cost.cells(), " $0.42 ".chars().count());
    }

    #[test]
    fn critical_pressure_yields_a_tokens_segment_carrying_that_level() {
        // The "Critical Context Pressure renders red" rule, asserted headless:
        // the semantic segment carries PressureLevel::Critical, and its kind
        // routes exactly that level into segment_style (which maps it to red).
        let bar = status_bar(
            200,
            Status::Running,
            "u",
            "qwen/model",
            false,
            false,
            tokens_only(TokenView {
                estimate: 99000,
                level: PressureLevel::Critical,
                dead_mass_pct: None,
            }),
            "Bot".to_string(),
        );
        let tokens = bar
            .right
            .iter()
            .find(|s| matches!(s, StatusSegment::Tokens { .. }))
            .expect("tokens segment present when an estimate exists");
        assert_eq!(
            *tokens,
            StatusSegment::Tokens {
                estimate: 99000,
                level: PressureLevel::Critical,
                dead_mass_pct: None,
            }
        );
        assert_eq!(tokens.kind(), SegmentKind::Tokens(PressureLevel::Critical));
    }

    #[test]
    fn every_pressure_level_flows_through_the_tokens_segment_unchanged() {
        for level in [
            PressureLevel::Ok,
            PressureLevel::Elevated,
            PressureLevel::Critical,
        ] {
            let bar = status_bar(
                200,
                Status::Idle,
                "u",
                "qwen/model",
                false,
                false,
                tokens_only(TokenView {
                    estimate: 1,
                    level,
                    dead_mass_pct: None,
                }),
                "Bot".to_string(),
            );
            let tokens = bar
                .right
                .iter()
                .find(|s| matches!(s, StatusSegment::Tokens { .. }))
                .expect("tokens segment present");
            assert_eq!(tokens.kind(), SegmentKind::Tokens(level));
        }
    }

    #[test]
    fn the_mode_segment_carries_idle_vs_running_not_the_spinner_frame() {
        // The semantic distinction is Idle vs. Running; the animation frame is
        // a drawing input the assembly never sees.
        let mode = |status| {
            status_bar(
                200,
                status,
                "u",
                "qwen/model",
                false,
                false,
                no_figures(),
                "Bot".to_string(),
            )
            .left
            .into_iter()
            .next()
            .unwrap()
        };
        assert_eq!(mode(Status::Idle), StatusSegment::Mode(ModeState::Idle));
        assert_eq!(
            mode(Status::Running),
            StatusSegment::Mode(ModeState::Running)
        );
    }

    #[test]
    fn the_running_mode_segment_paints_the_spinner_frame() {
        let running = StatusSegment::Mode(ModeState::Running);
        assert_eq!(running.paint(0), format!(" {} RUNNING ", SPINNER[0]));
        assert_eq!(running.paint(1), format!(" {} RUNNING ", SPINNER[1]));
        // The counter wraps around the frame set.
        assert_eq!(
            running.paint(SPINNER.len() as u64),
            format!(" {} RUNNING ", SPINNER[0])
        );
    }

    #[test]
    fn the_tokens_segment_paints_the_estimate_grouped() {
        assert_eq!(
            StatusSegment::Tokens {
                estimate: 1200,
                level: PressureLevel::Ok,
                dead_mass_pct: None,
            }
            .paint(0),
            " ~1,200 tokens "
        );
    }

    #[test]
    fn a_dead_mass_share_appends_a_percent_dead_tail() {
        // Once a live Dead Mass share is known the Tokens segment grows a `· N%
        // dead` tail (the integer percent, pre-rounded upstream); `None` paints
        // the old form. A live `Some(0)` is meaningful - it shows the tail.
        let with_dead = StatusSegment::Tokens {
            estimate: 1200,
            level: PressureLevel::Ok,
            dead_mass_pct: Some(12),
        };
        assert_eq!(with_dead.paint(0), " ~1,200 tokens · 12% dead ");

        let zero = StatusSegment::Tokens {
            estimate: 1200,
            level: PressureLevel::Ok,
            dead_mass_pct: Some(0),
        };
        assert_eq!(zero.paint(0), " ~1,200 tokens · 0% dead ");

        let without = StatusSegment::Tokens {
            estimate: 1200,
            level: PressureLevel::Ok,
            dead_mass_pct: None,
        };
        assert_eq!(without.paint(0), " ~1,200 tokens ");
    }

    #[test]
    fn the_tokens_segment_cells_match_its_paint_with_and_without_dead_mass() {
        // The load-bearing fit invariant: cells() must equal the painted width
        // in BOTH forms, or the bar over/underflows once a share lands.
        for dead_mass_pct in [None, Some(0), Some(12)] {
            let seg = StatusSegment::Tokens {
                estimate: 1200,
                level: PressureLevel::Ok,
                dead_mass_pct,
            };
            assert_eq!(
                seg.cells(),
                seg.paint(0).chars().count(),
                "{seg:?} cells() disagrees with painted width"
            );
        }
    }

    #[test]
    fn painted_width_matches_the_fit_measurement() {
        // The pure fit policy measures StatusSegment::cells; the painter draws
        // StatusSegment::paint. If they drift, the bar over/underflows. Assert
        // they agree for every segment the running-with-tokens bar assembles.
        let bar = bar_at(200);
        for segment in bar.left.iter().chain(&bar.right) {
            assert_eq!(
                segment.cells(),
                segment.paint(0).chars().count(),
                "{segment:?} cells() disagrees with painted width"
            );
        }
    }

    #[test]
    fn every_segment_style_carries_a_bg_for_the_separators() {
        // The powerline triangles are fg-over-neighbor-bg; a segment without
        // a bg would render a hole in the bar.
        for kind in [
            SegmentKind::ModeIdle,
            SegmentKind::ModeRunning,
            SegmentKind::Connection,
            SegmentKind::Model,
            SegmentKind::Thinking,
            SegmentKind::Tools,
            SegmentKind::Cost,
            SegmentKind::Tokens(PressureLevel::Ok),
            SegmentKind::Tokens(PressureLevel::Elevated),
            SegmentKind::Tokens(PressureLevel::Critical),
            SegmentKind::Position,
        ] {
            assert!(
                segment_style(kind, theme::dark()).bg.is_some(),
                "{kind:?} has no bg"
            );
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
    // The render cache, read through its accessors.
    //
    // The cache syncs against the Transcript STORE (ADR-0034): tests seed a
    // bare store through its verbs - the items Vec is not reachable, which is
    // the point (the extend-vs-rebuild contract is the store's revision).
    // The tests HERE observe only what the frame path observes (`settled`,
    // `streaming_tail`); the extend-vs-rebuild invariant itself is pinned by
    // sentinel tests inside `render_cache`, next to the private entries.
    // -----------------------------------------------------------------------

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn fresh_transcript() -> Transcript {
        Transcript::new(Vec::new())
    }

    #[test]
    fn cache_sync_builds_one_entry_per_settled_item_with_its_wrapped_count() {
        let mut t = fresh_transcript();
        // At width 10 the word-wrapper puts the unbreakable 16-char word
        // below the "› " gutter row and splits it: 3 rows.
        t.user("0123456789012345");
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 10, theme::dark());
        assert_eq!(cache.settled().count(), 1);
        // 3 wrapped rows + 1 trailing inter-turn blank separator.
        assert_eq!(cache.settled().next().unwrap().1, 4);
    }

    #[test]
    fn cache_sync_rebuilds_when_the_width_changes() {
        let mut t = fresh_transcript();
        t.user("0123456789012345");
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        // 1 content row + 1 trailing inter-turn blank separator.
        let wide = cache.settled().next().unwrap().1;
        assert_eq!(wide, 2);
        cache.sync(&t, Toggles::default(), 10, theme::dark()); // resize: every wrapped count is stale
        assert!(cache.settled().next().unwrap().1 > wide);
    }

    #[test]
    fn cache_sync_rebuilds_when_the_thinking_toggle_flips() {
        let mut t = fresh_transcript();
        t.push(TranscriptItem::Thinking {
            text: "line one\nline two".to_string(),
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        // Collapsed one-liner + 1 trailing inter-turn blank separator.
        assert_eq!(cache.settled().next().unwrap().0.len(), 2);
        cache.sync(
            &t,
            Toggles {
                thinking_expanded: true,
                ..Toggles::default()
            },
            80,
            theme::dark(),
        );
        // Header + both rows + 1 trailing inter-turn blank separator.
        assert_eq!(cache.settled().next().unwrap().0.len(), 4);
    }

    #[test]
    fn cache_sync_rebuilds_when_the_tools_toggle_flips() {
        // The Ctrl-O twin of the thinking-toggle test: a multi-line Block folds
        // to a single title line when collapsed and to the full body when
        // expanded, and flipping the toggle clears the cache so the change
        // takes effect. (+1 everywhere for Stage 1's trailing blank separator.)
        let mut t = fresh_transcript();
        t.push(TranscriptItem::Block {
            title: "edit_file src/foo.rs".to_string(),
            lines: vec![
                StyledLine::new(LineStyle::Added, "+ added line"),
                StyledLine::new(LineStyle::Removed, "- removed line"),
            ],
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        // Collapsed one-liner + 1 trailing inter-turn blank separator.
        let collapsed = cache.settled().next().unwrap().0;
        assert_eq!(collapsed.len(), 2);
        assert_eq!(
            line_text(&collapsed[0]),
            "  ⋯ edit_file src/foo.rs · ^O expand"
        );
        cache.sync(
            &t,
            Toggles {
                tools_expanded: true,
                ..Toggles::default()
            },
            80,
            theme::dark(),
        );
        // Title row + both body rows + 1 trailing inter-turn blank separator.
        let expanded = cache.settled().next().unwrap().0;
        assert_eq!(expanded.len(), 4);
        assert_eq!(line_text(&expanded[0]), "  ⋯ edit_file src/foo.rs");
    }

    /// Every fg across the cache's first settled item, in order: each line's
    /// own style fg (styled Lines carry their color there), then its spans'.
    fn settled_span_fgs(cache: &RenderCache) -> Vec<Option<Color>> {
        cache
            .settled()
            .next()
            .expect("one settled item")
            .0
            .iter()
            .flat_map(|l| std::iter::once(l.style.fg).chain(l.spans.iter().map(|s| s.style.fg)))
            .collect()
    }

    #[test]
    fn cache_sync_rebuilds_when_the_theme_changes() {
        // Cached lines BAKE their colors, so a Theme swap (Stage C's live
        // preview) must clear the cache: after syncing with a Theme that
        // recolors `muted`, the settled info line carries the new color.
        let mut t = fresh_transcript();
        t.info("a notice");
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        assert_eq!(settled_span_fgs(&cache)[0], Some(Color::DarkGray));

        let recolored = themed("[colors]\nmuted = \"#ff00ff\"\n");
        cache.sync(&t, Toggles::default(), 80, &recolored);
        assert_eq!(settled_span_fgs(&cache)[0], Some(Color::Rgb(255, 0, 255)));
    }

    #[test]
    fn cache_sync_repaints_highlighted_code_on_a_syntax_theme_swap() {
        // The stale-highlight hazard: syntect colors are baked into cached
        // spans, so a Theme differing only in its `syntax` slot must also
        // rebuild - the swap may not serve the old code colors.
        let mut t = fresh_transcript();
        t.push(TranscriptItem::Assistant {
            text: "```rust\nlet x = 1;\n```".to_string(),
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        let dark_colors = settled_span_fgs(&cache);

        let light_syntax = themed("syntax = \"InspiredGitHub\"\n");
        cache.sync(&t, Toggles::default(), 80, &light_syntax);
        assert_ne!(
            settled_span_fgs(&cache),
            dark_colors,
            "the swap repainted the cached code block"
        );
    }

    // --- Stage 3: merged one-liners + semantic fold ------------------------

    #[test]
    fn a_merged_result_renders_name_arg_dot_result() {
        let item = TranscriptItem::ToolResult {
            name: "read_file".to_string(),
            summary: "340 lines".to_string(),
            is_error: false,
            key_arg: Some("src/foo.rs".to_string()),
        };
        let lines = message_lines(&item, false, false, theme::dark());
        assert_eq!(lines.len(), 1);
        assert_eq!(
            line_text(&lines[0]),
            "  ⋯ read_file  src/foo.rs · 340 lines"
        );
    }

    #[test]
    fn an_unpaired_result_keeps_the_arrow_shape() {
        // No key_arg (governor-injected result): the older `name → result` form.
        let item = TranscriptItem::ToolResult {
            name: "run_command".to_string(),
            summary: "injected".to_string(),
            is_error: false,
            key_arg: None,
        };
        let lines = message_lines(&item, false, false, theme::dark());
        assert_eq!(line_text(&lines[0]), "  ⋯ run_command → injected");
    }

    #[test]
    fn a_failing_merged_result_keeps_the_arg_and_shows_a_single_badge_glyph() {
        // The summary already carries the plugin badge `✗ exit 1`; the error
        // line injects no glyph of its own, so there is a SINGLE `✗`, not `✗ ✗`.
        let item = TranscriptItem::ToolResult {
            name: "run_command".to_string(),
            summary: "✗ exit 1".to_string(),
            is_error: true,
            key_arg: Some("cargo test".to_string()),
        };
        let lines = message_lines(&item, false, false, theme::dark());
        assert_eq!(line_text(&lines[0]), "  ⚙ run_command  cargo test ✗ exit 1");
    }

    #[test]
    fn a_failing_result_without_a_badge_gets_an_injected_error_glyph() {
        // A tool whose summary carries no glyph (no badge plugin): the line
        // injects a leading `✗` so the failure is never missed - the ⚙ gutter,
        // the arg, then `✗ {summary}`, all red+bold.
        let item = TranscriptItem::ToolResult {
            name: "edit_file".to_string(),
            summary: "old_str not found".to_string(),
            is_error: true,
            key_arg: Some("src/foo.rs".to_string()),
        };
        let lines = message_lines(&item, false, false, theme::dark());
        assert_eq!(
            line_text(&lines[0]),
            "  ⚙ edit_file  src/foo.rs ✗ old_str not found"
        );
    }

    #[test]
    fn a_summary_already_starting_with_a_status_glyph_is_not_doubled() {
        // Guard the `✓`/`✗` prefix check: a badge that opens with EITHER glyph
        // suppresses the injected `✗`, so neither doubles up.
        for badge in ["✗ exit 137", "✓ exit 0"] {
            let item = TranscriptItem::ToolResult {
                name: "run_command".to_string(),
                summary: badge.to_string(),
                is_error: true,
                key_arg: None,
            };
            let lines = message_lines(&item, false, false, theme::dark());
            assert_eq!(line_text(&lines[0]), format!("  ⚙ run_command {badge}"));
        }
    }

    #[test]
    fn foldable_body_is_some_only_for_a_non_empty_block() {
        // A non-empty Block folds under Ctrl-O.
        let block = TranscriptItem::Block {
            title: "edit_file x".to_string(),
            lines: vec![StyledLine::new(LineStyle::Added, "+ a")],
        };
        assert!(block.foldable_body().is_some());

        // A one-line merged ToolResult has no body to fold.
        let result = TranscriptItem::ToolResult {
            name: "read_file".to_string(),
            summary: "340 lines".to_string(),
            is_error: false,
            key_arg: Some("src/foo.rs".to_string()),
        };
        assert!(result.foldable_body().is_none());

        // An empty Block has nothing to fold either.
        let empty = TranscriptItem::Block {
            title: "titled but empty".to_string(),
            lines: vec![],
        };
        assert!(empty.foldable_body().is_none());
    }

    #[test]
    fn ctrl_o_still_folds_a_diff_block_after_the_merge() {
        // A merge produces a lone diff Block (the call line removed). Ctrl-O
        // must still collapse it to its one-line title - the semantic fold
        // predicate keys on the Block's foldable body, unaffected by the merge.
        let block = TranscriptItem::Block {
            title: "edit_file src/foo.rs (+1 -1)".to_string(),
            lines: vec![
                StyledLine::new(LineStyle::Added, "+ new"),
                StyledLine::new(LineStyle::Removed, "- old"),
            ],
        };
        // Collapsed (tools_expanded = false): one title line with the affordance.
        let collapsed = message_lines(&block, false, false, theme::dark());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            line_text(&collapsed[0]),
            "  ⋯ edit_file src/foo.rs (+1 -1) · ^O expand"
        );
        // Expanded: title + both body rows.
        let expanded = message_lines(&block, false, true, theme::dark());
        assert_eq!(expanded.len(), 3);
    }

    // The Stage 2 review's deferred scroll test: an unpinned viewport stores an
    // absolute top offset, so flipping Ctrl-O and back - which changes the total
    // line count while expanded but restores it when collapsed - leaves the
    // clamped draw-time offset exactly where it was.
    #[test]
    fn a_ctrl_o_round_trip_leaves_the_viewport_position_stable() {
        use crate::ui::viewport::Viewport;

        let mut t = fresh_transcript();
        // Some prose above the fold, then a tall foldable block so expand/collapse
        // changes the total wrapped-line count.
        for i in 0..8 {
            t.info(format!("prose line {i}"));
        }
        t.push(TranscriptItem::Block {
            title: "edit_file big.rs".to_string(),
            lines: (0..30)
                .map(|i| StyledLine::new(LineStyle::Added, format!("+ line {i}")))
                .collect(),
        });

        let width = 80u16;
        let height = 10usize;

        let mut cache = RenderCache::new();

        let total_lines = |cache: &mut RenderCache, t: &Transcript, tools: bool| -> usize {
            cache.sync(
                t,
                Toggles {
                    tools_expanded: tools,
                    ..Toggles::default()
                },
                width,
                theme::dark(),
            );
            cache.settled().map(|(_, wrapped)| wrapped).sum()
        };

        // Collapsed: scroll up a few lines to an absolute offset (unpins).
        let collapsed_total = total_lines(&mut cache, &t, false);
        let mut vp = Viewport::new();
        vp.scroll_up(5, collapsed_total, height);
        let collapsed_top = vp.top_offset(collapsed_total, height);

        // Expand: the total grows, but the stored absolute offset is stationary.
        let expanded_total = total_lines(&mut cache, &t, true);
        assert!(expanded_total > collapsed_total, "expanding adds body rows");
        let expanded_top = vp.top_offset(expanded_total, height);
        assert_eq!(
            expanded_top, collapsed_top,
            "an unpinned viewport is stationary across the expand"
        );

        // Collapse again: the total returns, and so does the drawn offset.
        let collapsed_again_total = total_lines(&mut cache, &t, false);
        let collapsed_again_top = vp.top_offset(collapsed_again_total, height);
        assert_eq!(
            collapsed_top, collapsed_again_top,
            "a Ctrl-O round trip returns the viewport to the same position"
        );
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
                .map(|item| wrapped_count(message_lines(item, false, false, theme::dark()), width))
                .sum();
            let whole: Vec<Line> = items
                .iter()
                .flat_map(|item| message_lines(item, false, false, theme::dark()))
                .collect();
            assert_eq!(per_item, wrapped_count(whole, width), "width {width}");
        }
    }

    // -----------------------------------------------------------------------
    // Frame-level render tests (ratatui TestBackend): draw one frame into an
    // in-memory buffer and assert the meaningful facts land - titles, known
    // lines, the scrollbar gutter - not whole-screen snapshots.
    // -----------------------------------------------------------------------

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::content::ContentBlock;
    use crate::event::Event;
    use crate::llm::Delta;
    use crate::ui::screen::ScreenOpts;

    /// Draws one frame with `draw` on a fresh `width`×`height` test terminal
    /// and returns the terminal for buffer inspection.
    fn draw_frame(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| draw(frame)).expect("draw one frame");
        terminal
    }

    /// One buffer row's symbols, concatenated.
    fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).expect("cell in area").symbol())
            .collect()
    }

    /// The whole buffer as newline-joined rows, for `contains` assertions.
    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| row_text(terminal, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // --- render_composer_popup: the overlay variants ------------------------

    #[test]
    fn the_menu_popup_titles_commands_and_lists_the_rows_with_hints() {
        let view = OverlayView::Menu {
            rows: vec![
                SelectorRow::new("model", "/model", Some("pick the model".to_string())),
                SelectorRow::new("clear", "/clear", None),
            ],
            highlight: 0,
        };
        let terminal = draw_frame(40, 12, |f| {
            render_composer_popup(f, 10, f.area(), &view, theme::dark())
        });
        let text = buffer_text(&terminal);
        assert!(text.contains(" commands "), "bordered title:\n{text}");
        assert!(text.contains("/model"));
        assert!(text.contains("pick the model"), "the hint rides its row");
        assert!(text.contains("/clear"));
    }

    #[test]
    fn the_menu_popup_reverses_the_highlighted_row() {
        let view = OverlayView::Menu {
            rows: vec![
                SelectorRow::new("model", "/model", None),
                SelectorRow::new("clear", "/clear", None),
            ],
            highlight: 1,
        };
        let terminal = draw_frame(40, 12, |f| {
            render_composer_popup(f, 10, f.area(), &view, theme::dark())
        });
        // Geometry: 2 body rows + borders = height 4, anchored above row 10,
        // so the body sits at rows 7-8; the popup is inset one column and the
        // border one more, so row text starts at x = 2.
        let buf = terminal.backend().buffer();
        assert!(row_text(&terminal, 8).contains("/clear"));
        let highlighted = buf.cell((2u16, 8u16)).expect("highlight cell");
        assert_eq!(highlighted.symbol(), "/");
        assert!(
            highlighted
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        let unhighlighted = buf.cell((2u16, 7u16)).expect("plain cell");
        assert!(
            !unhighlighted
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn an_empty_menu_popup_shows_no_matches() {
        let view = OverlayView::Menu {
            rows: vec![],
            highlight: 0,
        };
        let terminal = draw_frame(40, 12, |f| {
            render_composer_popup(f, 10, f.area(), &view, theme::dark())
        });
        assert!(buffer_text(&terminal).contains("no matches"));
    }

    #[test]
    fn a_loading_selector_popup_shows_the_loading_line() {
        let view = OverlayView::Selector {
            command: "model".to_string(),
            status: OverlayStatus::Loading,
            rows: vec![],
            highlight: 0,
        };
        let terminal = draw_frame(40, 12, |f| {
            render_composer_popup(f, 10, f.area(), &view, theme::dark())
        });
        let text = buffer_text(&terminal);
        assert!(text.contains(" models "), "selector title:\n{text}");
        assert!(text.contains("loading models…"));
    }

    #[test]
    fn a_failed_selector_popup_shows_the_failure_message() {
        let view = OverlayView::Selector {
            command: "model".to_string(),
            status: OverlayStatus::Failed("connection refused".to_string()),
            rows: vec![],
            highlight: 0,
        };
        let terminal = draw_frame(40, 12, |f| {
            render_composer_popup(f, 10, f.area(), &view, theme::dark())
        });
        assert!(buffer_text(&terminal).contains("failed: connection refused"));
    }

    #[test]
    fn a_ready_selector_popup_lists_the_model_rows() {
        let view = OverlayView::Selector {
            command: "model".to_string(),
            status: OverlayStatus::Ready,
            rows: vec![
                SelectorRow::new("a", "qwen/qwen3-30b", None),
                SelectorRow::new("b", "meta/llama-3.1", None),
            ],
            highlight: 0,
        };
        let terminal = draw_frame(40, 12, |f| {
            render_composer_popup(f, 10, f.area(), &view, theme::dark())
        });
        let text = buffer_text(&terminal);
        assert!(text.contains(" models "));
        assert!(text.contains("qwen/qwen3-30b"));
        assert!(text.contains("meta/llama-3.1"));
    }

    #[test]
    fn the_selector_popup_titles_itself_after_its_command() {
        // The title pluralizes the opaque command name, so /theme's selector
        // reads " themes " without the renderer knowing any command.
        let view = OverlayView::Selector {
            command: "theme".to_string(),
            status: OverlayStatus::Loading,
            rows: vec![],
            highlight: 0,
        };
        let terminal = draw_frame(40, 12, |f| {
            render_composer_popup(f, 10, f.area(), &view, theme::dark())
        });
        let text = buffer_text(&terminal);
        assert!(text.contains(" themes "), "selector title:\n{text}");
        assert!(text.contains("loading themes…"));
    }

    #[test]
    fn the_popup_scrolls_the_highlighted_row_into_view() {
        // 20 rows against the POPUP_MAX_ROWS cap: highlighting the last row
        // must scroll the top rows out and bring it on screen.
        let rows: Vec<SelectorRow> = (0..20)
            .map(|i| SelectorRow::new(format!("m{i}"), format!("model-{i:02}"), None))
            .collect();
        let view = OverlayView::Selector {
            command: "model".to_string(),
            status: OverlayStatus::Ready,
            rows,
            highlight: 19,
        };
        let terminal = draw_frame(40, 14, |f| {
            render_composer_popup(f, 12, f.area(), &view, theme::dark())
        });
        let text = buffer_text(&terminal);
        assert!(text.contains("model-19"), "highlight scrolled into view");
        assert!(!text.contains("model-00"), "the top rows scrolled out");
    }

    // --- render_viewport: geometry, the scrollbar gutter, streaming ---------

    fn screen_with_notices(notices: Vec<String>) -> Screen {
        Screen::new(ScreenOpts {
            notices,
            ..ScreenOpts::default()
        })
    }

    #[test]
    fn the_viewport_draws_the_transcript_and_returns_the_measured_geometry() {
        let screen = screen_with_notices(vec!["a launch notice".to_string()]);
        let viewport = Viewport::new();
        let mut cache = RenderCache::new();
        let mut geometry = (0, 0);
        let terminal = draw_frame(80, 20, |f| {
            geometry = render_viewport(f, f.area(), &screen, &viewport, &mut cache, theme::dark());
        });
        let text = buffer_text(&terminal);
        assert!(text.contains("suspenders ready"), "the greeting:\n{text}");
        assert!(text.contains("a launch notice"));
        let (total_lines, height) = geometry;
        assert_eq!(height, 20, "height is the drawn area's");
        assert!(total_lines > 0 && total_lines <= height, "content fits");
        // Fitting content draws no scrollbar, but the gutter column is still
        // reserved: the rightmost column stays empty.
        for y in 0..20 {
            let row = row_text(&terminal, y);
            assert_eq!(row.chars().last(), Some(' '), "row {y}: {row:?}");
        }
    }

    #[test]
    fn an_overflowing_transcript_pins_the_tail_and_draws_the_scrollbar() {
        let notices: Vec<String> = (0..30).map(|i| format!("notice line {i:02}")).collect();
        let screen = screen_with_notices(notices);
        let viewport = Viewport::new();
        let mut cache = RenderCache::new();
        let mut geometry = (0, 0);
        let terminal = draw_frame(40, 8, |f| {
            geometry = render_viewport(f, f.area(), &screen, &viewport, &mut cache, theme::dark());
        });
        let (total_lines, height) = geometry;
        assert!(total_lines > height, "the content overflows");
        let text = buffer_text(&terminal);
        // A fresh viewport is pinned: the tail is on screen, the top is not.
        assert!(text.contains("notice line 29"));
        assert!(!text.contains("notice line 00"));
        // The reserved gutter now carries scrollbar glyphs.
        let gutter: Vec<char> = (0..8)
            .filter_map(|y| row_text(&terminal, y).chars().last())
            .collect();
        assert!(
            gutter.iter().any(|c| *c != ' '),
            "scrollbar in the gutter: {gutter:?}"
        );
    }

    #[test]
    fn a_streaming_thinking_snapshot_draws_the_one_line_indicator() {
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Thinking("pondering".to_string()),
            vec![ContentBlock::Thinking {
                text: "pondering the viewport".to_string(),
            }],
        ));
        let viewport = Viewport::new();
        let mut cache = RenderCache::new();
        let terminal = draw_frame(80, 20, |f| {
            render_viewport(f, f.area(), &screen, &viewport, &mut cache, theme::dark());
        });
        let text = buffer_text(&terminal);
        // The in-flight indicator is the one-liner with a token estimate -
        // never the thinking text itself.
        assert!(text.contains("thinking… (~"), "the indicator:\n{text}");
        assert!(text.contains("tokens)"));
        assert!(!text.contains("pondering the viewport"));
    }

    #[test]
    fn in_flight_assistant_text_renders_as_the_streaming_tail() {
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Text("a streaming reply".to_string()),
            vec![ContentBlock::text("a streaming reply")],
        ));
        let viewport = Viewport::new();
        let mut cache = RenderCache::new();
        let terminal = draw_frame(80, 20, |f| {
            render_viewport(f, f.area(), &screen, &viewport, &mut cache, theme::dark());
        });
        assert!(buffer_text(&terminal).contains("a streaming reply"));
    }
}
