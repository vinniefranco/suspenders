//! UI Components - the SINGLE mapping from the semantic display vocabulary
//! (ADR-0008) to ratatui `Style`/`Color`, plus the render helpers the frontend
//! draws with.
//!
//! This is the one place semantics become terminal colors: [`LineStyle`] →
//! color for a Block's lines, [`PressureLevel`] → color/emphasis for the status
//! bar. Extensions and the Screen core never touch ratatui; they speak the
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
use crate::ui::lull;
use crate::ui::markdown::{self, MdLine, MdStyle};
use crate::ui::picker::Picker;
use crate::ui::screen::{PressureLevel, Screen, Status};
use crate::ui::selector::{RowRole, SelectorRow};
use crate::ui::slash;
use crate::ui::theme::{self, Theme};
use crate::ui::transcript::{LineStyle, StyledLine, Tone, TranscriptItem};
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
/// (ADR-0008). Extensions produce styles; this runs them into the active
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
/// Splits `area` into the three vertical frame zones: `[viewport, status_bar,
/// composer]`. `composer_rows` is the already-capped Composer row count (see
/// [`composer::max_visible_rows`]). Pure - no frame access.
fn frame_chunks(area: Rect, composer_rows: usize) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                       // transcript viewport
            Constraint::Length(1),                    // status bar
            Constraint::Length(composer_rows as u16), // composer (grows with the draft)
        ])
        .split(area)
}

/// The Composer's visible row count for this frame: the layout's row count
/// capped by [`composer::max_visible_rows`] so a very tall draft never starves
/// the transcript viewport. Pure - no frame access.
fn capped_composer_height(layout: &ComposerLayout, frame_height: usize) -> usize {
    layout
        .rows
        .len()
        .min(composer::max_visible_rows(frame_height))
}

pub fn render(
    frame: &mut Frame,
    t: &Screen,
    conn: ConnectionView,
    anim: Anim,
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
    let composer_height = capped_composer_height(&layout, area.height as usize);
    let chunks = frame_chunks(area, composer_height);

    // The viewport renders FIRST: the status bar's position segment reads the
    // measured geometry (and the Viewport's clamped top) from this frame, not
    // a stale one.
    let geometry = render_viewport(
        frame,
        chunks[0],
        &mut ViewportParams {
            screen: t,
            viewport,
            cache,
            anim,
        },
        theme,
    );
    render_status_bar(
        frame,
        chunks[1],
        StatusBarCtx {
            screen: t,
            conn,
            viewport,
            geometry,
        },
        theme,
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

/// The scroll state and cache the viewport render needs each frame. Bundled so
/// [`render_viewport`] takes four args instead of six (SRP_PARAMS fix): `frame`,
/// `area`, `params`, and `theme` is the reduced call shape.
pub struct ViewportParams<'a> {
    pub screen: &'a Screen,
    pub viewport: &'a Viewport,
    pub cache: &'a mut RenderCache,
    pub anim: Anim,
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
    params: &mut ViewportParams<'_>,
    theme: &Theme,
) -> (usize, usize) {
    let t = params.screen;
    let viewport = params.viewport;
    let cache = &mut params.cache;
    let anim = params.anim;
    // The rightmost column is ALWAYS the scrollbar gutter, occupied or not:
    // reserving it only when the scrollbar shows would make the wrap width
    // depend on the line count and the line count on the wrap width.
    let text_area = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    // The leftmost LANE_GUTTER columns are ALWAYS reserved for the run-lane
    // spine / user caret (ADR-0040), occupied or not - the same unconditional
    // reservation as the scrollbar column, and for the same reason: content
    // wraps in the narrower `content_area`, so the wrap width never depends on
    // whether a given row carries a spine. Everything below (`cache.sync`, the
    // live tail's `wrapped_count`) MUST measure at `content_area.width` so
    // measuring and drawing agree exactly (ADR-0029, the load-bearing gutter
    // invariant).
    let content_area = Rect {
        x: text_area.x + LANE_GUTTER,
        width: text_area.width.saturating_sub(LANE_GUTTER),
        ..text_area
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

    // The live streaming snapshot renders below the settled items: the
    // animated `✦ Thinking` header + a rolling reasoning tail (rebuilt each
    // frame - the tail's window is non-monotonic so it is NOT cached; a few
    // lines are cheap) and the streaming markdown (cached - see
    // [`RenderCache::sync`]). `streaming_thinking()` stays whole in the store;
    // the last-N windowing is a display policy, so it lives here (ADR-0029).
    let thinking = t.transcript().streaming_thinking();
    let thinking_lines = live_thinking_lines(&thinking, anim.spinner, content_area.width, theme);

    // One (lines, wrapped-count, gutter-kind) entry per window "item": every
    // KEPT settled message, then the live tail - a single indexing shared by the
    // window selection, the slice assembly, and the per-visual-row gutter mapping
    // below. The lane is DERIVED here at render time (ADR-0040), never stored and
    // never in the RenderCache key: `lane_gutters` walks the settled items in
    // order, and both live entries (the reasoning tail, the streaming answer)
    // hang off the running Run's lane, so they take the spine. The lane is dense
    // - no per-item blank separator - so the spine stays continuous.
    let items = t.transcript().items();
    let lane = lane_gutters(items);
    // A collapsed run reads tidy: the LAST thought becomes a header, and beneath
    // it only the last few actions show as a rolling window - older low-signal
    // machinery (list/read) is suppressed, while code/diff Blocks and errors
    // always break out. Ctrl-T reveals every thought; Ctrl-O every action.
    let fold = run_fold(
        items,
        t.thinking_expanded,
        t.tools_expanded,
        MACHINERY_WINDOW,
    );
    // The fold's synthetic lines (a thought header carrying the LAST thought's
    // text at the FIRST thought's slot, or a `⋯ N earlier actions` count),
    // owned here so the assembly below can borrow them.
    let synthetic = fold_synthetic_lines(&fold, items, content_area.width, theme);
    // Apply the fold: kept items contribute their cached lines, Header/Elided
    // their synthetic line, Drops nothing - the per-item branch structure lives
    // in `assemble_settled` so it stays off `render_viewport`'s complexity.
    let (mut item_lines, mut counts, mut gutters) =
        assemble_settled(cache, &fold, &synthetic, &lane, content_area.width);
    if !thinking_lines.is_empty() {
        counts.push(wrapped_count(thinking_lines.clone(), content_area.width));
        item_lines.push(&thinking_lines);
        gutters.push(GutterKind::Spine);
    }
    // Captured once so the lull gate below can test `is_none()` without a
    // second borrow of `cache` (streaming_tail borrows it immutably).
    let tail = cache.streaming_tail();
    if let Some((lines, wrapped)) = tail {
        counts.push(wrapped);
        item_lines.push(lines);
        gutters.push(GutterKind::Spine);
    }
    // The lull "waiting" row is the third live entry, mutually exclusive with
    // the two above BY CONSTRUCTION: it draws only when the Run runs and
    // NEITHER a reasoning tail nor a streaming answer is on screen. Declared
    // in this scope so the `&lull_lines` reference `item_lines` holds outlives
    // the draw, exactly like `thinking_lines`. `push_live_entry` folds the
    // emptiness branch out of this body so appending the row does not raise
    // `render_viewport`'s complexity past its baseline (the settle window and
    // the gate already keep the row empty when it must not show).
    let lull_lines = if lull_visible(t.status, thinking_lines.is_empty(), tail.is_some()) {
        live_lull_lines(anim, content_area.width, theme)
    } else {
        Vec::new()
    };
    push_live_entry(
        &lull_lines,
        &mut item_lines,
        &mut counts,
        &mut gutters,
        content_area.width,
    );

    // The ONE per-visual-row mapping both the content and the gutter consume:
    // expanding the per-item kinds over each item's wrapped rows yields a flat
    // `RowGutter` per content row, in the same order the Paragraph lays rows
    // out. Slicing it by the absolute `top` offset is exactly the content's
    // `scroll`, so the gutter and the content can never desync (M3).
    let row_gutters = expand_gutters(&gutters, &counts);

    let total_lines: usize = counts.iter().sum();
    let height = area.height as usize;
    let top = viewport.top_offset(total_lines, height);
    let (range, offset) = visible_window(&counts, top, height);
    let visible: Vec<Line> = item_lines[range.clone()]
        .iter()
        .flat_map(|lines| lines.iter().cloned())
        .collect();
    let paragraph = Paragraph::new(visible).wrap(Wrap { trim: false });
    // The pure window math speaks usize; saturate only here, at the ratatui
    // boundary. The relative offset is bounded by ONE item's wrapped rows
    // (the item straddling the window top), never the session's. Content draws
    // into `content_area` (the gutter carved off the left); the gutter glyphs
    // are painted into the reserved columns per VISUAL row, so soft-wrapped
    // continuations keep their spine.
    let scroll = u16::try_from(offset).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll, 0)), content_area);
    paint_gutter(
        frame,
        text_area,
        &GutterCtx {
            row_gutters: &row_gutters,
            top,
            height,
        },
        theme,
    );

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

