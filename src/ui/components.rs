//! UI Components - the SINGLE mapping from the semantic display vocabulary
//! (ADR-0008) to ratatui `Style`/`Color`, plus the render helpers the frontend
//! draws with.
//!
//! This is the one place semantics become terminal colors: [`DiffSide`] →
//! color for a diff's lines, [`PressureLevel`] → color/emphasis for the status
//! bar. Extensions and the Screen core never touch ratatui; they speak the
//! vocabulary and this module renders it. Everything here is pure presentation
//! of [`TranscriptItem`]s - no state, no IO. Only this module and [`crate::ui`]
//! `use ratatui` / `use crossterm` (ADR-0019 invariant).

use std::sync::OnceLock;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use syntect::easy::HighlightLines;
use syntect::parsing::SyntaxSet;

use thousands::Separable;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::composer::{self, ComposerLayout, OverlayStatus, OverlayView};
use crate::ui::lull;
use crate::ui::markdown::{self, MdLine, MdStyle};
use crate::ui::picker::Picker;
use crate::ui::screen::{PressureLevel, Screen, Status};
use crate::ui::slash;
use crate::ui::theme::{self, Theme};
use crate::view_model::Tone;
use crate::view_model::{DiffHunk, DiffSide, TranscriptItem};
use crate::view_model::{RowRole, SelectorRow};

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

/// The ONE mapping from a diff's [`DiffSide`] to its fallback foreground
/// (ADR-0008): added reads green, removed red, context the muted context slot.
/// This is the fg the marker glyph always wears (so add/remove reads without
/// truecolor) and the code text falls back to when no syntect fragment colors
/// it. The added/removed background TINT is a separate mapping ([`diff_tint`]).
fn diff_side_fg(side: DiffSide, theme: &Theme) -> Color {
    match side {
        DiffSide::Added => tui_color(theme.added),
        DiffSide::Removed => tui_color(theme.removed),
        DiffSide::Context => tui_color(theme.context),
    }
}

/// The muted-italic style a diff's adapter CHROME wears (ADR-0008): the `@@ … @@`
/// hunk header and the `… N more lines` elision tail - neither is a code line,
/// so neither carries a marker, a tint, or syntect fg. One helper so both read
/// the same.
fn diff_chrome_style(theme: &Theme) -> Style {
    Style::default()
        .fg(tui_color(theme.muted))
        .add_modifier(Modifier::ITALIC)
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

/// The frame-animation clocks the adapter advances each ~100ms tick while a
/// Run runs. One value object so the render path takes a single animation
/// argument and new clocks are a field, not another parameter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Anim {
    /// The braille `✦ Thinking` spinner frame (advances every running tick).
    pub spinner: u64,
    /// Ticks of unbroken quiet in the CURRENT lull (reset when output streams,
    /// or when the Run ends). Drives the lull animation + its elapsed timer.
    pub quiet_ticks: u64,
    /// Which lull this is, session-wide (bumped when a new lull begins). Seeds
    /// the per-lull scene pick, so a fresh wait usually brings a fresh scene.
    pub lull_seq: u64,
}

/// The per-frame render context the inline pending path draws WITH: the
/// connection facts the status bar shows, the animation clocks, and the Theme
/// this frame renders in (the live `/theme` preview or the active Theme).
/// Bundled as ONE named-field carrier - the same style as [`PendingBodyParams`],
/// [`GutterCtx`], [`StatusBarCtx`] and the adapter's `AdapterCtx` - so
/// [`render_pending`] and the adapter's `draw`/`draw_previewed` take four args
/// instead of six, and a new frame-wide input is a field, not another parameter.
#[derive(Clone, Copy)]
pub struct FrameCtx<'a> {
    pub conn: ConnectionView<'a>,
    pub anim: Anim,
    pub theme: &'a Theme,
}

/// Splits the inline frame `area` into the three vertical zones the pending
/// region draws into: `[pending_body, status_bar, composer]` (ADR-0046). There
/// is no scroll state and no geometry return - native scrollback owns history,
/// so the pending body is simply bottom-anchored + top-clipped in the top zone.
///
/// The Composer GROWS with its draft: its height is the wrapped row count
/// (hard newlines and width-wrapping both), capped by
/// [`composer::max_visible_rows`] so a tall draft never starves the pending
/// body - which is expected to shrink as the Composer grows. The wrap math runs
/// at the exact width the Composer is drawn at (the frame minus the 2-cell
/// gutter), so the measured cursor cell is the drawn one. `composer_rows` is the
/// already-capped Composer row count. Pure - no frame access.
fn frame_chunks(area: Rect, composer_rows: usize) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                       // inline pending body (ADR-0046)
            Constraint::Length(1),                    // status bar
            Constraint::Length(composer_rows as u16), // composer (grows with the draft)
        ])
        .split(area)
}

/// The Composer's visible row count for this frame: the layout's row count
/// capped by [`composer::max_visible_rows`] so a very tall draft never starves
/// the pending body. Pure - no frame access.
fn capped_composer_height(layout: &ComposerLayout, frame_height: usize) -> usize {
    layout
        .rows
        .len()
        .min(composer::max_visible_rows(frame_height))
}

/// Renders the inline PENDING region (ADR-0046): the uncommitted transcript
/// tail (`cache.settled()[hw..]` plus the live reasoning tail, streaming answer,
/// and lull row), the status bar, the Composer, and any open overlay/approval.
/// Committed items are NOT drawn here - they were frozen into native scrollback
/// by [`render_committed_slice`] via the adapter's `insert_before`.
///
/// The transcript body is BOTTOM-ANCHORED in its zone and TOP-CLIPPED on
/// overflow (qwen's `MaxSizedBox overflowDirection:"top"`): the newest rows
/// always show, older rows drop off the top. There is no scroll state and no
/// scrollbar - native scrollback owns history.
pub fn render_pending(frame: &mut Frame, t: &Screen, cache: &mut RenderCache, ctx: FrameCtx) {
    let FrameCtx { conn, anim, theme } = ctx;
    let area = frame.area();
    let composer_view = t.composer().view();
    let layout = composer::layout(
        composer_view.draft,
        composer_view.cursor,
        area.width.saturating_sub(2) as usize,
    );
    let composer_height = capped_composer_height(&layout, area.height as usize);
    let chunks = frame_chunks(area, composer_height);

    let body_area = chunks[0];
    render_pending_body(
        frame,
        body_area,
        &mut PendingBodyParams {
            screen: t,
            cache,
            anim,
        },
        theme,
    );

    // The status bar's position segment is a literal `Bot` in the inline model
    // (ADR-0046): native scrollback owns history and the pending body always
    // follows the tail, so there is no scroll position to report.
    render_status_bar(frame, chunks[1], StatusBarCtx { screen: t, conn }, theme);
    render_composer(frame, chunks[2], t, &layout, theme);

    if let Some(overlay) = composer_view.overlay {
        render_composer_popup(frame, chunks[1].y, area, &overlay, theme);
    }
    if let Some(pending) = &t.pending_approval {
        render_approval_modal(frame, area, &pending.command, theme);
    }
}

/// The scroll-free state [`render_pending_body`] needs each frame: the Screen it
/// reads the pending items and live snapshot from, the cache the settled tail's
/// lines come from, and the animation counters. Bundled so the body render takes
/// four args (the reduced SRP_PARAMS call shape).
pub struct PendingBodyParams<'a> {
    pub screen: &'a Screen,
    pub cache: &'a mut RenderCache,
    pub anim: Anim,
}

/// Draws the pending transcript body into `area`, bottom-anchored and
/// top-clipped (ADR-0046). Returns the total wrapped-row count of the pending
/// stack (before clipping) so the caller can label the status bar. The assembly
/// is the pending pipeline - cache sync, the collapsed-run fold over the
/// full items, the three live entries - but slices the settled tail from the
/// high-water mark ([`assemble_pending`]) and anchors to the bottom instead of
/// consulting a [`Viewport`].
fn render_pending_body(
    frame: &mut Frame,
    area: Rect,
    params: &mut PendingBodyParams<'_>,
    theme: &Theme,
) -> usize {
    let hw = params.screen.transcript().committed_high_water();
    render_pending_body_at(frame, area, params, theme, hw)
}

/// Draws the pending body starting AT an explicit high-water mark `hw`: it emits
/// the uncommitted settled tail `items[hw..]` plus the live stream, bottom-
/// anchored and top-clipped (ADR-0046). [`render_pending_body`] calls this with
/// the store's live
/// [`committed_high_water`](crate::ui::transcript::Transcript::committed_high_water)
/// (committed items are already in native scrollback); passing `0` draws the
/// WHOLE settled transcript, which is what a headless test wants to see on a
/// TestBackend that has no real scrollback.
fn render_pending_body_at(
    frame: &mut Frame,
    area: Rect,
    params: &mut PendingBodyParams<'_>,
    theme: &Theme,
    hw: usize,
) -> usize {
    let t = params.screen;
    let cache = &mut params.cache;
    let anim = params.anim;

    let content_area = Rect {
        x: area.x + LANE_GUTTER,
        width: area.width.saturating_sub(LANE_GUTTER),
        ..area
    };
    cache.sync(
        t.transcript(),
        Toggles {
            thinking_expanded: t.thinking_expanded,
            tools_expanded: t.tools_expanded,
        },
        content_area.width,
        theme,
    );

    let thinking = t.transcript().streaming_thinking();
    let thinking_lines = live_thinking_lines(&thinking, anim.spinner, content_area.width, theme);

    let items = t.transcript().items();
    let lane = lane_gutters(items);
    // FULL-CONTENT pending body (ADR-0046): the uncommitted settled tail renders
    // each item from its cached lines EXACTLY as `render_committed_slice` blits
    // the committed prefix - no collapsed-run fold, no machinery window. Committed
    // and pending are the same rendering of the same cache, so nothing reflows at
    // the commit seam (qwen's `<Static>` prints history un-clamped; the ONLY
    // overflow reduction is the bottom-anchor + top-clip below). The stack's
    // three facts (lines, wrapped-count, gutter) travel together as one row so
    // they can never desync.
    let mut stack = assemble_pending(cache, &lane, hw);

    // The live entries follow the settled tail, newest last: the reasoning tail,
    // then the streaming answer, then (only when nothing is streaming) the lull
    // row. `thinking_lines`/`lull_lines` are borrowed into the stack, so they
    // outlive it here.
    stack.push_live(&thinking_lines, content_area.width);
    let tail = cache.streaming_tail();
    if let Some((lines, wrapped)) = tail {
        stack.push(lines, wrapped, GutterKind::Spine);
    }
    let lull_lines = if lull_visible(t.status, thinking_lines.is_empty(), tail.is_some()) {
        live_lull_lines(anim, content_area.width, theme)
    } else {
        Vec::new()
    };
    stack.push_live(&lull_lines, content_area.width);

    // Integration (IOSP): compute the anchor/clip geometry in the pure
    // [`anchor_clip`] operation, then only issue the draw calls. All the
    // bottom-anchor + top-clip arithmetic lives in the operation; here we just
    // paint the content, mirror the gutter, and (on overflow) the marker.
    let row_gutters = stack.expand_gutters();
    let clip = anchor_clip(stack.total_lines(), area, content_area);

    frame.render_widget(
        Paragraph::new(stack.flat_lines())
            .wrap(Wrap { trim: false })
            .scroll((clip.scroll, 0)),
        clip.content_draw,
    );
    // The gutter mirrors the content: same `top` offset into `row_gutters`, same
    // top-clip, so the spine/caret stay aligned with their rows.
    paint_gutter(
        frame,
        clip.gutter_draw,
        &GutterCtx {
            row_gutters: &row_gutters,
            top: clip.top,
            height: clip.gutter_draw.height as usize,
        },
        theme,
    );
    if let Some(marker_draw) = clip.marker_draw {
        draw_overflow_marker(frame, marker_draw, theme);
    }

    clip.total_lines
}

/// The bottom-anchor + top-clip geometry a pending body draws at (ADR-0046),
/// resolved from the stack's `total_lines` against the zone `area`/`content_area`.
/// Every field is a ready-to-draw value, so [`render_pending_body_at`] holds no
/// layout arithmetic of its own (IOSP). `marker_draw` is `Some` only when the
/// stack overflows the zone.
struct PendingClip {
    /// The stack's total wrapped rows, echoed back for the caller's return value.
    total_lines: usize,
    /// Relative scroll into the flat row stream (== the top-clipped row count).
    top: usize,
    /// Content Paragraph scroll offset (`top`, saturated into `u16`).
    scroll: u16,
    content_draw: Rect,
    gutter_draw: Rect,
    /// The `… Ctrl-S to show more` marker row, present only on overflow.
    marker_draw: Option<Rect>,
}

/// Operation (IOSP): the pure anchor/clip math for a pending body of
/// `total_lines` wrapped rows in a `content_area` inside the zone `area`. When the
/// stack overflows, keep the LAST `height` rows (drop from the top, qwen's
/// `overflowDirection:"top"`) and reserve the top row for the overflow marker; when
/// it fits, bottom-anchor it via `pad_top`. No frame access, no side effects.
fn anchor_clip(total_lines: usize, area: Rect, content_area: Rect) -> PendingClip {
    let height = area.height as usize;
    let overflowed = total_lines > height;

    let (top, drawn_rows, pad_top) = if overflowed {
        (total_lines - height + 1, height, 0)
    } else {
        (0, total_lines, height - total_lines)
    };

    // On overflow the top visible row is the marker, so the content/gutter both
    // start one row down and lose that row of height.
    let content_top_pad: u16 = if overflowed { 1 } else { 0 };
    let draw_height = drawn_rows.saturating_sub(content_top_pad as usize) as u16;
    let y_off = pad_top as u16 + content_top_pad;

    PendingClip {
        total_lines,
        top,
        scroll: u16::try_from(top).unwrap_or(u16::MAX),
        content_draw: Rect {
            y: content_area.y + y_off,
            height: draw_height,
            ..content_area
        },
        gutter_draw: Rect {
            y: area.y + y_off,
            height: draw_height,
            ..area
        },
        marker_draw: overflowed.then_some(Rect {
            y: area.y + pad_top as u16,
            height: 1,
            ..area
        }),
    }
}

/// Draws the `… Ctrl-S to show more` overflow marker (ADR-0046, qwen's
/// `ShowMoreLines`) on the reserved top row. Ctrl-S expand handling is deferred -
/// Phase 1 wires the marker + clip only.
// TODO(ADR-0046): Ctrl-S to flip an expanded, unclamped one-shot view.
fn draw_overflow_marker(frame: &mut Frame, area: Rect, theme: &Theme) {
    let marker_style = Style::default()
        .fg(tui_color(theme.muted))
        .add_modifier(Modifier::ITALIC);
    frame.render_widget(
        Paragraph::new(Line::styled("… Ctrl-S to show more", marker_style)),
        area,
    );
}

/// Computes the bounding rect for the Composer overlay popup: body rows plus
/// top/bottom border, capped so a long list never eats the screen, positioned
/// just above `anchor_y` and horizontally centered within `area`. Pure - no
/// frame access. `body_len` is the number of content lines the popup will hold.
fn popup_rect(anchor_y: u16, area: Rect, body_len: usize) -> Rect {
    let body_rows = body_len.max(1) as u16;
    let height = (body_rows + 2).min(POPUP_MAX_ROWS + 2).min(area.height);
    let width = area.width.saturating_sub(2).max(1);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = anchor_y.saturating_sub(height);
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Resolves the title string for a Selector popup: the command's `list_title`
/// from the registry, or the raw command name if it is not registered.
fn selector_popup_title(command: &str) -> String {
    slash::lookup(command)
        .map(|c| c.list_title.to_string())
        .unwrap_or_else(|| command.to_string())
}

/// Resolves the body lines for a Selector popup given the overlay status: a
/// loading/error status line, or the full row list when ready. Pure - no draw
/// calls, only styled [`Line`] construction.
fn selector_popup_lines(
    title: &str,
    status: &OverlayStatus,
    rows: &[SelectorRow],
    highlight: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    match status {
        OverlayStatus::Loading => vec![Line::styled(
            format!("loading {title}…"),
            Style::default()
                .fg(tui_color(theme.muted))
                .add_modifier(Modifier::ITALIC),
        )],
        OverlayStatus::Failed(msg) => vec![Line::styled(
            format!("failed: {msg}"),
            Style::default()
                .fg(tui_color(theme.error))
                .add_modifier(Modifier::BOLD),
        )],
        OverlayStatus::Ready => popup_rows(rows, highlight, theme),
    }
}

/// Derives the popup title and body lines from the current overlay view.
/// Pure - reads only the view and theme, emits no draw calls.
fn popup_title_and_lines(view: &OverlayView, theme: &Theme) -> (String, Vec<Line<'static>>) {
    match view {
        OverlayView::Menu { rows, highlight } => {
            ("commands".into(), popup_rows(rows, *highlight, theme))
        }
        OverlayView::Selector {
            command,
            status,
            rows,
            highlight,
        } => {
            let title = selector_popup_title(command);
            let lines = selector_popup_lines(&title, status, rows, *highlight, theme);
            (title, lines)
        }
    }
}

/// Returns the highlighted row index from an overlay view, used to scroll the
/// list so the cursor stays visible when the popup overflows its height.
fn popup_highlight(view: &OverlayView) -> usize {
    match view {
        OverlayView::Menu { highlight, .. } => *highlight,
        OverlayView::Selector { highlight, .. } => *highlight,
    }
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
    let (title, lines) = popup_title_and_lines(view, theme);
    let highlight = popup_highlight(view);
    let popup = popup_rect(anchor_y, area, lines.len());

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(padded(&title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tui_color(theme.popup_border)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Scroll the highlighted row into view when the list overflows the box.
    let visible = inner.height as usize;
    let top = composer::first_visible_row(highlight, visible.max(1));
    let shown: Vec<Line> = lines.into_iter().skip(top).take(visible).collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

/// The most body rows the Slash popup shows before it scrolls internally - keeps
/// the overlay compact even against a long model list.
const POPUP_MAX_ROWS: u16 = 8;

/// Padding (columns) added to a command's character count to size the modal width.
const APPROVAL_MODAL_PADDING: u16 = 8;

/// The minimum guaranteed modal width in columns. Wide enough to read the
/// keybinding line (`[y]es / [n]o / [a]lways`).
const MODAL_MIN_WIDTH: u16 = 44;

/// The maximum height (rows) of the Approval modal including borders.
const APPROVAL_MODAL_HEIGHT: u16 = 8;

/// The minimum content width (columns) of the Session Picker popup, including its
/// horizontal padding (+4 for the two border columns plus two inner padding cols).
const PICKER_MIN_WIDTH_EXTRA: u16 = 4;

/// The header/footer row overhead added to entry count to size the Picker height
/// (borders top+bottom plus the key-hint footer row).
const PICKER_HEIGHT_OVERHEAD: u16 = 3;

/// The cost threshold below which `cost_label` emits the `<$0.01` floor label
/// instead of a two-decimal dollar amount.
const COST_SUB_CENT: f64 = 0.01;

/// The sentinel session cost below which the Cost segment is hidden entirely: a
/// session that spent nothing (or whose provider carries no Catalog pricing) shows
/// exactly the bar it always did.
const COST_HIDDEN: f64 = 0.0;

/// The milliseconds-per-second divisor used when converting `quiet_ticks` (each
/// tick is `TICK_MS` ms) into an elapsed-seconds figure for the lull timer.
const MILLIS_PER_SEC: u64 = 1_000;

/// The number of priority tiers in the status-bar segment drop policy.
const DROP_TIER_COUNT: usize = 6;

/// The total horizontal side margin (columns) reserved outside the Approval
/// modal: two columns each side so the modal never bleeds to the terminal edge.
const APPROVAL_MODAL_SIDE_MARGIN: u16 = 4;

/// One `Line` per [`SelectorRow`]: the label, then the hint dimmed (a note's
/// hint may carry the reveal cap's "· N more" count, merged upstream by the
/// Composer's overlay view); the highlighted row is reversed so it reads as
/// the cursor. The row's role picks the label treatment - a header or note
/// (a Provider group header, an "unavailable" note) draws dim bold, a
/// collapsed member draws dim WITHOUT bold so it reads as a greyed model
/// rather than a header. Only a cursor stop (a member or a note) ever draws
/// reversed: a highlighted note is the stop anchoring a greyed group's view,
/// while headers and collapsed rows can never hold the cursor.
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
            let label_style = match row.role {
                RowRole::Member => Style::default(),
                RowRole::Collapsed => Style::default().fg(tui_color(theme.muted)),
                RowRole::Header | RowRole::Note => Style::default()
                    .fg(tui_color(theme.muted))
                    .add_modifier(Modifier::BOLD),
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
            if i == highlight && row.is_stop() {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect()
}

/// Brings the [`RenderCache`] up to date with `screen`'s Transcript at
/// `content_width` (ADR-0046): the adapter's public door onto the cache's
/// (crate-private) sync, so [`commit_items`](crate::ui::commit_items) can sync
/// at the SAME content width the committed slice draws at (frame width minus
/// [`LANE_GUTTER`]) before measuring and blitting - keeping measure == draw
/// (ADR-0029). The [`Toggles`] mirror the Screen's Ctrl-T/Ctrl-O flags.
pub fn sync_commit_cache(
    cache: &mut RenderCache,
    screen: &Screen,
    content_width: u16,
    theme: &Theme,
) {
    cache.sync(
        screen.transcript(),
        Toggles {
            thinking_expanded: screen.thinking_expanded,
            tools_expanded: screen.tools_expanded,
        },
        content_width,
        theme,
    );
}

/// The total wrapped height (visual rows) the committed slice `[hw, hw + count)`
/// draws to at `width` (ADR-0046): the sum of the cached wrapped counts, so the
/// adapter can size the [`Buffer`] `insert_before` scrolls into scrollback. The
/// slice is drawn WHOLE - a tall commit overflows into native scrollback above
/// the inline viewport, never clamped. `width` is the content width the cache
/// was synced at; the committed content sits one [`LANE_GUTTER`] in, exactly
/// like the pending region.
pub fn commit_slice_height(cache: &RenderCache, hw: usize, count: usize) -> u16 {
    let total: usize = cache
        .settled()
        .skip(hw)
        .take(count)
        .map(|(_, wrapped)| wrapped)
        .sum();
    u16::try_from(total).unwrap_or(u16::MAX)
}

/// Blits the committed slice `[hw, hw + count)` of the cached settled items into
/// `buf` (ADR-0046, the inline `insert_before` seam): each item's cached content
/// [`Line`]s draw at successive `y` in the content columns, with its lane gutter
/// (the user `› ` caret / dim `│ ` spine) painted into the reserved
/// [`LANE_GUTTER`] columns per visual row - the SAME two-plane layout the
/// pending region uses ([`render_pending`]), so a committed item looks identical
/// once it freezes. No scroll math: the caller sizes `buf` to
/// [`commit_slice_height`], and a slice taller than the terminal scrolls whole
/// into native scrollback. The lane state is derived over the FULL `items` list
/// (a lane opened by a `User` item before `hw` still spines the committed tail),
/// then only the slice's rows are painted.
///
/// Committed items render in their FULL cached form (qwen's `<Static>` feed
/// prints history un-clamped): the collapsed-run fold and the overflow clip are
/// live-region affordances only.
/// The committed slice `[hw, hw + count)` to freeze into scrollback: the cache to
/// blit from, the FULL `items` list the lane gutter is derived over (a lane opened
/// before `hw` still spines the committed tail), and the ACTIVE `theme` the frozen
/// rows bake. Bundled so [`render_committed_slice`] takes a single source arg
/// beside its `buf` target instead of five positional params (SRP_PARAMS fix),
/// matching the [`PendingBodyParams`]/[`GutterCtx`] param-struct style.
pub struct CommittedSlice<'a> {
    pub cache: &'a RenderCache,
    pub items: &'a [TranscriptItem],
    pub hw: usize,
    pub count: usize,
    pub theme: &'a Theme,
}

pub fn render_committed_slice(buf: &mut Buffer, slice: &CommittedSlice<'_>) {
    let CommittedSlice {
        cache,
        items,
        hw,
        count,
        theme,
    } = *slice;
    let width = buf.area.width;
    let content_x = buf.area.x + LANE_GUTTER;
    let content_width = width.saturating_sub(LANE_GUTTER);

    // The lane gutter over ALL items, sliced to the committed range - so a lane
    // opened before `hw` correctly spines the committed tail. The lane styles are
    // computed ONCE, the SAME way the pending gutter derives them - so a frozen
    // item's caret/spine is identical to the live one's (ADR-0046).
    let lane = lane_gutters(items);
    let styles = LaneStyles::from_theme(theme);

    let mut y = buf.area.y;
    for (i, (lines, wrapped)) in cache.settled().enumerate().skip(hw).take(count) {
        // The content plane: cached lines drawn one visual row apart. Each line
        // was measured with `Wrap { trim: false }` at `content_width`, so it
        // occupies exactly `wrapped` rows and never re-wraps here (measure ==
        // draw, ADR-0029).
        let content_area = Rect {
            x: content_x,
            y,
            width: content_width,
            height: wrapped as u16,
        };
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .render(content_area, buf);

        // The gutter plane: expand this ONE item's lane kind over its wrapped
        // rows (caret on the first row of a User item, spine on every in-lane
        // row) and paint the reserved columns via the SAME cell→widget rule the
        // pending gutter uses ([`gutter_cell_widget`]/[`gutter_rect`]).
        let kind = lane.get(i).copied().unwrap_or(GutterKind::Blank);
        for row in 0..wrapped {
            let cell = row_gutter_for(kind, row);
            if let Some(widget) = gutter_cell_widget(cell, styles) {
                widget.render(gutter_rect(buf.area.x, y + row as u16), buf);
            }
        }

        y = y.saturating_add(wrapped as u16);
    }
}

/// The rolling reasoning tail shown while a Run streams: an animated
/// `✦ Thinking ⠋` header (the braille [`SPINNER`] advanced by the adapter's
/// tick - motion lives HERE at the brain, not the status bar, ADR-0040), then
/// the last [`THINKING_TAIL_ROWS`] VISUAL rows of the reasoning, indented two
/// columns under the header as a sub-block. Empty when nothing is streaming.
///
/// Bounded by VISUAL rows, not source rows: one long unwrapped reasoning line
/// soft-wraps to many rows, which would let the "short tail" (Decision A) grow
/// to fill the viewport. Each source row is truncated (with an `…` marker) to
/// the content width so it occupies exactly one visual row and the tail is a
/// hard `THINKING_TAIL_ROWS` cap - truncation, not re-wrapping, so this never
/// drifts from what the Paragraph paints (ADR-0029). `width` is the
/// `content_area` width the tail draws in.
///
/// Uncached on purpose: the tail's window is non-monotonic (older lines scroll
/// off as it grows), so the char-length key the settled streaming cache relies
/// on would not hold. A handful of `Line`s per frame is cheap.
fn live_thinking_lines(
    thinking: &str,
    spinner: u64,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if thinking.is_empty() {
        return vec![];
    }
    let header_style = Style::default()
        .fg(tui_color(theme.thinking_header))
        .add_modifier(Modifier::ITALIC);
    let row_style = Style::default()
        .fg(tui_color(theme.thinking))
        .add_modifier(Modifier::ITALIC);
    let frame = SPINNER[(spinner as usize) % SPINNER.len()];
    let mut out = vec![Line::styled(format!("✦ Thinking {frame}"), header_style)];
    // The tail rows indent two columns, so their text budget is the content
    // width less that indent (never below 1).
    let row_width = (width as usize).saturating_sub(2).max(1);
    let rows = text_rows(thinking);
    let tail = &rows[rows.len().saturating_sub(THINKING_TAIL_ROWS)..];
    out.extend(
        tail.iter()
            .map(|row| Line::styled(format!("  {}", truncate_visual(row, row_width)), row_style)),
    );
    out
}

/// Whether the lull "waiting" row should draw this frame: the Run is Running
/// and NEITHER live entry (the reasoning tail, the streaming answer) is on
/// screen. The one gate, matching [`Screen::has_live_stream`] by construction
/// (`thinking_empty == streaming_thinking().is_empty()` and `tail_present ==
/// !streaming_text().is_empty()`) so the row and the adapter's lull clock never
/// disagree. Pulled out of `render_pending_body` so the multi-clause boolean and
/// its emptiness branch stay off that function's cyclomatic complexity.
fn lull_visible(status: Status, thinking_empty: bool, tail_present: bool) -> bool {
    status == Status::Running && thinking_empty && !tail_present
}

/// The lull "waiting" row shown while a Run runs but nothing streams: an
/// elapsed timer (left, fixed-width so the animation column never jitters) then
/// the current [`lull`] scene frame, indented two columns under the running
/// lane like the reasoning tail. Empty until the lull passes the settle window
/// (so a brief token gap never flashes a scene) and empty whenever output is
/// streaming (the caller gates on that - see `render_pending_body`).
///
/// `width` is the `content_area` width this draws in (the same measured==drawn
/// width the rest of the viewport uses, ADR-0029). The row is truncated to that
/// width so it stays exactly one visual row and cannot desync the lane spine.
/// (Live entries are appended to the render window by
/// [`PendingStack::push_live`], which owns the emptiness branch.)
fn live_lull_lines(anim: Anim, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let Some(glyph) = lull::frame(anim.quiet_ticks, anim.lull_seq) else {
        return vec![];
    };
    // Ticks -> seconds via the adapter's tick cadence (the one place ticks
    // become real time, a display decision - the pure `lull` clock stays in
    // ticks). TICK_MS is the adapter's frame interval.
    let secs = anim.quiet_ticks.saturating_mul(crate::ui::TICK_MS) / MILLIS_PER_SEC;
    // A fixed-width timer field keeps the animation anchored as the label grows
    // ("7s" -> "2m 03s"). 7 cols holds up to "59m 59s"; longer just shifts.
    let timer = format!("{:<7}", lull::format_elapsed(secs));
    let style = Style::default()
        .fg(tui_color(theme.lull))
        .add_modifier(Modifier::ITALIC);
    // Two-column indent (like the reasoning tail's sub-block), then timer, a
    // gap, and the scene. Truncated as a whole to one visual row.
    let text = format!("  {timer} {glyph}");
    let budget = width as usize;
    vec![Line::styled(truncate_visual(&text, budget), style)]
}

/// Truncates `text` to at most `width` display columns, replacing the trimmed
/// tail with a single `…` so an over-long reasoning line stays one visual row.
/// Char-based (like the rest of this module) - a truncated row is always `<=
/// width` chars, so the viewport's `Wrap` never breaks it onto a second row.
fn truncate_visual(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

/// Greedy word-wrap of `text` into segments each at most `width` chars, char
/// based (consistent with `truncate_visual`; no `unicode-width`, so the caller's
/// glyphs must be width-1 - the machinery/marker text is). Words are broken on
/// ASCII spaces; a single word longer than `width` is HARD-SPLIT across rows so
/// no segment ever exceeds `width` (the invariant `indented_lines` relies on to
/// keep measure==draw). A `width` of 0 is treated as 1. An empty input yields
/// one empty segment so a blank line survives as a blank row.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize;
    for word in text.split(' ') {
        let mut word = word;
        // Hard-split a word wider than the whole line before it ever tries to
        // sit on one: peel `width`-char chunks until the remainder fits.
        while word.chars().count() > width {
            if line_len > 0 {
                out.push(std::mem::take(&mut line));
                line_len = 0;
            }
            let head: String = word.chars().take(width).collect();
            let consumed = head.len();
            out.push(head);
            word = &word[consumed..];
        }
        let wlen = word.chars().count();
        // +1 for the space that would join this word to the current line.
        let needed = if line_len == 0 {
            wlen
        } else {
            line_len + 1 + wlen
        };
        if needed > width && line_len > 0 {
            out.push(std::mem::take(&mut line));
            line_len = 0;
        }
        if line_len > 0 {
            line.push(' ');
            line_len += 1;
        }
        line.push_str(word);
        line_len += wlen;
    }
    out.push(line);
    out
}

/// Renders `content` as styled lines that hang at `indent` columns: the content
/// is word-wrapped to `content_width - indent` and EVERY resulting visual row
/// (the first and every continuation) is prefixed with `indent` spaces. This
/// gives a block indent that ratatui's own `Wrap` cannot (it has no hanging
/// indent), and because each produced Line is `<= content_width` chars the
/// viewport never re-wraps it - so `wrapped_count` equals the rendered rows
/// (measure==draw, ADR-0029). Used by the indented machinery/marker arms.
fn indented_lines(
    content: &str,
    indent: usize,
    content_width: u16,
    style: Style,
) -> Vec<Line<'static>> {
    let inner = (content_width as usize).saturating_sub(indent).max(1);
    let pad = " ".repeat(indent);
    wrap_words(content, inner)
        .into_iter()
        .map(|seg| Line::styled(format!("{pad}{seg}"), style))
        .collect()
}

/// The reserved left-gutter width (columns): the run-lane spine / user caret
/// plane (ADR-0040). Two columns - a glyph and a trailing space - so content
/// sits one clear column off the spine. Carved unconditionally off the text
/// area so the content wrap width never depends on lane membership.
/// `pub(crate)` so the adapter can size the committed-slice content width
/// (ADR-0046) at the same gutter reservation the pending region uses.
pub(crate) const LANE_GUTTER: u16 = 2;

/// What the reserved left gutter draws beside one transcript item's rows
/// (ADR-0040). Derived at render time from the item sequence, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GutterKind {
    /// Before the first `User` item - the opening greeting/notices sit at the
    /// margin with no spine.
    Blank,
    /// A `User` prompt: the `› ` caret breaks to the margin on the item's first
    /// visual row, blank on any wrapped continuation.
    User,
    /// Everything the agent emits inside a Run: the dim `│ ` spine on every
    /// visual row, so the whole run reads as one object.
    Spine,
}

/// Derives the per-item lane gutter for the settled items, in order (ADR-0040):
/// a `User` item opens a lane and every item after it hangs off that lane until
/// the next `User`; the region before the first `User` is spineless. The lane is
/// the user REQUEST, not the Run - harness-produced work injects no `User` item,
/// so it correctly stays on the prior request's spine. Pure over the item
/// sequence, so it is asserted without a frame; the two live entries (reasoning
/// tail, streaming answer) are appended as `Spine` by the caller.
fn lane_gutters(items: &[TranscriptItem]) -> Vec<GutterKind> {
    let mut in_lane = false;
    items
        .iter()
        .map(|item| match item {
            TranscriptItem::User { .. } => {
                in_lane = true;
                GutterKind::User
            }
            _ if in_lane => GutterKind::Spine,
            _ => GutterKind::Blank,
        })
        .collect()
}

/// What one VISUAL content row draws in the reserved gutter (ADR-0040): the
/// user's `› ` caret, the dim `│ ` lane spine, or nothing. This is the flat
/// per-row mapping [`PendingStack::expand_gutters`] produces and both the content
/// and the gutter index by, so they can never desync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowGutter {
    /// A `User` prompt's first visual row - the caret at the margin.
    Caret,
    /// An in-lane row - the dim spine.
    Spine,
    /// A user continuation, a pre-lane row, or an item's trailing separator row.
    Blank,
}

impl RowGutter {
    /// The glyph this row paints into the [`LANE_GUTTER`] columns.
    fn glyph(self) -> &'static str {
        match self {
            RowGutter::Caret => "› ",
            RowGutter::Spine => "│ ",
            RowGutter::Blank => "  ",
        }
    }
}

/// The [`RowGutter`] one visual `row` of an item with lane `kind` draws: a
/// `User` item's caret shows only on row 0, a `Spine` item spines every row, a
/// `Blank` item never paints. The ONE lane-kind → row-glyph rule, shared by the
/// pending render's [`PendingStack::expand_gutters`] and the committed slice's
/// blit ([`render_committed_slice`]) so the two can never disagree.
fn row_gutter_for(kind: GutterKind, row: usize) -> RowGutter {
    match kind {
        GutterKind::User if row == 0 => RowGutter::Caret,
        GutterKind::User => RowGutter::Blank,
        GutterKind::Spine => RowGutter::Spine,
        GutterKind::Blank => RowGutter::Blank,
    }
}

/// One entry in the [`PendingStack`]: a borrowed run of content [`Line`]s, the
/// number of VISUAL rows they occupy once wrapped (`wrapped`), and the lane
/// [`GutterKind`] that paints their reserved gutter columns. Bundling the three
/// as ONE row makes the index-alignment invariant (the N-th lines, the N-th
/// count, and the N-th gutter always describe the same item) a TYPE property
/// rather than three parallel `Vec`s a caller could push out of step.
struct PendingRow<'a> {
    lines: &'a [Line<'static>],
    wrapped: usize,
    gutter: GutterKind,
}

/// The ordered stack of [`PendingRow`]s the inline pending body draws (ADR-0046):
/// the uncommitted settled tail plus the live entries (reasoning tail, streaming
/// answer, lull row), top to bottom. Replaces the three lockstep
/// `item_lines`/`counts`/`gutters` `Vec`s that used to be threaded together.
struct PendingStack<'a> {
    rows: Vec<PendingRow<'a>>,
}

impl<'a> PendingStack<'a> {
    /// Seats a precomputed run of [`PendingRow`]s as the initial stack (the
    /// settled tail from [`pending_tail_rows`]); live entries are pushed after.
    fn from_rows(rows: Vec<PendingRow<'a>>) -> Self {
        Self { rows }
    }

    /// Appends one row. The three facts travel together, so they can never
    /// desync.
    fn push(&mut self, lines: &'a [Line<'static>], wrapped: usize, gutter: GutterKind) {
        self.rows.push(PendingRow {
            lines,
            wrapped,
            gutter,
        });
    }

    /// Appends a LIVE entry (a reasoning tail, a streaming answer, or the lull
    /// row) as a single lane-spine row - but only when it carries lines. The
    /// emptiness branch lives HERE so the caller does not repeat it per entry.
    /// `width` is the content width the entry is measured at.
    fn push_live(&mut self, lines: &'a [Line<'static>], width: u16) {
        if lines.is_empty() {
            return;
        }
        let wrapped = wrapped_count(lines.to_vec(), width);
        self.push(lines, wrapped, GutterKind::Spine);
    }

    /// The total VISUAL rows the whole stack occupies (before any clip).
    fn total_lines(&self) -> usize {
        self.rows.iter().map(|r| r.wrapped).sum()
    }

    /// Every content [`Line`] in order, flattened - the Paragraph the body
    /// scrolls and clips.
    fn flat_lines(&self) -> Vec<Line<'static>> {
        self.rows
            .iter()
            .flat_map(|r| r.lines.iter().cloned())
            .collect()
    }

    /// The per-VISUAL-row [`RowGutter`] mapping the gutter paints, expanded from
    /// each row's `(gutter, wrapped)` in Paragraph layout order - the single
    /// mapping the content and the gutter share (M3), so they can never desync.
    fn expand_gutters(&self) -> Vec<RowGutter> {
        let mut out = Vec::with_capacity(self.total_lines());
        for r in &self.rows {
            for row in 0..r.wrapped {
                out.push(row_gutter_for(r.gutter, row));
            }
        }
        out
    }
}

/// Assembles the uncommitted settled tail of the transcript into a
/// [`PendingStack`] (ADR-0046): skips the committed items `[0, hw)` (already
/// frozen into scrollback) and takes each remaining item's cached lines VERBATIM,
/// the SAME cache slice [`render_committed_slice`] blits, so committed and
/// pending render identically (no collapsed-run fold, no machinery window; the
/// only overflow reduction is the caller's bottom-anchor + top-clip). The lane
/// is computed over the FULL item sequence so a lane opened before `hw` keeps
/// its spine over the pending tail; only the emitted rows start at `hw`.
fn assemble_pending<'a>(
    cache: &'a RenderCache,
    lane: &[GutterKind],
    hw: usize,
) -> PendingStack<'a> {
    // Integration (IOSP): compute the tail rows in the operation below, then only
    // seat them in a fresh stack here. No control flow of its own.
    PendingStack::from_rows(pending_tail_rows(cache, lane, hw))
}