/// The lull "waiting" row shown while a Run runs but nothing streams: an
/// elapsed timer (left, fixed-width so the animation column never jitters) then
/// the current [`lull`] scene frame, indented two columns under the running
/// lane like the reasoning tail. Empty until the lull passes the settle window
/// (so a brief token gap never flashes a scene) and empty whenever output is
/// streaming (the caller gates on that - see [`render_viewport`]).
///
/// `width` is the `content_area` width this draws in (the same measured==drawn
/// width the rest of the viewport uses, ADR-0029). The row is truncated to that
/// width so it stays exactly one visual row and cannot desync the lane spine.
/// Appends one live entry (a reasoning tail, a streaming answer, or the lull
/// row) to the render window as a single lane-spine item - but only when it
/// carries lines. Borrows `lines` for as long as `item_lines` holds the
/// reference, so the caller must keep the backing `Vec` alive until the draw.
/// The emptiness branch lives HERE so appending a live entry does not raise
/// [`render_viewport`]'s cyclomatic complexity.
fn push_live_entry<'a>(
    lines: &'a [Line<'static>],
    item_lines: &mut Vec<&'a [Line<'static>]>,
    counts: &mut Vec<usize>,
    gutters: &mut Vec<GutterKind>,
    width: u16,
) {
    if lines.is_empty() {
        return;
    }
    counts.push(wrapped_count(lines.to_vec(), width));
    item_lines.push(lines);
    gutters.push(GutterKind::Spine);
}