/// Operation (IOSP): the uncommitted settled tail as [`PendingRow`]s - the cached
/// items `[hw..]`, each paired with its lane [`GutterKind`]. Pure over the cache
/// and lane (no side effects, no I/O), so [`assemble_pending`] stays a straight
/// orchestration. The `[0, hw)` prefix is already frozen into scrollback, so it
/// is skipped.
fn pending_tail_rows<'a>(
    cache: &'a RenderCache,
    lane: &[GutterKind],
    hw: usize,
) -> Vec<PendingRow<'a>> {
    cache
        .settled()
        .enumerate()
        .skip(hw)
        .map(|(i, (lines, wrapped))| PendingRow {
            lines,
            wrapped,
            gutter: lane[i],
        })
        .collect()
}

/// The collapsed one-line thought (the fold header and the settled collapsed
/// form): `✦ thought: …` truncated to a single VISUAL row at the content
/// `width` so a long paragraph never wraps to fill the viewport.
fn collapsed_thought_line(text: &str, width: u16, theme: &Theme) -> Line<'static> {
    const PREFIX: &str = "✦ thought: ";
    let style = thinking_style(theme);
    let budget = (width as usize)
        .saturating_sub(PREFIX.chars().count())
        .max(1);
    Line::styled(
        format!("{PREFIX}{}", truncate_visual(first_line(text), budget)),
        style,
    )
}

/// The dim italic style settled Thinking (and its live tail) draws in.
fn thinking_style(theme: &Theme) -> Style {
    Style::default()
        .fg(tui_color(theme.thinking))
        .add_modifier(Modifier::ITALIC)
}

/// A settled Thinking item's lines: collapsed (default) is the one-line
/// [`collapsed_thought_line`]; expanded (Ctrl-T) is the `✦ thought:` header then
/// the full text, all dim italic. Split out of `message_lines` so its toggle
/// branch does not inflate that fold's complexity (the `✦` family unifies with
/// the live tail's header; `✦` is width-1, unlike the width-2 `🧠` that shifted
/// the spine in real terminals).
fn settled_thinking_lines(
    text: &str,
    thinking_expanded: bool,
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if !thinking_expanded {
        return vec![collapsed_thought_line(text, content_width, theme)];
    }
    let style = thinking_style(theme);
    let mut out = vec![Line::styled("✦ thought:", style)];
    out.extend(
        text_rows(text)
            .into_iter()
            .map(|row| Line::styled(row, style)),
    );
    out
}

/// The gutter paint parameters: the precomputed per-row gutter mapping, the
/// window position (`top`, `height`), and the frame area the gutter occupies.
/// Bundled so [`paint_gutter`] takes a single context arg instead of four
/// positional params (SRP_PARAMS fix).
struct GutterCtx<'a> {
    row_gutters: &'a [RowGutter],
    top: usize,
    height: usize,
}

/// The two lane-gutter paint styles, computed ONCE from the theme: the user
/// prompt caret (bold `prompt_gutter`) and the dim lane spine (`lane_spine`). A
/// newtype so the committed slice and the pending gutter derive them the same
/// way - "committed looks identical to pending once frozen" is then a code
/// guarantee, not two copies of the same `Style::default().fg(...)` chain that
/// could drift.
#[derive(Debug, Clone, Copy)]
struct LaneStyles {
    caret: Style,
    spine: Style,
}

impl LaneStyles {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            caret: Style::default()
                .fg(tui_color(theme.prompt_gutter))
                .add_modifier(Modifier::BOLD),
            spine: Style::default().fg(tui_color(theme.lane_spine)),
        }
    }

    /// The paint style for one gutter cell: `Some` for Caret/Spine, `None` for
    /// Blank (the reserved columns stay clear - nothing to paint).
    fn cell_style(&self, cell: RowGutter) -> Option<Style> {
        match cell {
            RowGutter::Blank => None,
            RowGutter::Caret => Some(self.caret),
            RowGutter::Spine => Some(self.spine),
        }
    }
}

/// The 1-row × [`LANE_GUTTER`]-wide rect one gutter cell paints into, at column
/// `x` and row `y`. One place for the BP-009 `Rect` boilerplate both gutter
/// painters (the pending frame and the committed-slice blit) reuse.
fn gutter_rect(x: u16, y: u16) -> Rect {
    Rect {
        x,
        y,
        width: LANE_GUTTER,
        height: 1,
    }
}

/// One gutter cell as a styled [`Paragraph`], or `None` when the cell is Blank.
/// The single glyph+style→widget rule the committed slice and the pending gutter
/// share, so a painted spine is byte-identical in both.
fn gutter_cell_widget(cell: RowGutter, styles: LaneStyles) -> Option<Paragraph<'static>> {
    styles
        .cell_style(cell)
        .map(|style| Paragraph::new(Line::styled(cell.glyph(), style)))
}

/// Paints the reserved left gutter per VISUAL row over the visible window: the
/// user caret in the prompt color, the lane spine in the dim `lane_spine` slot.
/// Consumes the flat [`RowGutter`] mapping the content shares, sliced by the
/// absolute `top` offset - the SAME slice the content Paragraph scrolls to - so
/// a gutter glyph lands on exactly the row its item occupies at any scroll
/// position, soft-wrapped continuations included (M3). Draws nothing outside the
/// item rows (a short transcript leaves the lower gutter clear).
fn paint_gutter(frame: &mut Frame, text_area: Rect, ctx: &GutterCtx<'_>, theme: &Theme) {
    let styles = LaneStyles::from_theme(theme);
    for (screen_row, cell) in ctx
        .row_gutters
        .iter()
        .skip(ctx.top)
        .take(ctx.height)
        .enumerate()
    {
        if let Some(widget) = gutter_cell_widget(*cell, styles) {
            let y = text_area.y + screen_row as u16;
            frame.render_widget(widget, gutter_rect(text_area.x, y));
        }
    }
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
pub(crate) struct Toggles {
    pub(crate) thinking_expanded: bool,
    pub(crate) tools_expanded: bool,
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

    /// Per-item render state for the inline pending body and the committed-slice
    /// blit (ADR-0046), owned by the adapter's run loop and threaded through
    /// [`super::render_pending`] and [`super::render_committed_slice`]. Holds
    /// ratatui [`Line`]s, so it lives HERE, not in the pure modules (ADR-0019).
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
    /// cache's width - the numbers the pending body does its
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
                let lines = message_lines(
                    item,
                    toggles.thinking_expanded,
                    toggles.tools_expanded,
                    width,
                    theme,
                );
                // No per-item blank separator: the lane stays DENSE with one
                // continuous spine (a blank row breaks the spine into segments
                // and burns vertical real estate - the two-planes coloring, not
                // whitespace, separates the run's parts).
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

        /// Syncs `t` into a fresh cache at width 80 + dark theme, then plants a
        /// sentinel line at items[0].lines[0]. The sentinel survives extend-only
        /// syncs and disappears on a full rebuild, so tests can assert which path
        /// the cache took without reading private revision counters (DUPLICATE fix).
        fn seeded_cache(t: &Transcript) -> RenderCache {
            let mut cache = RenderCache::new();
            cache.sync(t, Toggles::default(), 80, theme::dark());
            // A named constant makes the "sentinel survives / disappears" intent
            // explicit at the assertion sites and adds a 4th statement so this
            // helper does not trigger the FRAGMENT quality gate.
            let sentinel = Line::raw("sentinel");
            cache.items[0].lines[0] = sentinel;
            cache
        }

        #[test]
        fn cache_sync_extends_for_appends_without_rebuilding_settled_entries() {
            let mut t = fresh_transcript();
            t.info("first");
            // Plant a sentinel in the built entry: an append extends the cache
            // without touching settled entries, so the sentinel must survive
            // the next sync - a rebuild would have replaced it with "first".
            let mut cache = seeded_cache(&t);
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
            // The delivered steering removes its pending marker - a structural
            // edit that bumps the store's revision - so the cache rebuilds
            // from scratch: the sentinel is gone and the promoted user line is
            // seen. The `› ` caret now lives in the reserved lane gutter
            // (ADR-0040), so the cached User line is the bare prompt text.
            let mut cache = seeded_cache(&t);
            t.steering_delivered("check");
            cache.sync(&t, Toggles::default(), 80, theme::dark());
            assert_eq!(cache.items.len(), 1);
            assert_eq!(line_text(&cache.items[0].lines[0]), "check");
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

/// The backgrounded "machinery" style for tool-call lines: dim DarkGray, NOT
/// italic (italic stays reserved for Thinking/Info so those remain
/// distinguishable). Paired with a two-space indent + "⋯" gutter, it makes
/// tool machinery recede so the conversation owns the foreground.
fn machinery_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.machinery))
}

/// The lines one Transcript item renders as. `Diff` is the first-class rich item
/// of the semantic display vocabulary (ADR-0008): a titled diff whose lines take
/// a semantic tint from their [`DiffSide`]'s Theme slots and a syntect foreground.
/// `thinking_expanded` (Ctrl-T, the core's `Transcript::thinking_expanded`)
/// picks the collapsed one-liner or the full text for settled `Thinking` items;
/// `tools_expanded` (Ctrl-O, the core's `Transcript::tools_expanded`) does the
/// same for a multi-line `Diff` body - the same detail-on-demand rule applied to
/// the machinery plane. `content_width`
/// is the `content_area` width the lines draw in - the collapsed Thinking
/// one-liner truncates to it so it stays one visual row (a long newline-free
/// thought otherwise soft-wraps to many).
fn message_lines(
    item: &TranscriptItem,
    thinking_expanded: bool,
    tools_expanded: bool,
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // Detail-on-demand collapse (Ctrl-O), keyed on the SEMANTIC fold predicate
    // (Stage 2 review C2 / S1): any item with a foldable body collapses to its
    // `fold_title` one-liner, so the fold rule is NOT gated inside a per-variant
    // match arm - a future non-Diff foldable item folds the same way. The
    // affordance is a fixed `· ^O expand`, NOT a line count: a Diff's title
    // already carries its `(+A −R)` magnitude, and the body is display-capped
    // upstream, so a raw line count would misreport what was elided.
    if !tools_expanded
        && item.has_foldable_body()
        && let Some(title) = item.fold_title()
    {
        return vec![Line::styled(
            format!("  ⋯ {title} · ^O expand"),
            machinery_style(theme),
        )];
    }

    match item {
        // User prompts: bare rows at the content margin. The `› ` caret (first
        // visual row) and continuation blanks now live in the RESERVED lane
        // gutter (ADR-0040 - the user's voice breaks the spine to the margin),
        // painted per-visual-row by `paint_gutter`, so `message_lines` no longer
        // prepends a gutter of its own. Multi-line input renders as many rows.
        TranscriptItem::User { text } => text_rows(text).into_iter().map(Line::from).collect(),
        // Assistant text is markdown: the pure ui::markdown fold produces
        // semantic lines and [`md_style`] runs them into colors here.
        // Width-wrapping is left to the viewport Paragraph's Wrap.
        TranscriptItem::Assistant { text } => markdown_lines(text, theme),
        // Settled Thinking: collapsed is the one-line form; expanded (Ctrl-T)
        // is a header row then the full text. Delegated so the toggle branch
        // does not add to this fold's complexity.
        TranscriptItem::Thinking { text } => {
            settled_thinking_lines(text, thinking_expanded, content_width, theme)
        }
        // Tool-call machinery recedes into a dim, indented background gutter so
        // the conversation (assistant prose, user text) owns the foreground:
        // DarkGray (not italic - italic stays reserved for Thinking/Info), a
        // two-space block indent (wrapped continuations stay at column 2 -
        // [`indented_lines`]), and a quiet "⋯" glyph in place of the loud "⚙".
        TranscriptItem::ToolCall { name, summary, .. } => indented_lines(
            &format!("⋯ {}", join_summary(name, summary)),
            2,
            content_width,
            machinery_style(theme),
        ),
        // A merged one-liner (Stage 3): a paired call+result reads
        // `⋯ name  <key_arg> · <result>`; an unpaired result (no live call, so
        // no arg) keeps the older `⋯ name → result` shape.
        TranscriptItem::ToolResult {
            name,
            summary,
            is_error: false,
            key_arg,
        } => indented_lines(
            &format!("⋯ {}", join_merged(name, key_arg.as_deref(), summary)),
            2,
            content_width,
            machinery_style(theme),
        ),
        // Errors are the exception that belongs in the foreground: they keep
        // red + bold and the ⚙ gutter, share the two-space indent, and ALWAYS
        // carry a `✗` failed-marker so they can't be missed (the two-planes
        // design leans on this - red+bold alone is weaker for scanning and
        // colorblind users). The merged `key_arg` is kept so the failing
        // path/command stays visible. The one exception: when the `summary`
        // already begins with a status glyph - a extension badge like `✗ exit 1`
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
            indented_lines(
                &format!("⚙ {} {glyph}{summary}", join_arg(name, key_arg.as_deref())),
                2,
                content_width,
                Style::default()
                    .fg(tui_color(theme.error))
                    .add_modifier(Modifier::BOLD),
            )
        }
        // A foldable Diff reaches here only EXPANDED (Ctrl-O on) or when it has
        // no foldable body (empty) - the collapse is handled once at the top of
        // this fn. Expanded: the title, then each hunk's header and its code
        // lines as a full-width added/removed tint band with the syntect
        // foreground layered over it (ADR-0008), indented under the gutter.
        TranscriptItem::Diff {
            title,
            lang,
            hunks,
            elided,
        } => {
            let mut out = diff_lines(title, lang.as_deref(), hunks, content_width, theme);
            out.extend(diff_elided_tail(*elided, content_width, theme));
            out
        }
        // The quiet plane: adapter Info news and the tinted harness marker
        // plane (ADR-0040) share one shape - italic text rows. Info wears the
        // muted color; a Marker tints by TONE alone (never by text, the glyph
        // and wording were authored upstream). One arm, so the fold rule for
        // "a plain italic line" lives in one place.
        // Adapter Info news sits flush at the margin (the greeting, notices).
        TranscriptItem::Info { text } => {
            let style = marker_style(item, theme).add_modifier(Modifier::ITALIC);
            text_rows(text)
                .into_iter()
                .map(|row| Line::styled(row, style))
                .collect()
        }
        // A harness Marker (governing/housekeeping) indents two columns under the
        // thought header, like the tool work - it is part of the run's body, not
        // a foreground line. Wrapped continuations stay at column 2 too
        // ([`indented_lines`]). Tinted by TONE alone (the glyph/wording are
        // authored upstream, never sniffed here).
        TranscriptItem::Marker { text, .. } => {
            let style = marker_style(item, theme).add_modifier(Modifier::ITALIC);
            text_rows(text)
                .into_iter()
                .flat_map(|row| indented_lines(&row, 2, content_width, style))
                .collect()
        }
    }
}

/// The color an Info or Marker line draws in (ADR-0040): a Marker reads its
/// [`Tone`]'s own Theme slot (Steering the prompt gutter, Plain the muted
/// fallback); an Info line is always muted. Tone alone decides the tint, never
/// the text.
fn marker_style(item: &TranscriptItem, theme: &Theme) -> Style {
    let color = match item {
        TranscriptItem::Marker {
            tone: Tone::Housekeeping,
            ..
        } => theme.marker_housekeeping,
        TranscriptItem::Marker {
            tone: Tone::Aid, ..
        } => theme.marker_aid,
        TranscriptItem::Marker {
            tone: Tone::Constrain,
            ..
        } => theme.marker_constrain,
        TranscriptItem::Marker {
            tone: Tone::Steering,
            ..
        } => theme.prompt_gutter,
        // A Plain marker and an Info line both read muted.
        _ => theme.muted,
    };
    Style::default().fg(tui_color(color))
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

/// The inset prefix a bare code block indents under (ADR-0040 Decision E): two
/// columns, wearing the code background so the block reads as one solid inset
/// surface rather than a boxed one.
const CODE_INSET: &str = "  ";

/// Renders assistant markdown into ratatui lines: one `Line` per [`MdLine`],
/// each span styled by the single [`md_style`] mapping; an empty MdLine (block
/// separation) becomes a blank row. Consecutive code lines sharing a non-empty
/// `code_lang` render as one bare, inset code block (a blank row above/below,
/// each row inset under [`CODE_INSET`], no box or gutter): [`highlight_code`]
/// gives syntect fg over OUR code background; blocks with no/unknown language
/// fall back to the plain CodeBlock style, still inset.
fn markdown_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let md_lines = markdown::to_lines(text);
    let mut out = Vec::with_capacity(md_lines.len());
    let mut i = 0;
    while i < md_lines.len() {
        // Prose (`code_lang == None`) takes the per-line plain path; ANY fenced
        // code - including a bare ``` fence (`Some("")`, which local models emit
        // constantly) - enters the inset code-block branch below. An empty lang
        // simply won't resolve a syntax, so it falls to the plain-but-inset
        // fallback inside the branch, framed like every other code block.
        let lang = match md_lines[i].code_lang.as_deref() {
            Some(lang) => lang.to_string(),
            None => {
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
        // Bare, inset code block (ADR-0040 Decision E): a blank row above and
        // below frames the block, and each code row insets under
        // [`CODE_INSET`]; no box, no line-number gutter - the syntect fg over
        // our code bg carries it. The inset prefix wears the code bg so the
        // block reads as one solid surface.
        let code_bg = tui_color(theme.code_block_bg);
        let inset = || Span::styled(CODE_INSET, Style::default().bg(code_bg));
        out.push(Line::default());
        match highlight_code(&refs, &lang, &theme.syntax) {
            Some(highlighted) => {
                for (fragments, text) in highlighted.into_iter().zip(&texts) {
                    if fragments.is_empty() {
                        // Blank (or all-whitespace) code line: keep the same
                        // bg treatment the plain path gives it, still inset.
                        out.push(Line::from(vec![
                            inset(),
                            Span::styled(text.clone(), md_style(MdStyle::CodeBlock, theme)),
                        ]));
                    } else {
                        let mut spans = vec![inset()];
                        spans.extend(fragments.into_iter().map(|((r, g, b), text)| {
                            Span::styled(text, Style::default().fg(Color::Rgb(r, g, b)).bg(code_bg))
                        }));
                        out.push(Line::from(spans));
                    }
                }
            }
            // Unknown language: the plain CodeBlock rendering, still inset.
            None => out.extend(block.iter().map(|line| {
                let mut spans = vec![inset()];
                spans.extend(
                    line.spans
                        .iter()
                        .map(|span| Span::styled(span.text.clone(), md_style(span.style, theme))),
                );
                Line::from(spans)
            })),
        }
        out.push(Line::default());
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

/// Normalizes a diff line's raw code text for display: tabs become two spaces
/// (consistent with [`text_rows`]); an empty line stays empty (the tint band
/// fills it visibly, so no space-padding trick is needed as it was for a plain
/// [`Line`]).
fn normalize_diff_text(text: &str) -> String {
    text.replace('\t', "  ")
}

// ---------------------------------------------------------------------------
// Diff rendering (ADR-0008): the first-class `Diff` item's two color sources
// stay split - the SEMANTIC tag (added/removed/context) becomes a full-width
// background TINT from the Theme's slots, and the LEXICAL syntect foreground
// layers over it. The `+`/`-`/context marker glyph is added here, never baked
// into the core's text. The same syntect machinery highlights markdown fences.
// ---------------------------------------------------------------------------

/// The two-column indent a diff hangs under, matching the tool machinery plane:
/// the tint band starts AFTER this gutter, so the run-lane spine and this indent
/// stay untinted and the band reads as GitHub's content-area stripe.
const DIFF_INDENT: usize = 2;

/// The marker glyph a diff line's [`DiffSide`] draws (ADR-0008): the adapter
/// adds it, so the change still reads on a non-truecolor terminal and when the
/// tint is subtle. Two cells wide, so the code text aligns across the sides.
fn diff_marker(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Added => "+ ",
        DiffSide::Removed => "- ",
        DiffSide::Context => "  ",
    }
}

/// The background tint a diff line's [`DiffSide`] paints (ADR-0008): added and
/// removed read their Theme `*_bg` slots; context is untinted. The tint is the
/// SEMANTIC meaning; the syntect fg layers over it.
fn diff_tint(side: DiffSide, theme: &Theme) -> Option<Color> {
    match side {
        DiffSide::Added => Some(tui_color(theme.added_bg)),
        DiffSide::Removed => Some(tui_color(theme.removed_bg)),
        DiffSide::Context => None,
    }
}

/// Renders a first-class `Diff` item (ADR-0008) into ratatui lines: the title,
/// then each hunk's optional `@@ … @@` header (muted italic, no marker or tint)
/// and its tagged code lines as a full-width tint band with the marker glyph and
/// the syntect foreground, then the muted `… N more lines` tail from
/// [`diff_elided_tail`] (the caller appends it, so this stays integration-only).
///
/// Each produced [`Line`] is truncated to `content_width` so the viewport's
/// `Wrap` never re-breaks it - `wrapped_count` then equals the drawn rows
/// (measure==draw, ADR-0029). The tint is a FULL-WIDTH band: every code row is
/// padded to `content_width` with a bg-filled span, so the stripe reaches the
/// right edge like GitHub's.
fn diff_lines(
    title: &str,
    lang: Option<&str>,
    hunks: &[DiffHunk],
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = content_width as usize;
    let mut out = vec![Line::styled(
        truncate_cols(&format!("  ⋯ {title}"), width),
        machinery_style(theme),
    )];
    for hunk in hunks {
        out.extend(diff_hunk_lines(hunk, lang, width, theme));
    }
    out
}

/// One hunk's rows: its optional `@@ … @@` header (muted-italic chrome, no
/// marker or tint) followed by its tinted, highlighted code lines
/// ([`hunk_code_lines`]).
fn diff_hunk_lines(
    hunk: &DiffHunk,
    lang: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = hunk
        .header
        .iter()
        .map(|header| {
            Line::styled(
                truncate_cols(&format!("  {header}"), width),
                diff_chrome_style(theme),
            )
        })
        .collect();
    out.extend(hunk_code_lines(hunk, lang, width, theme));
    out
}

/// The muted `… N more lines` tail a display-capped diff ends with, or nothing
/// when the cap elided nothing (`elided == 0`). Kept out of [`diff_lines`] so
/// that function stays a pure sequence of extends (IOSP integration-only).
fn diff_elided_tail(elided: usize, content_width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if elided == 0 {
        return Vec::new();
    }
    vec![Line::styled(
        truncate_cols(&format!("  … {elided} more lines"), content_width as usize),
        diff_chrome_style(theme),
    )]
}

/// One hunk's code lines, syntect-highlighted two-pass so multi-line constructs
/// (a block comment, a raw string) color coherently across ALL their lines
/// (ADR-0008 recorded decision). The AFTER-image (context + added, in order) is
/// highlighted as ONE slice so syntect parse state carries; the BEFORE-image
/// (context + removed, in order) as another. A context line draws from the after
/// pass and advances both cursors; an added line draws from after; a removed
/// line from before - so a created file (one all-added hunk = the whole file)
/// colors its `/** … */` JSDoc as a comment across every line, not just line 1.
///
/// KNOWN LIMITATION (inherent to any before/after two-pass scheme): a multi-line
/// construct a single hunk STRADDLES via a removed opener and an added closer
/// (e.g. `/*` removed, `*/` added) can't color coherently - the two lines live
/// in different images. The common cases (whole created files, comments that
/// survive an edit as context) are coherent; a straddling rewrite is not.
fn hunk_code_lines(
    hunk: &DiffHunk,
    lang: Option<&str>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // Normalize each line's text ONCE, in file order, and reuse it for both the
    // image the highlighter sees and the row the renderer draws.
    let texts: Vec<String> = hunk
        .lines
        .iter()
        .map(|l| normalize_diff_text(&l.text))
        .collect();

    // The two images, in file order: added/context feed the after pass, and
    // removed/context the before pass, so syntect parse state carries per side.
    let image = |keep: fn(DiffSide) -> bool| -> Vec<&str> {
        hunk.lines
            .iter()
            .zip(&texts)
            .filter(|(l, _)| keep(l.side))
            .map(|(_, t)| t.as_str())
            .collect()
    };
    // Highlight each image as one slice (parse state carries) when a language
    // resolves; `None` (unknown/absent language) falls back to no fg fragments.
    let highlight =
        |refs: Vec<&str>| lang.and_then(|lang| highlight_code(&refs, lang, &theme.syntax));
    let after_fg = highlight(image(|s| matches!(s, DiffSide::Added | DiffSide::Context)));
    let before_fg = highlight(image(|s| {
        matches!(s, DiffSide::Removed | DiffSide::Context)
    }));

    let mut out = Vec::with_capacity(hunk.lines.len());
    let mut after_i = 0;
    let mut before_i = 0;
    for (line, text) in hunk.lines.iter().zip(&texts) {
        // Each line draws its fragments from the image it belongs to; a context
        // line draws from the after pass and advances BOTH cursors so the two
        // passes stay aligned to file order. Exhaustive over the three sides.
        let fragments = match line.side {
            DiffSide::Removed => {
                let fg = before_fg.as_ref().and_then(|f| f.get(before_i)).cloned();
                before_i += 1;
                fg
            }
            DiffSide::Added => {
                let fg = after_fg.as_ref().and_then(|f| f.get(after_i)).cloned();
                after_i += 1;
                fg
            }
            DiffSide::Context => {
                let fg = after_fg.as_ref().and_then(|f| f.get(after_i)).cloned();
                after_i += 1;
                before_i += 1;
                fg
            }
        };
        out.push(diff_code_row(line.side, text, fragments, width, theme));
    }
    out
}

/// One diff code row as a full-width tint band: the untinted [`DIFF_INDENT`]
/// gutter, then the marker glyph (semantic fg - added green, removed red, so the
/// change reads without truecolor) and the code (syntect fg when highlighted,
/// else the semantic fg), all over the side's background tint, padded to `width`
/// so the band reaches the right edge. Widths are DISPLAY COLUMNS (a wide CJK or
/// emoji glyph counts 2), so the row occupies exactly `width` columns and the
/// viewport's `Wrap` never re-breaks it - measure==draw, and the tint band never
/// shatters across rows (ADR-0029).
fn diff_code_row(
    side: DiffSide,
    text: &str,
    fragments: Option<Vec<CodeFragment>>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let tint = diff_tint(side, theme);
    let semantic = Style::default().fg(diff_side_fg(side, theme));
    let band = |mut s: Style| {
        if let Some(bg) = tint {
            s = s.bg(bg);
        }
        s
    };

    let indent = DIFF_INDENT.min(width);
    let mut spans: Vec<Span<'static>> = Vec::new();
    // The indent/spine gutter stays untinted so the band starts at the marker.
    spans.push(Span::raw(" ".repeat(indent)));
    let mut used = indent;

    // The marker glyph carries the SEMANTIC fg over the tint.
    used = push_cols(&mut spans, diff_marker(side), band(semantic), used, width);

    // The code: syntect fg fragments over the tint, or the semantic fg when no
    // language highlighted this line.
    match fragments {
        Some(frags) if !frags.is_empty() => {
            for ((r, g, b), frag) in frags {
                used = push_cols(
                    &mut spans,
                    &frag,
                    band(Style::default().fg(Color::Rgb(r, g, b))),
                    used,
                    width,
                );
            }
        }
        _ => {
            used = push_cols(&mut spans, text, band(semantic), used, width);
        }
    }

    // Pad the band to the right edge so the tint reads full-width.
    if let Some(bg) = tint
        && used < width
    {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    Line::from(spans)
}

/// Truncates `text` to at most `width` DISPLAY COLUMNS (a wide glyph counts 2),
/// replacing the trimmed tail with a single `…`. The diff path's chrome uses
/// this (not the char-based [`truncate_visual`]) so a CJK/emoji title or header
/// still occupies `<= width` columns and the viewport never re-wraps it
/// (measure==draw, ADR-0029).
fn truncate_cols(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    // Leave one column for the ellipsis; stop before a wide glyph would straddle.
    let (mut out, _) = clip_to_cols(text, width.saturating_sub(1));
    out.push('…');
    out
}

/// The longest char-boundary prefix of `text` that fits in `max` DISPLAY COLUMNS,
/// with its column width. A wide glyph that would straddle the cap is dropped
/// (never half-drawn), so the returned width is always `<= max`. The one place
/// the diff path's column clipping lives ([`truncate_cols`] and [`push_cols`]).
fn clip_to_cols(text: &str, max: usize) -> (String, usize) {
    let mut out = String::new();
    let mut cols = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if cols + w > max {
            break;
        }
        out.push(ch);
        cols += w;
    }
    (out, cols)
}

/// Pushes `text` styled onto `spans`, truncated so the row stays within `width`
/// DISPLAY COLUMNS. Returns the new used-column count. A wide glyph that would
/// straddle the cap is dropped (never half-drawn), so `used <= width` always and
/// the produced [`Line`] occupies `<= width` columns - what keeps every diff row
/// from soft-wrapping (measure==draw, ADR-0029).
fn push_cols(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    used: usize,
    width: usize,
) -> usize {
    if used >= width {
        return used;
    }
    let room = width - used;
    if text.width() <= room {
        let w = text.width();
        spans.push(Span::styled(text.to_string(), style));
        return used + w;
    }
    let (clipped, cols) = clip_to_cols(text, room);
    spans.push(Span::styled(clipped, style));
    used + cols
}

/// The running-spinner animation frames (braille), advanced by the adapter's
/// animation tick while a Run is running.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How many source rows of the live reasoning the rolling tail shows under the
/// `✦ Thinking` header (ADR-0040 Decision A: the short tail). Tunable.
const THINKING_TAIL_ROWS: usize = 3;

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
    /// The Agent is idle - no Run running.
    Idle,
    /// The Agent is running a Run.
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
    },
    /// The position marker - a literal `Bot` (ADR-0046). With native scrollback
    /// owning history, the inline pending region always follows the tail, so
    /// there is no scroll position to report; the segment is a unit.
    Position,
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
            StatusSegment::Position => SegmentKind::Position,
        }
    }

    /// The columns this segment occupies once painted, ratatui-free. Kept in
    /// lockstep with [`StatusSegment::paint`] so the pure fit policy
    /// ([`StatusBar::fit`]) measures exactly what the painter will draw. The
    /// mode dot and `▾`/`▸` marker are each one column, so the width does not
    /// depend on the mode the painter later chooses. Exhaustive so a new
    /// segment kind is a compile error here as well as in the painter.
    fn cells(&self) -> usize {
        self.paint().chars().count()
    }
}

/// The Tokens segment's display text: `~{estimate} tokens` (grouped with
/// thousands separators).
fn tokens_label(estimate: u64) -> String {
    let estimate = estimate.separate_with_commas();
    format!(" ~{estimate} tokens ")
}