/// Whether the lull "waiting" row should draw this frame: the Run is Running
/// and NEITHER live entry (the reasoning tail, the streaming answer) is on
/// screen. The one gate, matching [`Screen::has_live_stream`] by construction
/// (`thinking_empty == streaming_thinking().is_empty()` and `tail_present ==
/// !streaming_text().is_empty()`) so the row and the adapter's lull clock never
/// disagree. Pulled out of [`render_viewport`] so the multi-clause boolean and
/// its emptiness branch stay off that function's cyclomatic complexity.
fn lull_visible(status: Status, thinking_empty: bool, tail_present: bool) -> bool {
    status == Status::Running && thinking_empty && !tail_present
}

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
const LANE_GUTTER: u16 = 2;

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
/// the user REQUEST, not the Run - a Recovery Run injects no `User` item, so
/// its work correctly stays on the prior request's spine. Pure over the item
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
/// per-row mapping [`expand_gutters`] produces and both the content and the
/// gutter index by, so they can never desync.
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

/// Expands the per-item lane `gutters` over each item's `counts` wrapped rows
/// into one [`RowGutter`] per VISUAL content row, in Paragraph layout order -
/// the single mapping the content and the gutter share (M3). A `User` item's
/// caret shows only on its first row; a `Spine` item spines every row (the lane
/// is dense - no per-item blank separator - so the spine stays continuous).
fn expand_gutters(gutters: &[GutterKind], counts: &[usize]) -> Vec<RowGutter> {
    let mut rows = Vec::with_capacity(counts.iter().sum());
    for (i, &n) in counts.iter().enumerate() {
        for row in 0..n {
            let cell = match gutters[i] {
                GutterKind::User if row == 0 => RowGutter::Caret,
                GutterKind::User => RowGutter::Blank,
                GutterKind::Spine => RowGutter::Spine,
                GutterKind::Blank => RowGutter::Blank,
            };
            rows.push(cell);
        }
    }
    rows
}

/// How many of a run's most recent low-signal actions (list/read-style tool
/// one-liners) the collapsed view keeps as a rolling window; older ones are
/// suppressed. Errors and code/diff Blocks are never windowed - they break out.
const MACHINERY_WINDOW: usize = 4;

/// What the collapsed render does with one settled item ([`run_fold`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FoldAction {
    /// Render the item from its cached lines.
    Keep,
    /// Suppress it (a non-last thought, or an action older than the window).
    Drop,
    /// Render a synthetic one-line thought header here (the FIRST thought's slot)
    /// carrying the text of the lane's LAST thought (the index).
    Header(usize),
    /// Render a `⋯ N earlier actions` count here (the FIRST windowed-out action's
    /// slot), so a fold never silently hides work; the rest of the run `Drop`s.
    Elided(usize),
}

/// Folds a collapsed run to a tidy shape (ADR-0040): per lane (a `User`-opened
/// request), the reasoning collapses to a single header - the LAST thought's
/// text rendered at the FIRST thought's slot, with the intervening thoughts
/// dropped - and the low-signal machinery (paired/one-line tool results and
/// calls) becomes a rolling window of the last `window` items, older ones
/// dropped. Errors, code/diff [`Block`]s, assistant text, markers and prompts
/// always Keep (they break out). `thinking_expanded` (Ctrl-T) disables the
/// thought fold; `tools_expanded` (Ctrl-O) disables the machinery window.
///
/// [`Block`]: TranscriptItem::Block
fn run_fold(
    items: &[TranscriptItem],
    thinking_expanded: bool,
    tools_expanded: bool,
    window: usize,
) -> Vec<FoldAction> {
    let mut fold = vec![FoldAction::Keep; items.len()];
    // Lanes are delimited by `User` items; the region before the first is its
    // own (greeting) lane. Fold each independently.
    let mut start = 0;
    let fold_lane = |fold: &mut Vec<FoldAction>, range: std::ops::Range<usize>| {
        let mut thoughts = Vec::new();
        let mut machinery = Vec::new();
        for i in range {
            match &items[i] {
                TranscriptItem::Thinking { .. } => thoughts.push(i),
                // Low-signal machinery: a merged tool result or a bare call.
                // Errors and Blocks are NOT here - they always break out.
                TranscriptItem::ToolResult {
                    is_error: false, ..
                }
                | TranscriptItem::ToolCall { .. } => machinery.push(i),
                _ => {}
            }
        }
        if !thinking_expanded
            && let (Some(&first), Some(&last)) = (thoughts.first(), thoughts.last())
        {
            fold[first] = FoldAction::Header(last);
            for &t in &thoughts {
                if t != first {
                    fold[t] = FoldAction::Drop;
                }
            }
        }
        if !tools_expanded && machinery.len() > window {
            let dropped = &machinery[..machinery.len() - window];
            // A count marker at the first windowed-out slot, the rest suppressed.
            fold[dropped[0]] = FoldAction::Elided(dropped.len());
            for &m in &dropped[1..] {
                fold[m] = FoldAction::Drop;
            }
        }
    };
    for (i, item) in items.iter().enumerate() {
        if matches!(item, TranscriptItem::User { .. }) && i > start {
            fold_lane(&mut fold, start..i);
            start = i;
        }
    }
    fold_lane(&mut fold, start..items.len());
    fold
}

/// The synthetic line each [`FoldAction`] contributes, indexed to match the
/// items: `Header(last)` builds the collapsed thought header from the LAST
/// thought's text at the FIRST thought's slot; `Elided(n)` builds the
/// `⋯ N earlier actions` count; every other action contributes `None` (its own
/// cached lines are used). Split out of `render_viewport` so the fold's branch
/// structure does not inflate that function's complexity.
fn fold_synthetic_lines(
    fold: &[FoldAction],
    items: &[TranscriptItem],
    width: u16,
    theme: &Theme,
) -> Vec<Option<Vec<Line<'static>>>> {
    fold.iter()
        .map(|f| match f {
            FoldAction::Header(last) => match &items[*last] {
                TranscriptItem::Thinking { text } => {
                    Some(vec![collapsed_thought_line(text, width, theme)])
                }
                _ => None,
            },
            FoldAction::Elided(n) => Some(vec![elided_actions_line(*n, theme)]),
            _ => None,
        })
        .collect()
}