/// The Cost segment's display text (ADR-0037: the Session's cumulative
/// Catalog-priced total, in dollars). Two decimals from a cent up; a flat
/// `<$0.01` below that - a sub-cent figure would render `$0.00` and read as
/// free. Only prices a positive total: the assembly hides the segment
/// entirely at zero, so this never formats one.
pub fn cost_label(total: f64) -> String {
    if total < COST_SUB_CENT {
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
        let drop_order: [fn(&StatusSegment) -> bool; DROP_TIER_COUNT] = [
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
/// draws and the [`PressureLevel`] that colors it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenView {
    pub estimate: u64,
    pub level: PressureLevel,
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

/// All display facts for one status bar assembly, bundled to keep [`status_bar`]
/// within the 5-param SRP_PARAMS ceiling. Each field is an independent semantic
/// fact the bar renders; the struct is the boundary between the caller's state
/// and the pure assembly logic.
pub(crate) struct StatusBarView<'a> {
    pub(crate) status: Status,
    pub(crate) conn: ConnectionView<'a>,
    pub(crate) toggles: Toggles,
    pub(crate) figures: FigureView,
}

/// Assembles the status bar's MEANING, pure and ratatui-free (ADR-0019): the
/// ordered semantic segments the bar conveys, fitted to `width`. `view.figures`
/// carries the token facts (`None` when no estimate exists yet) and the
/// Session cost (segment hidden at zero). No colors, glyphs, or label
/// strings are decided here - that is the painter's job
/// ([`render_status_bar`]) - so every rule this expresses (segment order, the
/// fit/drop policy, which [`PressureLevel`] the tokens segment carries, the
/// tokens-absent-until-estimate and cost-hidden-at-zero rules) is a semantic
/// fact assertable without a frame.
pub(crate) fn status_bar(width: usize, view: StatusBarView<'_>) -> StatusBar {
    let StatusBarView {
        status,
        conn,
        toggles,
        figures,
    } = view;
    let mode = match status {
        Status::Idle => ModeState::Idle,
        Status::Running => ModeState::Running,
    };
    let left = vec![
        StatusSegment::Mode(mode),
        StatusSegment::Connection {
            base_url: conn.base_url.to_string(),
        },
        StatusSegment::Model {
            model: conn.model.to_string(),
        },
    ];

    let mut right = vec![
        StatusSegment::Thinking {
            expanded: toggles.thinking_expanded,
        },
        StatusSegment::Tools {
            expanded: toggles.tools_expanded,
        },
    ];
    if let Some(TokenView { estimate, level }) = figures.tokens {
        right.push(StatusSegment::Tokens { estimate, level });
    }
    // The cost segment exists only once a priced Response landed: at zero the
    // Session has spent nothing meterable and the bar stays as it always was.
    if figures.session_cost > COST_HIDDEN {
        right.push(StatusSegment::Cost {
            label: cost_label(figures.session_cost),
        });
    }
    // Native scrollback owns history (ADR-0046): the pending body always follows
    // the tail, so the position segment is a literal `Bot` - a unit, no state.
    right.push(StatusSegment::Position);

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
    /// place the drawing details live: the mode dot, the `▾`/`▸` Thinking
    /// marker, the `~N tokens` label, and the block padding. Semantics-in,
    /// terminal-text-out - the seam ADR-0019 wants. No spinner: the running
    /// animation moved to the `✦ Thinking` brain header (ADR-0040); the mode
    /// block is now a static dot (`●` running, pulsing color; `○` idle).
    fn paint(&self) -> String {
        match self {
            StatusSegment::Mode(ModeState::Running) => " ● ".to_string(),
            StatusSegment::Mode(ModeState::Idle) => " ○ ".to_string(),
            StatusSegment::Connection { base_url } => padded(&format!("suspenders · {base_url}")),
            StatusSegment::Model { model } => padded(&format!("model · {model}")),
            StatusSegment::Thinking { expanded } => {
                let marker = if *expanded { "▾" } else { "▸" };
                padded(&format!("{marker} thinking"))
            }
            StatusSegment::Tools { expanded } => {
                let marker = if *expanded { "▾" } else { "▸" };
                padded(&format!("{marker} tools"))
            }
            StatusSegment::Cost { label } => padded(label),
            StatusSegment::Tokens { estimate, .. } => tokens_label(*estimate),
            // Native scrollback owns history (ADR-0046): always the tail.
            StatusSegment::Position => padded("Bot"),
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
/// runs each [`StatusSegment`] into a styled span via [`StatusSegment::paint`]
/// and [`segment_style`].
/// Screen-state bundle for [`render_status_bar`], so the painter stays within
/// the 5-param SRP_PARAMS ceiling without changing the public `render` signature.
pub(crate) struct StatusBarCtx<'a> {
    pub(crate) screen: &'a Screen,
    pub(crate) conn: ConnectionView<'a>,
}

pub(crate) fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    ctx: StatusBarCtx<'_>,
    theme: &Theme,
) {
    let StatusBarCtx { screen: t, conn } = ctx;
    let bar = status_bar(
        area.width as usize,
        StatusBarView {
            status: t.status,
            conn,
            toggles: Toggles {
                thinking_expanded: t.thinking_expanded,
                tools_expanded: t.tools_expanded,
            },
            figures: FigureView {
                tokens: t.token_estimate.map(|estimate| TokenView {
                    estimate,
                    level: t.pressure_level,
                }),
                session_cost: t.session_cost,
            },
        },
    );

    let bar_bg = tui_color(theme.bar_bg);
    let mut spans: Vec<Span> = Vec::new();
    for (i, segment) in bar.left.iter().enumerate() {
        let kind = segment.kind();
        spans.push(Span::styled(segment.paint(), segment_style(kind, theme)));
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
        spans.push(Span::styled(segment.paint(), segment_style(kind, theme)));
        prev_bg = segment_bg(kind, theme);
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(bar_bg));
    frame.render_widget(bar, area);
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
    let width = (command.chars().count() as u16 + APPROVAL_MODAL_PADDING)
        .max(MODAL_MIN_WIDTH)
        .min(area.width.saturating_sub(APPROVAL_MODAL_SIDE_MARGIN));
    let height = APPROVAL_MODAL_HEIGHT.min(area.height.saturating_sub(2));
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

/// Computes the bounding rect for the Session Picker modal: derives the needed
/// content width from the entries and footer, clamps both dimensions to the
/// terminal, and returns a centered `Rect`. Pure - no frame access.
fn picker_rect(picker: &Picker, area: Rect) -> Rect {
    const FOOTER: &str = "↑/↓ select · Enter resume · Esc fresh session · q quit";

    let content_width = picker
        .entries
        .iter()
        .map(|e| e.stamp.chars().count() + 2 + e.label.chars().count())
        .chain(std::iter::once(FOOTER.chars().count()))
        .max()
        .unwrap_or(0) as u16;
    let width = (content_width + PICKER_MIN_WIDTH_EXTRA)
        .max(MODAL_MIN_WIDTH)
        .min(area.width.saturating_sub(2));
    let height =
        (picker.entries.len() as u16 + PICKER_HEIGHT_OVERHEAD).min(area.height.saturating_sub(2));
    centered_rect(width, height, area)
}

/// The `--resume` Session Picker: a centered bordered list, one row per
/// Session (`stamp  label`), the cursor row reversed+bold, and a dim key-hint
/// footer. Key handling lives in the pure [`Picker`] core; this only draws.
pub fn render_picker(frame: &mut Frame, picker: &Picker, theme: &Theme) {
    const FOOTER: &str = "↑/↓ select · Enter resume · Esc fresh session · q quit";

    let area = frame.area();
    let modal = picker_rect(picker, area);

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

// Whether a Tool Result summary already opens with a status glyph - a extension
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

/// Wraps `label` in a single space on each side: `" {label} "`. The ONE
/// shared format for the powerline segments and popup titles that pad with
/// exactly one space, so the repetition lives here rather than at each call
/// site (BP-010 BOILERPLATE fix).
fn padded(label: &str) -> String {
    format!(" {label} ")
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
    use crate::view_model::DiffLine;

    // A first-class `Diff` item (ADR-0008) with one all-added hunk of raw
    // (marker-free) code lines - the shape the Diff extension's Presenter emits.
    // `lang` is `None` so tests exercise the no-highlight fallback unless they
    // pass a real language explicitly.
    fn diff_item(title: &str, lines: Vec<DiffLine>) -> TranscriptItem {
        TranscriptItem::Diff {
            title: title.to_string(),
            lang: None,
            hunks: vec![DiffHunk {
                header: None,
                lines,
            }],
            elided: 0,
        }
    }

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
    // The lull "waiting" row: the render gate and the row builder.
    // -----------------------------------------------------------------------

    // The gate draws the lull ONLY when the Run is Running and NEITHER live
    // entry is on screen. Each of the three clauses must be able to veto it.
    #[test]
    fn lull_visible_only_when_running_and_no_live_entry() {
        // Running, no thinking tail, no streaming answer => the lull shows.
        assert!(lull_visible(Status::Running, true, false));
        // A running Run but the reasoning tail is on screen => no lull.
        assert!(!lull_visible(Status::Running, false, false));
        // A running Run but the streaming answer is on screen => no lull.
        assert!(!lull_visible(Status::Running, true, true));
        // Not running (idle) => no lull, even with both live entries clear.
        assert!(!lull_visible(Status::Idle, true, false));
    }

    // Inside the settle window `lull::frame` is None, so the row builder yields
    // nothing - a brief token gap never flashes a scene.
    #[test]
    fn live_lull_lines_is_empty_within_the_settle_window() {
        let lines = live_lull_lines(Anim::default(), 40, theme::dark());
        assert!(
            lines.is_empty(),
            "quiet_ticks 0 < SETTLE_TICKS: no lull row yet"
        );
    }

    // At the settle close the row appears: exactly ONE line (the single-row
    // invariant), carrying the timer that opens at "5s" (SETTLE_TICKS * TICK_MS
    // = 50 * 100ms = 5s).
    #[test]
    fn live_lull_lines_opens_one_row_with_the_five_second_timer() {
        let anim = Anim {
            quiet_ticks: lull::SETTLE_TICKS,
            ..Default::default()
        };
        let lines = live_lull_lines(anim, 40, theme::dark());
        assert_eq!(lines.len(), 1, "the lull is a single row");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("5s"), "the timer opens at 5s: {text:?}");
        assert!(!text.trim().is_empty(), "the row carries the scene glyph");
    }

    // The single-row invariant holds no matter how long the wait: a huge
    // `quiet_ticks` still yields one line, truncated to the width passed so it
    // can never desync the lane spine (ADR-0029).
    #[test]
    fn live_lull_lines_stays_one_truncated_row_for_a_long_wait() {
        let width = 20u16;
        let anim = Anim {
            quiet_ticks: lull::SETTLE_TICKS + 100_000,
            ..Default::default()
        };
        let lines = live_lull_lines(anim, width, theme::dark());
        assert_eq!(lines.len(), 1, "still exactly one row");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.chars().count() <= width as usize,
            "the row is truncated to the width passed: {} chars",
            text.chars().count()
        );
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
    fn dark_diff_side_fg_pins_the_palette() {
        let t = theme::dark();
        assert_eq!(diff_side_fg(DiffSide::Added, t), Color::Green);
        assert_eq!(diff_side_fg(DiffSide::Removed, t), Color::Red);
        assert_eq!(diff_side_fg(DiffSide::Context, t), Color::DarkGray);
    }

    #[test]
    fn diff_chrome_reads_muted_italic() {
        // The `@@` header and `… N more lines` tail wear one shared chrome style.
        assert_eq!(
            diff_chrome_style(theme::dark()),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        );
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
            diff_side_fg(DiffSide::Added, &t),
            Color::Rgb(0x12, 0x34, 0x56)
        );
        assert_eq!(md_style(MdStyle::Heading, &t).fg, Some(Color::Magenta));
        // Unstated slots still read the dark floor.
        assert_eq!(diff_side_fg(DiffSide::Removed, &t), Color::Red);
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

    /// A bare code block insets each row under [`CODE_INSET`] and frames the
    /// block with a blank row above and below (ADR-0040 Decision E). This finds
    /// the row whose code text (after the inset) matches `code`, and returns its
    /// spans WITHOUT the leading inset span - what the assertions below care
    /// about.
    fn code_row<'a>(lines: &'a [Line<'static>], code: &str) -> &'a [Span<'static>] {
        let line = lines
            .iter()
            .find(|l| line_text(l) == format!("{CODE_INSET}{code}"))
            .unwrap_or_else(|| panic!("the code row for {code:?}"));
        // The first span is always the inset (code bg, no fg); the code follows.
        assert_eq!(line.spans[0].content.as_ref(), CODE_INSET);
        assert_eq!(
            line.spans[0].style.bg,
            Some(tui_color(theme::dark().code_block_bg))
        );
        &line.spans[1..]
    }

    #[test]
    fn a_known_language_fence_is_highlighted_over_the_code_background() {
        let lines = markdown_lines("```rust\nlet x = 1;\n```", theme::dark());
        let code = code_row(&lines, "let x = 1;");
        // Syntect fragments the line; every fragment keeps OUR code bg under
        // its own syntect fg.
        assert!(code.len() > 1, "syntect splits the line");
        for span in code {
            assert_eq!(span.style.bg, Some(tui_color(theme::dark().code_block_bg)));
            assert!(matches!(span.style.fg, Some(Color::Rgb(..))));
        }
    }

    #[test]
    fn a_bare_code_block_is_framed_by_a_blank_row_above_and_below() {
        // The block is inset and bounded by one blank row on each side; no box,
        // no gutter (Decision E).
        let lines = markdown_lines("before\n\n```rust\nlet x = 1;\n```\n\nafter", theme::dark());
        let code_idx = lines
            .iter()
            .position(|l| line_text(l) == format!("{CODE_INSET}let x = 1;"))
            .expect("the inset code row");
        assert_eq!(line_text(&lines[code_idx - 1]), "", "blank row above");
        assert_eq!(line_text(&lines[code_idx + 1]), "", "blank row below");
    }

    #[test]
    fn an_unknown_language_fence_falls_back_to_the_plain_code_block_style() {
        let lines = markdown_lines("```notareallanguage\nsome code\n```", theme::dark());
        let code = code_row(&lines, "some code");
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].style, md_style(MdStyle::CodeBlock, theme::dark()));
    }

    #[test]
    fn a_bare_fence_with_no_language_gets_the_inset_framed_block() {
        // A bare ``` fence carries `Some("")` - the common case local models
        // emit. It skips syntect (empty lang resolves no syntax) but still gets
        // the SAME inset + blank-framed code block as a labeled fence (M1): the
        // plain CodeBlock style, inset under CODE_INSET, framed above and below.
        let lines = markdown_lines("before\n\n```\nunlabeled code\n```\n\nafter", theme::dark());
        let code = code_row(&lines, "unlabeled code");
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].style, md_style(MdStyle::CodeBlock, theme::dark()));
        // Framed: a blank row above and below the inset code row.
        let idx = lines
            .iter()
            .position(|l| line_text(l) == format!("{CODE_INSET}unlabeled code"))
            .expect("the inset code row");
        assert_eq!(line_text(&lines[idx - 1]), "", "blank row above");
        assert_eq!(line_text(&lines[idx + 1]), "", "blank row below");
    }

    #[test]
    fn a_blank_line_inside_a_highlighted_fence_keeps_the_code_background() {
        let lines = markdown_lines("```rust\nlet a = 1;\n\nlet b = 2;\n```", theme::dark());
        let a_idx = lines
            .iter()
            .position(|l| line_text(l) == format!("{CODE_INSET}let a = 1;"))
            .expect("the first code line");
        // The blank row between the statements yields no syntect fragments, so
        // it takes the plain CodeBlock treatment - same bg, no hole - and it is
        // still inset (the inset span, then the empty code span).
        let blank = &lines[a_idx + 1];
        assert_eq!(line_text(blank), CODE_INSET);
        assert_eq!(
            blank.spans[1].style,
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
            StatusBarView {
                status: Status::Running,
                conn: ConnectionView {
                    base_url: "http://localhost:8080",
                    model: "qwen/model",
                },
                toggles: Toggles::default(),
                figures: FigureView {
                    tokens: Some(TokenView {
                        estimate: 1200,
                        level: PressureLevel::Ok,
                    }),
                    session_cost: 0.42,
                },
            },
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

        // At 30 cost drops next; tokens survive it (they carry the pressure
        // level the operator steers by). The threshold sits lower than it once
        // did because the mode block is now a 3-col dot, not a spelled word.
        let bar = bar_at(30);
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

    /// Builds a wide idle bar at width 200 with the given toggles and no
    /// figures. Shared by the thinking and tools toggle tests to avoid
    /// near-identical closure bodies (DUPLICATE fix).
    fn idle_wide_bar(toggles: Toggles) -> StatusBar {
        status_bar(
            200,
            StatusBarView {
                status: Status::Idle,
                conn: ConnectionView {
                    base_url: "http://localhost:8080",
                    model: "qwen/model",
                },
                toggles,
                figures: no_figures(),
            },
        )
    }

    #[test]
    fn the_thinking_segment_carries_the_ctrl_t_state() {
        // The MEANING (expanded true/false) is a semantic fact; the ▾/▸ marker
        // it paints to is a drawing detail asserted separately below.
        let find_thinking = |expanded: bool| {
            idle_wide_bar(Toggles {
                thinking_expanded: expanded,
                tools_expanded: false,
            })
            .right
            .into_iter()
            .find(|s| matches!(s, StatusSegment::Thinking { .. }))
            .expect("thinking segment is always assembled")
        };
        assert_eq!(
            find_thinking(true),
            StatusSegment::Thinking { expanded: true }
        );
        assert_eq!(
            find_thinking(false),
            StatusSegment::Thinking { expanded: false }
        );
    }

    #[test]
    fn the_thinking_marker_paints_from_its_state() {
        assert_eq!(
            StatusSegment::Thinking { expanded: true }.paint(),
            " ▾ thinking "
        );
        assert_eq!(
            StatusSegment::Thinking { expanded: false }.paint(),
            " ▸ thinking "
        );
    }

    #[test]
    fn the_tools_segment_carries_the_ctrl_o_state() {
        // The twin of the thinking segment for the machinery plane: the MEANING
        // (expanded true/false) is the semantic fact; the ▾/▸ marker is a
        // drawing detail asserted separately below.
        let find_tools = |expanded: bool| {
            idle_wide_bar(Toggles {
                thinking_expanded: false,
                tools_expanded: expanded,
            })
            .right
            .into_iter()
            .find(|s| matches!(s, StatusSegment::Tools { .. }))
            .expect("tools segment is always assembled")
        };
        assert_eq!(find_tools(true), StatusSegment::Tools { expanded: true });
        assert_eq!(find_tools(false), StatusSegment::Tools { expanded: false });
    }

    #[test]
    fn the_tools_marker_paints_from_its_state() {
        assert_eq!(StatusSegment::Tools { expanded: true }.paint(), " ▾ tools ");
        assert_eq!(
            StatusSegment::Tools { expanded: false }.paint(),
            " ▸ tools "
        );
    }

    #[test]
    fn the_tokens_segment_is_absent_until_an_estimate_exists() {
        let bar = idle_wide_bar(Toggles::default());
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
            StatusBarView {
                status: Status::Idle,
                conn: ConnectionView {
                    base_url: "http://localhost:8080",
                    model: "qwen/model",
                },
                toggles: Toggles::default(),
                figures: tokens_only(TokenView {
                    estimate: 1200,
                    level: PressureLevel::Ok,
                }),
            },
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
        assert_eq!(cost.paint(), " $0.42 ");
        assert_eq!(cost.cells(), " $0.42 ".chars().count());
    }

    /// Assembles a wide bar with the given `figures` and returns the tokens
    /// segment. Shared by the pressure-level tests so the bar-build and segment
    /// search chains do not repeat (DUPLICATE fix).
    fn tokens_segment(figures: FigureView) -> StatusSegment {
        status_bar(
            200,
            StatusBarView {
                status: Status::Running,
                conn: ConnectionView {
                    base_url: "u",
                    model: "qwen/model",
                },
                toggles: Toggles::default(),
                figures,
            },
        )
        .right
        .into_iter()
        .find(|s| matches!(s, StatusSegment::Tokens { .. }))
        .expect("tokens segment present when an estimate exists")
    }

    #[test]
    fn critical_pressure_yields_a_tokens_segment_carrying_that_level() {
        // The "Critical Context Pressure renders red" rule, asserted headless:
        // the semantic segment carries PressureLevel::Critical, and its kind
        // routes exactly that level into segment_style (which maps it to red).
        let tokens = tokens_segment(tokens_only(TokenView {
            estimate: 99000,
            level: PressureLevel::Critical,
        }));
        assert_eq!(
            tokens,
            StatusSegment::Tokens {
                estimate: 99000,
                level: PressureLevel::Critical,
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
            let tokens = tokens_segment(tokens_only(TokenView { estimate: 1, level }));
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
                StatusBarView {
                    status,
                    conn: ConnectionView {
                        base_url: "u",
                        model: "qwen/model",
                    },
                    toggles: Toggles::default(),
                    figures: no_figures(),
                },
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
    fn the_mode_segment_paints_a_static_dot_no_spinner() {
        // The running animation moved to the `✦ Thinking` brain header
        // (ADR-0040); the mode block is now a static dot, and cells() agrees
        // with paint() in both modes (the drift invariant).
        for mode in [ModeState::Running, ModeState::Idle] {
            let seg = StatusSegment::Mode(mode);
            assert_eq!(
                seg.cells(),
                seg.paint().chars().count(),
                "{seg:?} cells() disagrees with painted width"
            );
        }
        assert_eq!(StatusSegment::Mode(ModeState::Running).paint(), " ● ");
        assert_eq!(StatusSegment::Mode(ModeState::Idle).paint(), " ○ ");
    }

    #[test]
    fn the_tokens_segment_paints_the_estimate_grouped() {
        assert_eq!(
            StatusSegment::Tokens {
                estimate: 1200,
                level: PressureLevel::Ok,
            }
            .paint(),
            " ~1,200 tokens "
        );
    }

    #[test]
    fn the_tokens_segment_cells_match_its_paint() {
        // The load-bearing fit invariant: cells() must equal the painted width,
        // or the bar over/underflows.
        let seg = StatusSegment::Tokens {
            estimate: 1200,
            level: PressureLevel::Ok,
        };
        assert_eq!(
            seg.cells(),
            seg.paint().chars().count(),
            "{seg:?} cells() disagrees with painted width"
        );
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
                segment.paint().chars().count(),
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
        // The `› ` caret now lives in the reserved lane gutter (ADR-0040), so
        // the cached User line is the bare 16-char word. At width 10 it wraps
        // to 2 rows (10 + 6).
        t.user("0123456789012345");
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 10, theme::dark());
        assert_eq!(cache.settled().count(), 1);
        // 2 wrapped rows, dense (no per-item blank separator).
        assert_eq!(cache.settled().next().unwrap().1, 2);
    }

    #[test]
    fn cache_sync_rebuilds_when_the_width_changes() {
        let mut t = fresh_transcript();
        t.user("0123456789012345");
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        // 1 content row, dense (no per-item blank separator).
        let wide = cache.settled().next().unwrap().1;
        assert_eq!(wide, 1);
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
        // Collapsed one-liner, dense (no per-item blank separator).
        assert_eq!(cache.settled().next().unwrap().0.len(), 1);
        cache.sync(
            &t,
            Toggles {
                thinking_expanded: true,
                ..Toggles::default()
            },
            80,
            theme::dark(),
        );
        // Header + both rows, dense (no per-item blank separator).
        assert_eq!(cache.settled().next().unwrap().0.len(), 3);
    }

    #[test]
    fn cache_sync_rebuilds_when_the_tools_toggle_flips() {
        // The Ctrl-O twin of the thinking-toggle test: a multi-line Diff folds
        // to a single title line when collapsed and to the full body when
        // expanded, and flipping the toggle clears the cache so the change
        // takes effect. The lane is dense now - no per-item blank separator.
        let mut t = fresh_transcript();
        t.push(diff_item(
            "edit_file src/foo.rs",
            vec![
                DiffLine::new(DiffSide::Added, "added line"),
                DiffLine::new(DiffSide::Removed, "removed line"),
            ],
        ));
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        // Collapsed one-liner, dense (no per-item blank separator).
        let collapsed = cache.settled().next().unwrap().0;
        assert_eq!(collapsed.len(), 1);
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
        // Title row + both body rows, dense (no per-item blank separator).
        let expanded = cache.settled().next().unwrap().0;
        assert_eq!(expanded.len(), 3);
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
        let lines = message_lines(&item, false, false, 80, theme::dark());
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
        let lines = message_lines(&item, false, false, 80, theme::dark());
        assert_eq!(line_text(&lines[0]), "  ⋯ run_command → injected");
    }

    #[test]
    fn a_failing_merged_result_keeps_the_arg_and_shows_a_single_badge_glyph() {
        // The summary already carries the extension badge `✗ exit 1`; the error
        // line injects no glyph of its own, so there is a SINGLE `✗`, not `✗ ✗`.
        let item = TranscriptItem::ToolResult {
            name: "run_command".to_string(),
            summary: "✗ exit 1".to_string(),
            is_error: true,
            key_arg: Some("cargo test".to_string()),
        };
        let lines = message_lines(&item, false, false, 80, theme::dark());
        assert_eq!(line_text(&lines[0]), "  ⚙ run_command  cargo test ✗ exit 1");
    }

    #[test]
    fn a_failing_result_without_a_badge_gets_an_injected_error_glyph() {
        // A tool whose summary carries no glyph (no badge extension): the line
        // injects a leading `✗` so the failure is never missed - the ⚙ gutter,
        // the arg, then `✗ {summary}`, all red+bold.
        let item = TranscriptItem::ToolResult {
            name: "edit_file".to_string(),
            summary: "old_str not found".to_string(),
            is_error: true,
            key_arg: Some("src/foo.rs".to_string()),
        };
        let lines = message_lines(&item, false, false, 80, theme::dark());
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
            let lines = message_lines(&item, false, false, 80, theme::dark());
            assert_eq!(line_text(&lines[0]), format!("  ⚙ run_command {badge}"));
        }
    }

    // The tinted marker plane (ADR-0040): each Tone renders in its OWN Theme
    // slot, tinted by tone alone - identical text under two tones tints
    // differently, proving the adapter never sniffs the line.
    #[test]
    fn a_marker_tints_by_its_tone_slot_not_its_text() {
        let theme = theme::dark();
        for (tone, expected) in [
            (Tone::Housekeeping, theme.marker_housekeeping),
            (Tone::Aid, theme.marker_aid),
            (Tone::Constrain, theme.marker_constrain),
            (Tone::Steering, theme.prompt_gutter),
            (Tone::Plain, theme.muted),
        ] {
            let item = TranscriptItem::Marker {
                // Same text for every tone: the tint cannot be coming from it.
                text: "harness marker".to_string(),
                tone,
            };
            let lines = message_lines(&item, false, false, 80, theme);
            assert_eq!(lines.len(), 1);
            // A Marker indents two columns under the thought header (ADR-0040);
            // the whole styled line (indent + text) carries the tone color.
            // `Line::styled` puts the style on the Line, which the spans inherit.
            assert_eq!(line_text(&lines[0]), "  harness marker", "{tone:?}");
            assert_eq!(lines[0].style.fg, Some(tui_color(expected)), "{tone:?}");
            assert!(
                lines[0].style.add_modifier.contains(Modifier::ITALIC),
                "{tone:?} marker should read as the quiet plane (italic)"
            );
        }
    }

    #[test]
    fn has_foldable_body_is_true_only_for_a_non_empty_diff() {
        // A non-empty Diff folds under Ctrl-O.
        let diff = diff_item("edit_file x", vec![DiffLine::new(DiffSide::Added, "a")]);
        assert!(diff.has_foldable_body());

        // A one-line merged ToolResult has no body to fold.
        let result = TranscriptItem::ToolResult {
            name: "read_file".to_string(),
            summary: "340 lines".to_string(),
            is_error: false,
            key_arg: Some("src/foo.rs".to_string()),
        };
        assert!(!result.has_foldable_body());

        // A Diff with no hunk lines has nothing to fold either.
        let empty = diff_item("titled but empty", vec![]);
        assert!(!empty.has_foldable_body());
    }

    #[test]
    fn ctrl_o_still_folds_a_diff_after_the_merge() {
        // A merge produces a lone Diff (the call line removed). Ctrl-O must still
        // collapse it to its one-line title - the semantic fold predicate keys
        // on the Diff's foldable body, unaffected by the merge.
        let diff = diff_item(
            "edit_file src/foo.rs (+1 -1)",
            vec![
                DiffLine::new(DiffSide::Added, "new"),
                DiffLine::new(DiffSide::Removed, "old"),
            ],
        );
        // Collapsed (tools_expanded = false): one title line with the affordance.
        let collapsed = message_lines(&diff, false, false, 80, theme::dark());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            line_text(&collapsed[0]),
            "  ⋯ edit_file src/foo.rs (+1 -1) · ^O expand"
        );
        // Expanded: title + both body rows.
        let expanded = message_lines(&diff, false, true, 80, theme::dark());
        assert_eq!(expanded.len(), 3);
    }

    // -----------------------------------------------------------------------
    // diff rendering (ADR-0008): the marker glyph, the full-width tint band,
    // and the two-pass hunk-coherent syntect highlighting.
    // -----------------------------------------------------------------------

    // A one-hunk Diff item: the shared builder every diff-render test routes
    // through, so the `TranscriptItem::Diff { … }` literal lives in one place.
    fn diff_of(lang: Option<&str>, header: Option<&str>, lines: Vec<DiffLine>) -> TranscriptItem {
        TranscriptItem::Diff {
            title: "edit_file foo".to_string(),
            lang: lang.map(str::to_string),
            hunks: vec![DiffHunk {
                header: header.map(str::to_string),
                lines,
            }],
            elided: 0,
        }
    }

    // A created-file Diff (one all-added hunk, `header: None`) of `content` in
    // `lang`, expanded so `message_lines` renders every code row.
    fn created_diff_rows(lang: &str, content: &[&str], width: u16) -> Vec<Line<'static>> {
        let lines = content
            .iter()
            .map(|t| DiffLine::new(DiffSide::Added, *t))
            .collect();
        let item = diff_of(Some(lang), None, lines);
        message_lines(&item, false, true, width, theme::dark())
    }

    // The distinct syntect foregrounds of a diff code row: the fgs AFTER the
    // marker glyph, dropping the trailing full-width pad (bg only, no fg). Used
    // to compare the color a line's code was highlighted with.
    fn code_fgs(row: &Line<'static>) -> Vec<Color> {
        row.spans
            .iter()
            .skip(2) // the untinted indent + the marker glyph
            .filter_map(|s| s.style.fg)
            .collect()
    }

    #[test]
    fn a_created_file_block_comment_colors_coherently_across_every_line() {
        // The ADR-0008 HARD requirement: a created file is one all-added hunk =
        // the whole file, highlighted as ONE slice so syntect parse state
        // carries. A multi-line `/** … */` JSDoc block MUST color as a comment
        // across ALL its lines - per-line-independent highlighting (which would
        // color only line 1 as a comment and lines 2-3 as plain text) is WRONG.
        let rows = created_diff_rows(
            "js",
            &[
                "/**",
                " * a doc comment",
                " * spanning lines",
                " */",
                "const x = 1;",
            ],
            80,
        );
        // rows[0] is the title; the 4 comment lines are rows[1..=4].
        let comment_fg = code_fgs(&rows[1]);
        assert!(
            comment_fg.iter().all(|c| matches!(c, Color::Rgb(..))),
            "the comment's first line is syntect-colored: {comment_fg:?}"
        );
        let first = comment_fg[0];
        for row in &rows[1..=4] {
            for fg in code_fgs(row) {
                assert_eq!(
                    fg,
                    first,
                    "every line of the block comment shares the comment color \
                     (parse state carried across the hunk): {:?}",
                    line_text(row)
                );
            }
        }
        // The trailing code line, by contrast, is NOT the comment color - proof
        // the comment actually closed and highlighting resumed.
        let code_fg = code_fgs(&rows[5]);
        assert!(
            code_fg.iter().any(|c| *c != first),
            "the `const x = 1;` line is not the comment color: {code_fg:?}"
        );
    }

    #[test]
    fn a_removed_line_highlights_from_the_before_image() {
        // The removed side of a hunk highlights as its own slice (the before
        // image): a removed comment line still colors as a comment, from the
        // before-image pass, not the after-image one.
        let item = diff_of(
            Some("js"),
            Some("@@ -1,2 +1,1 @@"),
            vec![
                DiffLine::new(DiffSide::Removed, "// gone"),
                DiffLine::new(DiffSide::Added, "kept();"),
            ],
        );
        let rows = message_lines(&item, false, true, 80, theme::dark());
        // rows: title, header, removed, added.
        let removed = &rows[2];
        assert_eq!(removed.spans[1].content.as_ref(), "- ");
        // The removed comment carried a syntect fg (before-image highlighted).
        assert!(
            code_fgs(removed)
                .iter()
                .all(|c| matches!(c, Color::Rgb(..))),
            "the removed comment is syntect-colored: {:?}",
            line_text(removed)
        );
    }

    #[test]
    fn an_added_line_reads_as_a_full_width_tint_band() {
        // The tint is GitHub-style: a full-width band. The row's LAST span pads
        // to the content width and carries the added_bg, and the marker glyph +
        // code carry that same bg over their fg.
        let rows = created_diff_rows("rs", &["let x = 1;"], 40);
        let row = &rows[1];
        let added_bg = Some(tui_color(theme::dark().added_bg));
        // The marker glyph carries the tint and the semantic (green) fg.
        assert_eq!(row.spans[1].content.as_ref(), "+ ");
        assert_eq!(row.spans[1].style.bg, added_bg);
        assert_eq!(row.spans[1].style.fg, Some(tui_color(theme::dark().added)));
        // Every span past the untinted indent carries the tint (band-wide).
        for span in row.spans.iter().skip(1) {
            assert_eq!(span.style.bg, added_bg, "band span keeps the tint");
        }
        // The row fills the width exactly, in DISPLAY COLUMNS (indent + marker +
        // code + pad).
        assert_eq!(row_display_width(row), 40, "the band reaches the edge");
        // The last span is the pad (bg only, no fg).
        let pad = row.spans.last().unwrap();
        assert_eq!(pad.style.bg, added_bg);
        assert_eq!(pad.style.fg, None);
    }

    #[test]
    fn a_context_line_is_untinted() {
        let item = diff_of(
            Some("rs"),
            None,
            vec![DiffLine::new(DiffSide::Context, "let x = 1;")],
        );
        let rows = message_lines(&item, false, true, 40, theme::dark());
        let ctx = &rows[1];
        // The context marker is two blanks and NO span carries a background.
        assert_eq!(ctx.spans[1].content.as_ref(), "  ");
        for span in &ctx.spans {
            assert_eq!(span.style.bg, None, "context is untinted");
        }
    }

    #[test]
    fn an_unknown_language_falls_back_to_the_semantic_foreground() {
        // No language resolves for a `.txt` extension (lang: None in practice);
        // the code still renders, tinted, with the semantic fg (no syntect).
        let item = diff_of(
            None,
            None,
            vec![DiffLine::new(DiffSide::Added, "just text")],
        );
        let rows = message_lines(&item, false, true, 40, theme::dark());
        let row = &rows[1];
        // The code span carries the semantic added fg (green), not a syntect Rgb.
        let code = &row.spans[2];
        assert_eq!(code.content.as_ref(), "just text");
        assert_eq!(code.style.fg, Some(tui_color(theme::dark().added)));
    }

    #[test]
    fn the_elided_tail_renders_as_a_muted_count() {
        let mut item = diff_of(None, None, vec![DiffLine::new(DiffSide::Added, "a")]);
        if let TranscriptItem::Diff { elided, .. } = &mut item {
            *elided = 40;
        }
        let rows = message_lines(&item, false, true, 40, theme::dark());
        let tail = rows.last().unwrap();
        assert_eq!(line_text(tail).trim_end(), "  … 40 more lines");
        assert_eq!(tail.style, diff_chrome_style(theme::dark()));
    }

    #[test]
    fn an_interleaved_hunk_aligns_each_line_to_its_own_image() {
        // The cursor-alignment path: a hunk that interleaves context, removed,
        // and added lines must draw each line from the RIGHT image (added/context
        // from the after pass, removed/context from the before) with no desync.
        // The `x` identifier appears on every line, so a coherent highlight gives
        // every row the SAME fg for that token; a desynced cursor would mis-color.
        let item = diff_of(
            Some("rs"),
            Some("@@ -1,4 +1,4 @@"),
            vec![
                DiffLine::new(DiffSide::Context, "let x = 0;"),
                DiffLine::new(DiffSide::Removed, "let x = 1;"),
                DiffLine::new(DiffSide::Removed, "let x = 2;"),
                DiffLine::new(DiffSide::Added, "let x = 3;"),
                DiffLine::new(DiffSide::Context, "let x = 4;"),
                DiffLine::new(DiffSide::Added, "let x = 5;"),
            ],
        );
        let rows = message_lines(&item, false, true, 80, theme::dark());
        // rows: title, header, then the 6 code rows in file order.
        let code = &rows[2..];
        assert_eq!(code.len(), 6);
        // The `let` keyword is fragment 0 of every code row; its fg is the syntect
        // keyword color, identical on every line iff the two passes stayed aligned.
        let keyword_fg = |row: &Line<'static>| code_fgs(row).first().copied();
        let first = keyword_fg(&code[0]).expect("the first row is highlighted");
        assert!(
            matches!(first, Color::Rgb(..)),
            "syntect colored it: {first:?}"
        );
        for row in code {
            assert_eq!(
                keyword_fg(row),
                Some(first),
                "every interleaved line's keyword shares one color: {:?}",
                line_text(row)
            );
        }
        // And each line wears the tint of ITS side (added/removed/context).
        let added_bg = Some(tui_color(theme::dark().added_bg));
        let removed_bg = Some(tui_color(theme::dark().removed_bg));
        assert_eq!(code[0].spans[1].content.as_ref(), "  "); // context marker
        assert_eq!(code[0].spans.last().unwrap().style.bg, None);
        assert_eq!(code[1].spans[1].content.as_ref(), "- "); // removed marker
        assert_eq!(code[1].spans.last().unwrap().style.bg, removed_bg);
        assert_eq!(code[3].spans[1].content.as_ref(), "+ "); // added marker
        assert_eq!(code[3].spans.last().unwrap().style.bg, added_bg);
    }

    #[test]
    fn an_all_removed_hunk_renders_from_the_before_image() {
        // A pure deletion: every line is Removed, so the after image is empty and
        // the whole hunk highlights from the before image. Each row wears the
        // removed marker, the removed tint, and a syntect fg.
        let item = diff_of(
            Some("rs"),
            Some("@@ -1,2 +0,0 @@"),
            vec![
                DiffLine::new(DiffSide::Removed, "fn gone() {}"),
                DiffLine::new(DiffSide::Removed, "fn also() {}"),
            ],
        );
        let rows = message_lines(&item, false, true, 80, theme::dark());
        let removed_bg = Some(tui_color(theme::dark().removed_bg));
        for row in &rows[2..] {
            assert_eq!(row.spans[1].content.as_ref(), "- ");
            assert_eq!(row.spans.last().unwrap().style.bg, removed_bg);
            assert!(
                code_fgs(row).iter().all(|c| matches!(c, Color::Rgb(..))),
                "the removed code is syntect-colored: {:?}",
                line_text(row)
            );
        }
    }

    #[test]
    fn a_tab_in_a_diff_line_expands_through_the_full_row() {
        // The tab→two-spaces normalization survives the whole render path, not
        // just the unit: a `\t`-indented code line draws with the tab expanded.
        let rows = created_diff_rows("rs", &["\tlet x = 1;"], 80);
        let row = &rows[1];
        // indent (2) + marker (2) then the code, tab expanded to two spaces.
        let text = line_text(row);
        assert!(
            text.starts_with("  +   let x = 1;"),
            "tab expanded in the rendered row: {text:?}"
        );
        assert!(!text.contains('\t'), "no raw tab survives: {text:?}");
    }

    #[test]
    fn an_over_wide_code_row_is_clipped_to_the_width() {
        // The clip branch of `push_cols`: a code line wider than the content area
        // is truncated so the row occupies exactly `width` columns (and thus never
        // soft-wraps). Width 20, a 40-char line.
        let long = "x".repeat(40);
        let rows = created_diff_rows("rs", &[&long], 20);
        let row = &rows[1];
        assert_eq!(
            row_display_width(row),
            20,
            "the row is clipped to the width"
        );
        // One visual row: the viewport's own wrap math agrees (measure==draw).
        assert_eq!(wrapped_count(vec![row.clone()], 20), 1);
    }

    #[test]
    fn a_wide_cjk_diff_row_stays_one_visual_row() {
        // The MAJOR width-correctness fix (review #2): widths are DISPLAY COLUMNS,
        // not char counts. A CJK line char-padded to `width` would render WIDER
        // than `width` columns and the viewport `Wrap` would re-break it, shatter-
        // ing the tint band. Assert via the SAME `wrapped_count` the viewport uses
        // that a wide-glyph row occupies exactly one visual row at several widths.
        for width in [12u16, 20, 41] {
            // Each CJK ideograph is two columns; mix in ASCII and an emoji.
            let rows = created_diff_rows("txt", &["語 = 実装 ✨ done"], width);
            let row = &rows[1];
            assert!(
                row_display_width(row) <= width as usize,
                "row is within {width} columns: got {} for {:?}",
                row_display_width(row),
                line_text(row)
            );
            assert_eq!(
                wrapped_count(vec![row.clone()], width),
                1,
                "the wide-glyph row stays ONE visual row at width {width}: {:?}",
                line_text(row)
            );
        }
    }

    // The rendered display width of a diff row (sum of its spans' column widths).
    fn row_display_width(row: &Line<'static>) -> usize {
        row.spans.iter().map(|s| s.content.width()).sum()
    }

    // (The Ctrl-O viewport-stability test is retired: there is no adapter-side
    // viewport now - native scrollback owns history, ADR-0046. Ctrl-O's effect
    // on the cached line counts is still covered by the cache toggle tests.)

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
                .map(|item| {
                    wrapped_count(
                        message_lines(item, false, false, width, theme::dark()),
                        width,
                    )
                })
                .sum();
            let whole: Vec<Line> = items
                .iter()
                .flat_map(|item| message_lines(item, false, false, width, theme::dark()))
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
    use crate::llm::response::StopReason;
    use crate::ui::screen::{Key, ScreenOpts};

    /// Draws one frame with `draw` on a fresh `width`×`height` test terminal
    /// and returns the terminal for buffer inspection.
    fn draw_frame(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
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

    /// Returns the combined style modifiers of a single buffer cell. Shared by
    /// the popup style assertions so the `buf.cell(...).expect(...).style().add_modifier`
    /// chain is not repeated per-cell (DUPLICATE fix).
    fn cell_modifier(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> Modifier {
        terminal
            .backend()
            .buffer()
            .cell((x, y))
            .expect("cell in test buffer")
            .style()
            .add_modifier
    }

    /// Draws the inline PENDING transcript body (ADR-0046) for `screen` into a
    /// fresh `width`x`height` terminal, TOP-aligned so the content-assertion
    /// tests (which scan rows for known text/gutter glyphs) read a stable layout.
    /// Uses [`render_pending_body_at`] directly (no status bar / composer) over
    /// the whole area; the pending body draws the uncommitted settled tail plus
    /// the live stream. Fresh cache, default anim, dark theme - the
    /// overwhelmingly common test shape.
    ///
    /// Top-aligned: when the content FITS, [`render_pending_body`] bottom-anchors,
    /// so we draw into a body zone exactly as tall as the content when it fits,
    /// and the full area when it overflows (top-clipped, newest kept).
    fn draw_viewport(width: u16, height: u16, screen: &Screen) -> Terminal<TestBackend> {
        let mut cache = RenderCache::new();
        draw_frame(width, height, |f| {
            let area = f.area();
            // Measure the pending stack once to decide the zone height: a fitting
            // stack draws in a zone its own height (so it top-aligns), an
            // overflowing one uses the full area (top-clipped).
            let total = pending_body_height(screen, &mut cache, area.width, theme::dark());
            let zone_h = (total as u16).min(area.height).max(1);
            let zone = Rect {
                height: zone_h,
                ..area
            };
            // hw = 0: draw the WHOLE settled transcript (committed items live in
            // scrollback on a real TTY, but a headless content test wants them).
            render_pending_body_at(
                f,
                zone,
                &mut PendingBodyParams {
                    screen,
                    cache: &mut cache,
                    anim: Anim::default(),
                },
                theme::dark(),
                0,
            );
        })
    }

    /// The pending stack's total wrapped rows for `screen` at `width` (test
    /// helper): mirrors [`render_pending_body`]'s measurement so `draw_viewport`
    /// can top-align a fitting stack.
    fn pending_body_height(
        screen: &Screen,
        cache: &mut RenderCache,
        width: u16,
        theme: &Theme,
    ) -> usize {
        let content_width = width.saturating_sub(LANE_GUTTER);
        cache.sync(
            screen.transcript(),
            Toggles {
                thinking_expanded: screen.thinking_expanded,
                tools_expanded: screen.tools_expanded,
            },
            content_width,
            theme,
        );
        let items = screen.transcript().items();
        // hw = 0: measure the WHOLE settled transcript (the test helper draws it
        // all top-aligned). Full-content, no fold (ADR-0046).
        let lane = lane_gutters(items);
        let mut total: usize = assemble_pending(cache, &lane, 0).total_lines();
        // Add the live stream rows the body would append.
        let thinking = screen.transcript().streaming_thinking();
        let thinking_lines = live_thinking_lines(&thinking, 0, content_width, theme);
        if !thinking_lines.is_empty() {
            total += wrapped_count(thinking_lines, content_width);
        }
        if let Some((_, wrapped)) = cache.streaming_tail() {
            total += wrapped;
        }
        total
    }

    // --- render_committed_slice (ADR-0046, the inline `insert_before` seam) ---

    /// A whole [`Buffer`] as newline-joined rows of symbols.
    fn commit_buffer_text(buf: &Buffer) -> String {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).expect("cell in area").symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Blits `[hw, hw + count)` of `cache` into `buf` under the dark theme - the
    /// one place these tests spell the [`CommittedSlice`] bundle, so each case
    /// reads as the `(hw, count)` window it exercises.
    fn blit_slice(
        buf: &mut Buffer,
        cache: &RenderCache,
        items: &[TranscriptItem],
        hw: usize,
        count: usize,
    ) {
        render_committed_slice(
            buf,
            &CommittedSlice {
                cache,
                items,
                hw,
                count,
                theme: theme::dark(),
            },
        );
    }

    // A committed slice blits each item's cached content one visual row apart,
    // with the same two-plane gutter the pending region uses: the user `› `
    // caret on the request row, the dim `│ ` spine on the agent lines that hang
    // off it. Golden against the exact rows the pending body draws for the same
    // items (see the seam-identity test above).
    #[test]
    fn render_committed_slice_blits_content_and_the_lane_gutter() {
        // Author a tiny request lane directly on a bare store: an info line,
        // a User prompt, then one agent answer line.
        let mut t = crate::ui::transcript::Transcript::new(Vec::new());
        t.info("opening");
        t.user("do a thing");
        t.push(TranscriptItem::Assistant {
            text: "sure".into(),
        });

        let items: Vec<TranscriptItem> = t.items().to_vec();
        let count = items.len();

        // Sync the cache at the SAME content width the slice draws at.
        let width: u16 = 40;
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), width - LANE_GUTTER, theme::dark());

        let height = commit_slice_height(&cache, 0, count);
        assert!(height >= 3, "info + user + answer are at least 3 rows");

        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        blit_slice(&mut buf, &cache, &items, 0, count);

        let text = commit_buffer_text(&buf);
        // The content landed, one item per its rows.
        assert!(text.contains("do a thing"), "user prompt drawn:\n{text}");
        assert!(text.contains("sure"), "answer drawn:\n{text}");
        // The caret marks the user request row; the spine marks the agent line.
        assert!(
            text.lines().any(|l| l.starts_with("› ")),
            "user caret in the gutter:\n{text}"
        );
        assert!(
            text.lines().any(|l| l.starts_with("│ ")),
            "lane spine in the gutter:\n{text}"
        );
    }

    // MEASURE == DRAW (ADR-0029/0046): `commit_slice_height` (what the adapter
    // sizes the `insert_before` buffer to) must equal the number of NON-BLANK
    // rows `render_committed_slice` actually writes into an OVERSIZED buffer. If
    // measure and draw drifted (a width mismatch, a wrap discrepancy), the freeze
    // would clip content or leave a gap; this pins them together. `Screen::demo`
    // exercises every item kind (thoughts, machinery, markers, an error,
    // wrapping assistant text, code) so the agreement holds across them all.
    #[test]
    fn commit_slice_height_agrees_with_the_rows_render_committed_slice_writes() {
        let screen = Screen::demo();
        let width: u16 = 100;
        let count = screen.transcript().items().len();

        let mut cache = RenderCache::new();
        cache.sync(
            screen.transcript(),
            Toggles::default(),
            width - LANE_GUTTER,
            theme::dark(),
        );
        let items: Vec<TranscriptItem> = screen.transcript().items().to_vec();

        let measured = commit_slice_height(&cache, 0, count);
        assert!(measured > 0, "the demo run has content");

        // Draw into a buffer TALLER than the measurement, then count the rows
        // that actually got content. A blank row past the content proves nothing
        // overflowed the measured height; a blank row WITHIN it would mean the
        // draw under-filled what it measured.
        let oversized = measured + 5;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, oversized));
        blit_slice(&mut buf, &cache, &items, 0, count);

        let text = commit_buffer_text(&buf);
        let non_blank = text.lines().filter(|l| !l.trim().is_empty()).count();
        // The demo has interior blank rows (code fences, spacing), so compare the
        // LAST non-blank row's index + 1 against the measured height: the draw
        // occupies exactly `[0, measured)` and writes nothing at/after `measured`.
        let last_non_blank = text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, _)| i)
            .max()
            .expect("some content drew");
        assert!(
            (last_non_blank as u16) < measured,
            "draw wrote past the measured height ({last_non_blank} >= {measured})"
        );
        assert!(
            non_blank > 0 && non_blank <= measured as usize,
            "non-blank rows ({non_blank}) fit within the measured height ({measured})"
        );
        // No content leaked into the oversized tail rows `[measured, oversized)`.
        for y in measured..oversized {
            let row = row_symbols(&buf, y);
            assert!(
                row.trim().is_empty(),
                "row {y} past the measured height must be blank: {row:?}"
            );
        }
    }

    /// One buffer row as its concatenated symbols (test helper for the
    /// measure==draw agreement check).
    fn row_symbols(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).expect("cell in area").symbol())
            .collect()
    }

    // The committed slice honors the high-water offset: committing only the tail
    // `[hw, hw + count)` draws that tail and nothing before it.
    #[test]
    fn render_committed_slice_draws_only_the_requested_range() {
        let mut t = crate::ui::transcript::Transcript::new(Vec::new());
        t.info("EARLIER");
        t.info("LATER");

        let items: Vec<TranscriptItem> = t.items().to_vec();
        let width: u16 = 40;
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), width - LANE_GUTTER, theme::dark());

        // Skip EARLIER (hw = 1), commit only LATER (count = 1).
        let hw = items.len() - 1;
        let height = commit_slice_height(&cache, hw, 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height.max(1)));
        blit_slice(&mut buf, &cache, &items, hw, 1);

        let text = commit_buffer_text(&buf);
        assert!(text.contains("LATER"), "the requested tail drew:\n{text}");
        assert!(
            !text.contains("EARLIER"),
            "items before the high-water mark are not redrawn:\n{text}"
        );
    }

    // THE identity guarantee (ADR-0046): the committed slice for a whole run,
    // and the pending body's rendering of that SAME prefix, produce the
    // IDENTICAL rows - gutter and content - so nothing reflows when an item
    // crosses the commit seam. This is the property `run_fold`'s retirement buys:
    // both paths read the SAME cache lines (no collapse, no window) and paint the
    // SAME two-plane gutter. Uses `Screen::demo()` so the run has thoughts,
    // machinery, markers, an error, closing text and code - every item kind.
    #[test]
    fn the_committed_slice_equals_the_pending_body_for_the_same_prefix() {
        let screen = Screen::demo();
        let width: u16 = 100;
        let count = screen.transcript().items().len();

        // (a) The committed slice `[0, count)` blitted into a bare buffer.
        let mut commit_cache = RenderCache::new();
        commit_cache.sync(
            screen.transcript(),
            Toggles::default(),
            width - LANE_GUTTER,
            theme::dark(),
        );
        let items: Vec<TranscriptItem> = screen.transcript().items().to_vec();
        let height = commit_slice_height(&commit_cache, 0, count);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        blit_slice(&mut buf, &commit_cache, &items, 0, count);
        let committed = commit_buffer_text(&buf);

        // (b) The pending body (hw = 0) drawn TOP-aligned into a zone exactly as
        // tall as the content, so the two are directly comparable row-for-row.
        let terminal = draw_viewport(width, height, &screen);
        let pending: String = (0..height)
            .map(|y| row_text(&terminal, y).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        let committed_trimmed: String = committed
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            committed_trimmed, pending,
            "committed and pending must render the same prefix identically (no seam reflow)"
        );
    }

    // --- render_pending (ADR-0046): bottom-anchor + top-clip -----------------

    /// Draws one full inline pending frame (transcript body + status +
    /// composer) for the given screen into a fresh `width`x`height` terminal.
    fn draw_pending(width: u16, height: u16, screen: &Screen) -> Terminal<TestBackend> {
        let mut cache = RenderCache::new();
        let conn = ConnectionFacts {
            base_url: "http://test".into(),
            model: "m".into(),
        };
        draw_frame(width, height, |f| {
            render_pending(
                f,
                screen,
                &mut cache,
                FrameCtx {
                    conn: conn.view(),
                    anim: Anim::default(),
                    theme: theme::dark(),
                },
            );
        })
    }

    // A short pending stack is anchored to the BOTTOM of its zone: the top rows
    // of the body zone are blank and the content sits just above the status bar.
    #[test]
    fn render_pending_bottom_anchors_a_short_stack() {
        // A fresh screen: only the greeting Info line is pending.
        let screen = Screen::new(ScreenOpts::default());
        let terminal = draw_pending(60, 12, &screen);

        // The greeting wraps to a few rows and anchors to the BOTTOM of the
        // body zone, so the top rows are blank and the content sits low. Find
        // the first non-blank body row: it must be past the top of the zone.
        let first_content = (0..10)
            .find(|&y| !row_text(&terminal, y).trim().is_empty())
            .expect("some content drew");
        assert!(
            first_content > 0,
            "the top of the body zone is blank (bottom-anchored); first content at row {first_content}"
        );
        // The greeting text is present in the drawn body.
        assert!(
            buffer_text(&terminal).contains("suspenders ready"),
            "the greeting drew in the body"
        );
    }

    // An overflowing pending stack is top-clipped: the NEWEST rows survive and
    // the oldest drop off the top (qwen's overflowDirection:"top"), with the
    // `… Ctrl-S to show more` marker on the top row.
    #[test]
    fn render_pending_top_clips_an_overflowing_stack() {
        // Many notice lines overflow a short terminal.
        let screen = Screen::new(ScreenOpts {
            notices: (1..=40).map(|i| format!("notice-{i:02}")).collect(),
            ..ScreenOpts::default()
        });
        let terminal = draw_pending(40, 10, &screen);
        let text = buffer_text(&terminal);

        // The newest notice is on screen; the oldest scrolled off the top.
        assert!(text.contains("notice-40"), "newest kept:\n{text}");
        assert!(
            !text.contains("notice-01"),
            "oldest clipped off the top:\n{text}"
        );
        // The overflow marker is on the top row of the body zone (ADR-0046).
        assert!(
            text.contains("Ctrl-S to show more"),
            "the overflow marker draws:\n{text}"
        );
    }

    /// Draws a composer overlay popup on a 40x12 test terminal with the standard
    /// anchor row (10) and the dark theme, returning the terminal. Covers the
    /// standard popup test shape: fixed geometry, dark theme, anchor row 10.
    fn draw_popup(view: &OverlayView) -> Terminal<TestBackend> {
        draw_frame(40, 12, |f| {
            render_composer_popup(f, 10, f.area(), view, theme::dark())
        })
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
        let text = buffer_text(&draw_popup(&view));
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
        let terminal = draw_popup(&view);
        // Geometry: 2 body rows + borders = height 4, anchored above row 10,
        // so the body sits at rows 7-8; the popup is inset one column and the
        // border one more, so row text starts at x = 2.
        assert!(row_text(&terminal, 8).contains("/clear"));
        assert!(cell_modifier(&terminal, 2, 8).contains(Modifier::REVERSED));
        assert!(!cell_modifier(&terminal, 2, 7).contains(Modifier::REVERSED));
    }

    #[test]
    fn an_empty_menu_popup_shows_no_matches() {
        let view = OverlayView::Menu {
            rows: vec![],
            highlight: 0,
        };
        assert!(buffer_text(&draw_popup(&view)).contains("no matches"));
    }

    #[test]
    fn a_loading_selector_popup_shows_the_loading_line() {
        let view = OverlayView::Selector {
            command: "model".to_string(),
            status: OverlayStatus::Loading,
            rows: vec![],
            highlight: 0,
        };
        let text = buffer_text(&draw_popup(&view));
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
        assert!(buffer_text(&draw_popup(&view)).contains("failed: connection refused"));
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
        let text = buffer_text(&draw_popup(&view));
        assert!(text.contains(" models "));
        assert!(text.contains("qwen/qwen3-30b"));
        assert!(text.contains("meta/llama-3.1"));
    }

    #[test]
    fn a_collapsed_row_draws_muted_without_the_headers_bold() {
        let view = OverlayView::Selector {
            command: "model".to_string(),
            status: OverlayStatus::Ready,
            rows: vec![
                SelectorRow::header("openrouter"),
                SelectorRow::collapsed("openrouter/kimi-k2"),
            ],
            highlight: 0,
        };
        let terminal = draw_popup(&view);
        // Geometry as above: 2 body rows + borders = height 4 above row 10,
        // so the header sits at row 7 and the collapsed row at 8, text from
        // x = 2.
        assert!(cell_modifier(&terminal, 2, 7).contains(Modifier::BOLD));
        assert!(
            !cell_modifier(&terminal, 2, 8).contains(Modifier::BOLD),
            "a collapsed member reads as a greyed model, not a header"
        );
        let buf = terminal.backend().buffer();
        let header_fg = buf.cell((2u16, 7u16)).expect("header cell").style().fg;
        let collapsed_fg = buf.cell((2u16, 8u16)).expect("collapsed cell").style().fg;
        assert_eq!(collapsed_fg, header_fg, "both muted");
    }

    #[test]
    fn a_highlighted_note_draws_reversed_like_the_cursor_stop_it_is() {
        let view = OverlayView::Selector {
            command: "model".to_string(),
            status: OverlayStatus::Ready,
            rows: vec![
                SelectorRow::header("openrouter"),
                SelectorRow::note("  unavailable", Some("set OPENROUTER_API_KEY".into())),
            ],
            highlight: 1,
        };
        let terminal = draw_popup(&view);
        // Geometry as above: body rows 7 (header) and 8 (note), text at x = 2.
        assert!(
            !cell_modifier(&terminal, 2, 7).contains(Modifier::REVERSED),
            "the cursor can never rest on a header"
        );
        assert!(cell_modifier(&terminal, 2, 8).contains(Modifier::REVERSED));
    }

    #[test]
    fn a_capped_group_fits_the_popup_window_when_its_note_is_highlighted() {
        use crate::ui::selector::{COLLAPSED_REVEAL_CAP, Selector};
        use crate::view_model::RowRole;

        // The reachability contract behind note-last ordering and the cap:
        // the popup window shows POPUP_MAX_ROWS body rows ending at the
        // highlight (composer::first_visible_row), and a fully-capped group
        // is header + COLLAPSED_REVEAL_CAP rows + note - so the cursor
        // resting on the trailing note pulls the whole group into view.
        let mut rows = vec![SelectorRow::header("openrouter")];
        rows.extend((0..300).map(|i| SelectorRow::collapsed(format!("openrouter/m{i:03}"))));
        rows.push(SelectorRow::note(
            "  unavailable",
            Some("set OPENROUTER_API_KEY".into()),
        ));
        let s = Selector::new(rows);
        let highlight = s.highlight("openrouter");
        let view = s.filtered("openrouter");
        assert_eq!(
            view[highlight].row.role,
            RowRole::Note,
            "snapped to the note"
        );
        assert_eq!(view.len(), 1 + COLLAPSED_REVEAL_CAP + 1);
        assert!(view.len() <= POPUP_MAX_ROWS as usize);
        let top = composer::first_visible_row(highlight, POPUP_MAX_ROWS as usize);
        assert_eq!(top, 0, "the group's header is the window's first row");
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
        let text = buffer_text(&draw_popup(&view));
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

    // --- pending body: layout, the lane gutter, streaming -------------------

    fn screen_with_notices(notices: Vec<String>) -> Screen {
        Screen::new(ScreenOpts {
            notices,
            ..ScreenOpts::default()
        })
    }

    /// Builds a screen that has submitted `prompt`, started message 1, and
    /// received one in-flight thinking update with `thinking_text`. The caller
    /// continues from here (settle, nudge, draw). Shared by tests that need a
    /// screen-with-live-thought setup to avoid the submitted+message_start+
    /// message_update triple repeating (FRAGMENT DRY-003 fix).
    fn screen_with_thinking(prompt: &str, thinking_text: impl Into<String>) -> Screen {
        let (screen, _) = screen_with_notices(vec![]).submitted(prompt, Ok(()));
        let thinking = thinking_text.into();
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Thinking("t".to_string()),
            vec![ContentBlock::Thinking { text: thinking }],
        ));
        screen
    }

    #[test]
    #[ignore = "manual: cargo nextest run dump_demo_render --run-ignored all --no-capture"]
    fn dump_demo_render() {
        let screen = Screen::demo();
        let terminal = draw_viewport(100, 70, &screen);
        let mut out = String::from("\n");
        for y in 0..70 {
            // Bracket the leftmost 2 gutter columns so the spine / caret / blank
            // is unambiguous, then the content.
            let row = row_text(&terminal, y);
            let split = row.char_indices().nth(2).map_or(row.len(), |(i, _)| i);
            let (gutter, rest) = row.split_at(split);
            out.push_str(&format!("{y:>2}|{gutter}|{}\n", rest.trim_end()));
        }
        eprintln!("{out}");
    }

    // The non-interactive smoke for the `diff-demo` binary: the seeded
    // `Screen::demo_diffs()` renders through the real pending-body path (the same
    // one a live inline frame uses) without panicking, in BOTH diff-fold states -
    // collapsed (each diff a fold-title one-liner, the app's default) and
    // expanded (Ctrl-O / the binary's `o` key: the code rows and the diff's own
    // elided tail). The binary only adds the terminal lifecycle on top of this.
    #[test]
    fn the_diff_demo_screen_renders_its_diffs_without_panicking() {
        // Collapsed (default): the lane opens and each diff shows its fold title.
        let collapsed = buffer_text(&draw_viewport(100, 70, &Screen::demo_diffs()));
        assert!(
            collapsed.contains("clean up the tokenizer"),
            "the request:\n{collapsed}"
        );
        for title in [
            "edit_file src/lexer.rs",
            "src/greet.js",
            "package.json",
            "src/generated.js",
        ] {
            assert!(collapsed.contains(title), "the {title} title:\n{collapsed}");
        }

        // Expanded (Ctrl-O): the code rows and the capped diff's elision tail.
        let (expanded_screen, _) = Screen::demo_diffs().handle_key(Key::ToggleTools);
        let expanded = buffer_text(&draw_viewport(100, 70, &expanded_screen));
        assert!(
            expanded.contains("split_whitespace"),
            "the rust hunk body:\n{expanded}"
        );
        assert!(
            expanded.contains("Greets a user by name"),
            "the jsdoc body:\n{expanded}"
        );
        assert!(
            expanded.contains("37 more lines"),
            "the elided tail:\n{expanded}"
        );
    }

    #[test]
    fn the_demo_render_matches_the_confirmed_full_content_run_shape() {
        // The demo is the living spec (ADR-0040/0046): the pending body renders
        // each item in FULL, identically to the frozen committed slice - NO
        // run-level thought fold, NO machinery window (qwen's `<Static>` prints
        // history un-clamped). So every thought shows (each a one-line cached
        // `✦ thought:` under Ctrl-T's default collapse) and every tool one-liner
        // shows. Pin the load-bearing rows so a reflow regression trips here, not
        // only in a manual dump. Rows are `(gutter, content)`.
        let terminal = draw_viewport(100, 70, &Screen::demo());
        let split = |y: u16| -> (String, String) {
            let row = row_text(&terminal, y);
            let at = row.char_indices().nth(2).map_or(row.len(), |(i, _)| i);
            let (g, r) = row.split_at(at);
            (g.to_string(), r.trim_end().to_string())
        };
        // The user prompt breaks to the caret at the margin.
        assert_eq!(split(1), ("› ".into(), "evaluate this project".into()));
        // Assistant text is flush under the spine.
        assert_eq!(split(2).0, "│ ");
        assert!(split(2).1.starts_with("I'll evaluate this project"));
        // The FIRST thought now shows (not folded away): a one-line cached
        // `✦ thought:` under the spine.
        assert_eq!(split(4).0, "│ ");
        assert!(
            split(4)
                .1
                .starts_with("✦ thought: The user wants me to evaluate"),
            "the first thought shows in full-content mode: {:?}",
            split(4).1
        );
        // Every tool one-liner shows (no `⋯ N earlier actions` elision): the
        // first list_files action is now a real row, indented two columns.
        assert_eq!(split(5).0, "│ ");
        assert!(
            split(5).1.starts_with("  ⋯ list_files"),
            "machinery shows in full, not windowed: {:?}",
            split(5).1
        );
        // A governing marker indents two columns; its wrapped continuation stays
        // indented (task 1: the wrap-indent fix).
        assert_eq!(split(13).0, "│ ");
        assert!(split(13).1.starts_with("  » [reading file after file"));
        assert_eq!(split(14).0, "│ ");
        assert!(
            split(14).1.starts_with("  instead;"),
            "wrapped marker stays indented: {:?}",
            split(14).1
        );
        // The error tool result breaks out (always shown), indented two columns,
        // with the ⚙ gutter.
        assert_eq!(split(23).0, "│ ");
        assert!(split(23).1.starts_with("  ⚙ run_command"));
        assert!(split(23).1.contains("command denied"));
        // Assistant text after the tools is flush again.
        assert_eq!(split(24).0, "│ ");
        assert!(split(24).1.starts_with("The project is a well-structured"));
        // Code breaks out, inset two columns, under the spine.
        assert_eq!(split(27).0, "│ ");
        assert!(split(27).1.contains("fn tokenize"));
    }

    #[test]
    fn the_lane_spine_is_dense_and_continuous() {
        // The lane has NO per-item blank separator: every content row of the
        // run (from the first assistant line through the last tool line) carries
        // the `│` spine, with no bare gap rows breaking it into segments. Rows
        // 2..=23 are the agent's run (assistant, thoughts, machinery, markers,
        // error) in full-content mode (ADR-0046).
        let terminal = draw_viewport(100, 70, &Screen::demo());
        for y in 2..=23u16 {
            let row = row_text(&terminal, y);
            assert!(
                row.starts_with('│'),
                "row {y} broke the dense spine: {row:?}"
            );
        }
    }

    /// Draws the inline pending body with a caller-supplied [`Anim`] (for the
    /// lull-row test) TOP-aligned, like [`draw_viewport`].
    fn draw_viewport_anim(
        width: u16,
        height: u16,
        screen: &Screen,
        anim: Anim,
    ) -> Terminal<TestBackend> {
        let mut cache = RenderCache::new();
        draw_frame(width, height, |f| {
            let area = f.area();
            let total = pending_body_height(screen, &mut cache, area.width, theme::dark());
            let zone_h = (total as u16).min(area.height).max(1);
            let zone = Rect {
                height: zone_h,
                ..area
            };
            render_pending_body_at(
                f,
                zone,
                &mut PendingBodyParams {
                    screen,
                    cache: &mut cache,
                    anim,
                },
                theme::dark(),
                0,
            );
        })
    }

    #[test]
    fn the_pending_body_draws_the_transcript() {
        let screen = screen_with_notices(vec!["a launch notice".to_string()]);
        let terminal = draw_viewport(80, 20, &screen);
        let text = buffer_text(&terminal);
        assert!(text.contains("suspenders ready"), "the greeting:\n{text}");
        assert!(text.contains("a launch notice"));
    }

    // The lull row draws through the pending render path: a Running Run with
    // nothing streaming, quiet past the settle window, paints the timer into the
    // buffer as a third live entry under the running lane.
    #[test]
    fn the_pending_body_draws_the_lull_row_when_running_and_quiet() {
        let (screen, _) = screen_with_notices(vec!["a launch notice".to_string()])
            .apply_event(Event::run_started("r1"));
        assert!(
            !screen.has_live_stream(),
            "the Turn runs but nothing streams"
        );
        let anim = Anim {
            quiet_ticks: lull::SETTLE_TICKS + 4,
            lull_seq: 0,
            ..Default::default()
        };
        let terminal = draw_viewport_anim(80, 20, &screen, anim);
        let text = buffer_text(&terminal);
        assert!(text.contains("5s"), "the lull timer opens at 5s:\n{text}");
    }

    // An overflowing pending body top-clips (ADR-0046): the tail (newest) is on
    // screen and the top is dropped. There is no scrollbar - native scrollback
    // owns history.
    #[test]
    fn an_overflowing_pending_body_top_clips_and_keeps_the_tail() {
        let notices: Vec<String> = (0..30).map(|i| format!("notice line {i:02}")).collect();
        let screen = screen_with_notices(notices);
        let terminal = draw_viewport(40, 8, &screen);
        let text = buffer_text(&terminal);
        // The tail is on screen, the top is clipped.
        assert!(text.contains("notice line 29"));
        assert!(!text.contains("notice line 00"));
    }

    #[test]
    fn lane_gutters_derives_the_spine_from_user_boundaries() {
        // Before the first User the region is spineless (Blank); a User opens a
        // lane (User caret) and every item until the next User hangs off it
        // (Spine). A second User opens a fresh lane. The lane is the request,
        // not the Run - agent items with no intervening User stay on the spine.
        let items = vec![
            TranscriptItem::Info {
                text: "greeting".into(),
            },
            TranscriptItem::User {
                text: "first".into(),
            },
            TranscriptItem::Thinking { text: "hm".into() },
            TranscriptItem::Assistant {
                text: "answer".into(),
            },
            TranscriptItem::User {
                text: "second".into(),
            },
            TranscriptItem::Assistant {
                text: "reply".into(),
            },
        ];
        assert_eq!(
            lane_gutters(&items),
            vec![
                GutterKind::Blank,
                GutterKind::User,
                GutterKind::Spine,
                GutterKind::Spine,
                GutterKind::User,
                GutterKind::Spine,
            ]
        );
    }

    // ---- wrap_words / indented_lines: the hanging block indent (task 1) ----

    #[test]
    fn wrap_words_greedily_wraps_on_spaces_within_width() {
        assert_eq!(
            wrap_words("the quick brown fox", 9),
            vec!["the quick", "brown fox"]
        );
        // Every segment fits the width.
        for seg in wrap_words("the quick brown fox jumps over", 9) {
            assert!(seg.chars().count() <= 9, "segment over width: {seg:?}");
        }
    }

    #[test]
    fn wrap_words_hard_splits_a_word_longer_than_the_width() {
        // A single 10-char word at width 4 splits into 4+4+2, never overflowing.
        let segs = wrap_words("abcdefghij", 4);
        assert_eq!(segs, vec!["abcd", "efgh", "ij"]);
        for seg in &segs {
            assert!(seg.chars().count() <= 4);
        }
    }

    #[test]
    fn indented_lines_prefixes_every_wrapped_row_at_the_indent() {
        // A long machinery line wraps; EVERY resulting row (first + continuation)
        // is prefixed with the 2-space block indent and stays <= content width.
        let style = Style::default();
        let content = "⋯ list_files docs/adr with a very long trailing summary here";
        let width = 20u16;
        let lines = indented_lines(content, 2, width, style);
        assert!(lines.len() > 1, "the long line wrapped");
        for line in &lines {
            let text = line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>();
            assert!(text.starts_with("  "), "row not indented: {text:?}");
            assert!(
                text.chars().count() <= width as usize,
                "row over content width: {text:?}"
            );
        }
    }

    #[test]
    fn a_wrapped_marker_continuation_lands_at_column_two_in_the_render() {
        // Task 1 acceptance: a long marker soft-wraps and its continuation must
        // sit at column 2 (block indent), not fall back to column 0. Render the
        // real path and inspect the marker's second visual row.
        let long = "reading file after file fills your context dispatch \
                    a focused search instead and let a helper report back with just \
                    the answer you actually need to keep moving";
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.submitted("go", Ok(()));
        let (screen, _) = screen.apply_event(Event::compaction_progress(long));
        let terminal = draw_viewport(60, 20, &screen);
        // The marker's FIRST row carries the compaction glyph `⟨`; its
        // continuation is the next row.
        let marker_y = (0..20)
            .find(|&y| row_text(&terminal, y).contains('⟨'))
            .expect("the marker row");
        let cont = row_text(&terminal, marker_y + 1);
        // Columns are per-cell symbols. Column 0 is the spine `│`, column 1 the
        // trailing gutter space; the content area begins at column LANE_GUTTER.
        // The marker's own 2-space block indent means the content-area columns 0
        // and 1 are blank on the continuation - it is NOT flush at the margin.
        let cols: Vec<char> = cont.chars().collect();
        assert_eq!(cols[0], '│', "the continuation keeps the spine");
        let g = LANE_GUTTER as usize;
        assert_eq!(
            (cols[g], cols[g + 1]),
            (' ', ' '),
            "marker continuation not block-indented (content cols 0-1 not blank): {cont:?}"
        );
        // ...and real content follows the indent (not a fully blank row).
        assert!(
            cols[g + 2] != ' ',
            "the continuation should carry text after the indent: {cont:?}"
        );
    }

    // ---- collapsed_thought_line: one-row truncation ------------------------

    #[test]
    fn collapsed_thought_line_truncates_to_one_visual_row() {
        let long = "z".repeat(400);
        let line = collapsed_thought_line(&long, 40, theme::dark());
        let text = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.starts_with("✦ thought: "));
        assert!(
            text.chars().count() <= 40,
            "one row: {}",
            text.chars().count()
        );
        assert!(text.ends_with('…'), "truncated: {text:?}");
    }

    #[test]
    fn collapsed_thought_line_takes_the_first_source_line_only() {
        let line = collapsed_thought_line("one\ntwo\nthree", 60, theme::dark());
        let text = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert_eq!(text, "✦ thought: one");
    }

    #[test]
    fn the_reserved_gutter_forces_wrapping_at_the_reduced_content_width() {
        // RED-1 (ADR-0029): the lane gutter is carved off the left, so content
        // wraps in the narrower `content_area` and is DRAWN two columns in. This
        // pins the reservation with a notice sized to wrap ONLY at the reduced
        // width - it fits the full area but overflows the content area
        // (ADR-0046: there is no scrollbar column now, so the only reservation
        // is the lane gutter):
        //
        //   area.width 40 → content 38 (gutter). A 39-char word fits in 40 but
        //   must wrap in 38.
        //
        // Two facts both DEPEND on the 2-col reservation, so deleting LANE_GUTTER
        // from the wrap width (measuring/drawing at 40) breaks both: the word
        // (1) draws starting at column LANE_GUTTER, not column 0, and (2) wraps
        // to a second row instead of fitting on one. A single 39-char token has
        // no break point, so it wraps only because the width shrank.
        let word = "x".repeat(39);
        let screen = Screen::new(ScreenOpts {
            notices: vec![word.clone()],
            ..ScreenOpts::default()
        });
        let terminal = draw_viewport(40, 20, &screen);
        // A local cache synced at the same content width the body drew at, for
        // the wrapped-count assertion below.
        let mut cache = RenderCache::new();
        cache.sync(
            screen.transcript(),
            Toggles::default(),
            40 - LANE_GUTTER,
            theme::dark(),
        );

        // (1) The notice is drawn two columns in: the first row carrying the
        // word begins with exactly LANE_GUTTER blank gutter cells (the notice is
        // pre-lane, so the gutter is blank, not a spine), then the x's.
        let word_row = (0..20)
            .map(|y| row_text(&terminal, y))
            .find(|r| r.contains('x'))
            .expect("the notice row");
        let indent = word_row.chars().take_while(|c| *c == ' ').count();
        assert_eq!(
            indent, LANE_GUTTER as usize,
            "content draws at the reserved gutter offset, not column 0: {word_row:?}"
        );

        // (2) The 39-char word wrapped to exactly 2 visual content rows, which
        // happens ONLY at the reduced 38-col width (the lane is dense - no
        // trailing separator). At the un-reserved 40 cols it would be one row.
        let word_rows: usize = cache
            .settled()
            .find_map(|(lines, wrapped)| {
                lines
                    .iter()
                    .any(|l| l.spans.iter().any(|s| s.content.contains('x')))
                    .then_some(wrapped)
            })
            .expect("the notice's cached entry");
        assert_eq!(
            word_rows, 2,
            "the word wrapped to 2 rows at the reduced width, got {word_rows}"
        );
    }

    #[test]
    fn a_user_prompt_breaks_to_the_caret_and_the_agent_run_hangs_off_the_spine() {
        // The greeting (pre-lane) is spineless; a User prompt shows the `›`
        // caret at the margin; the agent's answer in that run shows the `│`
        // spine. Column 0 carries the gutter glyph.
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.submitted("do the thing", Ok(()));
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_end(
            vec![ContentBlock::text("done")],
            StopReason::EndTurn,
        ));
        let terminal = draw_viewport(40, 20, &screen);
        // Gather the first column of every row and the text, to find each line.
        let mut saw_caret_on_user = false;
        let mut saw_spine_on_answer = false;
        let mut saw_blank_on_greeting = false;
        for y in 0..20 {
            let row = row_text(&terminal, y);
            let first = row.chars().next();
            if row.contains("do the thing") {
                saw_caret_on_user = first == Some('›');
            }
            if row.contains("done") {
                saw_spine_on_answer = first == Some('│');
            }
            if row.contains("suspenders ready") {
                saw_blank_on_greeting = first == Some(' ');
            }
        }
        assert!(saw_caret_on_user, "user prompt caret at the margin");
        assert!(saw_spine_on_answer, "agent answer hangs off the spine");
        assert!(saw_blank_on_greeting, "the greeting is spineless");
    }

    #[test]
    fn the_spine_stays_aligned_with_the_answer_when_top_clipped() {
        // M3 (ADR-0046 inline variant): with a multi-row agent answer that
        // OVERFLOWS the pending body, the top-clip keeps the newest rows and
        // drops the oldest; every VISIBLE answer row must still carry the `│`
        // spine in column 0. This exercises the `skip(top)` slice of the flat
        // row mapping - a desync (gutter indexed differently from content) would
        // land the spine off the answer rows and trip an assertion below.
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.submitted("the question", Ok(()));
        // A tall answer: many SHORT paragraphs (each one visual row at width 40,
        // so no soft-wrap) each carrying a unique "ANSWER" marker.
        let answer = (0..14)
            .map(|i| format!("ANSWER-{i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_end(
            vec![ContentBlock::text(&answer)],
            StopReason::EndTurn,
        ));

        // A short body zone forces the top-clip.
        let terminal = draw_viewport(40, 10, &screen);
        let mut answer_rows_seen = 0;
        for y in 0..10 {
            let row = row_text(&terminal, y);
            if row.contains("ANSWER") {
                answer_rows_seen += 1;
                assert_eq!(
                    row.chars().next(),
                    Some('│'),
                    "answer row {y} lost its spine after the top-clip: {row:?}"
                );
            }
        }
        assert!(answer_rows_seen >= 2, "several answer rows must be visible");
        // The top-clip kept the NEWEST answer paragraph; the oldest dropped.
        let text = buffer_text(&terminal);
        assert!(text.contains("ANSWER-13"), "the tail (newest) is kept");
        assert!(!text.contains("ANSWER-0"), "the oldest clipped off the top");
    }

    /// Vinnie's `evaluate this project` shape (~60 cols): a User prompt, a long
    /// settled thought that would wrap, a wrapping in-lane marker, and a tool
    /// call. Returns the rendered terminal.
    fn evaluate_project_screen(width: u16, height: u16) -> Terminal<TestBackend> {
        // A long Compaction marker (`⟨ compaction: ... → summary ⟩`) that soft-
        // wraps at 60 cols, standing in for the wrapping in-lane marker this
        // shape exercises.
        let status = "reading the manifest and the entry point and every other file \
                     that could plausibly appear so the marker wraps across several \
                     visual rows here";
        let thinking = "I should read the manifest and the entry point and the tests \
                        and then form a plan about what to evaluate first here";
        let screen = screen_with_thinking("evaluate this project", thinking);
        // Settle the thought (empty final content → thinking materializes).
        let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));
        // A wrapping in-lane Housekeeping marker (the `⟨ compaction: ... ⟩` line).
        let (screen, _) = screen.apply_event(Event::compaction_progress(status));
        let (screen, _) = screen.apply_event(Event::tool_call(
            "id1",
            "read_file",
            serde_json::json!({"path": "Cargo.toml"}),
        ));
        draw_viewport(width, height, &screen)
    }

    #[test]
    fn a_long_settled_thought_collapses_to_one_visual_row() {
        // Symptom 1: a long newline-free thought must fold to ONE visual row,
        // not soft-wrap to many. The collapsed line truncates to the content
        // width with a trailing `…`.
        let long = "z".repeat(400);
        let screen = screen_with_thinking("q", long);
        let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));
        let terminal = draw_viewport(60, 20, &screen);
        // Exactly one row carries the collapsed thought, and it is truncated.
        let thought_rows: Vec<String> = (0..20)
            .map(|y| row_text(&terminal, y))
            .filter(|r| r.contains("thought:"))
            .collect();
        assert_eq!(
            thought_rows.len(),
            1,
            "the thought folds to one row: {thought_rows:?}"
        );
        assert!(
            thought_rows[0].contains('…'),
            "truncated: {:?}",
            thought_rows[0]
        );
        // The z's did not spill onto a second row.
        let z_rows = (0..20)
            .filter(|&y| row_text(&terminal, y).contains('z'))
            .count();
        assert_eq!(z_rows, 1, "the long thought did not wrap to more rows");
    }

    #[test]
    fn settled_thinking_uses_the_star_glyph_not_the_brain_emoji() {
        // Symptom 3: settled thinking unifies on the `✦` family with the live
        // tail, and drops the width-2 `🧠` emoji.
        let collapsed = message_lines(
            &TranscriptItem::Thinking {
                text: "a short thought".into(),
            },
            false,
            false,
            80,
            theme::dark(),
        );
        assert_eq!(line_text(&collapsed[0]), "✦ thought: a short thought");
        assert!(!line_text(&collapsed[0]).contains('🧠'));

        let expanded = message_lines(
            &TranscriptItem::Thinking {
                text: "line one\nline two".into(),
            },
            true,
            false,
            80,
            theme::dark(),
        );
        assert_eq!(line_text(&expanded[0]), "✦ thought:");
        assert!(!line_text(&expanded[0]).contains('🧠'));
    }

    #[test]
    fn every_in_lane_visual_row_keeps_its_spine_in_the_evaluate_shape() {
        // Symptom 2, TestBackend reproduction: the `evaluate this project` shape
        // at 60 cols - a wrapped thought, a wrapped machinery marker, a tool call.
        // In TestBackend EVERY in-lane visual row (including soft-wrapped
        // continuations of the marker and the machinery line) keeps its `│`
        // spine at column 0 and its content stays inside content_area. So the
        // screenshot's column-0 fallback does NOT reproduce here - it is a
        // terminal glyph-width artifact (the width-2 `🧠` emoji, now `✦`), not a
        // gutter/content desync. If a real desync regressed expand_gutters, an
        // in-lane content row would lose its spine and trip this.
        let terminal = evaluate_project_screen(60, 24);
        for y in 0..24 {
            let row = row_text(&terminal, y);
            let first = row.chars().next();
            // Rows carrying agent content (the thought, the `⟨ compaction …`
            // marker and its wrapped body, the `⋯` machinery) are all in-lane
            // and must start with the spine.
            let is_agent_content = row.contains("thought:")
                || row.contains('⟨')
                || row.contains('⋯')
                || row.contains("compaction:")
                || row.contains("every other file");
            if is_agent_content {
                assert_eq!(first, Some('│'), "row {y} lost its spine: {row:?}");
            }
        }
    }

    #[test]
    fn a_streaming_thinking_snapshot_draws_the_animated_header_and_reasoning_tail() {
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Thinking("pondering".to_string()),
            vec![ContentBlock::Thinking {
                text: "pondering the viewport".to_string(),
            }],
        ));
        let terminal = draw_viewport(80, 20, &screen);
        let text = buffer_text(&terminal);
        // Live reasoning is content, not a metric (ADR-0040): the animated
        // `✦ Thinking` header sits above the reasoning tail, and the reasoning
        // text itself is shown - not a token count.
        assert!(text.contains("✦ Thinking"), "the header:\n{text}");
        assert!(
            text.contains("pondering the viewport"),
            "the reasoning tail:\n{text}"
        );
        assert!(!text.contains("tokens)"));
    }

    #[test]
    fn the_reasoning_tail_shows_only_the_last_rows_under_the_header() {
        // The rolling tail is the last THINKING_TAIL_ROWS source rows; older
        // reasoning scrolls off the top of the sub-block.
        let reasoning = "row one\nrow two\nrow three\nrow four\nrow five";
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Thinking("…".to_string()),
            vec![ContentBlock::Thinking {
                text: reasoning.to_string(),
            }],
        ));
        let terminal = draw_viewport(80, 20, &screen);
        let text = buffer_text(&terminal);
        assert!(text.contains("row three") && text.contains("row five"));
        // "row one"/"row two" scrolled off the three-row tail.
        assert!(!text.contains("row one") && !text.contains("row two"));
    }

    #[test]
    fn a_long_reasoning_line_is_truncated_so_the_tail_stays_bounded() {
        // SHOULD-3: one very long unwrapped reasoning line would soft-wrap to
        // many visual rows and let the "short tail" fill the viewport. The tail
        // truncates each source row to the content width so it stays one visual
        // row (marked with `…`), keeping the sub-block to header + N rows.
        let long = "z".repeat(400); // far wider than any terminal
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Thinking("…".to_string()),
            vec![ContentBlock::Thinking { text: long }],
        ));
        let terminal = draw_viewport(40, 20, &screen);
        // Exactly one row carries the reasoning z's, and it ends in the `…`
        // truncation marker - the long line did not balloon into many rows.
        let z_rows: Vec<String> = (0..20)
            .map(|y| row_text(&terminal, y))
            .filter(|r| r.contains('z'))
            .collect();
        assert_eq!(
            z_rows.len(),
            1,
            "the long line stays one visual row: {z_rows:?}"
        );
        assert!(z_rows[0].contains('…'), "it is truncated: {:?}", z_rows[0]);
    }

    #[test]
    fn in_flight_assistant_text_renders_as_the_streaming_tail() {
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Text("a streaming reply".to_string()),
            vec![ContentBlock::text("a streaming reply")],
        ));
        let terminal = draw_viewport(80, 20, &screen);
        assert!(buffer_text(&terminal).contains("a streaming reply"));
    }

    // -----------------------------------------------------------------------
    // normalize_diff_text: tab -> two spaces, the only text rule a diff code
    // line needs (the tint band fills empty lines, so no empty -> space trick);
    // render tests pin the VISIBLE output, these pin the TEXT rule.
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_diff_text_replaces_tabs_with_two_spaces() {
        assert_eq!(normalize_diff_text("a\tb"), "a  b");
    }

    #[test]
    fn normalize_diff_text_leaves_ordinary_text_unchanged() {
        assert_eq!(normalize_diff_text("hello world"), "hello world");
    }

    // -----------------------------------------------------------------------
    // LaneStyles::cell_style: the Blank -> None, Caret/Spine -> Some mapping; the
    // returned style is the concrete lane style (not a default).
    // -----------------------------------------------------------------------

    fn test_lane_styles() -> LaneStyles {
        LaneStyles {
            caret: Style::default().fg(Color::Green),
            spine: Style::default().fg(Color::Blue),
        }
    }

    #[test]
    fn gutter_cell_style_blank_returns_none() {
        assert_eq!(test_lane_styles().cell_style(RowGutter::Blank), None);
    }

    #[test]
    fn gutter_cell_style_caret_returns_caret_style() {
        let styles = test_lane_styles();
        assert_eq!(styles.cell_style(RowGutter::Caret), Some(styles.caret));
    }

    #[test]
    fn gutter_cell_style_spine_returns_spine_style() {
        let styles = test_lane_styles();
        assert_eq!(styles.cell_style(RowGutter::Spine), Some(styles.spine));
    }

    // -----------------------------------------------------------------------
    // popup_rect: the popup geometry is the only pure math that was tangled
    // into render_composer_popup; tested here at the calculation level.
    // -----------------------------------------------------------------------

    #[test]
    fn popup_rect_height_is_body_plus_two_borders() {
        let area = Rect::new(0, 0, 80, 24);
        let r = popup_rect(10, area, 3); // 3 body rows -> height 5
        assert_eq!(r.height, 5);
    }

    #[test]
    fn popup_rect_height_is_capped_at_popup_max_plus_two() {
        let area = Rect::new(0, 0, 80, 24);
        // 100 body rows would be POPUP_MAX_ROWS + 2 once capped.
        let r = popup_rect(20, area, 100);
        assert_eq!(r.height, POPUP_MAX_ROWS + 2);
    }

    #[test]
    fn popup_rect_is_anchored_above_anchor_y() {
        let area = Rect::new(0, 0, 80, 24);
        let r = popup_rect(10, area, 3); // height 5, y = 10 - 5 = 5
        assert_eq!(r.y, 5);
    }
}