/// Applies the collapsed-run [`run_fold`] to the cached settled items,
/// producing the parallel `(lines, wrapped-count, gutter-kind)` the viewport's
/// window math and gutter mapping consume. A `Keep` contributes the item's
/// cached lines and count; a `Header`/`Elided` its synthetic line (re-measured
/// at `width`); a `Drop` nothing. Borrows the cached and synthetic lines, so the
/// returned slices live as long as both. Split out of `render_viewport` to keep
/// the fold's branch structure off that function's complexity.
fn assemble_settled<'a>(
    cache: &'a RenderCache,
    fold: &[FoldAction],
    synthetic: &'a [Option<Vec<Line<'static>>>],
    lane: &[GutterKind],
    width: u16,
) -> (Vec<&'a [Line<'static>]>, Vec<usize>, Vec<GutterKind>) {
    let mut item_lines: Vec<&[Line<'static>]> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut gutters: Vec<GutterKind> = Vec::new();
    for (i, (cached, wrapped)) in cache.settled().enumerate() {
        match fold[i] {
            FoldAction::Drop => continue,
            FoldAction::Keep => {
                item_lines.push(cached);
                counts.push(wrapped);
            }
            FoldAction::Header(_) | FoldAction::Elided(_) => {
                let syn = synthetic[i].as_deref().unwrap_or(cached);
                counts.push(wrapped_count(syn.to_vec(), width));
                item_lines.push(syn);
            }
        }
        gutters.push(lane[i]);
    }
    (item_lines, counts, gutters)
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

/// The `⋯ N earlier actions` count that stands in for a run of windowed-out
/// low-signal machinery, indented under the thought header like the tool work,
/// so a fold never silently hides what the agent did (Ctrl-O reveals it all).
fn elided_actions_line(n: usize, theme: &Theme) -> Line<'static> {
    Line::styled(
        format!("  ⋯ {n} earlier actions · ^O expand"),
        machinery_style(theme),
    )
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

/// Paints the reserved left gutter per VISUAL row over the visible window: the
/// user caret in the prompt color, the lane spine in the dim `lane_spine` slot.
/// Consumes the flat [`RowGutter`] mapping the content shares, sliced by the
/// absolute `top` offset - the SAME slice the content Paragraph scrolls to - so
/// a gutter glyph lands on exactly the row its item occupies at any scroll
/// position, soft-wrapped continuations included (M3). Draws nothing outside the
/// item rows (a short transcript leaves the lower gutter clear).
/// Resolves the paint style for one gutter cell: `Some(style)` when the cell
/// should be painted (Caret or Spine), `None` for Blank (the reserved columns
/// stay clear - nothing to paint).
fn gutter_cell_style(cell: RowGutter, caret: Style, spine: Style) -> Option<Style> {
    match cell {
        RowGutter::Blank => None,
        RowGutter::Caret => Some(caret),
        RowGutter::Spine => Some(spine),
    }
}

fn paint_gutter(frame: &mut Frame, text_area: Rect, ctx: &GutterCtx<'_>, theme: &Theme) {
    let caret = Style::default()
        .fg(tui_color(theme.prompt_gutter))
        .add_modifier(Modifier::BOLD);
    let spine = Style::default().fg(tui_color(theme.lane_spine));

    for (screen_row, cell) in ctx
        .row_gutters
        .iter()
        .skip(ctx.top)
        .take(ctx.height)
        .enumerate()
    {
        let Some(style) = gutter_cell_style(*cell, caret, spine) else {
            continue;
        };
        let y = text_area.y + screen_row as u16;
        frame.render_widget(
            Paragraph::new(Line::styled(cell.glyph(), style)),
            Rect {
                x: text_area.x,
                y,
                width: LANE_GUTTER,
                height: 1,
            },
        );
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
/// the same detail-on-demand rule applied to the machinery plane. `content_width`
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

/// Normalizes a [`StyledLine`]'s text for display: an empty line expands to a
/// single space so ratatui renders it as a visible blank row; tabs become two
/// spaces (consistent with [`text_rows`]).
fn normalize_block_text(line: &StyledLine) -> String {
    if line.text.is_empty() {
        " ".to_string()
    } else {
        line.text.replace('\t', "  ")
    }
}

fn block_line(line: &StyledLine, theme: &Theme) -> Line<'static> {
    Line::styled(normalize_block_text(line), line_style(line.style, theme))
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
    /// mode dot and `▾`/`▸` marker are each one column, so the width does not
    /// depend on the mode the painter later chooses. Exhaustive so a new
    /// segment kind is a compile error here as well as in the painter.
    fn cells(&self) -> usize {
        self.paint().chars().count()
    }
}

/// The Tokens segment's display text: `~{estimate} tokens` (grouped with
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

/// All display facts for one status bar assembly, bundled to keep [`status_bar`]
/// within the 5-param SRP_PARAMS ceiling. Each field is an independent semantic
/// fact the bar renders; the struct is the boundary between the caller's state
/// and the pure assembly logic.
pub(crate) struct StatusBarView<'a> {
    pub(crate) status: Status,
    pub(crate) conn: ConnectionView<'a>,
    pub(crate) toggles: Toggles,
    pub(crate) figures: FigureView,
    pub(crate) position: String,
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
        position,
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
    if figures.session_cost > COST_HIDDEN {
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
            StatusSegment::Tokens {
                estimate,
                dead_mass_pct,
                ..
            } => tokens_label(*estimate, *dead_mass_pct),
            StatusSegment::Position { label } => padded(label),
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
    pub(crate) viewport: &'a Viewport,
    pub(crate) geometry: (usize, usize),
}

pub(crate) fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    ctx: StatusBarCtx<'_>,
    theme: &Theme,
) {
    let StatusBarCtx {
        screen: t,
        conn,
        viewport,
        geometry,
    } = ctx;
    let (total_lines, height) = geometry;
    let position = scroll_position_label(
        viewport.top_offset(total_lines, height),
        total_lines,
        height,
    );
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
                    dead_mass_pct: t.dead_mass_pct,
                }),
                session_cost: t.session_cost,
            },
            position,
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
                        dead_mass_pct: None,
                    }),
                    session_cost: 0.42,
                },
                position: "Bot".to_string(),
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
                position: "Bot".to_string(),
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
                    dead_mass_pct: None,
                }),
                position: "Bot".to_string(),
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
                position: "Bot".to_string(),
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
            dead_mass_pct: None,
        }));
        assert_eq!(
            tokens,
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
            let tokens = tokens_segment(tokens_only(TokenView {
                estimate: 1,
                level,
                dead_mass_pct: None,
            }));
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
                    position: "Bot".to_string(),
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
                dead_mass_pct: None,
            }
            .paint(),
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
        assert_eq!(with_dead.paint(), " ~1,200 tokens · 12% dead ");

        let zero = StatusSegment::Tokens {
            estimate: 1200,
            level: PressureLevel::Ok,
            dead_mass_pct: Some(0),
        };
        assert_eq!(zero.paint(), " ~1,200 tokens · 0% dead ");

        let without = StatusSegment::Tokens {
            estimate: 1200,
            level: PressureLevel::Ok,
            dead_mass_pct: None,
        };
        assert_eq!(without.paint(), " ~1,200 tokens ");
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
                seg.paint().chars().count(),
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
        // The Ctrl-O twin of the thinking-toggle test: a multi-line Block folds
        // to a single title line when collapsed and to the full body when
        // expanded, and flipping the toggle clears the cache so the change
        // takes effect. The lane is dense now - no per-item blank separator.
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
        let collapsed = message_lines(&block, false, false, 80, theme::dark());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            line_text(&collapsed[0]),
            "  ⋯ edit_file src/foo.rs (+1 -1) · ^O expand"
        );
        // Expanded: title + both body rows.
        let expanded = message_lines(&block, false, true, 80, theme::dark());
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
        // Enough prose above the fold that the COLLAPSED content already
        // overflows the viewport (the lane is dense now - no per-item blank
        // separators - so more rows are needed to overflow), then a tall
        // foldable block so expand/collapse changes the total wrapped-line count.
        // Overflow-while-collapsed is what makes `scroll_up` truly unpin, which
        // is the precondition for the stationary-across-expand invariant.
        for i in 0..16 {
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
    use crate::ui::screen::ScreenOpts;

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

    /// Draws one viewport frame with default animation and the dark theme into a
    /// fresh `width`x`height` terminal. Covers the overwhelmingly common test shape:
    /// fresh viewport, fresh cache, default anim. Returns the terminal for inspection.
    fn draw_viewport(width: u16, height: u16, screen: &Screen) -> Terminal<TestBackend> {
        let mut cache = RenderCache::new();
        draw_frame(width, height, |f| {
            render_viewport(
                f,
                f.area(),
                &mut ViewportParams {
                    screen,
                    viewport: &Viewport::new(),
                    cache: &mut cache,
                    anim: Anim::default(),
                },
                theme::dark(),
            );
        })
    }

    /// Draws one viewport frame with a caller-supplied [`ViewportParams`], for
    /// tests that need to inspect the measured geometry or control scroll state.
    fn draw_viewport_with(
        width: u16,
        height: u16,
        params: &mut ViewportParams<'_>,
    ) -> (Terminal<TestBackend>, (usize, usize)) {
        let mut geometry = (0usize, 0usize);
        let terminal = draw_frame(width, height, |f| {
            geometry = render_viewport(f, f.area(), params, theme::dark());
        });
        (terminal, geometry)
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
        use crate::ui::selector::{COLLAPSED_REVEAL_CAP, RowRole, Selector};

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

    // --- render_viewport: geometry, the scrollbar gutter, streaming ---------

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

    #[test]
    fn the_demo_render_matches_the_confirmed_collapsed_run_shape() {
        // The demo is the living spec (ADR-0040): pin the load-bearing rows of
        // the confirmed collapsed-run shape so a regression trips here, not only
        // in a manual dump. Rows are `(gutter, content)` where gutter is the
        // leftmost LANE_GUTTER columns.
        let terminal = draw_viewport(100, 70, &Screen::demo());
        let split = |y: u16| -> (String, String) {
            let row = row_text(&terminal, y);
            let at = row.char_indices().nth(2).map_or(row.len(), |(i, _)| i);
            let (g, r) = row.split_at(at);
            (g.to_string(), r.trim_end().to_string())
        };
        // The user prompt breaks to the caret at the margin.
        assert_eq!(split(2), ("› ".into(), "evaluate this project".into()));
        // Assistant text is flush under the spine.
        assert_eq!(split(3).0, "│ ");
        assert!(split(3).1.starts_with("I'll evaluate this project"));
        // The lane's thoughts fold to ONE header - the LAST thought's text -
        // flush under the spine.
        assert_eq!(
            split(5),
            (
                "│ ".into(),
                "✦ thought: Let me check the build health and test coverage.".into()
            )
        );
        // The windowed-out machinery collapses to a `⋯ N earlier actions` count,
        // indented two columns.
        assert_eq!(
            split(6),
            ("│ ".into(), "  ⋯ 6 earlier actions · ^O expand".into())
        );
        // A governing marker indents two columns; its wrapped continuation stays
        // indented (task 1: the wrap-indent fix).
        assert_eq!(split(7).0, "│ ");
        assert!(split(7).1.starts_with("  » [reading file after file"));
        assert_eq!(split(8).0, "│ ");
        assert!(
            split(8).1.starts_with("  instead;"),
            "wrapped marker stays indented: {:?}",
            split(8).1
        );
        // The error tool result breaks out (always shown), indented two columns,
        // with the ⚙ gutter.
        assert_eq!(split(14).0, "│ ");
        assert!(split(14).1.starts_with("  ⚙ run_command"));
        assert!(split(14).1.contains("command denied"));
        // Assistant text after the tools is flush again.
        assert_eq!(split(15).0, "│ ");
        assert!(split(15).1.starts_with("The project is a well-structured"));
        // Code breaks out, inset two columns, under the spine.
        assert_eq!(split(18).0, "│ ");
        assert!(split(18).1.contains("fn tokenize"));
    }

    #[test]
    fn the_collapsed_lane_spine_is_dense_and_continuous() {
        // The lane has NO per-item blank separator: every content row of the
        // run (from the first assistant line through the last) carries the `│`
        // spine, with no bare gap rows breaking it into segments.
        let terminal = draw_viewport(100, 70, &Screen::demo());
        // Rows 3..=24 are the agent's run (assistant, folded thought, machinery,
        // markers, error, closing assistant + code). Every one starts with the
        // spine - no blank separator rows interleave the machinery.
        for y in 3..=14u16 {
            let row = row_text(&terminal, y);
            assert!(
                row.starts_with('│'),
                "row {y} broke the dense spine: {row:?}"
            );
        }
    }

    #[test]
    fn the_viewport_draws_the_transcript_and_returns_the_measured_geometry() {
        let screen = screen_with_notices(vec!["a launch notice".to_string()]);
        let mut cache = RenderCache::new();
        let viewport = Viewport::new();
        let (terminal, geometry) = draw_viewport_with(
            80,
            20,
            &mut ViewportParams {
                screen: &screen,
                viewport: &viewport,
                cache: &mut cache,
                anim: Anim::default(),
            },
        );
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

    // The lull row draws through the full render path: a Running Run with
    // nothing streaming, quiet past the settle window, paints the timer into the
    // buffer as a third live entry under the running lane.
    #[test]
    fn the_viewport_draws_the_lull_row_when_running_and_quiet() {
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
        let mut cache = RenderCache::new();
        let viewport = Viewport::new();
        let (terminal, _) = draw_viewport_with(
            80,
            20,
            &mut ViewportParams {
                screen: &screen,
                viewport: &viewport,
                cache: &mut cache,
                anim,
            },
        );
        let text = buffer_text(&terminal);
        assert!(text.contains("5s"), "the lull timer opens at 5s:\n{text}");
    }

    #[test]
    fn an_overflowing_transcript_pins_the_tail_and_draws_the_scrollbar() {
        let notices: Vec<String> = (0..30).map(|i| format!("notice line {i:02}")).collect();
        let screen = screen_with_notices(notices);
        let viewport = Viewport::new();
        let mut cache = RenderCache::new();
        let (terminal, geometry) = draw_viewport_with(
            40,
            8,
            &mut ViewportParams {
                screen: &screen,
                viewport: &viewport,
                cache: &mut cache,
                anim: Anim::default(),
            },
        );
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

    // ---- run_fold: the rolling-window collapsed run (ADR-0040) -----------

    fn thought(t: &str) -> TranscriptItem {
        TranscriptItem::Thinking { text: t.into() }
    }
    fn tool(name: &str) -> TranscriptItem {
        TranscriptItem::ToolResult {
            name: name.into(),
            summary: "ok".into(),
            is_error: false,
            key_arg: None,
        }
    }
    fn tool_err() -> TranscriptItem {
        TranscriptItem::ToolResult {
            name: "run".into(),
            summary: "✗ boom".into(),
            is_error: true,
            key_arg: None,
        }
    }

    #[test]
    fn run_fold_headers_the_last_thought_at_the_first_thoughts_slot() {
        // Collapsed default: the LAST thought's text renders as a header at the
        // FIRST thought's slot; the intervening thoughts drop.
        let items = vec![
            TranscriptItem::User { text: "go".into() },
            thought("first"),
            thought("middle"),
            thought("LAST"),
        ];
        let fold = run_fold(&items, false, false, MACHINERY_WINDOW);
        // index 3 is the last thought; the header lands at index 1 (first slot).
        assert_eq!(fold[1], FoldAction::Header(3));
        assert_eq!(fold[2], FoldAction::Drop);
        assert_eq!(fold[3], FoldAction::Drop);
    }

    #[test]
    fn run_fold_windows_machinery_and_elides_the_rest_with_a_count() {
        // With window=2 and 5 machinery items, the last 2 Keep; the earlier 3
        // collapse to an `Elided(3)` count at the first windowed-out slot.
        let mut items = vec![TranscriptItem::User { text: "go".into() }];
        for i in 0..5 {
            items.push(tool(&format!("t{i}")));
        }
        let fold = run_fold(&items, false, false, 2);
        assert_eq!(fold[1], FoldAction::Elided(3)); // first windowed-out slot
        assert_eq!(fold[2], FoldAction::Drop);
        assert_eq!(fold[3], FoldAction::Drop);
        assert_eq!(fold[4], FoldAction::Keep); // last two survive the window
        assert_eq!(fold[5], FoldAction::Keep);
    }

    #[test]
    fn run_fold_keeps_errors_assistant_markers_and_blocks() {
        // Errors, assistant text, markers and Blocks always Keep - they break
        // out of the fold regardless of the window.
        let items = vec![
            TranscriptItem::User { text: "go".into() },
            tool_err(),
            TranscriptItem::Assistant {
                text: "answer".into(),
            },
            TranscriptItem::Marker {
                text: "» nudge".into(),
                tone: Tone::Aid,
            },
            TranscriptItem::Block {
                title: "diff".into(),
                lines: vec![],
            },
        ];
        let fold = run_fold(&items, false, false, 0);
        for (i, action) in fold.iter().enumerate().skip(1) {
            assert_eq!(*action, FoldAction::Keep, "item {i} must break out");
        }
    }

    #[test]
    fn ctrl_t_disables_the_thought_fold() {
        let items = vec![
            TranscriptItem::User { text: "go".into() },
            thought("first"),
            thought("last"),
        ];
        // thinking_expanded = true: every thought Keeps, no header.
        let fold = run_fold(&items, true, false, MACHINERY_WINDOW);
        assert_eq!(fold[1], FoldAction::Keep);
        assert_eq!(fold[2], FoldAction::Keep);
    }

    #[test]
    fn ctrl_o_disables_the_machinery_window() {
        let mut items = vec![TranscriptItem::User { text: "go".into() }];
        for i in 0..5 {
            items.push(tool(&format!("t{i}")));
        }
        // tools_expanded = true: every action Keeps, no Elided count.
        let fold = run_fold(&items, false, true, 2);
        for (i, action) in fold.iter().enumerate().skip(1) {
            assert_eq!(*action, FoldAction::Keep, "action {i} must show");
        }
    }

    #[test]
    fn run_fold_folds_each_lane_independently() {
        // Two User-opened lanes: the second lane's thoughts fold on their own,
        // not merged with the first lane's.
        let items = vec![
            TranscriptItem::User { text: "one".into() },
            thought("a1"),
            thought("a2"),
            TranscriptItem::User { text: "two".into() },
            thought("b1"),
            thought("b2"),
        ];
        let fold = run_fold(&items, false, false, MACHINERY_WINDOW);
        assert_eq!(fold[1], FoldAction::Header(2)); // lane one: last is a2 (idx 2)
        assert_eq!(fold[4], FoldAction::Header(5)); // lane two: last is b2 (idx 5)
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
        let long = "[reading file after file fills your context - dispatch \
                    explore with one focused question instead; a Scout searches \
                    and reports back]";
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.submitted("go", Ok(()));
        let (screen, _) = screen.apply_event(Event::ExploreNudge { text: long.into() });
        let terminal = draw_viewport(60, 20, &screen);
        // The marker's FIRST row carries `»`; its continuation is the next row.
        let marker_y = (0..20)
            .find(|&y| row_text(&terminal, y).contains('»'))
            .expect("the marker row");
        let cont = row_text(&terminal, marker_y + 1);
        assert!(cont.contains("instead"), "the continuation row: {cont:?}");
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
        // width - it fits the text area but overflows the content area:
        //
        //   area.width 40 → text_area 39 (scrollbar col) → content 37 (gutter).
        //   A 38-char word fits in 39 but must wrap in 37.
        //
        // Two facts both DEPEND on the 2-col reservation, so deleting LANE_GUTTER
        // from the wrap width (measuring/drawing at 39) breaks both: the word
        // (1) draws starting at column LANE_GUTTER, not column 0, and (2) wraps
        // to a second row instead of fitting on one. A single 38-char token has
        // no break point, so it wraps only because the width shrank.
        let word = "x".repeat(38);
        let screen = Screen::new(ScreenOpts {
            notices: vec![word.clone()],
            ..ScreenOpts::default()
        });
        let mut cache = RenderCache::new();
        let viewport = Viewport::new();
        let (terminal, _) = draw_viewport_with(
            40,
            20,
            &mut ViewportParams {
                screen: &screen,
                viewport: &viewport,
                cache: &mut cache,
                anim: Anim::default(),
            },
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

        // (2) The 38-char word wrapped to exactly 2 visual content rows, which
        // happens ONLY at the reduced 37-col width (the lane is dense - no
        // trailing separator). At the un-reserved 39 cols it would be one row.
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
    fn the_spine_stays_aligned_with_the_answer_when_scrolled_mid_item() {
        // M3: at a NONZERO scroll offset, with a multi-row agent answer
        // straddling the viewport top, every visible answer row must still carry
        // the `│` spine and every non-answer row must not. This exercises the
        // `skip(top)` slice of the flat row mapping - the path a `top == 0`
        // test never reaches. A desync (gutter indexed differently from content)
        // would land the spine on the wrong rows and trip an assertion below.
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.submitted("the question", Ok(()));
        // A tall answer: many SHORT paragraphs (each one visual row at width 40,
        // so no soft-wrap) each carrying a unique "ANSWER" marker, so every
        // answer row is identifiable and none is a marker-less continuation.
        let answer = (0..14)
            .map(|i| format!("ANSWER-{i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_end(
            vec![ContentBlock::text(&answer)],
            StopReason::EndTurn,
        ));

        // Measure the geometry once, then scroll up so the top lands mid-answer.
        let mut viewport = Viewport::new();
        let mut cache = RenderCache::new();
        let (_, (total, height)) = draw_viewport_with(
            40,
            10,
            &mut ViewportParams {
                screen: &screen,
                viewport: &viewport,
                cache: &mut cache,
                anim: Anim::default(),
            },
        );
        assert!(total > height, "the answer overflows the viewport");
        viewport.scroll_up(4, total, height); // unpin, land 4 rows above the tail

        let (terminal, _) = draw_viewport_with(
            40,
            10,
            &mut ViewportParams {
                screen: &screen,
                viewport: &viewport,
                cache: &mut cache,
                anim: Anim::default(),
            },
        );
        // Every visible answer row must still carry the spine in column 0 at
        // this nonzero scroll. A desync (the gutter sliced differently from the
        // content) would land the spine off the answer rows and drop one here.
        // Answer paragraphs are short single-row lines, so an "ANSWER" row is
        // never a marker-less soft-wrap continuation.
        let mut answer_rows_seen = 0;
        for y in 0..10 {
            let row = row_text(&terminal, y);
            if row.contains("ANSWER") {
                answer_rows_seen += 1;
                assert_eq!(
                    row.chars().next(),
                    Some('│'),
                    "answer row {y} lost its spine at this scroll: {row:?}"
                );
            }
        }
        assert!(answer_rows_seen >= 2, "several answer rows must be visible");
        // The scroll actually happened (we are not pinned at the tail): the
        // last answer paragraph is off-screen below.
        assert!(
            !buffer_text(&terminal).contains("ANSWER-13"),
            "the viewport scrolled up off the tail"
        );
    }

    /// Vinnie's `evaluate this project` shape (~60 cols): a User prompt, a long
    /// settled thought that would wrap, a real Aid nudge marker, and a tool
    /// call. Returns the rendered terminal.
    fn evaluate_project_screen(width: u16, height: u16) -> Terminal<TestBackend> {
        // The exact ExploreNudge wording from voice.rs, so it matches Vinnie's
        // `» [reading file after file...]` line.
        let nudge = "[reading file after file fills your context - dispatch \
                     explore with one focused question instead; a Scout searches \
                     and reports back]";
        let thinking = "I should read the manifest and the entry point and the tests \
                        and then form a plan about what to evaluate first here";
        let screen = screen_with_thinking("evaluate this project", thinking);
        // Settle the thought (empty final content → thinking materializes).
        let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));
        // An Aid nudge marker (the `» [...]` line); the screen prepends `» `.
        let (screen, _) = screen.apply_event(Event::ExploreNudge {
            text: nudge.to_string(),
        });
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
        // at 60 cols - a wrapped thought, a wrapped nudge marker, a tool call.
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
            // Rows carrying agent content (the thought, the `»` marker, the `⋯`
            // machinery) are all in-lane and must start with the spine.
            let is_agent_content = row.contains("thought:")
                || row.contains('»')
                || row.contains('⋯')
                || row.contains("explore with")
                || row.contains("searches and report");
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
    // normalize_block_text: the two normalization rules (empty -> space, tab
    // -> two spaces) are the only logic in `block_line`; tested here so that
    // render tests pin the VISIBLE output and these tests pin the TEXT rule.
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_block_text_expands_empty_to_a_space() {
        let line = StyledLine {
            text: String::new(),
            style: LineStyle::Default,
        };
        assert_eq!(normalize_block_text(&line), " ");
    }

    #[test]
    fn normalize_block_text_replaces_tabs_with_two_spaces() {
        let line = StyledLine {
            text: "a\tb".to_string(),
            style: LineStyle::Default,
        };
        assert_eq!(normalize_block_text(&line), "a  b");
    }

    #[test]
    fn normalize_block_text_leaves_ordinary_text_unchanged() {
        let line = StyledLine {
            text: "hello world".to_string(),
            style: LineStyle::Default,
        };
        assert_eq!(normalize_block_text(&line), "hello world");
    }

    // -----------------------------------------------------------------------
    // gutter_cell_style: the Blank -> None, Caret/Spine -> Some mapping; the
    // returned style is the concrete one the painter supplies (not a default).
    // -----------------------------------------------------------------------

    #[test]
    fn gutter_cell_style_blank_returns_none() {
        let caret = Style::default().fg(Color::Green);
        let spine = Style::default().fg(Color::Blue);
        assert_eq!(gutter_cell_style(RowGutter::Blank, caret, spine), None);
    }

    #[test]
    fn gutter_cell_style_caret_returns_caret_style() {
        let caret = Style::default().fg(Color::Green);
        let spine = Style::default().fg(Color::Blue);
        assert_eq!(
            gutter_cell_style(RowGutter::Caret, caret, spine),
            Some(caret)
        );
    }

    #[test]
    fn gutter_cell_style_spine_returns_spine_style() {
        let caret = Style::default().fg(Color::Green);
        let spine = Style::default().fg(Color::Blue);
        assert_eq!(
            gutter_cell_style(RowGutter::Spine, caret, spine),
            Some(spine)
        );
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
