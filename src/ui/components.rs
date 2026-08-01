//! UI Components - the SINGLE mapping from the semantic display vocabulary
//! (ADR-0008) to ratatui `Style`/`Color`, plus the render helpers the frontend
//! draws with.
//!
//! This is the one place semantics become terminal colors: [`DiffSide`] →
//! color for a diff's lines, [`FooterItem`] → the flat footer's grey right
//! group. Extensions and the Screen core never touch ratatui; they speak the
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

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::approvals::ApprovalMode;
use crate::plan::{TodoItem, TodoStatus};
use crate::ui::completion;
use crate::ui::composer::{self, ComposerLayout, ComposerView, OverlayStatus, OverlayView};
use crate::ui::lull;
use crate::ui::markdown::{self, MdLine, MdStyle};
use crate::ui::mcp_command::{McpDialogView, McpRow, McpStyle};
use crate::ui::picker::Picker;
use crate::ui::screen::{
    ConfirmKind, OTHER_OPTION_LABEL, PendingApproval, PendingQuestion, Screen, Status,
};
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

// ---------------------------------------------------------------------------
// qwen v0.16.0 colour ROLES → suspenders Theme slots (Phase 2, ADR-0046).
//
// qwen-code paints its committed chrome from a handful of semantic ROLES
// (`text.accent`, `text.secondary`, `status.success`, …). This port maps each
// role onto a suspenders Theme slot behind ONE helper apiece, so a theme
// reconcile is a single edit per role rather than a hunt through the render
// body. Phase 7 (ADR-0008/0053) carved the four qwen roles that used to borrow
// a neighbouring slot into real slots: `text.primary`→`foreground` (was the
// terminal default), `text.accent`→`accent` (was cyan `prompt_gutter`),
// `status.success`→`success` (was diff `added`), `status.warning`→`warning`
// (was warm amber `marker_aid`). `text.secondary`/`ui.symbol`/`border.default`
// still share the neutral gray `muted` slot by design.
// ---------------------------------------------------------------------------

/// qwen `text.accent` (AccentPurple `#D2A6FF`): the user `>` caret + the
/// assistant `✦` marker. Reads the dedicated `accent` slot (Phase 7, ADR-0008),
/// a distinct role rather than the cyan `prompt_gutter` it once borrowed.
fn accent_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.accent))
}

/// qwen `text.secondary` (Gray): thought glyph/body, tool descriptions, retry,
/// hints. Maps to the `muted` slot.
fn secondary_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.muted))
}

/// qwen `status.success` (AccentGreen `#AAD94C`): the success prefix + the
/// `✓`/`o` tool markers. Reads the dedicated `success` slot (Phase 7,
/// ADR-0008), a distinct role rather than the diff `added` green it once borrowed.
fn success_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.success))
}

/// qwen `status.warning` (AccentYellow `#FFD700`): the `△` warning prefix + a
/// pending tool-group border. Reads the dedicated `warning` slot (Phase 7,
/// ADR-0008), a distinct role rather than the warm amber `marker_aid` it once
/// borrowed.
fn warning_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.warning))
}

/// qwen `status.error` (AccentRed): the `✕`/`x` error prefix + marker. Maps to
/// the `error` slot.
fn error_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.error))
}

/// qwen `text.primary` (Foreground `#bfbdb6`): body text, info bodies, tool
/// names. Reads the dedicated `foreground` slot (Phase 7, ADR-0008) - a real
/// pinned colour rather than the terminal default, so body text matches
/// QwenDark on any background.
fn primary_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.foreground))
}

/// qwen `ui.symbol` (Gray): a shell tool's marker + a shell tool-group's border.
/// Maps to the `muted` slot (same read as `border.default`).
fn symbol_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.muted))
}

/// qwen `border.default` (Gray): the resting tool-group box border. Maps to the
/// `muted` slot.
fn border_style(theme: &Theme) -> Style {
    Style::default().fg(tui_color(theme.muted))
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

// ---------------------------------------------------------------------------
// Render helpers.
// ---------------------------------------------------------------------------

/// The two connection facts the footer shows (ADR-0033): the fixed endpoint
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
/// connection facts the footer shows, the animation clocks, and the Theme
/// this frame renders in (the live `/theme` preview or the active Theme).
/// Bundled as ONE named-field carrier - the same style as [`PendingBodyParams`],
/// [`FooterCtx`] and the adapter's `AdapterCtx` - so
/// [`render_pending`] and the adapter's `draw`/`draw_previewed` take four args
/// instead of six, and a new frame-wide input is a field, not another parameter.
#[derive(Clone, Copy)]
pub struct FrameCtx<'a> {
    pub conn: ConnectionView<'a>,
    pub anim: Anim,
    pub theme: &'a Theme,
}

/// Splits the inline frame `area` into the three vertical zones the pending
/// region draws into: `[pending_body, footer, composer]` (ADR-0046). There
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
fn frame_chunks(area: Rect, sticky_rows: usize, composer_rows: usize) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),                       // inline pending body (ADR-0046)
            Constraint::Length(sticky_rows as u16),   // sticky "Current tasks" box (ADR-0048)
            Constraint::Length(1),                    // flat footer (ADR-0053)
            Constraint::Length(composer_rows as u16), // composer (grows with the draft)
        ])
        .split(area)
}

/// The Composer's zone height for this frame: the draft's capped row count plus
/// the two chrome rows (the top dash rule + the bottom border, ADR-0048). The
/// draft rows are capped by [`composer::max_visible_rows`] so a very tall draft
/// never starves the pending body. Pure - no frame access.
fn capped_composer_height(layout: &ComposerLayout, frame_height: usize) -> usize {
    let draft = layout
        .rows
        .len()
        .min(composer::max_visible_rows(frame_height));
    draft + COMPOSER_CHROME_ROWS
}

/// Renders the inline PENDING region (ADR-0046): the uncommitted transcript
/// tail (`cache.settled()[hw..]` plus the live reasoning tail, streaming answer,
/// and lull row), the flat footer, the Composer, and any open overlay/approval.
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
    // Operation → Integration (IOSP): the pure [`pending_layout`] decides every
    // Rect and whether the sticky box / overlay show; below we only issue the
    // draw calls against that plan.
    let plan = pending_layout(area, &composer_view, t);
    // The Help overlay (qwen `Help`) takes the whole body region when open,
    // replacing the transcript body with the bordered shortcuts panel - the same
    // slot the inline Approval draws in. It is a modal, so nothing behind it
    // renders (the composer/footer stay so the `? for shortcuts` promise reads as
    // resolved). Presence-branched HERE so the body path stays one call (IOSP).
    // The `/mcp` management dialog (ADR-0065 Phase E) takes the whole body region
    // when open, like the Help overlay: a bordered box (header / content /
    // footer), NOT the compact popup above the composer. Branched HERE (above the
    // Help/body fork) so the McpDialog owns the body slot and no popup draws for
    // it. Checked before Help since the two are mutually exclusive (the dialog
    // holds the keyboard; `?` cannot open Help behind it).
    if let Some(OverlayView::McpDialog(dialog)) = &composer_view.overlay {
        render_mcp_dialog(frame, plan.body, dialog, theme);
    } else if t.help_open {
        render_help_overlay(frame, plan.body, theme);
    } else {
        render_pending_body(
            frame,
            plan.body,
            &mut PendingBodyParams {
                screen: t,
                cache,
                anim,
            },
            theme,
        );
    }
    render_sticky_slot(frame, plan.sticky_box, plan.sticky_items, theme);
    // The flat footer (ADR-0053): a single row, model | context% | cost on the
    // right, the AutoAcceptIndicator or `? for shortcuts` on the left. Native
    // scrollback owns history (ADR-0046), so there is no scroll position to report.
    render_footer(frame, plan.status, FooterCtx { screen: t, conn }, theme);
    render_composer(frame, plan.composer, t, &plan.draft, theme);
    render_composer_popup_slot(frame, plan.popup_top, area, &composer_view.overlay, theme);
    // The Approval is rendered INLINE now (ADR-0049): the confirming ToolCall's
    // box carries the question + radio, drawn as part of the pending body above.
    // No modal overlay.
}

/// The pending region's whole geometry (the compute-plan / parameter object
/// behind [`render_pending`]): the four zone Rects, the pre-wrapped composer
/// [`ComposerLayout`] the composer + cursor draw from, the resolved sticky
/// box Rect + its items (`None` when the box is dropped), and the status
/// bar's y for the overlay anchor. Built by the pure [`pending_layout`]
/// operation so [`render_pending`] is a call-only assembler (IOSP, ADR-0029
/// measure == draw). Borrows `t` for the sticky items' lifetime.
struct PendingLayout<'a> {
    body: Rect,
    sticky_box: Rect,
    sticky_items: Option<&'a [TodoItem]>,
    status: Rect,
    composer: Rect,
    draft: ComposerLayout,
    popup_top: u16,
}

/// Operation (IOSP): the pending region's geometry for this frame. Wraps the
/// draft, caps the composer zone, decides the sticky "Current tasks" box
/// (ADR-0048) - reserved only when it fits alongside the status row, composer,
/// and one body row (ADR-0029 measure == draw) - and splits `area` into the
/// four zones. Pure: no frame access, no drawing.
fn pending_layout<'a>(area: Rect, view: &ComposerView<'_>, t: &'a Screen) -> PendingLayout<'a> {
    let draft = composer::layout(
        view.draft,
        view.cursor,
        area.width.saturating_sub(2) as usize,
    );
    let composer_height = capped_composer_height(&draft, area.height as usize);
    // The sticky box DERIVES from the latest committed Todo item; a pending or
    // all-completed list, or a frame too short to also hold the box, drops it
    // (costs no rows). The `.filter` is the measure == draw guard: reserving a
    // zone we cannot fully draw would paint a headless fragment over the
    // composer.
    //
    // An OPEN approval (ADR-0049) also drops the box: the approval renders inside
    // the pending body, and the informational sticky box would starve that body
    // (`Constraint::Min(1)`) and top-clip the "Apply this change?" question out of
    // view. A visible approval takes priority over the sticky list, so we reserve
    // NO sticky zone while `pending_approval.is_some()`.
    let sticky_items = (t.pending_approval.is_none() && t.pending_question.is_none())
        .then(|| {
            sticky_todos(
                t.transcript().latest_todo(),
                t.transcript().committed_high_water(),
            )
        })
        .flatten()
        .filter(|items| {
            sticky_fits(
                area.height as usize,
                sticky_todos_height(items.len()),
                composer_height,
            )
        });
    let sticky_height = sticky_items.map_or(0, |items| sticky_todos_height(items.len()));
    let chunks = frame_chunks(area, sticky_height, composer_height);
    PendingLayout {
        body: chunks[0],
        sticky_box: sticky_box_area(chunks[1]),
        sticky_items,
        status: chunks[2],
        composer: chunks[3],
        draft,
        popup_top: chunks[2].y,
    }
}

/// The sticky "Current tasks" slot: draws the box when the plan reserved one
/// (`Some` items), else nothing. The presence branch lives HERE, so
/// [`render_pending`] calls it unconditionally (IOSP).
fn render_sticky_slot(frame: &mut Frame, area: Rect, items: Option<&[TodoItem]>, theme: &Theme) {
    if let Some(items) = items {
        render_sticky_todos(frame, area, items, theme);
    }
}

/// The Composer overlay slot: draws the popup when an overlay is open, else
/// nothing. The presence branch lives HERE, so [`render_pending`] calls it
/// unconditionally (IOSP).
fn render_composer_popup_slot(
    frame: &mut Frame,
    popup_top: u16,
    area: Rect,
    overlay: &Option<OverlayView>,
    theme: &Theme,
) {
    // The `/mcp` dialog (ADR-0065 Phase E) draws in the BODY region (like Help),
    // not this compact popup, so it is skipped here - `render_pending` drew it.
    match overlay {
        Some(OverlayView::McpDialog(_)) | None => {}
        Some(overlay) => render_composer_popup(frame, popup_top, area, overlay, theme),
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

/// Assembles the FULL, UNCLAMPED pending body line set at content `width` and
/// high-water mark `hw` (ADR-0046): the uncommitted settled tail `items[hw..]`
/// through the SAME [`grouped_rows`] fold the commit blit uses (so pending and
/// committed stay byte-identical), then the live entries newest-last (the
/// reasoning tail, the streaming answer, the spinner). This is the PRE-CLIP line
/// set `render_pending_body_at` anchors/clips for the live viewport, AND the exact
/// content Ctrl-S's peek ([`Effect::PeekPending`]) blits into scrollback unclamped.
/// Syncs the cache as a side effect (the settled tail's lines come from it); no
/// frame access, no anchor/clip math (IOSP).
fn pending_body_lines(
    params: &mut PendingBodyParams<'_>,
    theme: &Theme,
    hw: usize,
    width: u16,
) -> Vec<Line<'static>> {
    let t = params.screen;
    let cache = &mut params.cache;
    let anim = params.anim;

    cache.sync(
        t.transcript(),
        Toggles {
            compact: t.compact_mode,
        },
        width,
        theme,
    );

    let thinking = t.transcript().streaming_thinking();
    // Compact suppresses the live thinking tail (qwen `HistoryItemDisplay.tsx:155`
    // gates the pending `gemini_thought` under `!compactMode` too; Phase-6 design
    // "live thinking tail suppressed"). The spinner SUBJECT still shows below -
    // qwen does not compact-gate the LoadingIndicator.
    let thinking_lines = if t.compact_mode {
        Vec::new()
    } else {
        live_thinking_lines(&thinking, anim.spinner, width, theme)
    };

    let items = t.transcript().items();
    // The inline approval (ADR-0049): when an Approval is pending, it attaches to
    // the newest live ToolCall (the confirming call, found by position - the
    // batch runs sequentially). Its group renders with the `?` marker, warning
    // border, and the approval block appended. `None` otherwise, so the pending
    // body is byte-identical to the committed blit (which never carries it).
    let approving = t.pending_approval.as_ref().and_then(|pending| {
        newest_live_tool_index(items).map(|call_index| Approving {
            pending,
            call_index,
        })
    });
    // FULL-CONTENT pending body (ADR-0046): the uncommitted settled tail renders
    // through the SAME [`grouped_rows`] fold `render_committed_slice` blits with,
    // so committed and pending are byte-identical and nothing reflows at the
    // commit seam (qwen's `<Static>` prints history un-clamped; the ONLY overflow
    // reduction is the bottom-anchor + top-clip the caller applies).
    let mut lines = grouped_rows_with_approval(&GroupedRows {
        cache,
        items,
        hw,
        width,
        theme,
        approving: approving.as_ref(),
    });

    // While an approval is OPEN, the approval block MUST stay bottom-most so the
    // top-clip ([`anchor_clip`]) can never eat the "Apply this change?" /
    // "Allow execution of..." question on a short terminal: the Run is not
    // "loading", it is waiting on the USER, so the confirming ToolCall + its
    // approval are the salient content, not the LoadingIndicator's
    // waiting-for-confirmation state (qwen keeps the approval the focused
    // interactive element). Suppress every trailing LIVE row below the approval -
    // the reasoning tail, the streaming-answer tail, and the spinner - so the
    // approving group ends the pending body and survives the clip. These rows are
    // pending-only overlays never present in the committed slice, so
    // measure==draw and committed==pending identity are unaffected.
    if approving.is_none() {
        // The live entries follow the settled tail, newest last: the reasoning
        // tail, then the streaming answer, then (whenever the Run is Running) the
        // spinner line - the LoadingIndicator (ADR-0048), which keeps the lull
        // scene as its phrase content and carries the elapsed/cancel affordance.
        append_live(&mut lines, &thinking_lines);
        let tail = cache.streaming_tail();
        let receiving = if let Some((tail_lines, _)) = tail {
            append_live(&mut lines, tail_lines);
            true
        } else {
            false
        };
        // The rolling thought subject (Phase 6, qwen `thought?.subject ||
        // currentLoadingPhrase`): the reasoning head the spinner shows in place of
        // the lull phrase while the Run reasons. Bound outside the `if` so its
        // `String` outlives the borrow the `SpinnerState` takes.
        //
        // Gated on compact mode to avoid DOUBLING the reasoning on screen: in
        // non-compact the `✦ Thinking` tail above already shows the reasoning
        // head, so the full ladder's last-line fallback would echo that exact
        // line onto the spinner (the bug). There the spinner takes only a
        // DISTINCT bold `**subject**`, else the lull phrase. Under compact the
        // tail is suppressed, so the spinner is the sole reasoning surface and
        // keeps the full head fallback.
        let subject = if t.compact_mode {
            t.transcript().thought_subject()
        } else {
            t.transcript().thought_subject_bold()
        };
        let spinner = if t.status == Status::Running {
            // `subject` is the Phase-6 thought-subject seam (wins over the lull
            // phrase when `Some`); `tokens` is left `None` to avoid per-frame
            // jitter.
            let state = SpinnerState {
                subject: subject.as_deref(),
                receiving,
                ..SpinnerState::default()
            };
            spinner_line(anim, state, width, theme)
        } else {
            Vec::new()
        };
        append_live(&mut lines, &spinner);
    }

    // The question modal (ADR-0057, qwen `ask_user_question`): a standalone
    // bordered box appended BOTTOM-MOST so the top-clip never eats it - the Run
    // is waiting on the USER, so the questions are the salient content (the same
    // rule the open approval follows). Unlike an approval it is NOT tied to a
    // transcript ToolCall, so it draws as its own box rather than inside a tool
    // group. `None` when no question is pending, so the pending body stays
    // byte-identical to the committed blit (which never carries it).
    if let Some(pending) = t.pending_question.as_ref() {
        append_live(&mut lines, &question_modal_lines(pending, width, theme));
    }
    lines
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
    let content_area = Rect {
        x: area.x + CONTENT_MARGIN,
        width: area.width.saturating_sub(2 * CONTENT_MARGIN),
        ..area
    };
    // Operation (IOSP): assemble the FULL, unclamped pending body once; the draw
    // below only anchors/clips it. Ctrl-S's peek blits this SAME line set
    // (ADR-0046, [`pending_body_lines`]).
    let lines = pending_body_lines(params, theme, hw, content_area.width);

    // Integration (IOSP): compute the anchor/clip geometry in the pure
    // [`anchor_clip`] operation, then only issue the draw calls.
    let total = wrapped_count(lines.clone(), content_area.width);
    let clip = anchor_clip(total, area, content_area);

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((clip.scroll, 0)),
        clip.content_draw,
    );
    if let Some(marker_draw) = clip.marker_draw {
        draw_overflow_marker(frame, marker_draw, theme);
    }

    clip.total_lines
}

/// Appends a LIVE entry's lines (a reasoning tail, a streaming answer, or the
/// lull row) after the settled body, with the `marginTop:1` blank separator every
/// item carries - but only when the entry is non-empty. The emptiness branch
/// lives HERE so the caller does not repeat it per entry.
fn append_live(lines: &mut Vec<Line<'static>>, entry: &[Line<'static>]) {
    if entry.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.extend(entry.iter().cloned());
}

/// The bottom-anchor + top-clip geometry a pending body draws at (ADR-0046),
/// resolved from the stack's `total_lines` against the zone `area`/`content_area`.
/// Every field is a ready-to-draw value, so [`render_pending_body_at`] holds no
/// layout arithmetic of its own (IOSP). `marker_draw` is `Some` only when the
/// stack overflows the zone.
struct PendingClip {
    /// The stack's total wrapped rows, echoed back for the caller's return value.
    total_lines: usize,
    /// Content Paragraph scroll offset (the top-clipped row count, saturated).
    scroll: u16,
    content_draw: Rect,
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

    // On overflow the top visible row is the marker, so the content starts one
    // row down and loses that row of height.
    let content_top_pad: u16 = if overflowed { 1 } else { 0 };
    let draw_height = drawn_rows.saturating_sub(content_top_pad as usize) as u16;
    let y_off = pad_top as u16 + content_top_pad;

    PendingClip {
        total_lines,
        scroll: u16::try_from(top).unwrap_or(u16::MAX),
        content_draw: Rect {
            y: content_area.y + y_off,
            height: draw_height,
            ..content_area
        },
        marker_draw: overflowed.then_some(Rect {
            y: area.y + pad_top as u16,
            height: 1,
            ..area
        }),
    }
}

/// Draws the `… Ctrl-S to show more` overflow marker (ADR-0046, qwen's
/// `ShowMoreLines`) on the reserved top row. Ctrl-S is wired: it blits the FULL,
/// unclamped body into scrollback as a non-committing peek ([`Effect::PeekPending`]
/// / [`render_pending_peek`]) - the fixed inline viewport cannot grow, so the
/// clipped rows are revealed ABOVE the live region rather than in place.
///
/// [`Effect::PeekPending`]: crate::ui::screen::Effect::PeekPending
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
/// top/bottom border, capped at `body_cap` so a long list never eats the
/// screen, positioned just above `anchor_y` and horizontally centered within
/// `area`. Pure - no frame access. `body_len` is the number of content lines
/// the popup will hold; `body_cap` is the most body rows it may occupy (System
/// A dialogs cap at [`POPUP_MAX_ROWS`] and scroll; System B is already windowed
/// to its suggestions + `▲/▼`/counter chrome, so it caps higher).
fn popup_rect(anchor_y: u16, area: Rect, body_len: usize, body_cap: u16) -> Rect {
    let body_rows = body_len.max(1) as u16;
    let height = (body_rows + 2).min(body_cap + 2).min(area.height);
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

/// The body lines a status line spells: a Loading or Failed dialog draws one
/// muted/error line. `Ready` returns `None` (the caller draws the rows). Pure.
fn dialog_status_line(status: &OverlayStatus, title: &str, theme: &Theme) -> Option<Line<'static>> {
    match status {
        OverlayStatus::Loading => Some(Line::styled(
            format!("loading {title}…"),
            Style::default()
                .fg(tui_color(theme.muted))
                .add_modifier(Modifier::ITALIC),
        )),
        OverlayStatus::Failed(msg) => Some(Line::styled(
            format!("failed: {msg}"),
            Style::default()
                .fg(tui_color(theme.error))
                .add_modifier(Modifier::BOLD),
        )),
        OverlayStatus::Ready => None,
    }
}

// What one overlay draws: the box title, its body lines, and (System A only)
// the active row the box scrolls to keep visible + the body-row cap. A pure
// Parameter Object so the popup painter is one integration step over it (IOSP).
struct PopupDraw {
    title: String,
    lines: Vec<Line<'static>>,
    /// `Some` for System A (the box re-scrolls to this active row); `None` for
    /// System B (already windowed).
    scroll_active: Option<usize>,
    body_cap: u16,
}

/// The System B palette body cap: the [`MAX_SUGGESTIONS`] window plus the three
/// chrome rows (`▲`, `▼`, and the `(n/m)` counter) it may add.
const MENU_BODY_CAP: u16 = MAX_SUGGESTIONS as u16 + 3;

/// The System B palette's cursor state (Parameter Object): the highlighted row,
/// the scroll-window top, and whether the active row is expanded (`←/→`).
/// Bundled so the palette draw pipeline stays integration steps.
#[derive(Debug, Clone, Copy)]
struct MenuCursor {
    active: usize,
    scroll: usize,
    expanded: bool,
}

// The System B (`/` palette) draw plan: the color-only suggestion rows +
// windowing chrome; the box never re-scrolls (`scroll_active` None).
fn menu_draw(
    suggestions: &[completion::Suggestion],
    cursor: MenuCursor,
    inner_width: u16,
    theme: &Theme,
) -> PopupDraw {
    PopupDraw {
        title: "commands".to_string(),
        lines: suggestion_rows(suggestions, cursor, inner_width, theme),
        scroll_active: None,
        body_cap: MENU_BODY_CAP,
    }
}

// The `@path` file picker draw plan (Phase C2, qwen `useAtCompletion`): the
// fuzzy path rows drawn like System B (color-only, no numbers), titled "files".
// While the async fetch is in flight with nothing to show yet, a subtle
// "searching…" line stands in for the rows (qwen shows a loading state).
fn at_draw(
    suggestions: &[completion::Suggestion],
    cursor: MenuCursor,
    loading: bool,
    inner_width: u16,
    theme: &Theme,
) -> PopupDraw {
    let lines = if loading && suggestions.is_empty() {
        vec![searching_line(theme)]
    } else {
        suggestion_rows(suggestions, cursor, inner_width, theme)
    };
    PopupDraw {
        title: "files".to_string(),
        lines,
        scroll_active: None,
        body_cap: MENU_BODY_CAP,
    }
}

/// The "searching…" placeholder line (muted italic) shown while an AT file
/// search is in flight with no rows yet (qwen's loading state).
fn searching_line(theme: &Theme) -> Line<'static> {
    Line::styled(
        "searching…",
        Style::default()
            .fg(tui_color(theme.muted))
            .add_modifier(Modifier::ITALIC),
    )
}

// The System A (numbered `›` dialog) draw plan: a status line, or the numbered
// rows the box scrolls to keep the active row visible.
fn dialog_draw(
    command: &str,
    status: &OverlayStatus,
    rows: &[SelectorRow],
    active: usize,
    inner_width: u16,
    theme: &Theme,
) -> PopupDraw {
    let title = selector_popup_title(command);
    let lines = match dialog_status_line(status, &title, theme) {
        Some(line) => vec![line],
        None => dialog_rows(rows, active, inner_width, theme),
    };
    PopupDraw {
        title,
        lines,
        scroll_active: Some(active),
        body_cap: POPUP_MAX_ROWS,
    }
}

/// The inline Composer overlay popup (ADR-0051): a compact bordered list
/// anchored just above `anchor_y`. The TWO systems render distinctly - `Menu`
/// (System B) is the fuzzy `/` palette ([`menu_draw`]), `Dialog` (System A) a
/// committed command's numbered `›` list ([`dialog_draw`]). This is the one
/// integration step: pick the draw plan, size the box, blit the scrolled
/// window (IOSP). Inline and height-bounded - never the full screen.
fn render_composer_popup(
    frame: &mut Frame,
    anchor_y: u16,
    area: Rect,
    view: &OverlayView,
    theme: &Theme,
) {
    let inner_width = area.width.saturating_sub(4).max(1);
    let plan = match view {
        OverlayView::Menu {
            suggestions,
            active,
            scroll,
            query: _,
            expanded,
        } => menu_draw(
            suggestions,
            MenuCursor {
                active: *active,
                scroll: *scroll,
                expanded: *expanded,
            },
            inner_width,
            theme,
        ),
        OverlayView::Dialog {
            command,
            status,
            rows,
            active,
            detail: _,
        } => dialog_draw(command, status, rows, *active, inner_width, theme),
        OverlayView::AtFiles {
            suggestions,
            active,
            scroll,
            query: _,
            loading,
        } => at_draw(
            suggestions,
            MenuCursor {
                active: *active,
                scroll: *scroll,
                expanded: false,
            },
            *loading,
            inner_width,
            theme,
        ),
        // The `/mcp` dialog draws in the body region ([`render_mcp_dialog`]), not
        // this popup: [`render_composer_popup_slot`] skips it before it reaches
        // here, so this arm is unreachable.
        OverlayView::McpDialog(_) => return,
    };

    let popup = popup_rect(anchor_y, area, plan.lines.len(), plan.body_cap);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(padded(&plan.title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tui_color(theme.popup_border)));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let visible = inner.height as usize;
    let top = plan
        .scroll_active
        .map(|a| composer::first_visible_row(a, visible.max(1)))
        .unwrap_or(0);
    let shown: Vec<Line> = plan.lines.into_iter().skip(top).take(visible).collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

/// The System B (`/` palette) suggestion rows (qwen `SuggestionsDisplay.tsx`):
/// color-only, NO `›` marker, NO numbers. The active row reads `text.accent`,
/// the rest `text.secondary`; two columns (command | description) with the
/// command column capped at half the width; the fuzzy match substring is drawn
/// INVERTED (qwen `PrepareLabel`). Only the [`MAX_SUGGESTIONS`] window from
/// `scroll` is emitted, framed by `▲`/`▼` when there is more above/below and a
/// trailing `(active+1/total)` counter when the list overflows the window.
fn suggestion_rows(
    suggestions: &[completion::Suggestion],
    cursor: MenuCursor,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if suggestions.is_empty() {
        return vec![no_matches_line(theme)];
    }
    let width = inner_width as usize;
    let frame = suggestion_frame(suggestions, cursor.scroll, width);
    up_arrow_line(&frame, theme)
        .into_iter()
        .chain(suggestion_body_lines(
            suggestions,
            cursor,
            &frame,
            width,
            theme,
        ))
        .chain(down_arrow_line(&frame, theme))
        .chain(counter_line(
            suggestions.len(),
            cursor.active,
            &frame,
            theme,
        ))
        .collect()
}

/// The leading `▲` scroll indicator, present only when rows are scrolled off
/// the top (a one-branch pure row builder).
fn up_arrow_line(frame: &SuggestionFrame, theme: &Theme) -> Option<Line<'static>> {
    frame
        .show_up
        .then(|| Line::styled("▲", primary_style(theme)))
}

/// The windowed suggestion rows, each with its active flag resolved against
/// `frame.start` (a pure row builder).
fn suggestion_body_lines(
    suggestions: &[completion::Suggestion],
    cursor: MenuCursor,
    frame: &SuggestionFrame,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    suggestions[frame.start..frame.end]
        .iter()
        .enumerate()
        .map(|(offset, s)| {
            let is_active = frame.start + offset == cursor.active;
            let state = RowState {
                active: is_active,
                expanded: is_active && cursor.expanded,
            };
            suggestion_row(s, state, frame.cmd_col, width, theme)
        })
        .collect()
}

/// The trailing `▼` scroll indicator, present only when rows extend below the
/// window (a one-branch pure row builder).
fn down_arrow_line(frame: &SuggestionFrame, theme: &Theme) -> Option<Line<'static>> {
    frame
        .show_down
        .then(|| Line::styled("▼", secondary_style(theme)))
}

/// The trailing `(active+1/total)` counter, present only when the list
/// overflows the window (a one-branch pure row builder).
fn counter_line(
    total: usize,
    active: usize,
    frame: &SuggestionFrame,
    theme: &Theme,
) -> Option<Line<'static>> {
    frame
        .show_counter
        .then(|| Line::styled(format!("({}/{total})", active + 1), secondary_style(theme)))
}

/// The pure frame computation behind [`suggestion_rows`] (compute-plan
/// pattern): the visible `[start, end)` window from `scroll`, the command
/// column width, and whether the `▲`/`▼` scroll arrows and the `(n/m)` counter
/// chrome rows apply. All the arithmetic and branching lives here so
/// [`suggestion_rows`] is a call-only assembler folding this into `Line`s
/// (IOSP). Assumes `suggestions` is non-empty (the caller guards).
struct SuggestionFrame {
    start: usize,
    end: usize,
    cmd_col: usize,
    show_up: bool,
    show_down: bool,
    show_counter: bool,
}

fn suggestion_frame(
    suggestions: &[completion::Suggestion],
    scroll: usize,
    width: usize,
) -> SuggestionFrame {
    let total = suggestions.len();
    let start = scroll.min(total.saturating_sub(1));
    let end = (start + MAX_SUGGESTIONS).min(total);
    SuggestionFrame {
        start,
        end,
        cmd_col: command_column_width(suggestions, width),
        show_up: start > 0,
        show_down: end < total,
        show_counter: total > MAX_SUGGESTIONS,
    }
}

/// The "no matches" placeholder line (muted italic) - the empty-palette body.
fn no_matches_line(theme: &Theme) -> Line<'static> {
    Line::styled(
        "no matches",
        Style::default()
            .fg(tui_color(theme.muted))
            .add_modifier(Modifier::ITALIC),
    )
}

/// The width of the ` → `/` ← ` expand affordance (qwen SuggestionsDisplay), so
/// the label column can reserve room for it when a long row would show it.
const EXPAND_AFFORDANCE_COLS: usize = 3;

/// The command column width (qwen `commandColumnWidth`): the widest label,
/// floored at one column. Capped at HALF the popup width when a second
/// (description) column shares the row - the slash palette - to leave that
/// column room. When every suggestion's description is EMPTY (the AT file
/// picker: paths, no descriptions), there is no second column to reserve for, so
/// the label column uses the FULL inner width and a long path renders whole
/// instead of chopped at width/2 - minus the ` → ` affordance's columns when a
/// long row could show it, so the affordance never falls off the row's end. Pure.
fn command_column_width(suggestions: &[completion::Suggestion], width: usize) -> usize {
    let max_label = suggestions
        .iter()
        .map(|s| s.label.width())
        .max()
        .unwrap_or(0);
    let has_descriptions = suggestions.iter().any(|s| !s.description.is_empty());
    let cap = if has_descriptions {
        width / 2
    } else {
        // No description column: give the label the full inner width, but keep
        // the expand affordance's trailing columns when a long row could show it.
        let long_row = suggestions.iter().any(|s| label_is_long(&s.label));
        if long_row {
            width.saturating_sub(EXPAND_AFFORDANCE_COLS)
        } else {
            width
        }
    };
    max_label.min(cap).max(1)
}

/// One System B suggestion row's transient state (Parameter Object): whether it
/// is the active (highlighted) row and whether it is currently expanded (`←/→`).
/// Bundled so the row/label builders stay integration steps, not long
/// parameter lists.
#[derive(Debug, Clone, Copy)]
struct RowState {
    active: bool,
    expanded: bool,
}

/// One System B suggestion row: the label (fuzzy match inverted) in the command
/// column, padded to the boundary, then the description in the second column.
/// The active row reads `text.accent`, the rest `text.secondary`.
fn suggestion_row(
    s: &completion::Suggestion,
    state: RowState,
    cmd_col: usize,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let text_color = if state.active {
        accent_style(theme)
    } else {
        secondary_style(theme)
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = push_label_with_match(&mut spans, s, state.expanded, text_color, cmd_col);
    if used < cmd_col {
        used = push_cols(
            &mut spans,
            &" ".repeat(cmd_col - used),
            Style::default(),
            used,
            width,
        );
    }
    if !s.description.is_empty() {
        used = push_cols(&mut spans, "  ", Style::default(), used, width);
        used = push_cols(&mut spans, &s.description, text_color, used, width);
    }
    // The ` → `/` ← ` expand affordance (qwen SuggestionsDisplay:144-148):
    // only on a LONG active row - collapsed shows ` → ` (press → to expand),
    // expanded shows ` ← ` (press ← to collapse). Gray, trailing the row.
    if state.active && label_is_long(&s.label) {
        let indicator = if state.expanded { " ← " } else { " → " };
        let _ = push_cols(&mut spans, indicator, secondary_style(theme), used, width);
    }
    Line::from(spans)
}

/// Whether a label is "long" (chars `>= MAX_WIDTH`, qwen PrepareLabel): a long
/// row on the active line collapses to a truncated window until expanded.
fn label_is_long(label: &str) -> bool {
    label.chars().count() >= completion::MAX_WIDTH
}

/// Pushes a suggestion's label with its fuzzy match window drawn INVERTED (qwen
/// `PrepareLabel`: the match substring reversed against the row color). Returns
/// the new used-column count. The match window is `[start, end)` char indices
/// over the label; when absent the label draws plain. When the label is long
/// (`>= MAX_WIDTH`) and NOT `is_expanded`, it collapses to a truncated window
/// (qwen `PrepareLabel` cases 1-3), so the row fits; `is_expanded` shows it in
/// full.
fn push_label_with_match(
    spans: &mut Vec<Span<'static>>,
    s: &completion::Suggestion,
    is_expanded: bool,
    color: Style,
    width: usize,
) -> usize {
    let (before, matched, after) = prepare_label(&s.label, s.matched, is_expanded);
    let mut u = push_cols(spans, &before, color, 0, width);
    if !matched.is_empty() {
        u = push_cols(
            spans,
            &matched,
            color.add_modifier(Modifier::REVERSED),
            u,
            width,
        );
    }
    push_cols(spans, &after, color, u, width)
}

/// The qwen `PrepareLabel` split: `(before, matched, after)` char strings over
/// `label`, with the match window collapsed to a MAX_WIDTH-bounded window when
/// the label is long and not expanded. Pure - no ratatui.
///
/// - No match (or an out-of-range window): the whole label is `before`,
///   truncated to `MAX_WIDTH` + `...` when long and not expanded (qwen's
///   no-match branch).
/// - Expanded or already short (`<= MAX_WIDTH`): the full label split at the
///   match (qwen Case 1).
/// - Long + a match wider than MAX_WIDTH: only a truncated slice of the match
///   (qwen Case 2).
/// - Long + a shorter match: a window centred on the match with `...` elisions
///   at the clipped ends (qwen Case 3).
fn prepare_label(
    label: &str,
    matched: Option<(usize, usize)>,
    is_expanded: bool,
) -> (String, String, String) {
    let chars: Vec<char> = label.chars().collect();
    let len = chars.len();
    let slice = |a: usize, b: usize| -> String { chars[a.min(len)..b.min(len)].iter().collect() };
    let long = len > completion::MAX_WIDTH;

    let hit = matched.filter(|&(m_start, m_end)| m_start < len && m_start < m_end);
    let Some((m_start, raw_end)) = hit else {
        // No match: plain label, truncated when long and not expanded.
        let before = if !is_expanded && long {
            format!("{}...", slice(0, completion::MAX_WIDTH))
        } else {
            label.to_string()
        };
        return (before, String::new(), String::new());
    };
    let m_end = raw_end.min(len);
    let match_len = m_end - m_start;

    if is_expanded || !long {
        // Case 1: full label split at the match.
        return (slice(0, m_start), slice(m_start, m_end), slice(m_end, len));
    }
    if match_len >= completion::MAX_WIDTH {
        // Case 2: the match itself overflows - a truncated slice of it.
        let cut = m_start + completion::MAX_WIDTH - 1;
        return (
            String::new(),
            format!("{}...", slice(m_start, cut)),
            String::new(),
        );
    }
    // Case 3: a window centred on the match, `...`-elided at clipped ends.
    let context = completion::MAX_WIDTH - match_len;
    let before_space = context / 2;
    let after_space = context - before_space;
    let mut start = m_start.saturating_sub(before_space);
    let mut end = m_end + after_space;
    if m_start < before_space {
        end += before_space - m_start; // slide window right
    }
    if end > len {
        start = start.saturating_sub(end - len); // slide window left
        end = len;
    }
    let mut before = slice(start, m_start);
    let matched_str = slice(m_start, m_end);
    let mut after = slice(m_end, end);
    if start > 0 {
        before = elide_prefix(&before);
    }
    if end < len {
        after = elide_suffix(&after);
    }
    (before, matched_str, after)
}

// Replaces the first 3 chars of `s` with `...` (qwen `'...' + before.slice(3)`),
// or `...` when shorter than 3.
fn elide_prefix(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= 3 {
        format!("...{}", chars[3..].iter().collect::<String>())
    } else {
        "...".to_string()
    }
}

// Replaces the last 3 chars of `s` with `...` (qwen `after.slice(0, -3) +
// '...'`), or `...` when shorter than 3.
fn elide_suffix(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= 3 {
        format!("{}...", chars[..chars.len() - 3].iter().collect::<String>())
    } else {
        "...".to_string()
    }
}

/// The System A (numbered `›` dialog) rows for a committed command's list
/// (ADR-0051): the `selection_rows` shape, but over [`SelectorRow`]s whose role
/// decides the disabled mask (headers/greyed rows are dim + unnavigable, the
/// active navigable row is the `›`-marked success-green one) and whose hint
/// trails dimmed. Numbered per the row's position, matching the digit
/// quick-select.
fn dialog_rows(
    rows: &[SelectorRow],
    active: usize,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return vec![Line::styled(
            "no matches",
            Style::default()
                .fg(tui_color(theme.muted))
                .add_modifier(Modifier::ITALIC),
        )];
    }
    let width = inner_width as usize;
    // The number field is as wide as the widest `N.`.
    let num_field = format!("{}.", rows.len()).width();
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let is_active = i == active && row.is_stop();
            let navigable = row.is_stop();
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut used = 0;
            // The 2-wide `›`/space gutter (only a navigable active row marks).
            if is_active {
                used = push_cols(
                    &mut spans,
                    SELECTION_MARKER,
                    success_style(theme),
                    used,
                    width,
                );
                used = push_cols(&mut spans, " ", Style::default(), used, width);
            } else {
                used = push_cols(
                    &mut spans,
                    &" ".repeat(SELECTION_GUTTER_WIDTH),
                    Style::default(),
                    used,
                    width,
                );
            }
            // The right-aligned `N.` number (success-green when active, else
            // secondary; a non-navigable row draws no number).
            if navigable {
                let num = format!("{}.", i + 1);
                let pad = num_field.saturating_sub(num.width());
                if pad > 0 {
                    used = push_cols(&mut spans, &" ".repeat(pad), Style::default(), used, width);
                }
                let num_style = if is_active {
                    success_style(theme)
                } else {
                    secondary_style(theme)
                };
                used = push_cols(&mut spans, &num, num_style, used, width);
                used = push_cols(&mut spans, " ", Style::default(), used, width);
            } else {
                // A header/greyed row keeps the number field's width blank so
                // its label lines up with the members below it.
                used = push_cols(
                    &mut spans,
                    &" ".repeat(num_field + 1),
                    Style::default(),
                    used,
                    width,
                );
            }
            // The label: success-green when active, dim for header/greyed/note,
            // else primary.
            let label_style = match (is_active, row.role) {
                (true, _) => success_style(theme),
                (false, RowRole::Member) => primary_style(theme),
                (false, _) => secondary_style(theme),
            };
            used = push_cols(&mut spans, &row.label, label_style, used, width);
            // The hint trails dimmed (a "(current)" marker, a note's reason).
            if let Some(hint) = &row.hint {
                used = push_cols(&mut spans, "  ", Style::default(), used, width);
                let _ = push_cols(
                    &mut spans,
                    hint,
                    Style::default()
                        .fg(tui_color(theme.muted))
                        .add_modifier(Modifier::ITALIC),
                    used,
                    width,
                );
            }
            Line::from(spans)
        })
        .collect()
}

/// The most System-B suggestion rows the palette shows before it scrolls (qwen
/// `MAX_SUGGESTIONS_TO_SHOW`); mirrors [`completion::MAX_SUGGESTIONS_TO_SHOW`].
const MAX_SUGGESTIONS: usize = completion::MAX_SUGGESTIONS_TO_SHOW;

/// The most body rows the Slash popup shows before it scrolls internally - keeps
/// the overlay compact even against a long model list.
const POPUP_MAX_ROWS: u16 = 8;

/// The minimum guaranteed Session Picker width in columns.
const MODAL_MIN_WIDTH: u16 = 44;

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

/// Brings the [`RenderCache`] up to date with `screen`'s Transcript at
/// `content_width` (ADR-0046): the adapter's public door onto the cache's
/// (crate-private) sync, so [`commit_items`](crate::ui::commit_items) can sync
/// at the SAME content width the committed slice draws at (frame width minus
/// the two `CONTENT_MARGIN` columns) before measuring and blitting - keeping
/// measure == draw
/// (ADR-0029). The [`Toggles`] mirror the Screen's Ctrl+O compact flag.
pub fn sync_commit_cache(
    cache: &mut RenderCache,
    screen: &Screen,
    content_width: u16,
    theme: &Theme,
) {
    cache.sync(
        screen.transcript(),
        Toggles {
            compact: screen.compact_mode,
        },
        content_width,
        theme,
    );
}

/// The total wrapped height (visual rows) the committed slice `[hw, hw + count)`
/// draws to at `width` (ADR-0046): the wrapped-row count of the SAME
/// [`grouped_rows`] fold the pending body and [`render_committed_slice`] draw, so
/// the box borders + `marginTop:1` separators + gaps are counted (measure ==
/// draw, ADR-0029). A tall commit overflows into native scrollback, never
/// clamped. `slice.items` must be bounded to `hw + count`; `content_width` is the
/// width the cache was synced at (the frame width minus the two [`CONTENT_MARGIN`]
/// columns).
pub fn commit_slice_height(slice: &CommittedSlice<'_>, content_width: u16) -> u16 {
    let lines = grouped_rows(
        slice.cache,
        slice.items,
        slice.hw,
        content_width,
        slice.theme,
    );
    u16::try_from(wrapped_count(lines, content_width)).unwrap_or(u16::MAX)
}

/// The committed slice `[hw, hw + count)` to freeze into scrollback (ADR-0046):
/// the cache to draw from, the item list (BOUNDED to `hw + count` by the caller so
/// [`grouped_rows`] stops a tool group at the slice edge), and the ACTIVE `theme`
/// the frozen rows bake. Bundled so [`render_committed_slice`] and
/// [`commit_slice_height`] take a single source arg. `count` is retained for the
/// caller's bookkeeping; the fold stops at `items.len()`.
pub struct CommittedSlice<'a> {
    pub cache: &'a RenderCache,
    pub items: &'a [TranscriptItem],
    pub hw: usize,
    pub count: usize,
    pub theme: &'a Theme,
}

/// Blits the committed slice `[hw, hw + count)` into `buf` (ADR-0046, the inline
/// `insert_before` seam): the SAME [`grouped_rows`] fold the pending body draws -
/// prose items with baked prefixes, tool runs boxed, `marginTop:1` separators -
/// so a committed item is byte-identical to the live one before it froze. The
/// content sits [`CONTENT_MARGIN`] columns in (matching qwen `marginLeft:2`); the
/// caller sizes `buf` to [`commit_slice_height`], and a slice taller than the
/// terminal scrolls whole into native scrollback (no clamp, qwen `<Static>`).
pub fn render_committed_slice(buf: &mut Buffer, slice: &CommittedSlice<'_>) {
    let content_width = buf.area.width.saturating_sub(2 * CONTENT_MARGIN);
    let lines = grouped_rows(
        slice.cache,
        slice.items,
        slice.hw,
        content_width,
        slice.theme,
    );
    blit_body_lines(buf, lines);
}

/// Renders an already-assembled body line set into `buf`'s content column
/// ([`CONTENT_MARGIN`] columns in, matching qwen `marginLeft:2`), sized to its own
/// wrapped height so a stack taller than `buf` scrolls whole into native
/// scrollback (no clamp, qwen `<Static>`). The ONE `insert_before` blit shared by
/// the committed freeze ([`render_committed_slice`]) and the Ctrl-S peek
/// ([`render_pending_peek`]) - both hand it their `grouped_rows` output, so the
/// content-column geometry + wrap live in one place.
fn blit_body_lines(buf: &mut Buffer, lines: Vec<Line<'static>>) {
    let content_width = buf.area.width.saturating_sub(2 * CONTENT_MARGIN);
    let height = wrapped_count(lines.clone(), content_width) as u16;
    let content_area = Rect {
        x: buf.area.x + CONTENT_MARGIN,
        y: buf.area.y,
        width: content_width,
        height,
    };
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(content_area, buf);
}

/// The Ctrl-S peek (ADR-0046, [`Effect::PeekPending`]): the whole pending body
/// blitted UNCLAMPED into scrollback so the user can read the rows the live
/// viewport top-clips away. Bundles the mutable cache the body's settled tail
/// draws from, the `Screen` the pending items + live snapshot come from, the
/// animation counters (so the reasoning/spinner match the live frame), and the
/// ACTIVE theme the frozen rows bake. It renders at the LIVE high-water mark
/// ([`committed_high_water`](crate::ui::transcript::Transcript::committed_high_water),
/// the same `hw` the live draw uses), so the peek is the live stack plus its
/// clipped-off top, never the already-committed prefix (which is in scrollback).
///
/// [`Effect::PeekPending`]: crate::ui::screen::Effect::PeekPending
pub struct PendingPeek<'a> {
    pub cache: &'a mut RenderCache,
    pub screen: &'a Screen,
    pub anim: Anim,
    pub theme: &'a Theme,
}

impl PendingPeek<'_> {
    /// The full, unclamped pending body lines at content `width` and the live
    /// high-water mark. Syncs the cache as a side effect (the settled tail draws
    /// from it), exactly as the live body does.
    fn lines(&mut self, width: u16) -> Vec<Line<'static>> {
        let hw = self.screen.transcript().committed_high_water();
        let mut params = PendingBodyParams {
            screen: self.screen,
            cache: self.cache,
            anim: self.anim,
        };
        pending_body_lines(&mut params, self.theme, hw, width)
    }
}

/// The rows the Ctrl-S peek blits for a live viewport `area` (ADR-0046): the
/// wrapped-row count of the FULL, unclamped pending body when it OVERFLOWS its
/// body zone, else `0`. `area` is the inline viewport rect the live frame draws
/// in - the peek measures the body at that width and gates on the SAME overflow
/// condition the `… Ctrl-S to show more` marker uses ([`anchor_clip`]).
///
/// The `0` gate is the fix for the peek stacking duplicate copies into
/// scrollback: when the body FITS the viewport nothing is top-clipped, so no
/// marker shows and Ctrl-S has nothing to reveal. Without the gate every press
/// re-blitted the whole (fully-visible) body, so holding Ctrl-S piled up
/// identical copies. Mirrors [`commit_slice_height`]'s `0`-is-a-no-op contract.
pub fn pending_peek_height(peek: &mut PendingPeek<'_>, area: Rect) -> u16 {
    // The peek blits at FULL viewport width (its `insert_before` buffer is
    // full-width), so measure the unclamped body there - the SAME width the live
    // body zone draws at (the layout splits `area` vertically only).
    let content_width = area.width.saturating_sub(2 * CONTENT_MARGIN);
    let total = wrapped_count(peek.lines(content_width), content_width);
    let view = peek.screen.composer().view();
    let body_zone = pending_layout(area, &view, peek.screen).body;
    if total > body_zone.height as usize {
        u16::try_from(total).unwrap_or(u16::MAX)
    } else {
        0
    }
}

/// Blits the FULL, UNCLAMPED pending body into `buf` (ADR-0046, the Ctrl-S peek's
/// `insert_before` seam): the SAME line set the live body draws, but WITHOUT the
/// `anchor_clip` top-clip, so every row the live viewport hides lands in
/// scrollback for the user to scroll up to. The content sits `CONTENT_MARGIN`
/// columns in (matching the live body + committed blit); the caller sizes `buf` to
/// [`pending_peek_height`]. A PEEK, not a commit: the caller does NOT advance the
/// high-water mark, so the same body redraws (clipped) in the live viewport next
/// frame. Mirrors [`render_committed_slice`].
pub fn render_pending_peek(buf: &mut Buffer, peek: &mut PendingPeek<'_>) {
    let content_width = buf.area.width.saturating_sub(2 * CONTENT_MARGIN);
    let lines = peek.lines(content_width);
    blit_body_lines(buf, lines);
}

/// The rolling reasoning tail shown while a Run streams: an animated
/// `✦ Thinking ⠋` header (the braille [`SPINNER`] advanced by the adapter's
/// tick - motion lives HERE at the reasoning header, not the status bar), then
/// the last [`THINKING_TAIL_ROWS`] VISUAL rows of the reasoning, indented two
/// columns under the header as a sub-block. Empty when nothing is streaming.
///
/// Bounded by VISUAL rows, not source rows: one long unwrapped reasoning line
/// soft-wraps to many rows, which would let the short reasoning tail grow
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

// The lull "waiting" row (`lull_visible`/`live_lull_lines`) was folded into
// [`spinner_line`] (ADR-0048): the LoadingIndicator shows whenever the Run is
// Running and keeps the lull scene as its phrase content, so the separate quiet-
// only row is gone. The lull clock + scenes ([`lull`]) still drive the phrase.

/// The `k` (thousand) grouping unit `format_token_count` divides by.
const TOKEN_K: u64 = 1_000;
/// The `m` (million) grouping unit: at/above it, `format_token_count` renders
/// `N.Nm` (qwen `value >= 1_000_000 -> (value/1_000_000).toFixed(1) + "m"`).
const TOKEN_M: u64 = 1_000_000;
/// The threshold at/above which `format_token_count` drops the decimal (`Nk`),
/// and below which it shows one decimal (`N.Nk`).
const TOKEN_K_DECIMAL_LIMIT: u64 = 10_000;
/// The hundredths divisor used to round a token count to one decimal `k`: `count
/// / 100` rounded, then `/ 10`, matches JS `(count/1000).toFixed(1)`.
const TOKEN_HUNDREDTHS: f64 = 100.0;
/// The tenths divisor completing the one-decimal `k` rounding.
const TOKEN_TENTHS: f64 = 10.0;

/// A compact token count (qwen `formatTokenCount`, statusLinePresets.ts:217): the
/// bare number under 1000, `N.Nk` (one decimal, rounded) from 1000 to 9999, `Nk`
/// (floored) from 10000 to 999999, and `N.Nm` (one decimal, rounded) at 1000000
/// and above (qwen `2_400_000 -> "2.4m"`). Used by the spinner's `↑ 1.2k tokens`
/// figure.
fn format_token_count(count: u64) -> String {
    if count < TOKEN_K {
        return count.to_string();
    }
    if count < TOKEN_K_DECIMAL_LIMIT {
        // One decimal, ROUNDED (qwen's `.toFixed(1)` rounds 9999 -> "10.0k").
        let tenths = (count as f64 / TOKEN_HUNDREDTHS).round() / TOKEN_TENTHS;
        return format!("{tenths:.1}k");
    }
    if count < TOKEN_M {
        return format!("{}k", count / TOKEN_K);
    }
    // One decimal, ROUNDED (qwen `(value / 1_000_000).toFixed(1)`).
    let tenths = (count as f64 / (TOKEN_M as f64 / TOKEN_TENTHS)).round() / TOKEN_TENTHS;
    format!("{tenths:.1}m")
}

/// The in-flight facts the spinner line renders WITH (a Parameter Object so
/// [`spinner_line`] stays within the SRP param ceiling): the optional thought
/// `subject` (Phase-6 seam - wins over the lull phrase when `Some`, matching qwen
/// `thought?.subject || currentLoadingPhrase`), the optional live token `count`
/// (Phase-6 seam - shipped `None` to avoid per-frame jitter), and whether the
/// stream is `receiving` (streaming text non-empty - picks the `↑`/`↓` arrow).
#[derive(Debug, Clone, Copy, Default)]
struct SpinnerState<'a> {
    subject: Option<&'a str>,
    tokens: Option<u64>,
    receiving: bool,
}

/// The running spinner line (qwen `LoadingIndicator.tsx`, ADR-0041/0048): a
/// braille [`SPINNER`] frame, the phrase (the current lull scene content - a
/// deliberate divergence from qwen's `usePhraseCycler`, kept for the whimsy; the
/// [`SpinnerState::subject`] wins when `Some`), then the cancel group
/// `(<elapsed> [· <arrow> <tokens> tokens] · esc to cancel)` in secondary.
/// paddingLeft 2. Every produced row is truncated to `width` so it stays one
/// visual row (measure==draw, ADR-0029). Empty when the lull is still settling
/// (no phrase yet).
fn spinner_line(
    anim: Anim,
    state: SpinnerState<'_>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // The phrase is the current lull scene; while the lull settles there is no
    // scene yet, so the spinner line waits too (the lull row's settle window).
    let Some(phrase) = state
        .subject
        .or_else(|| lull::frame(anim.quiet_ticks, anim.lull_seq))
    else {
        return vec![];
    };
    let glyph = SPINNER[(anim.spinner as usize) % SPINNER.len()];
    let secs = anim.quiet_ticks.saturating_mul(crate::ui::TICK_MS) / MILLIS_PER_SEC;
    let elapsed = lull::format_elapsed(secs);
    let arrow = if state.receiving { "↓" } else { "↑" };
    let tokens_part = state
        .tokens
        .map(|n| format!(" · {arrow} {} tokens", format_token_count(n)))
        .unwrap_or_default();
    let cancel = format!("({elapsed}{tokens_part} · esc to cancel)");

    let style = Style::default()
        .fg(tui_color(theme.lull))
        .add_modifier(Modifier::ITALIC);
    let secondary = secondary_style(theme);
    // paddingLeft 2, then `<glyph> <phrase>  <cancel>` - built span-by-span so the
    // cancel group reads secondary while the phrase reads the lull colour, then
    // truncated as a whole to one visual row.
    let text = format!("  {glyph} {phrase}  {cancel}");
    // The phrase+glyph fit first; if the whole line overflows, truncate it (the
    // rare narrow case) - the common case is well within width.
    if text.chars().count() <= width as usize {
        return vec![Line::from(vec![
            Span::styled(format!("  {glyph} {phrase}  "), style),
            Span::styled(cancel, secondary),
        ])];
    }
    vec![Line::styled(truncate_visual(&text, width as usize), style)]
}

// ---------------------------------------------------------------------------
// The sticky "Current tasks" box (qwen `StickyTodoList.tsx` + `todoSnapshot.ts`,
// ADR-0048). A LIVE overlay (uncached, pending-only, never in grouped_rows/the
// committed slice - like the lull row): it DERIVES from the Transcript's latest
// `Todo` item, so it and the committed inline copy read one source of truth.
// ---------------------------------------------------------------------------

/// The most sticky-todo rows shown before the overflow line (qwen
/// `STICKY_TODO_MAX_VISIBLE_ITEMS = 5`, todoSnapshot.ts:29).
const STICKY_TODO_MAX_VISIBLE: usize = 5;

/// The status-priority order the sticky box lists items in (qwen
/// `STICKY_TODO_STATUS_PRIORITY`, todoSnapshot.ts:32): in_progress first, then
/// pending, then completed - a stable sort keyed by the ORIGINAL index breaks
/// ties, so the number label stays the item's real position.
fn sticky_status_priority(status: TodoStatus) -> u8 {
    match status {
        TodoStatus::InProgress => 0,
        TodoStatus::Pending => 1,
        TodoStatus::Completed => 2,
    }
}

/// The sticky box's items in display order (qwen `getOrderedStickyTodos`): a
/// STABLE sort by status priority, each paired with its ORIGINAL index so the
/// number label (`index + 1`) survives the reorder. Pure.
fn ordered_sticky_todos(items: &[TodoItem]) -> Vec<(usize, &TodoItem)> {
    let mut ordered: Vec<(usize, &TodoItem)> = items.iter().enumerate().collect();
    // `sort_by_key` is stable, so equal-priority items keep their original order
    // (the index tie-break qwen spells out explicitly).
    ordered.sort_by_key(|(_, item)| sticky_status_priority(item.status));
    ordered
}

/// Whether the sticky "Current tasks" box shows this frame, and the items it
/// draws (qwen `getStickyTodos`, todoSnapshot.ts:120, ADR-0048): the latest
/// `Todo`'s items show iff the list is NON-EMPTY, NOT all-completed, AND the
/// item has COMMITTED (`latest_index < high_water`). The high-water gate
/// collapses qwen's pending/recent guards onto the ADR-0046 commit fact: while
/// the todo is still pending it renders inline above the composer, so the sticky
/// box would double it; once it commits to scrollback the inline copy scrolls
/// away and the sticky box takes over. Pure - a testable predicate, no frame.
fn sticky_todos(latest: Option<(usize, &[TodoItem])>, high_water: usize) -> Option<&[TodoItem]> {
    let (index, items) = latest?;
    let non_empty = !items.is_empty();
    let all_completed = non_empty && items.iter().all(|i| i.status == TodoStatus::Completed);
    let committed = index < high_water;
    (non_empty && !all_completed && committed).then_some(items)
}

/// The vertical rows the sticky box occupies for `visible` shown items and
/// `overflowed` (whether an overflow line is needed): a rounded top + bottom
/// border (2), the `Current tasks` header (1), the visible rows (capped at
/// [`STICKY_TODO_MAX_VISIBLE`]), and one overflow row when hidden items remain.
/// Pure - the exact height `frame_chunks` reserves so measure==draw (ADR-0029).
fn sticky_todos_height(count: usize) -> usize {
    let visible = count.min(STICKY_TODO_MAX_VISIBLE);
    let overflow = usize::from(count > STICKY_TODO_MAX_VISIBLE);
    2 + 1 + visible + overflow
}

/// The minimum body height the pending region keeps when the sticky box shows:
/// one row (`Constraint::Min(1)`) so the live tail never fully collapses.
const STICKY_MIN_BODY_ROWS: usize = 1;

/// Whether a `sticky_height`-row "Current tasks" box fits this frame alongside
/// the status row (1), the composer, and at least one body row. Pure predicate -
/// the show/hide guard so a short terminal drops the box rather than letting
/// Layout squeeze its zone below the measured height (ADR-0029 measure==draw).
fn sticky_fits(frame_height: usize, sticky_height: usize, composer_height: usize) -> bool {
    let reserved = sticky_height
        .saturating_add(1) // status bar
        .saturating_add(composer_height)
        .saturating_add(STICKY_MIN_BODY_ROWS);
    reserved <= frame_height
}

/// The sticky box's draw rect inside its zone: the zone inset by the marginX 2
/// gutter (qwen `marginX={2}`) so the box aligns under the [`CONTENT_MARGIN`]
/// pending body. Pure.
fn sticky_box_area(zone: Rect) -> Rect {
    Rect {
        x: zone.x + CONTENT_MARGIN,
        width: zone.width.saturating_sub(2 * CONTENT_MARGIN),
        ..zone
    }
}

/// The sticky "Current tasks" box's lines (qwen `StickyTodoList.tsx`), a rounded
/// box marginX 2 paddingX 1: a GREY bold `Current tasks` header (secondary, NOT
/// accent), then up to [`STICKY_TODO_MAX_VISIBLE`] rows in priority order - each
/// a `N.` number label (the ORIGINAL index+1, secondary), the status glyph
/// (in_progress green else primary), and the content truncated-end (completed
/// crossed-out) - then a secondary `... and N more` overflow row. Every row is
/// funneled through [`box_row`] to exactly the inner width so the box corners
/// align (measure==draw, ADR-0029). `width` is the FULL box width (the frame
/// less the marginX gutter the caller applied).
fn render_sticky_todos(frame: &mut Frame, area: Rect, items: &[TodoItem], theme: &Theme) {
    // Integration (IOSP): the pure line-builder shapes every row; here we only
    // issue the draw call.
    let inner = (area.width as usize).saturating_sub(2); // the two `│` columns
    let mut lines = sticky_todos_lines(items, inner, theme);
    // Clamp to the zone height: if Layout shrank the sticky zone (a short frame),
    // draw only what fits rather than letting the Paragraph over-draw the rows
    // below (ADR-0029 measure==draw - the show/hide guard should keep these equal,
    // but the clamp holds even if the zone is ever squeezed).
    lines.truncate(area.height as usize);
    frame.render_widget(Paragraph::new(lines), area);
}

/// The 2-column glyph column width every sticky row reserves for its status
/// glyph (qwen `<Box width={2}>`): the 1-cell circle plus one clear cell.
const STICKY_GLYPH_COL: usize = 2;

/// The pure column arithmetic for a sticky box (qwen `StickyTodoList` layout
/// math), lifted out of [`sticky_todos_lines`] so that Integration folds
/// pre-computed columns instead of interleaving arithmetic with `box_row` calls
/// (IOSP: an Operation returns a value; the Integration only calls). `visible` is
/// the shown-row count (capped at [`STICKY_TODO_MAX_VISIBLE`]), `hidden` the
/// overflow, `num_col`/`content_col` the two content columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StickyColumns {
    visible: usize,
    hidden: usize,
    num_col: usize,
    content_col: usize,
}

/// Operation (IOSP): the sticky box's column arithmetic for `ordered` rows at box
/// `inner` width. Pure value - no calls beyond the leaf width helper.
fn sticky_columns(ordered: &[(usize, &TodoItem)], inner: usize) -> StickyColumns {
    let visible = ordered.len().min(STICKY_TODO_MAX_VISIBLE);
    let hidden = ordered.len() - visible;
    let num_col = sticky_number_col(&ordered[..visible]);
    // The content column: inner width less the number and glyph columns (qwen
    // truncates the content, never wraps it).
    let content_col = inner.saturating_sub(num_col + STICKY_GLYPH_COL).max(1);
    StickyColumns {
        visible,
        hidden,
        num_col,
        content_col,
    }
}

/// Operation (IOSP): the un-boxed spans of every sticky content row (header,
/// then the priority-ordered task rows, then the `... and N more` overflow row)
/// in draw order. Returns pre-computed span vectors so [`sticky_todos_lines`]
/// only folds them through [`box_row`] - the split that keeps that Integration
/// call-only (no interleaved arithmetic). Pure.
fn sticky_rows(
    ordered: &[(usize, &TodoItem)],
    cols: StickyColumns,
    theme: &Theme,
) -> Vec<Vec<Span<'static>>> {
    // The header: GREY bold (qwen `text.secondary` bold), inside the box.
    let mut rows = vec![vec![Span::styled(
        "Current tasks",
        secondary_style(theme).add_modifier(Modifier::BOLD),
    )]];
    rows.extend(
        ordered[..cols.visible].iter().map(|(orig, item)| {
            sticky_row_spans(*orig, item, cols.num_col, cols.content_col, theme)
        }),
    );
    if cols.hidden > 0 {
        rows.push(sticky_overflow_spans(
            cols.hidden,
            cols.num_col,
            cols.content_col,
            theme,
        ));
    }
    rows
}

/// Integration (IOSP): the sticky box's lines for `items` at box `inner` width -
/// the rounded top border, the pre-computed content rows ([`sticky_rows`], using
/// the pre-computed [`sticky_columns`]), and the bottom border. Every content row
/// is funneled through [`box_row`] to exactly `inner + 2` columns (measure==draw,
/// ADR-0029). No arithmetic here - it only calls. Pure - no frame.
fn sticky_todos_lines(items: &[TodoItem], inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    let border = border_style(theme);
    let ordered = ordered_sticky_todos(items);
    let cols = sticky_columns(&ordered, inner);

    let mut lines = vec![box_top(inner, border)];
    lines.extend(
        sticky_rows(&ordered, cols, theme)
            .iter()
            .map(|spans| box_row(spans, inner, border)),
    );
    lines.push(box_bottom(inner, border));
    lines
}

/// The rounded top border row `╭───╮` at `inner` interior width. Leaf.
fn box_top(inner: usize, border: Style) -> Line<'static> {
    Line::styled(format!("╭{}╮", "─".repeat(inner)), border)
}

/// The rounded bottom border row `╰───╯` at `inner` interior width. Leaf.
fn box_bottom(inner: usize, border: Style) -> Line<'static> {
    Line::styled(format!("╰{}╯", "─".repeat(inner)), border)
}

/// The number-column width for the shown rows (qwen `numberColumnWidth`): the
/// widest `N.` label plus one clear column, so the glyph column always aligns.
fn sticky_number_col(shown: &[(usize, &TodoItem)]) -> usize {
    shown
        .iter()
        .map(|(orig, _)| format!("{}.", orig + 1).chars().count())
        .max()
        .unwrap_or(2)
        + 1
}

/// The overflow row's spans (qwen `... and {{count}} more`): hung under the
/// content column (past the number + glyph columns), secondary.
fn sticky_overflow_spans(
    hidden: usize,
    num_col: usize,
    content_col: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    vec![
        Span::raw(" ".repeat(num_col + STICKY_GLYPH_COL)),
        Span::styled(
            truncate_visual(&format!("... and {hidden} more"), content_col),
            secondary_style(theme),
        ),
    ]
}

/// The spans for one sticky-todo row: the `N.` number label (original index+1,
/// secondary) padded to `num_col`, the status glyph (in_progress green else
/// primary) in a 2-wide column, and the content truncated to `content_col`
/// (completed crossed-out). qwen `StickyTodoList` `TodoItemRow`.
fn sticky_row_spans(
    orig_index: usize,
    item: &TodoItem,
    num_col: usize,
    content_col: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let item_style = if item.status == TodoStatus::InProgress {
        success_style(theme)
    } else {
        primary_style(theme)
    };
    let content_style = if item.status == TodoStatus::Completed {
        item_style.add_modifier(Modifier::CROSSED_OUT)
    } else {
        item_style
    };
    let label = format!("{}.", orig_index + 1);
    let label = format!("{label:<num_col$}");
    vec![
        Span::styled(label, secondary_style(theme)),
        Span::styled(format!("{} ", item.status.glyph()), item_style),
        Span::styled(truncate_visual(&item.content, content_col), content_style),
    ]
}

/// Truncates `text` to at most `width` display columns, replacing the trimmed
/// tail with a single `…` so an over-long reasoning line stays one visual row.
/// Char-based (like the rest of this module) - a truncated row is always `<=
/// width` chars, so the viewport's `Wrap` never breaks it onto a second row.
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

/// The content side margin (columns): qwen `HistoryItemDisplay` wraps every item
/// in `marginLeft:2, marginRight:2` (HistoryItemDisplay.tsx:64), so content is
/// the frame width minus a 2-col left AND 2-col right margin. `pub(crate)` so the
/// adapter sizes the committed-slice content width (ADR-0046) at the same margin
/// the pending region uses.
pub(crate) const CONTENT_MARGIN: u16 = 2;

/// The blank `marginTop:1` separator row between committed items (qwen
/// `HistoryItemDisplay.tsx:64`; continuation types get `marginTop:0`). Emitted at
/// assembly by [`grouped_rows`], never cached.
fn separator_row() -> Line<'static> {
    Line::default()
}

/// Folds the settled items `[hw..]` into the flat committed body every render
/// path draws (ADR-0046 + the render-time tool-group ADR): a non-tool item passes
/// through as its cached lines; a MAXIMAL contiguous run of tool items
/// (ToolCall/ToolResult/Diff) is boxed by [`render_tool_group`]; a blank
/// [`separator_row`] sits between items (qwen `marginTop:1`). BOTH the pending
/// body and [`render_committed_slice`] call this, so committed == pending is
/// byte-identical. `items` is the FULL item list; only `[hw..]` is emitted (the
/// prefix is already frozen into scrollback). `width` is the content width the
/// cache was synced at.
fn grouped_rows(
    cache: &RenderCache,
    items: &[TranscriptItem],
    hw: usize,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    grouped_rows_with_approval(&GroupedRows {
        cache,
        items,
        hw,
        width,
        theme,
        approving: None,
    })
}

/// The confirming context for the inline approval (ADR-0049): the pending
/// Approval and the item index of the confirming ToolCall (the newest live one).
/// A Parameter Object so the confirming state threads through the group render
/// as one borrow rather than two loose args. `None` at every committed / test
/// call site - the confirming call never commits (it has no result), so a frozen
/// slice never carries it.
struct Approving<'a> {
    pending: &'a PendingApproval,
    /// The index into `items` of the confirming ToolCall.
    call_index: usize,
}

/// The full input to the grouped-rows fold (a Parameter Object): the cache, the
/// item list + high-water mark, the content width, the theme, and the optional
/// inline approval. Bundled so [`grouped_rows_with_approval`] takes one borrow.
struct GroupedRows<'a> {
    cache: &'a RenderCache,
    items: &'a [TranscriptItem],
    hw: usize,
    width: u16,
    theme: &'a Theme,
    approving: Option<&'a Approving<'a>>,
}

/// [`grouped_rows`] with an optional inline approval (ADR-0049): when `approving`
/// names a confirming ToolCall inside a tool group, that group renders with a
/// `warning` border, a `?` marker on the confirming call, and the approval block
/// (question + radio) appended inside its box.
fn grouped_rows_with_approval(spec: &GroupedRows<'_>) -> Vec<Line<'static>> {
    let &GroupedRows {
        cache,
        items,
        hw,
        width,
        theme,
        approving,
    } = spec;
    // Integration (IOSP): the pure fold below decides the segments; here we only
    // render each and interleave the `marginTop:1` separators.
    let cached: Vec<&[Line<'static>]> = cache.settled().map(|(lines, _)| lines).collect();
    let ctx = GroupCtx {
        items,
        cached: &cached,
        width,
        theme,
        approving,
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    for (n, segment) in group_segments(items, hw).into_iter().enumerate() {
        if n > 0 {
            out.push(separator_row());
        }
        out.extend(render_segment(segment, &ctx));
    }
    out
}

/// The invariant render context threaded through the tool-group fold (a
/// Parameter Object): the item list, their cached inner lines, the box width,
/// the active theme, and the optional inline approval. Bundled so the group
/// render functions take one borrow instead of five loose args - the segment
/// index is the only per-call variable.
struct GroupCtx<'a> {
    items: &'a [TranscriptItem],
    cached: &'a [&'a [Line<'static>]],
    width: u16,
    theme: &'a Theme,
    approving: Option<&'a Approving<'a>>,
}

/// One render segment of the settled tail (ADR-0047): either a single non-tool
/// item drawn from its cached lines, or a maximal contiguous run of tool items
/// boxed together. A range `[start, end)` into the item list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    /// A single non-tool item at this index (passes through as cached lines).
    Item(usize),
    /// A `[start, end)` run of tool items (rendered as one box).
    ToolGroup(usize, usize),
}

/// Operation (IOSP): segments the settled tail `[hw..]` into passthrough items
/// and maximal tool-runs (ADR-0047). Pure over the item sequence - no cache, no
/// draw - so the grouping rule is asserted without a frame.
fn group_segments(items: &[TranscriptItem], hw: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut i = hw;
    while i < items.len() {
        if is_tool_item(&items[i]) {
            let start = i;
            while i < items.len() && is_tool_item(&items[i]) {
                i += 1;
            }
            out.push(Segment::ToolGroup(start, i));
        } else {
            out.push(Segment::Item(i));
            i += 1;
        }
    }
    out
}

/// Renders one [`Segment`] to its lines: a passthrough item's cached lines, or the
/// boxed tool run ([`render_tool_group`]).
fn render_segment(segment: Segment, ctx: &GroupCtx<'_>) -> Vec<Line<'static>> {
    match segment {
        Segment::Item(i) => ctx.cached.get(i).map(|l| l.to_vec()).unwrap_or_default(),
        Segment::ToolGroup(start, end) => {
            // The confirming call (if any) falls in THIS group iff its index is
            // inside `[start, end)`; its group-local offset drives the marker
            // flip + approval block.
            let confirming = ctx
                .approving
                .filter(|a| (start..end).contains(&a.call_index))
                .map(|a| (a.call_index - start, a.pending));
            render_tool_group(
                &ctx.items[start..end],
                &ctx.cached[start..end],
                ctx.width,
                ctx.theme,
                confirming,
            )
        }
    }
}

/// Draws a contiguous tool run as ONE rounded box (qwen `ToolGroupMessage`,
/// borderStyle:"round"): a top border, each tool's cached INNER lines wrapped with
/// `│` side borders + padded to the full box width, a blank `gap:1` row between
/// tools, then a bottom border. `borderColor` precedence (ToolGroupMessage.tsx
/// :325): a shell tool → `ui.symbol`; else `border.default` (a settled group is
/// never pending). Every boxed row is funneled through [`box_row`] and padded to
/// exactly `width` (the box-rigidity invariant, ADR-0029).
fn render_tool_group(
    items: &[TranscriptItem],
    cached: &[&[Line<'static>]],
    width: u16,
    theme: &Theme,
    confirming: Option<(usize, &PendingApproval)>,
) -> Vec<Line<'static>> {
    // Integration (IOSP): the border colour + the inner body rows are computed in
    // the operations below; here we only stack the top border, the body, and the
    // bottom border.
    let inner = (width as usize).saturating_sub(2); // the two `│` border columns
    let border = group_border_style(items, confirming.is_some(), theme);
    let body = BoxBody {
        items,
        cached,
        inner,
        border,
        theme,
        confirming,
    };
    let mut out = vec![Line::styled(format!("╭{}╮", "─".repeat(inner)), border)];
    out.extend(box_body_rows(&body));
    out.push(Line::styled(format!("╰{}╯", "─".repeat(inner)), border));
    out
}

/// The boxed-body render context (a Parameter Object for [`box_body_rows`]): the
/// tool items, their cached inner lines, the inner width, the border style, the
/// theme, and the optional confirming `(group-local index, pending)`. Bundled so
/// the body render takes one borrow.
struct BoxBody<'a> {
    items: &'a [TranscriptItem],
    cached: &'a [&'a [Line<'static>]],
    inner: usize,
    border: Style,
    theme: &'a Theme,
    confirming: Option<(usize, &'a PendingApproval)>,
}

/// The border colour a tool group wears (qwen ToolGroupMessage.tsx:325, with the
/// Phase-4 warning branch): the precedence is shell → `ui.symbol` (grey) >
/// confirming → `status.warning` > `border.default`. A confirming group (one
/// holding a ToolCall awaiting an Approval decision) reads warning UNLESS a shell
/// tool in the group already claims the symbol colour - qwen's shell precedence
/// wins, so `run_command` (a shell tool) keeps its grey border even mid-approval.
fn group_border_style(items: &[TranscriptItem], confirming: bool, theme: &Theme) -> Style {
    if items.iter().any(is_group_shell) {
        symbol_style(theme)
    } else if confirming {
        warning_style(theme)
    } else {
        border_style(theme)
    }
}

/// Operation (IOSP): the boxed body rows for a tool run - each tool's inner lines
/// wrapped with side borders, a bordered `gap:1` blank row between tools. Cached
/// lines drive every tool EXCEPT the confirming one (ADR-0049): that call is
/// re-rendered fresh so its marker flips `⊷`→`?`, and the approval block
/// (question + radio) is appended after it. Every row is funneled through
/// [`box_row`] to the exact inner width (ADR-0029).
fn box_body_rows(body: &BoxBody<'_>) -> Vec<Line<'static>> {
    let BoxBody {
        items,
        cached,
        inner,
        border,
        theme,
        confirming,
    } = *body;
    let mut out = Vec::new();
    for (t, lines) in cached.iter().enumerate() {
        if t > 0 {
            out.push(box_row(&[], inner, border));
        }
        // The confirming call re-renders with a `?` marker + the approval block
        // appended, instead of drawing from the cache (which knows nothing of
        // the pending Approval - keeping committed==pending byte-identity).
        if let Some((idx, pending)) = confirming
            && idx == t
        {
            let fresh = confirming_inner_lines(&items[t], pending, inner as u16, theme);
            out.extend(fresh.iter().map(|line| box_row(&line.spans, inner, border)));
            continue;
        }
        out.extend(lines.iter().map(|line| box_row(&line.spans, inner, border)));
    }
    out
}

/// One boxed content row: the `│` left border, the row's spans (truncated so the
/// row never exceeds the inner width), a pad to exactly `inner` columns, then the
/// `│` right border. The rigidity workhorse - every boxed row is exactly
/// `inner + 2` columns so the right border always aligns (ADR-0029). qwen adds a
/// `paddingX:1` inside the border; that pad is the first/last inner column here.
fn box_row(spans: &[Span<'static>], inner: usize, border: Style) -> Line<'static> {
    let mut out = vec![Span::styled("│", border)];
    // paddingX:1 left.
    let mut used = 0;
    used = push_cols(&mut out, " ", Style::default(), used, inner);
    for span in spans {
        used = push_cols(&mut out, &span.content, span.style, used, inner);
    }
    if used < inner {
        out.push(Span::raw(" ".repeat(inner - used)));
    }
    out.push(Span::styled("│", border));
    Line::from(out)
}

/// The index of the newest live ToolCall (ADR-0049): the last
/// `TranscriptItem::ToolCall` still awaiting its result (a ToolResult supersedes
/// the call, so any surviving ToolCall item is unresolved). The confirming
/// Approval attaches here. `None` when no call is live.
fn newest_live_tool_index(items: &[TranscriptItem]) -> Option<usize> {
    items
        .iter()
        .rposition(|item| matches!(item, TranscriptItem::ToolCall { .. }))
}

/// Whether an item is a shell tool call/result (drives the group's border colour).
fn is_group_shell(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::ToolCall { name, .. } | TranscriptItem::ToolResult { name, .. } => {
            is_shell_tool(name)
        }
        _ => false,
    }
}

/// The grey style settled Thinking draws in (qwen `ThinkMessage`
/// `text.secondary`). No italic - qwen thoughts read as plain grey markdown.
fn thinking_style(theme: &Theme) -> Style {
    secondary_style(theme)
}

/// A settled Thinking item's lines (qwen `ThinkMessage`, ConversationMessages.tsx
/// :250): the grey `✦` U+2726 marker + grey markdown body, hung under the 2-col
/// prefix. qwen has NO per-thought collapse - a thought either shows in full or
/// is hidden entirely by compact mode (the show/hide decision is the caller's,
/// ADR-0052), so this always renders the full grey body.
fn settled_thinking_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    prefixed_markdown_lines(
        "✦",
        thinking_style(theme),
        markdown_lines(text, theme)
            .into_iter()
            .map(|line| recolor_line(line, thinking_style(theme)))
            .collect(),
    )
}

/// Overrides every span's fg with `style`'s colour while keeping modifiers, so a
/// Thinking body reads uniformly grey (qwen colours the whole `ThinkMessage`
/// markdown `text.secondary`).
fn recolor_line(line: Line<'static>, style: Style) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(style)))
            .collect::<Vec<_>>(),
    )
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

/// The single detail-on-demand display toggle the settled lines are built with:
/// compact mode (Ctrl+O, qwen `compactMode`, ADR-0052). `compact == true` hides
/// settled Thinking items entirely and folds tool result bodies to their header
/// rows. Named field (not a bare `bool` parameter) so the cache key reads at
/// every call site and a future second display fact has an obvious home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Toggles {
    pub(crate) compact: bool,
}

pub use render_cache::RenderCache;

/// The cache is a private child module (the same move ADR-0034 made for the
/// store's streaming snapshot): still in `ui/components`, still ratatui
/// [`Line`]s (ADR-0029 rejects a frame-free extraction). The boundary exists
/// so the fields are genuinely private - the frame path reads through the two
/// accessors - and the extend-vs-rebuild invariant is pinned by unit tests at
/// this seam, next to the state they inspect, not through full-screen renders.
mod render_cache;

/// The rows `lines` wrap to at `width`, measured by a throwaway `Paragraph`
/// with the SAME `Wrap { trim: false }` the viewport draws with - the window
/// math is only correct if measuring and drawing agree exactly.
fn wrapped_count(lines: Vec<Line<'static>>, width: u16) -> usize {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// The ASCII wordmark logo (qwen `AsciiArt.ts` `shortAsciiLogo`), "suspenders"
/// in the ANSI-Shadow block font. All 6 rows are EXACTLY 83 display columns wide,
/// so the two-column width gate ([`header_lines`]) can size the layout against a
/// fixed logo width. Drawn in the theme accent colour.
const HEADER_LOGO: &str = "\
███████╗██╗   ██╗███████╗██████╗ ███████╗███╗   ██╗██████╗ ███████╗██████╗ ███████╗
██╔════╝██║   ██║██╔════╝██╔══██╗██╔════╝████╗  ██║██╔══██╗██╔════╝██╔══██╗██╔════╝
███████╗██║   ██║███████╗██████╔╝█████╗  ██╔██╗ ██║██║  ██║█████╗  ██████╔╝███████╗
╚════██║██║   ██║╚════██║██╔═══╝ ██╔══╝  ██║╚██╗██║██║  ██║██╔══╝  ██╔══██╗╚════██║
███████║╚██████╔╝███████║██║     ███████╗██║ ╚████║██████╔╝███████╗██║  ██║███████║
╚══════╝ ╚═════╝ ╚══════╝╚═╝     ╚══════╝╚═╝  ╚═══╝╚═════╝ ╚══════╝╚═╝  ╚═╝╚══════╝";

/// The fixed display width of every [`HEADER_LOGO`] row (qwen `getAsciiArtWidth`).
const HEADER_LOGO_WIDTH: usize = 83;

/// The gap columns between the logo and the info panel (qwen `logoGap`).
const HEADER_LOGO_GAP: usize = 2;

/// The minimum readable working-directory path width (qwen `minPathLength`); with
/// the box chrome it sets the minimum info-panel width the logo must leave room
/// for before the two-column layout is used.
const HEADER_MIN_PATH: usize = 40;

/// The info panel's inner content width in a two-column layout is capped here
/// (qwen `maxInfoPanelWidth = 60`, minus the box chrome), so a very wide terminal
/// does not stretch the panel across the whole screen beside the logo.
const HEADER_MAX_PANEL_INNER: usize = 60 - HEADER_BOX_CHROME;

/// The box chrome width the info panel spends on borders + padding: `│ ` left and
/// ` │` right (qwen `borderWidth 2 + paddingX*2`).
const HEADER_BOX_CHROME: usize = 4;

/// The borrowed startup Header facts the render path takes (qwen `AppHeader`
/// props): the brand title, crate version, scoped model id, working directory,
/// and the startup tip. A value object so [`header_lines`] takes one borrow.
struct HeaderView<'a> {
    title: &'a str,
    version: &'a str,
    model: &'a str,
    cwd: &'a str,
    tip: &'a str,
}

/// The widest the STACKED tier lets its info panel + tips grow (columns): a full
/// content width beyond this reads the cap, so the box and tips do not sprawl the
/// whole screen under a full-width logo banner. Chosen at qwen's `maxInfoPanelWidth`.
const HEADER_STACKED_MAX_WIDTH: usize = 80;

/// Which of the three width tiers the startup [`TranscriptItem::Header`] draws in,
/// resolved from the content width `W` against the fixed 83-col logo. The gate is
/// the ONE place the tier boundaries live so the render and the tests agree:
///
/// * [`HeaderTier::SideBySide`] - `W >= 83 + gap(2) + min_panel(44) = 129`: the
///   logo left, the boxed panel right.
/// * [`HeaderTier::Stacked`] - `83 <= W < 129`: the full-width logo banner on top,
///   the boxed panel (capped at [`HEADER_STACKED_MAX_WIDTH`]) below it, left-aligned.
/// * [`HeaderTier::NoLogo`] - `W < 83`: the logo cannot fit, so the panel (+ tips)
///   render alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderTier {
    SideBySide,
    Stacked,
    NoLogo,
}

/// Resolves the [`HeaderTier`] for a content width (the ONE gate, so render and
/// tests share the boundary math).
fn header_tier(available: usize) -> HeaderTier {
    let min_panel = HEADER_MIN_PATH + HEADER_BOX_CHROME;
    if available >= HEADER_LOGO_WIDTH + HEADER_LOGO_GAP + min_panel {
        HeaderTier::SideBySide
    } else if available >= HEADER_LOGO_WIDTH {
        HeaderTier::Stacked
    } else {
        HeaderTier::NoLogo
    }
}

/// The lines the startup [`TranscriptItem::Header`] banner renders as (qwen
/// `AppHeader` = `Header` + `Tips`): the ASCII wordmark logo (accent), a single-
/// border info panel, and a `Tips:` line below - arranged by [`header_tier`] into
/// one of three width tiers (side-by-side / stacked / no-logo). The epic wordmark
/// shows on ANY terminal that can fit its 83 columns (tiers 1-2); only a truly
/// narrow terminal (< 83) hides it. Every produced [`Line`] is `<= content_width`
/// columns (the box rows funnelled through the same column-exact assembly as the
/// tool-group box), so the viewport's `Wrap` never re-breaks it (measure==draw,
/// ADR-0029).
fn header_lines(view: &HeaderView<'_>, content_width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let available = content_width as usize;
    let tier = header_tier(available);

    // The info panel's inner content width, per tier:
    // - SideBySide: the space left of the logo + gap, capped at qwen's max.
    // - Stacked:    the width up to HEADER_STACKED_MAX_WIDTH, minus box chrome.
    // - NoLogo:     the full width minus box chrome.
    let panel_inner = match tier {
        HeaderTier::SideBySide => {
            (available - HEADER_LOGO_WIDTH - HEADER_LOGO_GAP - HEADER_BOX_CHROME)
                .min(HEADER_MAX_PANEL_INNER)
        }
        HeaderTier::Stacked => available
            .min(HEADER_STACKED_MAX_WIDTH)
            .saturating_sub(HEADER_BOX_CHROME),
        HeaderTier::NoLogo => available.saturating_sub(HEADER_BOX_CHROME),
    }
    .max(1);

    // The bordered info panel (qwen `Header` info column): the 4 content rows
    // wrapped in a single-line box - always drawn; the logo placement is the tier's.
    let panel = header_boxed_panel(view, panel_inner, theme);
    let mut out = match tier {
        HeaderTier::SideBySide => header_two_column(&panel, theme),
        HeaderTier::Stacked => header_stacked(panel, theme),
        HeaderTier::NoLogo => panel,
    };
    // The Tips line below the box (qwen `Tips`), in secondary, `<= content_width`.
    out.push(header_tips_line(view.tip, available, theme));
    out
}

/// The bordered info panel (qwen `Header`): the 4 content rows funnelled through
/// [`box_row`] to the exact `inner` width, framed with a single-line top/bottom
/// border - exactly `inner + 2` columns per row and 6 rows tall (1 top + 4 content
/// + 1 bottom), so it lines up beside the 6-row logo in the two-column layout.
fn header_boxed_panel(view: &HeaderView<'_>, inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    let border = border_style(theme);
    let mut rows: Vec<Line<'static>> = Vec::with_capacity(6);
    rows.push(Line::styled(format!("╭{}╮", "─".repeat(inner)), border));
    for row in header_panel_rows(view, inner, theme) {
        rows.push(box_row(&row.spans, inner, border));
    }
    rows.push(Line::styled(format!("╰{}╯", "─".repeat(inner)), border));
    rows
}

/// The four info-panel content rows (qwen `Header` info column), each already
/// clipped to `inner` columns: the bold accent title + secondary version, a blank
/// spacer, the scoped model id with a ` (/model to change)` hint when it fits, and
/// the tilde-shortened working directory. Borderless spans - [`header_two_column`]
/// or the one-column path wraps them in the box.
fn header_panel_rows(view: &HeaderView<'_>, inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    // Title line: `>_ suspenders` bold accent, then ` (v<version>)` secondary.
    let title_line = {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        used = push_cols(
            &mut spans,
            &format!(">_ {}", view.title),
            accent_style(theme).add_modifier(Modifier::BOLD),
            used,
            inner,
        );
        push_cols(
            &mut spans,
            &format!(" (v{})", view.version),
            secondary_style(theme),
            used,
            inner,
        );
        Line::from(spans)
    };

    // Model line: the scoped id, plus ` (/model to change)` when the whole line
    // still fits the inner width (qwen `showModelHint`).
    let model_line = {
        let hint = " (/model to change)";
        let mut spans: Vec<Span<'static>> = Vec::new();
        let used = push_cols(&mut spans, view.model, secondary_style(theme), 0, inner);
        // The hint rides along only when the whole line still fits (qwen
        // `showModelHint`); otherwise the model id shows alone.
        if view.model.width() + hint.width() <= inner {
            push_cols(&mut spans, hint, secondary_style(theme), used, inner);
        }
        Line::from(spans)
    };

    // Directory line: tilde-abbreviated then column-clipped to the inner width.
    let dir_line = {
        let path = tildeify_path(view.cwd);
        Line::from(Span::styled(
            truncate_cols(&path, inner),
            secondary_style(theme),
        ))
    };

    vec![title_line, Line::default(), model_line, dir_line]
}

/// The logo + boxed info panel side by side (qwen two-column `Header`): the 6
/// accent logo rows on the left, a [`HEADER_LOGO_GAP`]-col gap, then the pre-built
/// 6-row bordered `panel` box (they line up 1:1). Every row is exactly
/// `HEADER_LOGO_WIDTH + gap + inner + 2` columns (measure==draw, ADR-0029).
fn header_two_column(panel: &[Line<'static>], theme: &Theme) -> Vec<Line<'static>> {
    let gap = " ".repeat(HEADER_LOGO_GAP);
    // Zip the 6 logo rows against the 6 box rows into one row each. When the box
    // has fewer rows than the logo (never today - it is always 6), the extra logo
    // rows draw the logo alone; when it has more, the extra box rows draw alone.
    HEADER_LOGO
        .lines()
        .zip(panel)
        .map(|(logo, boxed)| {
            let mut spans = vec![
                Span::styled(logo.to_string(), accent_style(theme)),
                Span::raw(gap.clone()),
            ];
            spans.extend(boxed.spans.clone());
            Line::from(spans)
        })
        .collect()
}

/// The logo STACKED above the boxed info panel (the middle tier): the 6 accent
/// logo rows as a full-width TOP banner (each exactly [`HEADER_LOGO_WIDTH`] cols),
/// then the pre-built bordered `panel` box below it. Left-aligned to the content
/// gutter (no centering), so it lines up with the composer. Every logo row is 83
/// columns and every box row is `inner + 2` - both `<= content_width` in this tier
/// by construction (measure==draw, ADR-0029).
fn header_stacked(panel: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = HEADER_LOGO
        .lines()
        .map(|logo| Line::from(Span::styled(logo.to_string(), accent_style(theme))))
        .collect();
    out.extend(panel);
    out
}

/// The `Tips: <tip>` line below the box (qwen `Tips`), in secondary, clipped to
/// `width` columns so it never soft-wraps (measure==draw, ADR-0029).
fn header_tips_line(tip: &str, width: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        truncate_cols(&format!("Tips: {tip}"), width),
        secondary_style(theme),
    ))
}

/// Abbreviates a leading `$HOME` in `path` to `~` (qwen `tildeifyPath`); other
/// paths pass through unchanged. Reads the home directory from the environment at
/// this one edge, then delegates to the pure [`tildeify_with_home`] rewrite - so
/// the string logic is testable without touching process env (ADR-0019).
fn tildeify_path(path: &str) -> String {
    let home = std::env::var_os("HOME").and_then(|h| h.into_string().ok());
    tildeify_with_home(path, home.as_deref())
}

/// The pure `~`-abbreviation of `path` against a known `home` (qwen `tildeifyPath`):
/// an exact-match home becomes `~`, a home-prefixed path keeps its `~`-rooted tail,
/// everything else (including no/empty home) passes through unchanged. Pure text,
/// no IO - the env read lives in [`tildeify_path`].
fn tildeify_with_home(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() && path == home => "~".to_string(),
        Some(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

/// The lines one Transcript item renders as. `Diff` is the first-class rich item
/// of the semantic display vocabulary (ADR-0008): a titled diff whose lines take
/// a semantic tint from their [`DiffSide`]'s Theme slots and a syntect foreground.
/// `compact` (Ctrl+O, qwen `compactMode`, the core's `Screen::compact_mode`) hides
/// settled `Thinking` items ENTIRELY and folds a tool RESULT body (a multi-line
/// `Diff`, or a `Todo` checklist) to its header row - keeping the transcript terse
/// (ADR-0052). `content_width` is the `content_area` width the lines draw in.
fn message_lines(
    item: &TranscriptItem,
    compact: bool,
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    match item {
        // User prompt (qwen `UserMessage`, ConversationMessages.tsx:186): the
        // `>` U+003E caret + text both `text.accent`, hanging under a 2-col
        // prefix (`stringWidth(">")+1`). Multi-line input renders as many rows.
        TranscriptItem::User { text } => prefixed_text_lines(
            ">",
            accent_style(theme),
            text,
            accent_style(theme),
            content_width,
        ),
        // Assistant markdown (qwen `AssistantMessage`, ConversationMessages.tsx
        // :210): the `✦` U+2726 marker `text.accent` on row 0, the full markdown
        // body hanging under a 2-col prefix.
        TranscriptItem::Assistant { text } => {
            prefixed_markdown_lines("✦", accent_style(theme), markdown_lines(text, theme))
        }
        // Settled Thinking (qwen `ThinkMessage`, ConversationMessages.tsx:250):
        // the same `✦` U+2726 marker but `text.secondary` (grey) for BOTH glyph
        // and body. Compact mode HIDES it entirely (qwen `!compactMode`, ADR-0052:
        // show/hide, never a collapsed one-liner); otherwise the full grey body.
        TranscriptItem::Thinking { text } => {
            if compact {
                Vec::new()
            } else {
                settled_thinking_lines(text, theme)
            }
        }
        // Tool items render INSIDE the group box (qwen `ToolGroupMessage`); their
        // INNER content is built here at the box's inner width and wrapped with
        // borders at assembly by [`grouped_rows`]. Reached only via that path.
        // Under compact the RESULT body folds to the header row (qwen
        // `!compactMode || forceShowResult`).
        TranscriptItem::ToolCall { .. }
        | TranscriptItem::ToolResult { .. }
        | TranscriptItem::Diff { .. }
        | TranscriptItem::Todo { .. } => {
            tool_inner_lines(item, compact, tool_inner_width(content_width), theme)
        }
        // The startup banner (qwen `AppHeader` = `Header` + `Tips`): the ASCII
        // wordmark logo (accent) left, a single-border info panel right, and the
        // `Tips:` line below. Drawn at the FULL content width so the width gate
        // ([`header_lines`]) can decide whether the 83-col logo + gap + a minimum
        // info panel fits, hiding the logo when it does not.
        TranscriptItem::Header {
            title,
            version,
            model,
            cwd,
            tip,
        } => header_lines(
            &HeaderView {
                title,
                version,
                model,
                cwd,
                tip,
            },
            content_width,
            theme,
        ),
        // Info/notification (qwen `InfoMessage`, StatusMessages.tsx:64): the `●`
        // U+25CF prefix `text.primary`, body `text.primary`, hanging under a
        // 2-col prefix. A Marker tints its prefix + body by TONE alone.
        TranscriptItem::Info { text } => prefixed_text_lines(
            "●",
            primary_style(theme),
            text,
            primary_style(theme),
            content_width,
        ),
        // A harness Marker: the prefix glyph + tint chosen by the marker's
        // [`Tone`] (qwen StatusMessages set - Constrain reads the `△` warning
        // status, everything else the `●` info status). Tone alone decides,
        // never the text.
        TranscriptItem::Marker { .. } => {
            let (glyph, style) = marker_prefix_and_style(item, theme);
            prefixed_text_lines(glyph, style, marker_text(item), style, content_width)
        }
    }
}

/// The plain text an Info/Marker item carries (both are text rows, no markdown).
fn marker_text(item: &TranscriptItem) -> &str {
    match item {
        TranscriptItem::Info { text } | TranscriptItem::Marker { text, .. } => text,
        _ => "",
    }
}

/// The 2-column prefix width every single-glyph committed prefix hangs under
/// (qwen `getPrefixWidth = stringWidth(prefix) + 1`, ConversationMessages.tsx:90
/// / StatusMessages.tsx:44): one glyph column plus one clear column so the body
/// never touches the marker. All Phase-2 prefixes (`>`,`✦`,`●`) are width-1.
const PREFIX_WIDTH: usize = 2;

/// Lines for a prefixed PLAIN-TEXT item (qwen `PrefixedTextMessage`): the `glyph`
/// in `prefix_style` on row 0, then the wrapping text in `text_style` hung under
/// the [`PREFIX_WIDTH`] prefix column. Every produced [`Line`] is `<= content_width`
/// columns (the body wrapped to `content_width - PREFIX_WIDTH`, both prefix and
/// continuation padded to the prefix column), so the viewport's `Wrap` never
/// re-breaks it (measure==draw, ADR-0029).
fn prefixed_text_lines(
    glyph: &str,
    prefix_style: Style,
    text: &str,
    text_style: Style,
    content_width: u16,
) -> Vec<Line<'static>> {
    let inner = (content_width as usize).saturating_sub(PREFIX_WIDTH).max(1);
    let pad = " ".repeat(PREFIX_WIDTH);
    let mut out = Vec::new();
    let mut first = true;
    for source in text_rows(text) {
        for seg in wrap_words(&source, inner) {
            let lead = if first {
                Span::styled(format!("{glyph} "), prefix_style)
            } else {
                Span::raw(pad.clone())
            };
            out.push(Line::from(vec![lead, Span::styled(seg, text_style)]));
            first = false;
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(format!("{glyph} "), prefix_style)));
    }
    out
}

/// Lines for a prefixed MARKDOWN item (qwen `PrefixedMarkdownMessage`): the
/// `glyph` in `prefix_style` on the first body row, every row (row 0 and each
/// continuation) hung under the [`PREFIX_WIDTH`] prefix column. The markdown
/// `body` is already styled; this only prepends the marker/indent column. Because
/// the body was built at the reduced width by the cache, the prefixed lines stay
/// `<= content_width` (measure==draw, ADR-0029).
fn prefixed_markdown_lines(
    glyph: &str,
    prefix_style: Style,
    body: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let pad = " ".repeat(PREFIX_WIDTH);
    let mut first = true;
    body.into_iter()
        .map(|line| {
            let lead = if first {
                Span::styled(format!("{glyph} "), prefix_style)
            } else {
                Span::raw(pad.clone())
            };
            first = false;
            let mut spans = vec![lead];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tool-group box (qwen `ToolGroupMessage`/`ToolMessage`, ADR "tool groups at
// render time"). A maximal contiguous run of ToolCall/ToolResult/tool-Diff items
// renders as ONE rounded box. Each item's INNER content is built here at the
// box's inner width; [`render_tool_group`] wraps a contiguous run with borders +
// gaps at assembly. Borders/box are uncached (cheap); the Diff syntect stays
// cached per-item (its inner lines are what the cache holds).
// ---------------------------------------------------------------------------

/// The 3-wide status-marker gutter every tool row and result body indents under
/// (qwen `STATUS_INDICATOR_WIDTH = 3`, ToolStatusIndicator.tsx:17).
const STATUS_INDICATOR_WIDTH: usize = 3;

/// The rounded-box overhead subtracted from the box width to get the inner
/// content width: 1 border + 1 `paddingX` on each side (qwen `ToolMessage`
/// `paddingX={1}` inside a `borderStyle:"round"` box, ToolMessage.tsx:665). Four
/// columns total.
const BOX_CHROME: usize = 4;

/// The inner content width tool items build at: the box width less the border +
/// padding chrome ([`BOX_CHROME`]), floored at 1.
fn tool_inner_width(content_width: u16) -> u16 {
    content_width.saturating_sub(BOX_CHROME as u16).max(1)
}

/// Whether an item belongs to a tool group (grouped into the box at render):
/// ToolCall, ToolResult, a tool Diff, or a Todo list. The ONE membership
/// predicate the grouping fold ([`group_segments`]) keys on. A `Todo` is a
/// tool item so it renders INSIDE the same rounded box as the `todo_write` it
/// stands in for (ADR-0047/0048), the identity every consumer relies on:
/// committed and pending draw byte-identically down the same box path.
fn is_tool_item(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::ToolCall { .. }
            | TranscriptItem::ToolResult { .. }
            | TranscriptItem::Diff { .. }
            | TranscriptItem::Todo { .. }
    )
}

/// The INNER box content one tool item renders as (no borders): a status-marker
/// header row (`marker + bold name + dim desc`, truncate-end) for a call/result,
/// or an indented result body (the diff, indented under the marker column) for a
/// Diff. Every produced [`Line`] is `<= inner_width` columns so the box wrapper
/// never re-breaks it (measure==draw, ADR-0029). `compact` (Ctrl+O, qwen
/// `compactMode`) folds a tool RESULT body (the `Diff` body, the `Todo`
/// checklist) to its header row, keeping the transcript terse (ADR-0052).
fn tool_inner_lines(
    item: &TranscriptItem,
    compact: bool,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    match item {
        TranscriptItem::ToolCall { name, summary, .. } => vec![tool_header_row(
            tool_marker(ToolMarker::Executing, name, theme),
            name,
            summary,
            inner_width,
            theme,
        )],
        TranscriptItem::ToolResult {
            name,
            summary,
            is_error,
            key_arg,
        } => {
            let marker = if *is_error {
                ToolMarker::Error
            } else {
                ToolMarker::Success
            };
            let desc = tool_desc(key_arg.as_deref(), summary);
            vec![tool_header_row(
                tool_marker(marker, name, theme),
                name,
                &desc,
                inner_width,
                theme,
            )]
        }
        // A Diff renders its title header row then, unless folded by compact, its
        // body indented under the marker column (delegated so the fold branch does
        // not add to this dispatch's logic).
        TranscriptItem::Diff { .. } => tool_diff_lines(item, compact, inner_width, theme),
        // A Todo renders a clean `✓ todo_write` header (no key_arg, so the raw
        // JSON args are gone STRUCTURALLY) then the circle checklist indented
        // under the marker column - folded away to the header under compact.
        TranscriptItem::Todo { items } => tool_todo_lines(items, compact, inner_width, theme),
        _ => Vec::new(),
    }
}

/// The confirming ToolCall's inner box lines (ADR-0049): its header row with the
/// `?` (Confirming) marker in place of `⊷`, then the inline approval block
/// (gap + question + radio). The confirming item is always a `ToolCall` (the
/// newest live one); a defensive non-call falls back to its plain inner lines
/// plus the block so a future gated shape never renders empty.
fn confirming_inner_lines(
    item: &TranscriptItem,
    pending: &PendingApproval,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut rows = match item {
        TranscriptItem::ToolCall { name, summary, .. } => vec![tool_header_row(
            tool_marker(ToolMarker::Confirming, name, theme),
            name,
            summary,
            inner_width,
            theme,
        )],
        // Defensive: any other confirming shape keeps its normal inner lines.
        other => tool_inner_lines(other, false, inner_width, theme),
    };
    rows.extend(approval_block_rows(pending, inner_width, theme));
    rows
}

/// A Todo tool item's inner box lines (ADR-0048, qwen `TodoDisplay`/`TodoItemRow`):
/// a clean `✓ todo_write` header row with an EMPTY description - the Presenter
/// dropped the raw JSON args when it swapped the Tool Result for a [`Todo`], so
/// there is nothing to leak - then one circle-glyph row per item indented under
/// the 3-wide marker column. The glyph is [`crate::plan::TodoStatus::glyph`]
/// (`○ ◐ ●`); in_progress reads `success_style` (green), completed reads
/// `primary_style` + [`Modifier::CROSSED_OUT`] (qwen colours completed
/// Foreground, NOT green - only in_progress is green), everything else
/// `primary_style`. Content word-wraps to `inner_width - STATUS_INDICATOR_WIDTH`
/// so every produced row is `<= inner_width` columns (measure==draw, ADR-0029).
///
/// [`Todo`]: TranscriptItem::Todo
fn tool_todo_lines(
    items: &[TodoItem],
    compact: bool,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut out = vec![tool_header_row(
        tool_marker(ToolMarker::Success, "todo_write", theme),
        "todo_write",
        "",
        inner_width,
        theme,
    )];
    // Compact folds the checklist body away (qwen `!compactMode`), keeping only
    // the header row (ADR-0052).
    if compact {
        return out;
    }
    let content_width = inner_width
        .saturating_sub(STATUS_INDICATOR_WIDTH as u16)
        .max(1) as usize;
    for item in items {
        out.extend(todo_item_rows(item, content_width, theme));
    }
    out
}

/// The wrapped rows for ONE todo item (ADR-0048): the status glyph in its
/// 3-wide gutter on the first row, the content word-wrapped under it, every row
/// hung at [`STATUS_INDICATOR_WIDTH`] so the glyph column stays clear. The
/// in_progress-green / completed-strikethrough treatment is applied HERE (the
/// pure [`TodoStatus`] carries only the glyph, ADR-0019).
fn todo_item_rows(item: &TodoItem, content_width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let style = match item.status {
        TodoStatus::InProgress => success_style(theme),
        TodoStatus::Completed => primary_style(theme).add_modifier(Modifier::CROSSED_OUT),
        TodoStatus::Pending => primary_style(theme),
    };
    let gutter = " ".repeat(STATUS_INDICATOR_WIDTH);
    let mut out = Vec::new();
    for (row, seg) in wrap_words(&item.content, content_width)
        .into_iter()
        .enumerate()
    {
        let lead = if row == 0 {
            // The glyph occupies 1 column of the 3-wide gutter, then one clear
            // column so the content never touches it (the ToolStatusIndicator
            // shape, STATUS_INDICATOR_WIDTH=3).
            Span::styled(format!("{}  ", item.status.glyph()), style)
        } else {
            Span::raw(gutter.clone())
        };
        out.push(Line::from(vec![lead, Span::styled(seg, style)]));
    }
    out
}

/// A Diff tool item's inner box lines: the folded one-liner (compact on a
/// foldable body), or the `diff` header row + the diff body (each row indented
/// under the marker column) + the elided tail. Split out of [`tool_inner_lines`]
/// so its fold branch stays off that dispatch. Panics on a non-Diff item.
fn tool_diff_lines(
    item: &TranscriptItem,
    compact: bool,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let TranscriptItem::Diff {
        title,
        lang,
        hunks,
        elided,
    } = item
    else {
        return Vec::new();
    };
    if compact && item.has_foldable_body() {
        return vec![tool_diff_fold_row(title, inner_width, theme)];
    }
    let body_width = inner_width
        .saturating_sub(STATUS_INDICATOR_WIDTH as u16)
        .max(1);
    let mut out = vec![tool_header_row(
        tool_marker(ToolMarker::Success, "diff", theme),
        "diff",
        title,
        inner_width,
        theme,
    )];
    out.extend(indent_box_body(diff_lines(
        lang.as_deref(),
        hunks,
        body_width,
        theme,
    )));
    out.extend(indent_box_body(diff_elided_tail(
        *elided, body_width, theme,
    )));
    out
}

/// Indents every row of a diff/result body under the 3-wide marker column (qwen
/// `paddingLeft:STATUS_INDICATOR_WIDTH`), so the body sits inside the box under
/// its tool header. Pure.
fn indent_box_body(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let indent = " ".repeat(STATUS_INDICATOR_WIDTH);
    lines
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, Span::raw(indent.clone()));
            line
        })
        .collect()
}

/// One tool row (qwen `ToolInfo`, ToolMessage.tsx): the 3-wide status marker,
/// then the bold `name` (`text.primary`) + a space + the dim `desc`
/// (`text.secondary`), the WHOLE line truncate-end at `inner_width` (never wraps,
/// `…` at the edge). Funneled through [`push_cols`] so the row is exactly one
/// visual line `<= inner_width` columns (measure==draw, ADR-0029).
fn tool_header_row(
    marker: Span<'static>,
    name: &str,
    desc: &str,
    inner_width: u16,
    theme: &Theme,
) -> Line<'static> {
    let width = inner_width as usize;
    let mut spans = vec![marker];
    // The marker is 1 glyph in a 3-wide gutter: pad to STATUS_INDICATOR_WIDTH.
    let mut used = STATUS_INDICATOR_WIDTH.min(width);
    if used > 1 {
        spans.push(Span::raw(" ".repeat(used - 1)));
    }
    used = push_cols(
        &mut spans,
        name,
        primary_style(theme).add_modifier(Modifier::BOLD),
        used,
        width,
    );
    if !desc.is_empty() {
        used = push_cols(&mut spans, " ", secondary_style(theme), used, width);
        let _ = push_cols(&mut spans, desc, secondary_style(theme), used, width);
    }
    Line::from(spans)
}

/// The `›` U+203A active-row marker (qwen `BaseSelectionList`), success-green
/// when the row is active. It sits in a 2-wide gutter (the marker + a trailing
/// space, or two spaces when inactive).
const SELECTION_MARKER: &str = "›";
/// The width of the selection gutter (marker + one space).
const SELECTION_GUTTER_WIDTH: usize = 2;

/// The numbered radio rows of a [`SelectionList`] (ADR-0049, qwen
/// `BaseSelectionList.tsx`): each row is `‹gutter›N. label`, where the gutter
/// carries the `›` marker (success-green) on the active row else two spaces, the
/// `N.` number is right-aligned in a fixed field (`showNumbers`) and turns
/// success-green on the active row (with the marker + label) else secondary, and
/// the label reads success-green when active else primary. Every row is truncate-end at
/// `inner_width` so the box wrapper never re-breaks it (measure==draw, ADR-0029).
/// `active` is the highlighted 0-based row.
fn selection_rows(
    items: &[&str],
    active: usize,
    show_numbers: bool,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = inner_width as usize;
    // The number field is as wide as the widest `N.` (e.g. `9.` = 2, `12.` = 3).
    let num_field = if show_numbers {
        format!("{}.", items.len()).width()
    } else {
        0
    };
    items
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let is_active = i == active;
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut used = 0;
            // The 2-wide `›`/space gutter.
            if is_active {
                used = push_cols(
                    &mut spans,
                    SELECTION_MARKER,
                    success_style(theme),
                    used,
                    width,
                );
                used = push_cols(&mut spans, " ", Style::default(), used, width);
            } else {
                used = push_cols(
                    &mut spans,
                    &" ".repeat(SELECTION_GUTTER_WIDTH),
                    Style::default(),
                    used,
                    width,
                );
            }
            // The right-aligned `N.` number field: qwen turns the number
            // `status.success` (green) together with the marker + label on the
            // active row (`BaseSelectionList.tsx:113-118`), else `text.secondary`.
            if show_numbers {
                let num = format!("{}.", i + 1);
                let pad = num_field.saturating_sub(num.width());
                if pad > 0 {
                    used = push_cols(&mut spans, &" ".repeat(pad), Style::default(), used, width);
                }
                let num_style = if is_active {
                    success_style(theme)
                } else {
                    secondary_style(theme)
                };
                used = push_cols(&mut spans, &num, num_style, used, width);
                used = push_cols(&mut spans, " ", Style::default(), used, width);
            }
            // The label: success-green when active, else primary.
            let label_style = if is_active {
                success_style(theme)
            } else {
                primary_style(theme)
            };
            let _ = push_cols(&mut spans, label, label_style, used, width);
            Line::from(spans)
        })
        .collect()
}

/// The verbatim Approval options in order (ADR-0049, qwen exec/info sets): once /
/// always-in-project / no-suggest. The single `Always allow in this project`
/// (the qwen no-`{{action}}` fallback) collapses BOTH qwen always-variants onto
/// suspenders' one session-scoped ApproveAlways (ADR-0005). Row indices match
/// `screen::decision_for_option` (0 Approve / 1 ApproveAlways / 2 Deny).
const APPROVAL_OPTIONS: [&str; 3] = [
    "Yes, allow once",
    "Always allow in this project",
    "No, suggest changes (esc)",
];

/// The Approval question line (ADR-0049, qwen verbatim): `Exec` reads `Allow
/// execution of: '{command}'?`, `Info` reads `Do you want to proceed?`.
fn approval_question(kind: ConfirmKind, command: &str) -> String {
    match kind {
        ConfirmKind::Exec => format!("Allow execution of: '{command}'?"),
        ConfirmKind::Info => "Do you want to proceed?".to_string(),
    }
}

// ---------------------------------------------------------------------------
// The keyboard-shortcuts Help overlay (qwen `Help.tsx`, the `?` affordance the
// footer's `? for shortcuts` promises). A single bordered panel - suspenders has
// only two built-in commands and none of qwen's custom-command/MCP/skill/plugin
// ecosystem, so qwen's THREE-tab chrome (general / commands / custom-commands)
// would be two empty tabs of vaporware; we port the CONTENT (shortcuts + the
// built-in COMMANDS registry) into one panel and drop the tab chrome. The
// Screen's `help_open` flag gates it and [`Screen::handle_help_key`] closes it.
// ---------------------------------------------------------------------------

/// The width of the accent key column in a shortcut row (qwen `KEY_COL_WIDTH`,
/// Help.tsx:42): the fixed-width column the key sits in before its description.
const HELP_KEY_COL_WIDTH: usize = 12;

/// The gap (columns) between the two shortcut columns (qwen `GeneralHelp` `gap:2`).
const HELP_COL_GAP: usize = 2;

/// The (key, description) shortcut rows the Help panel lists, verified against
/// `ui.rs` `map_key` + `screen.rs` routing. `@` is already promised by the
/// composer placeholder (the AT-completion phase wires its behaviour), so it is
/// listed here alongside the live bindings.
const HELP_SHORTCUTS: &[(&str, &str)] = &[
    ("/", "Open the command menu"),
    ("@", "Add files or folders as context"),
    ("?", "Show this help"),
    ("Enter", "Submit (steer a running turn)"),
    ("Alt+Enter", "Insert a newline"),
    ("Esc", "Cancel a running turn / close a menu"),
    ("Ctrl+O", "Toggle compact mode"),
    ("Ctrl+S", "Peek the full pending output into scrollback"),
    ("Ctrl+C", "Quit"),
    ("Shift+Tab", "Cycle approval mode"),
    ("Tab", "Accept the highlighted suggestion"),
    ("↑/↓", "Cycle prompt history / move the cursor"),
];

/// Draws the Help overlay (qwen `Help`) into `area`: the bordered shortcuts +
/// commands panel, top-clipped to the zone if it is taller than the body. The
/// panel is built once by the pure [`help_panel_lines`] (measure==draw), then
/// bottom-anchored so its footer (`Esc to close`) sits just above the composer -
/// the same anchor the pending body uses (ADR-0046).
fn render_help_overlay(frame: &mut Frame, area: Rect, theme: &Theme) {
    let content_area = Rect {
        x: area.x + CONTENT_MARGIN,
        width: area.width.saturating_sub(2 * CONTENT_MARGIN),
        ..area
    };
    let lines = help_panel_lines(content_area.width, theme);
    // Bottom-anchor + top-clip exactly like the pending body: keep the LAST
    // `height` rows (qwen's `overflowDirection:"top"`) when the panel is tall, else
    // pad the top so the footer meets the composer.
    let total = lines.len();
    let clip = anchor_clip(total, area, content_area);
    frame.render_widget(
        Paragraph::new(lines).scroll((clip.scroll, 0)),
        clip.content_draw,
    );
    if let Some(marker_draw) = clip.marker_draw {
        draw_overflow_marker(frame, marker_draw, theme);
    }
}

/// The Help panel's lines (qwen `Help`), framed with a single-line border to the
/// `inner` inner width (the same box-drawing the header panel uses): a title row
/// (`suspenders` bold accent + ` keyboard shortcuts`), a `Shortcuts` heading + the
/// shortcut rows (two columns when the width allows, one otherwise), a `Commands`
/// heading + the built-in [`slash::COMMANDS`] (derived, so a future command shows
/// up automatically), and an italic `Esc to close` footer. Every row is exactly
/// `inner + 2` columns (measure==draw, ADR-0029) so the viewport never re-breaks it.
fn help_panel_lines(content_width: u16, theme: &Theme) -> Vec<Line<'static>> {
    // The panel takes the full content width minus the 2-col box chrome (`│…│`),
    // floored so a tiny terminal still draws a legible sliver.
    let inner = (content_width as usize).saturating_sub(2).max(1);
    let border = border_style(theme);

    let mut rows: Vec<Line<'static>> = Vec::new();
    rows.push(Line::styled(format!("╭{}╮", "─".repeat(inner)), border));
    for row in help_panel_body_rows(inner, theme) {
        rows.push(box_row(&row.spans, inner, border));
    }
    rows.push(Line::styled(format!("╰{}╯", "─".repeat(inner)), border));
    rows
}

/// The Help panel's borderless content rows (qwen `GeneralHelp` + `CommandsHelp`),
/// each clipped to `inner` columns - [`help_panel_lines`] wraps them in the box.
/// The order: title, blank, `Shortcuts` heading, the shortcut rows, blank,
/// `Commands` heading, the built-in command rows, blank, the `Esc to close` footer.
fn help_panel_body_rows(inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    // Title row: `suspenders` bold accent + ` keyboard shortcuts` primary (qwen's
    // `Qwen Code` help header).
    {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let used = push_cols(
            &mut spans,
            "suspenders",
            accent_style(theme).add_modifier(Modifier::BOLD),
            0,
            inner,
        );
        push_cols(
            &mut spans,
            " keyboard shortcuts",
            primary_style(theme),
            used,
            inner,
        );
        out.push(Line::from(spans));
    }
    out.push(Line::default());

    // Shortcuts section.
    out.push(help_heading_row("Shortcuts", inner, theme));
    out.extend(help_shortcut_rows(inner, theme));
    out.push(Line::default());

    // Commands section, derived from the registry so a future command appears
    // without touching this panel.
    out.push(help_heading_row("Commands", inner, theme));
    for cmd in slash::COMMANDS {
        out.push(help_command_row(cmd, inner, theme));
    }
    out.push(Line::default());

    // Footer: `Esc to close`, italic secondary (qwen's `Esc to cancel`).
    out.push(Line::from(Span::styled(
        truncate_cols("Esc to close", inner),
        secondary_style(theme).add_modifier(Modifier::ITALIC),
    )));

    out
}

/// A section heading row (qwen `Text bold`): the label bold primary, clipped to
/// `inner`.
fn help_heading_row(label: &str, inner: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        truncate_cols(label, inner),
        primary_style(theme).add_modifier(Modifier::BOLD),
    ))
}

/// The widest single shortcut cell that fits every entry WITHOUT truncation: the
/// fixed key column plus the longest description in [`HELP_SHORTCUTS`]. The
/// two-column layout only engages when the inner width holds two of these side by
/// side (plus the gap), so neither column ever chops a description - suspenders'
/// descriptions are longer than qwen's, so a single clean column is the norm.
fn help_full_cell_width() -> usize {
    let longest_desc = HELP_SHORTCUTS
        .iter()
        .map(|(_, desc)| desc.width())
        .max()
        .unwrap_or(0);
    HELP_KEY_COL_WIDTH + longest_desc
}

/// The shortcut rows (qwen `GeneralHelp`), DEFAULTING TO ONE CLEAN COLUMN:
/// suspenders' descriptions are longer than qwen's, so a single column reads
/// better and never truncates, and an overlay has the vertical room for ~12 rows.
/// Two columns engage ONLY at genuinely wide widths where BOTH columns' FULL
/// descriptions fit without truncation (`inner >= 2*(key_col + longest_desc) +
/// gap`, ~114 cols given the ~44-col longest description) - so the two-column
/// branch is rarely hit. In it, the left cell is padded to an exact fixed width so
/// the right column aligns vertically; if a description must ever clip it does so
/// with one trailing ellipsis (never a hard mid-word cut).
fn help_shortcut_rows(inner: usize, theme: &Theme) -> Vec<Line<'static>> {
    let full_cell = help_full_cell_width();
    let two_col = inner >= full_cell * 2 + HELP_COL_GAP;
    if two_col {
        // Both columns get the full untruncated cell width; the leftover inner
        // padding rides in the gap so the left cell stays a fixed column.
        let col_width = full_cell;
        let half = HELP_SHORTCUTS.len().div_ceil(2);
        let (left, right) = HELP_SHORTCUTS.split_at(half);
        (0..half)
            .map(|i| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                match right.get(i) {
                    // A left cell WITH a right column: pad the left cell out to the
                    // fixed width so the right column aligns vertically.
                    Some(row) => {
                        help_shortcut_cell(&mut spans, left[i], col_width, true, theme);
                        spans.push(Span::raw(" ".repeat(HELP_COL_GAP)));
                        help_shortcut_cell(&mut spans, *row, col_width, false, theme);
                    }
                    // The shorter (right) half leaves later rows single-column.
                    None => help_shortcut_cell(&mut spans, left[i], col_width, false, theme),
                }
                Line::from(spans)
            })
            .collect()
    } else {
        HELP_SHORTCUTS
            .iter()
            .map(|row| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                help_shortcut_cell(&mut spans, *row, inner, false, theme);
                Line::from(spans)
            })
            .collect()
    }
}

/// One shortcut cell up to `cell_width` columns: the accent key padded to the
/// fixed [`HELP_KEY_COL_WIDTH`], then the secondary description. The key column is
/// always padded (so descriptions line up). `pad_trailing` pads the WHOLE cell out
/// to `cell_width` - set only for a LEFT cell that a right column must align after;
/// a single/last cell leaves no trailing filler. A description that overflows the
/// cell is clipped with ONE trailing ellipsis ([`truncate_cols`]), never a hard
/// mid-word cut.
fn help_shortcut_cell(
    spans: &mut Vec<Span<'static>>,
    (key, desc): (&str, &str),
    cell_width: usize,
    pad_trailing: bool,
    theme: &Theme,
) {
    let key_col = HELP_KEY_COL_WIDTH.min(cell_width);
    // The accent key, clipped to and padded out to the fixed key column.
    let key_text = truncate_cols(key, key_col);
    let key_pad = key_col.saturating_sub(key_text.width());
    spans.push(Span::styled(key_text, accent_style(theme)));
    if key_pad > 0 {
        spans.push(Span::raw(" ".repeat(key_pad)));
    }
    // The description fills the rest of the cell, ellipsis-clipped if it must.
    let desc_room = cell_width.saturating_sub(key_col);
    let desc_text = truncate_cols(desc, desc_room);
    let desc_width = desc_text.width();
    spans.push(Span::styled(desc_text, secondary_style(theme)));
    // A left cell pads out to the full cell so the right column starts on grid.
    if pad_trailing {
        let pad = desc_room.saturating_sub(desc_width);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
    }
}

/// One built-in command row (qwen `CommandsHelp` signature + description): the
/// accent `/name`, then a secondary ` — help` on the same row, clipped to `inner`.
fn help_command_row(cmd: &slash::SlashCommand, inner: usize, theme: &Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let key_end = HELP_KEY_COL_WIDTH.min(inner);
    let mut used = push_cols(
        &mut spans,
        &format!("/{}", cmd.name),
        accent_style(theme),
        0,
        key_end,
    );
    if used < key_end {
        spans.push(Span::raw(" ".repeat(key_end - used)));
        used = key_end;
    }
    push_cols(&mut spans, cmd.help, secondary_style(theme), used, inner);
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// The `/mcp` management dialog overlay (qwen `MCPManagementDialog`, ADR-0065
// Phase E). A single bordered box drawn in the BODY region (like the Help
// overlay), NOT the compact composer popup: a navigation-stack wizard whose
// active step's header / content / footer the pure [`crate::ui::mcp_command`]
// builds as styled [`McpRow`]s. This adapter only maps each row's semantic
// [`McpStyle`] to the active Theme and frames the rows in the box.
// ---------------------------------------------------------------------------

/// Draws the `/mcp` dialog (qwen `MCPManagementDialog`) into `area`: the active
/// step's bordered box (header, a blank, the content, a blank, the footer),
/// bottom-anchored and top-clipped exactly like the Help overlay + pending body
/// (ADR-0046) so its footer meets the composer. The box lines are built once by
/// the pure [`mcp_dialog_lines`] (measure==draw), so the viewport never
/// re-breaks a row.
fn render_mcp_dialog(frame: &mut Frame, area: Rect, dialog: &McpDialogView, theme: &Theme) {
    let content_area = Rect {
        x: area.x + CONTENT_MARGIN,
        width: area.width.saturating_sub(2 * CONTENT_MARGIN),
        ..area
    };
    let lines = mcp_dialog_lines(dialog, content_area.width, theme);
    let clip = anchor_clip(lines.len(), area, content_area);
    frame.render_widget(
        Paragraph::new(lines).scroll((clip.scroll, 0)),
        clip.content_draw,
    );
    if let Some(marker_draw) = clip.marker_draw {
        draw_overflow_marker(frame, marker_draw, theme);
    }
}

/// The `/mcp` dialog's box lines (qwen `MCPManagementDialog`'s single-border
/// box): the header rows, a blank (qwen's `gap:1`), the content rows, a blank,
/// then the footer, framed with the same single-line border the header/Help
/// panels use. Every row is exactly `inner + 2` columns (measure==draw,
/// ADR-0029). Pure over the [`McpDialogView`] + width; the adapter maps each
/// [`McpStyle`] to a Theme slot.
fn mcp_dialog_lines(
    dialog: &McpDialogView,
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let inner = (content_width as usize).saturating_sub(2).max(1);
    let border = border_style(theme);

    let mut body: Vec<Line<'static>> = Vec::new();
    body.extend(dialog.header.iter().map(|r| mcp_row_line(r, theme)));
    body.push(Line::default());
    body.extend(dialog.content.iter().map(|r| mcp_row_line(r, theme)));
    body.push(Line::default());
    body.push(mcp_row_line(&dialog.footer, theme));

    let mut rows: Vec<Line<'static>> = Vec::new();
    rows.push(Line::styled(format!("╭{}╮", "─".repeat(inner)), border));
    for line in body {
        rows.push(box_row(&line.spans, inner, border));
    }
    rows.push(Line::styled(format!("╰{}╯", "─".repeat(inner)), border));
    rows
}

/// One rendered [`McpRow`] as a borderless [`Line`]: each [`McpSpan`] mapped
/// from its semantic [`McpStyle`] to the active Theme, plus [`Modifier::BOLD`]
/// when the span asserts `bold` (qwen's orthogonal `<Text bold>` emphasis on the
/// header titles, group headings, and TOOL_DETAIL labels). [`mcp_dialog_lines`]
/// wraps the line in the box (so no border chars here).
fn mcp_row_line(row: &McpRow, theme: &Theme) -> Line<'static> {
    Line::from(
        row.spans
            .iter()
            .map(|span| {
                let mut style = mcp_span_style(span.style, theme);
                if span.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                Span::styled(span.text.clone(), style)
            })
            .collect::<Vec<_>>(),
    )
}

/// The Theme [`Style`] a semantic [`McpStyle`] maps to (qwen's `semantic-colors`
/// reads): the accent/primary/secondary body tones and the success/warning/error
/// status colours, drawn from the same slots the rest of the UI reads.
fn mcp_span_style(style: McpStyle, theme: &Theme) -> Style {
    match style {
        McpStyle::Accent => accent_style(theme),
        McpStyle::Primary => primary_style(theme),
        McpStyle::Secondary => secondary_style(theme),
        McpStyle::Success => success_style(theme),
        McpStyle::Warning => warning_style(theme),
        McpStyle::Error => error_style(theme),
    }
}

/// The inline approval block's inner rows (ADR-0049), appended after the
/// confirming ToolCall's header INSIDE its box: a blank gap row (qwen
/// `marginBottom:1`), the question line (`primary`, truncate-end), then the
/// numbered radio rows driven by the pending [`SelectionList`]. Every row is
/// `<= inner_width` columns (measure==draw, ADR-0029) so [`box_row`] never
/// re-breaks it.
fn approval_block_rows(
    pending: &PendingApproval,
    inner_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = inner_width as usize;
    let mut rows = vec![Line::from(Vec::<Span<'static>>::new())];
    let question = approval_question(pending.kind, &pending.command);
    let mut spans = Vec::new();
    let _ = push_cols(&mut spans, &question, primary_style(theme), 0, width);
    rows.push(Line::from(spans));
    rows.extend(selection_rows(
        &APPROVAL_OPTIONS,
        pending.selection.active(),
        true,
        inner_width,
        theme,
    ));
    rows
}

/// The question-modal title (ADR-0057, qwen `askUserQuestion` confirmation
/// title, VERBATIM).
const QUESTION_MODAL_TITLE: &str = "Please answer the following question(s):";

/// The free-form "Other" capture hint shown under a question while the composer
/// collects the answer (ADR-0057): tells the user to type below and submit.
const QUESTION_OTHER_HINT: &str = "Type your answer below, then press Enter.";

/// The question modal as a standalone bordered box (ADR-0057, qwen
/// `ask_user_question`): a rounded box with the title, then each question's text
/// and its numbered radio (its options PLUS the auto-appended "Other" row).
/// Answered questions show their recorded answer; the one collecting a free-form
/// "Other" answer shows the composer hint. Every content row is `<= inner`
/// columns and boxed to exactly `inner + 2` (measure==draw, ADR-0029). Rendered
/// bottom-most in the pending body so the top-clip never eats it.
fn question_modal_lines(
    pending: &PendingQuestion,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // The box interior width (border + paddingX:1 each side is the box's own; the
    // interior here is width - 2 for the two border columns).
    let inner = (width as usize).saturating_sub(2);
    let inner_u16 = inner as u16;
    let border = border_style(theme);

    let mut body: Vec<Line<'static>> = Vec::new();

    // Title row.
    let mut title_spans = Vec::new();
    let _ = push_cols(
        &mut title_spans,
        QUESTION_MODAL_TITLE,
        primary_style(theme).add_modifier(Modifier::BOLD),
        0,
        inner,
    );
    body.push(Line::from(title_spans));

    for (i, question) in pending.questions.iter().enumerate() {
        // A blank gap before each question (qwen `marginBottom:1`).
        body.push(Line::from(Vec::<Span<'static>>::new()));

        // The `[header]` chip + the question text (secondary chip, primary text).
        let mut q_spans = Vec::new();
        let used = push_cols(
            &mut q_spans,
            &format!("[{}] ", question.header),
            secondary_style(theme),
            0,
            inner,
        );
        let _ = push_cols(
            &mut q_spans,
            &question.question,
            primary_style(theme),
            used,
            inner,
        );
        body.push(Line::from(q_spans));

        // The per-question rows: the recorded answer, the free-form hint, or the
        // interactive radio - one branch per state.
        if let Some(Some(answer)) = pending.answers.get(i) {
            // Answered: a success-green `✓ answer` line.
            let mut a_spans = Vec::new();
            let _ = push_cols(
                &mut a_spans,
                &format!("✓ {answer}"),
                success_style(theme),
                0,
                inner,
            );
            body.push(Line::from(a_spans));
        } else if pending.collecting_other == Some(i) {
            // Collecting a free-form "Other" answer: the hint (the composer draws
            // below this box).
            let mut h_spans = Vec::new();
            let _ = push_cols(
                &mut h_spans,
                QUESTION_OTHER_HINT,
                secondary_style(theme),
                0,
                inner,
            );
            body.push(Line::from(h_spans));
        } else {
            // The interactive radio: the question's option labels PLUS the
            // auto-appended "Other" row. `active` reads the per-question
            // SelectionList; only the CURRENT question (cursor) is highlighted.
            let mut labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
            labels.push(OTHER_OPTION_LABEL);
            let active = pending.per_question.get(i).map(|s| s.active()).unwrap_or(0);
            body.extend(selection_rows(&labels, active, true, inner_u16, theme));
        }
    }

    // Frame the body in a rounded box, every row exactly `inner + 2` columns.
    let mut lines = vec![box_top(inner, border)];
    lines.extend(body.iter().map(|line| box_row(&line.spans, inner, border)));
    lines.push(box_bottom(inner, border));
    lines
}

/// A folded Diff's one-line row inside the box: the marker gutter, the title, and
/// the `· ^O expand` affordance, truncate-end at `inner_width`.
fn tool_diff_fold_row(title: &str, inner_width: u16, theme: &Theme) -> Line<'static> {
    let width = inner_width as usize;
    let mut spans = vec![Span::raw(" ".repeat(STATUS_INDICATOR_WIDTH.min(width)))];
    let used = push_cols(
        &mut spans,
        &format!("{title} · ^O expand"),
        secondary_style(theme),
        STATUS_INDICATOR_WIDTH.min(width),
        width,
    );
    let _ = used;
    Line::from(spans)
}

/// The tool status the marker glyph reflects (qwen `TOOL_STATUS`, constants.ts:22
/// — the 0.16.0 ASCII set). CONFIRMING/CANCELED/PENDING are Phase-4 states not
/// reachable from a settled Transcript item.
#[derive(Debug, Clone, Copy)]
enum ToolMarker {
    /// A pending/live `ToolCall`: `⊷` U+22B7 (EXECUTING).
    Executing,
    /// A `ToolCall` awaiting an Approval decision (ADR-0049): `?` U+003F in
    /// `status.warning`. Replaces the executing marker on the confirming call
    /// while the inline approval block holds the keyboard.
    Confirming,
    /// A successful `ToolResult`: `✓` U+2713 (SUCCESS).
    Success,
    /// A failed `ToolResult`: `x` U+0078 (ERROR), bold. NOT main's `✗`.
    Error,
}

/// The styled status-marker glyph (qwen `ToolStatusIndicator`, width 3): SUCCESS
/// `✓`/EXECUTING `⊷` in `status.success`; ERROR `x` bold in `status.error`. A
/// shell tool's marker reads `ui.symbol` (grey), else `status.success`. The
/// glyph occupies 1 column; the caller pads the 3-wide gutter.
fn tool_marker(marker: ToolMarker, name: &str, theme: &Theme) -> Span<'static> {
    let shell = is_shell_tool(name);
    match marker {
        ToolMarker::Success => {
            let style = if shell {
                symbol_style(theme)
            } else {
                success_style(theme)
            };
            Span::styled("✓", style)
        }
        ToolMarker::Executing => {
            let style = if shell {
                symbol_style(theme)
            } else {
                success_style(theme)
            };
            Span::styled("⊷", style)
        }
        ToolMarker::Confirming => Span::styled("?", warning_style(theme)),
        ToolMarker::Error => Span::styled("x", error_style(theme).add_modifier(Modifier::BOLD)),
    }
}

/// Whether a tool name is a shell command (qwen `SHELL_COMMAND_NAME`/`SHELL_NAME`)
/// - shell tools border their group + colour their marker with `ui.symbol` (grey).
fn is_shell_tool(name: &str) -> bool {
    matches!(name, "run_shell_command" | "shell" | "Shell")
}

/// A Marker's prefix glyph + style, chosen by its [`Tone`] (qwen `StatusMessages`
/// set): a Constrain marker (the loop-detector's run-close - a guard on the model)
/// reads the `△` U+25B3 warning status; a Steering marker the `●` info glyph in
/// the accent (the user's own voice reaching a running Run); everything else the
/// `●` info glyph, secondary/muted. Tone alone decides, never the text.
fn marker_prefix_and_style(item: &TranscriptItem, theme: &Theme) -> (&'static str, Style) {
    match item {
        TranscriptItem::Marker {
            tone: Tone::Constrain,
            ..
        } => ("△", warning_style(theme)),
        TranscriptItem::Marker {
            tone: Tone::Steering,
            ..
        } => ("●", accent_style(theme)),
        // Housekeeping/Aid/Plain all read the quiet `●` info glyph, secondary.
        _ => ("●", secondary_style(theme)),
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

/// The inset prefix a bare code block indents under: two
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
        // Bare, inset code block: a blank row above and
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
    lang: Option<&str>,
    hunks: &[DiffHunk],
    content_width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // Integration (IOSP): the gutter width + each hunk's rows come from the
    // operations below; here we only join the hunk blocks with the `═` separator.
    let width = content_width as usize;
    let gutter_width = diff_gutter_width(hunks);
    let separator = Line::styled("═".repeat(width), diff_chrome_style(theme));
    let blocks: Vec<Vec<Line<'static>>> = hunks
        .iter()
        .map(|hunk| hunk_code_lines(hunk, lang, gutter_width, width, theme))
        .collect();
    join_blocks(blocks, separator)
}

/// Joins row `blocks` with a `separator` row between each (never before the first
/// or after the last) - the flatten-with-separator the diff hunk rule needs
/// without a branch inside the fold (qwen `═` U+2550 hunk rule, DiffRenderer.tsx
/// :272). Pure.
fn join_blocks(blocks: Vec<Vec<Line<'static>>>, separator: Line<'static>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, block) in blocks.into_iter().enumerate() {
        if i > 0 {
            out.push(separator.clone());
        }
        out.extend(block);
    }
    out
}

/// The line-number gutter width (columns) a diff draws: the digit count of the
/// largest line number any hunk reaches, floored at 1 (qwen DiffRenderer.tsx
/// :213-218). Parsed from each hunk's `@@ -old,_ +new,_ @@` header and the line
/// count that follows, so no core change is needed (render-side `@@` parse).
fn diff_gutter_width(hunks: &[DiffHunk]) -> usize {
    let mut max = 1u32;
    for hunk in hunks {
        let (old_start, new_start) = parse_hunk_header(hunk.header.as_deref());
        let (mut old_n, mut new_n) = (old_start, new_start);
        for line in &hunk.lines {
            match line.side {
                DiffSide::Context => {
                    max = max.max(new_n);
                    old_n += 1;
                    new_n += 1;
                }
                DiffSide::Added => {
                    max = max.max(new_n);
                    new_n += 1;
                }
                DiffSide::Removed => {
                    max = max.max(old_n);
                    old_n += 1;
                }
            }
        }
    }
    max.to_string().len().max(1)
}

/// Parses the `(old_start, new_start)` 1-based line numbers from a `@@ -a,b +c,d
/// @@` unified-diff header (qwen `hunkHeaderRegex`, DiffRenderer.tsx:29). A `None`
/// header (a created file) starts both at 1.
fn parse_hunk_header(header: Option<&str>) -> (u32, u32) {
    let Some(header) = header else {
        return (1, 1);
    };
    // `@@ -old[,n] +new[,n] @@` — take the first number after `-` and after `+`.
    let field = |marker: char| -> u32 {
        header
            .split(marker)
            .nth(1)
            .and_then(|rest| rest.split([',', ' ']).next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(1)
    };
    (field('-'), field('+'))
}

/// The muted `... last N lines hidden ...` tail a display-capped diff ends with,
/// or nothing when the cap elided nothing (`elided == 0`). Worded to match qwen's
/// overflow banner (DiffRenderer `MaxSizedBox` → `... N lines hidden ...`).
fn diff_elided_tail(elided: usize, content_width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if elided == 0 {
        return Vec::new();
    }
    vec![Line::styled(
        truncate_cols(
            &format!("... last {elided} lines hidden ..."),
            content_width as usize,
        ),
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
    gutter_width: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    // The 1-based line numbers each row draws in the gutter, parsed from the
    // hunk header (render-side, no core change - qwen DiffRenderer.tsx:279-301).
    let numbers = hunk_line_numbers(hunk);
    // Normalize each line's text ONCE, in file order, then strip the common
    // leading indentation shared by every displayable line (qwen DiffRenderer.tsx
    // :225-241) so a deeply-nested edit still reads.
    let raw: Vec<String> = hunk
        .lines
        .iter()
        .map(|l| normalize_diff_text(&l.text))
        .collect();
    let strip = common_indent(&raw);
    let texts: Vec<String> = raw
        .iter()
        .map(|t| t.chars().skip(strip).collect())
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
        let gutter = diff_gutter_cell(numbers[out.len()], gutter_width);
        out.push(diff_code_row(
            line.side, &gutter, text, fragments, width, theme,
        ));
    }
    out
}

/// The per-row 1-based line numbers a hunk draws in its gutter, in display order:
/// a Context/Added row shows its NEW line number, a Removed row its OLD one (qwen
/// DiffRenderer.tsx:279-301). Parsed from the hunk header start numbers.
fn hunk_line_numbers(hunk: &DiffHunk) -> Vec<u32> {
    let (mut old_n, mut new_n) = parse_hunk_header(hunk.header.as_deref());
    hunk.lines
        .iter()
        .map(|line| match line.side {
            DiffSide::Context => {
                let n = new_n;
                old_n += 1;
                new_n += 1;
                n
            }
            DiffSide::Added => {
                let n = new_n;
                new_n += 1;
                n
            }
            DiffSide::Removed => {
                let n = old_n;
                old_n += 1;
                n
            }
        })
        .collect()
}

/// The common leading-space count shared by every non-blank line (qwen strips it
/// per hunk so a deeply-indented edit still reads at the box edge). A hunk of
/// only blank lines strips nothing.
fn common_indent(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|l| l.chars().any(|c| !c.is_whitespace()))
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0)
}

/// One diff line-number gutter cell: the number right-aligned in `gutter_width`
/// columns plus a trailing space, the muted-italic diff chrome. The single place
/// the gutter's alignment lives.
fn diff_gutter_cell(number: u32, gutter_width: usize) -> String {
    format!("{number:>gutter_width$} ")
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
    gutter: &str,
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

    let mut spans: Vec<Span<'static>> = Vec::new();
    // The line-number gutter (qwen DiffRenderer): the number tinted `text.secondary`
    // over the side's diff background, so the band starts at the gutter's left edge.
    let mut used = push_cols(&mut spans, gutter, band(secondary_style(theme)), 0, width);

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
/// `✦ Thinking` header (the short reasoning tail). Tunable.
const THINKING_TAIL_ROWS: usize = 3;

// ---------------------------------------------------------------------------
// The flat footer (ADR-0053, qwen `Footer.tsx`): ONE row, space-between,
// `paddingX:2`, no powerline triangles, no block backgrounds. Replaces the
// powerline status bar (retiring ADR-0046's `status_bar` and the ADR-0008/0040
// segment palette).
// ---------------------------------------------------------------------------

/// The horizontal inset the footer row wears on each side (qwen `paddingX:2`).
const FOOTER_PADDING_X: usize = 2;

/// The ` | ` separator qwen joins the right-group items with (`text.secondary`),
/// emitted BETWEEN items only - no leading separator (qwen `index > 0`).
const FOOTER_SEP: &str = " | ";

// Note: qwen's `isNarrowWidth` (< 80) switches the footer to a two-line column;
// suspenders instead stays ONE row and sheds right items (cost → model), a
// documented divergence (ADR-0046 fixed a 1-row footer zone, ADR-0053). The shed
// is width-driven (see [`Footer::fit`]), so there is no narrow-threshold const.

/// One right-group footer item's MEANING, ratatui-free (ADR-0019). The pure
/// assembly ([`footer`]) emits these carrying only the display state they
/// convey - no colours (that is [`render_footer`], ADR-0008), no separators.
/// The testable seam: the footer's contents can be asserted without a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterItem {
    /// The Active Model (qwen keeps model/cost out of its footer; suspenders
    /// keeps them as the load-bearing local-first facts, Option B, ADR-0053).
    /// Rendered `text.secondary` like the whole right group.
    Model {
        /// The Active Model identifier.
        model: String,
    },
    /// The context-usage figure (qwen `ContextUsageDisplay`, ADR-0048): the
    /// flat `NN.N% used` / `NN.N% context used` label (the pure
    /// [`context_percent_label`] rule) and whether usage is over the budget (the
    /// painter reads `error` when so, else `text.secondary`). Assembled only when
    /// a budget exists.
    Context {
        /// The flat `context_percent_label` figure (no block padding).
        label: String,
        /// Whether the Conversation is over its context budget.
        over_limit: bool,
    },
    /// The Session's cumulative dollar cost (ADR-0037), the pre-formatted
    /// [`cost_label`] total. Assembled only when the total is positive.
    Cost {
        /// The [`cost_label`]-formatted total, e.g. `$0.42` or `<$0.01`.
        label: String,
    },
    /// The MCP health pill (ADR-0065 Phase F, qwen `MCPHealthPill`): `N MCP{s}
    /// offline` when servers are disconnected-and-not-disabled, rendered in the
    /// warning colour (unlike the secondary right group). Assembled only when the
    /// offline count is positive (qwen's `getPillLabel` returns `''` at zero).
    McpOffline {
        /// The [`mcp_offline_label`] text, e.g. `1 MCP offline` / `3 MCPs offline`.
        label: String,
    },
}

impl FooterItem {
    /// This item's display text (no separator, no padding). Semantics-in,
    /// text-out - the seam ADR-0019 wants.
    fn text(&self) -> String {
        match self {
            FooterItem::Model { model } => format!("model {model}"),
            FooterItem::Context { label, .. } => label.clone(),
            FooterItem::Cost { label } => label.clone(),
            FooterItem::McpOffline { label } => label.clone(),
        }
    }

    /// The columns this item occupies once painted, ratatui-free. Kept in
    /// lockstep with [`FooterItem::text`] so the fit policy measures what the
    /// painter draws.
    fn cells(&self) -> usize {
        self.text().chars().count()
    }
}

/// The footer's left content (qwen `leftBottomContent`): the AutoAcceptIndicator
/// when the Approval mode is not Default, else the `? for shortcuts` hint.
/// Ratatui-free - the painter picks the per-mode colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterLeft {
    /// The AutoAcceptIndicator (qwen `AutoAcceptIndicator.tsx`, ADR-0050): the
    /// mode's label + the ` (shift + tab to cycle)` hint. Never `Default` (that
    /// falls to [`FooterLeft::Shortcuts`], matching qwen rendering nothing).
    AutoAccept(ApprovalMode),
    /// The resting `? for shortcuts` hint (qwen's fallthrough), `text.secondary`.
    Shortcuts,
}

/// The ratio at/above which the context figure reads `>100` (qwen `>1`).
const CONTEXT_FULL_RATIO: f64 = 1.0;
/// The percent scaling applied to the usage ratio for display.
const CONTEXT_PERCENT_SCALE: f64 = 100.0;
/// The terminal width below which the context label shortens to `% used` (qwen's
/// `terminalWidth < 100` rule).
const CONTEXT_NARROW_WIDTH: usize = 100;

/// The context-usage figure (qwen `ContextUsageDisplay` / `formatPercentageUsed`,
/// ADR-0048): `percentage = tokens / budget`; `>1 → ">100"` else `(p*100)` to one
/// decimal, then the label - `% used` when the terminal is narrower than
/// [`CONTEXT_NARROW_WIDTH`] else `% context used` (the leading `%` is part of the
/// label). FLAT - no block padding (the powerline wrapper is retired, ADR-0053);
/// the footer joins items with ` | ` instead. A zero (or missing) budget has no
/// figure to show.
fn context_percent_label(tokens: u64, budget: u64, width: usize) -> Option<String> {
    if budget == 0 {
        return None;
    }
    let ratio = tokens as f64 / budget as f64;
    let figure = if ratio > CONTEXT_FULL_RATIO {
        ">100".to_string()
    } else {
        format!("{:.1}", ratio * CONTEXT_PERCENT_SCALE)
    };
    let label = if width < CONTEXT_NARROW_WIDTH {
        "% used"
    } else {
        "% context used"
    };
    Some(format!("{figure}{label}"))
}

/// Whether the context usage is over the budget (qwen `isOverLimit`): the figure
/// reads `error` rather than secondary. Separated from the label so the painter
/// routes the colour without re-deriving the ratio.
fn context_over_limit(tokens: u64, budget: u64) -> bool {
    budget > 0 && tokens > budget
}

/// The Cost item's display text (ADR-0037: the Session's cumulative
/// Catalog-priced total, in dollars). Two decimals from a cent up; a flat
/// `<$0.01` below that - a sub-cent figure would render `$0.00` and read as
/// free. Only prices a positive total: the assembly hides the item entirely at
/// zero, so this never formats one.
pub fn cost_label(total: f64) -> String {
    if total < COST_SUB_CENT {
        "<$0.01".to_string()
    } else {
        format!("${total:.2}")
    }
}

/// The footer's assembled MEANING (ADR-0053): the left content and the ordered
/// ` | `-joined right group (model, context%, cost), already fitted to the
/// terminal width. Pure and ratatui-free - what the colocated tests assert
/// against without drawing a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    /// The left content (AutoAcceptIndicator or the shortcuts hint).
    pub left: FooterLeft,
    /// The right group, in display order (model, context%, cost), post-fit.
    pub right: Vec<FooterItem>,
}

impl Footer {
    /// Drops right-group items until the footer fits `width`, lowest-value
    /// first: cost, then model - context% NEVER drops (it is qwen's sole footer
    /// figure and the one the operator steers by). Which items show at a given
    /// width is a SEMANTIC decision, so it lives here in the pure layer; the
    /// arithmetic reads each item's own [`FooterItem::cells`] plus the left
    /// content, the ` | ` joins, and the two-side padding. This is the narrow
    /// "shed, don't wrap" divergence from qwen's two-line stack (ADR-0053).
    fn fit(mut self, width: usize, left_cells: usize) -> Footer {
        let drop_order: [fn(&FooterItem) -> bool; FOOTER_DROP_TIER_COUNT] = [
            |i| matches!(i, FooterItem::Cost { .. }),
            |i| matches!(i, FooterItem::Model { .. }),
        ];
        for dropped in drop_order {
            if self.cells(left_cells) <= width {
                break;
            }
            self.right.retain(|i| !dropped(i));
        }
        self
    }

    /// The columns the footer occupies: the two-side padding, the left content,
    /// a gap of at least one cell, and the right group (item texts + one ` | `
    /// join between each pair).
    fn cells(&self, left_cells: usize) -> usize {
        FOOTER_PADDING_X * 2 + left_cells + FOOTER_MIN_GAP + self.right_cells()
    }

    /// The right group's painted width: the item texts plus one ` | ` separator
    /// between each adjacent pair (no leading separator).
    fn right_cells(&self) -> usize {
        let text: usize = self.right.iter().map(FooterItem::cells).sum();
        let joins = self.right.len().saturating_sub(1) * FOOTER_SEP.chars().count();
        text + joins
    }
}

/// The figures the footer's right side draws: the token estimate (`None` before
/// any estimate exists) the context figure measures against the budget, the
/// Session's cumulative dollar cost (ADR-0037; `0.0` hides the Cost item), and
/// the context budget for the `% used` figure (ADR-0048).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FigureView {
    /// The token estimate the context figure divides by the budget. The
    /// powerline's per-`PressureLevel` block colour retired with it (ADR-0053);
    /// the flat footer routes context colour off `over_limit` alone, so only the
    /// count survives.
    pub tokens: Option<u64>,
    pub session_cost: f64,
    /// The Conversation's context budget (the model's usable window). `None`
    /// before any estimate/budget arrives; a zero budget shows no figure.
    pub context_budget: Option<u64>,
    /// The MCP health count (ADR-0065 Phase F, qwen `MCPHealthPill`): the number
    /// of disconnected-and-not-disabled servers. `0` hides the pill.
    pub mcp_offline: usize,
}

/// All display facts for one footer assembly, bundled to keep [`footer`] within
/// the param ceiling. Each field is an independent semantic fact the footer
/// renders.
pub(crate) struct FooterView<'a> {
    pub(crate) conn: ConnectionView<'a>,
    pub(crate) figures: FigureView,
    /// The current Approval mode (ADR-0050), for the left AutoAcceptIndicator.
    /// `Default` shows the shortcuts hint instead.
    pub(crate) approval_mode: ApprovalMode,
}

/// Assembles the footer's MEANING, pure and ratatui-free (ADR-0019, ADR-0053):
/// the left content and the ordered right group (model, context%, cost), fitted
/// to `width`. No colours, glyphs, or separators are decided here - that is the
/// painter's job ([`render_footer`]) - so every rule (item order, the shed/drop
/// policy, the model-always / cost-hidden-at-zero / context-needs-a-budget
/// rules, the Default-mode-shows-shortcuts rule) is assertable without a frame.
pub(crate) fn footer(width: usize, view: FooterView<'_>) -> Footer {
    let FooterView {
        conn,
        figures,
        approval_mode,
    } = view;

    let left = if approval_mode == ApprovalMode::Default {
        FooterLeft::Shortcuts
    } else {
        FooterLeft::AutoAccept(approval_mode)
    };

    let mut right = vec![FooterItem::Model {
        model: conn.model.to_string(),
    }];
    // The context-usage figure (qwen `ContextUsageDisplay`, ADR-0048): shown once
    // a budget exists. The label's `% used` / `% context used` form depends on the
    // terminal `width` (qwen's <100-column rule).
    if let Some(estimate) = figures.tokens
        && let Some(budget) = figures.context_budget
        && let Some(label) = context_percent_label(estimate, budget, width)
    {
        right.push(FooterItem::Context {
            label,
            over_limit: context_over_limit(estimate, budget),
        });
    }
    // The cost item exists only once a priced Response landed: at zero the
    // Session has spent nothing meterable and the footer omits it.
    if figures.session_cost > COST_HIDDEN {
        right.push(FooterItem::Cost {
            label: cost_label(figures.session_cost),
        });
    }
    // The MCP health pill (qwen `MCPHealthPill`, ADR-0065 Phase F): shown only
    // when servers are offline (qwen's `getPillLabel` hides it at zero).
    if let Some(label) = mcp_offline_label(figures.mcp_offline) {
        right.push(FooterItem::McpOffline { label });
    }

    Footer { left, right }.fit(width, footer_left_cells(left))
}

/// The MCP health pill label (qwen `MCPHealthPill.getPillLabel`): `N MCP offline`
/// at exactly one offline server, `N MCPs offline` otherwise; `None` at zero (the
/// pill is hidden). `connecting` never reaches here (the count already excludes
/// it), matching qwen's disconnected-only rule.
fn mcp_offline_label(offline: usize) -> Option<String> {
    if offline == 0 {
        return None;
    }
    let noun = if offline == 1 { "MCP" } else { "MCPs" };
    Some(format!("{offline} {noun} offline"))
}

/// The AutoAcceptIndicator's mode label (qwen `AutoAcceptIndicator.tsx` verbatim,
/// ADR-0050): `plan mode` / `auto-accept edits` / `auto mode (classifier-
/// evaluated)` / `YOLO mode`. `Default` has no label (it never renders as an
/// indicator); it maps to the empty string defensively so the function is total.
fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Plan => "plan mode",
        ApprovalMode::AutoEdit => "auto-accept edits",
        ApprovalMode::Auto => "auto mode (classifier-evaluated)",
        ApprovalMode::Yolo => "YOLO mode",
        ApprovalMode::Default => "",
    }
}

/// The AutoAcceptIndicator's per-mode label colour (qwen `AutoAcceptIndicator`):
/// plan → success, auto-edit/auto → warning, yolo → error. Default never renders
/// as an indicator, so it borrows the neutral secondary defensively.
fn approval_mode_style(mode: ApprovalMode, theme: &Theme) -> Style {
    match mode {
        ApprovalMode::Plan => success_style(theme),
        ApprovalMode::AutoEdit | ApprovalMode::Auto => warning_style(theme),
        ApprovalMode::Yolo => error_style(theme),
        ApprovalMode::Default => secondary_style(theme),
    }
}

/// The AutoAcceptIndicator's cycle hint (qwen ` (shift + tab to cycle)`), carried
/// with a leading space so it reads as one phrase across the colour boundary.
const CYCLE_HINT: &str = " (shift + tab to cycle)";

/// The resting left hint (qwen `? for shortcuts`).
const SHORTCUTS_HINT: &str = "? for shortcuts";

/// The left content's painted width (ratatui-free), so the pure [`Footer::fit`]
/// measures the same cells the painter draws.
fn footer_left_cells(left: FooterLeft) -> usize {
    match left {
        FooterLeft::AutoAccept(mode) => {
            approval_mode_label(mode).chars().count() + CYCLE_HINT.chars().count()
        }
        FooterLeft::Shortcuts => SHORTCUTS_HINT.chars().count(),
    }
}

/// The number of shed tiers in the footer's right-group drop policy.
const FOOTER_DROP_TIER_COUNT: usize = 2;

/// The minimum gap the footer reserves between the left content and the right
/// group (qwen's space-between never lets them touch).
const FOOTER_MIN_GAP: usize = 1;

/// Screen-state bundle for [`render_footer`], so the painter stays within the
/// param ceiling.
pub(crate) struct FooterCtx<'a> {
    pub(crate) screen: &'a Screen,
    pub(crate) conn: ConnectionView<'a>,
}

/// The flat bottom footer (ADR-0053, qwen `Footer.tsx`): ONE row, hand-rolled
/// space-between with a [`FOOTER_PADDING_X`] inset on each side and NO background
/// fill. A thin painter over the pure [`footer`] assembly (which items, in what
/// order): the left content wears its per-mode colour, the right group is
/// ` | `-joined in `text.secondary` (context% goes `error` when over-limit).
pub(crate) fn render_footer(frame: &mut Frame, area: Rect, ctx: FooterCtx<'_>, theme: &Theme) {
    let FooterCtx { screen: t, conn } = ctx;
    let width = area.width as usize;
    let bar = footer(
        width,
        FooterView {
            conn,
            figures: FigureView {
                tokens: t.token_estimate,
                session_cost: t.session_cost,
                context_budget: t.context_budget,
                mcp_offline: t.mcp_offline,
            },
            approval_mode: t.approval_mode,
        },
    );

    // Left content: the AutoAcceptIndicator (mode label + hint) or the shortcuts
    // hint. The hint is always `text.secondary` (qwen); the mode label wears its
    // per-mode colour.
    let mut left_spans: Vec<Span> = Vec::new();
    match bar.left {
        FooterLeft::AutoAccept(mode) => {
            left_spans.push(Span::styled(
                approval_mode_label(mode).to_string(),
                approval_mode_style(mode, theme),
            ));
            left_spans.push(Span::styled(CYCLE_HINT.to_string(), secondary_style(theme)));
        }
        FooterLeft::Shortcuts => {
            left_spans.push(Span::styled(
                SHORTCUTS_HINT.to_string(),
                secondary_style(theme),
            ));
        }
    }

    // Right group: ` | `-joined in `text.secondary`, no leading separator; the
    // context% item goes `error` when over the budget (qwen's inner colour).
    let mut right_spans: Vec<Span> = Vec::new();
    for (i, item) in bar.right.iter().enumerate() {
        if i > 0 {
            right_spans.push(Span::styled(FOOTER_SEP, secondary_style(theme)));
        }
        let style = match item {
            FooterItem::Context {
                over_limit: true, ..
            } => error_style(theme),
            // The MCP health pill reads warning (qwen `theme.status.warning`),
            // standing out from the secondary right group.
            FooterItem::McpOffline { .. } => warning_style(theme),
            _ => secondary_style(theme),
        };
        right_spans.push(Span::styled(item.text(), style));
    }

    // Hand-rolled space-between (qwen `justifyContent:"space-between"`): pad the
    // gap between the inset left and right groups so the right group sits flush
    // against the right inset.
    let left_cells: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let right_cells: usize = right_spans.iter().map(|s| s.content.chars().count()).sum();
    let content = FOOTER_PADDING_X * 2 + left_cells + right_cells;
    let gap = width.saturating_sub(content).max(FOOTER_MIN_GAP);

    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" ".repeat(FOOTER_PADDING_X)));
    spans.append(&mut left_spans);
    spans.push(Span::raw(" ".repeat(gap)));
    spans.append(&mut right_spans);
    spans.push(Span::raw(" ".repeat(FOOTER_PADDING_X)));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The two-row chrome the Composer wears above and below its draft (ADR-0048,
/// qwen `InputPrompt`): a top dash rule and a bottom single-border row. The
/// composer zone is grown by exactly this so the draft never loses a row to the
/// chrome, and the cursor y-offset accounts for the top rule (the correctness-
/// critical +1 unit-tested by `composer_cursor_sits_below_the_top_rule`).
const COMPOSER_CHROME_ROWS: usize = 2;

/// The Composer placeholder shown when the draft is empty (qwen `InputPrompt`):
/// TWO leading spaces then the hint. The first glyph draws `REVERSED` (a resting
/// block cursor), the rest secondary.
const COMPOSER_PLACEHOLDER: &str = "  Type your message or @path/to/file";

/// The Composer's border colour (qwen `borderColor`): `border.focused`
/// (link-blue) when the Composer owns the keyboard, else `border.default`
/// (grey). The Composer is unfocused exactly while the Approval modal holds the
/// keyboard (Phase-4 seam: mode variants of the prompt come later).
fn composer_border_style(focused: bool, theme: &Theme) -> Style {
    if focused {
        Style::default().fg(tui_color(theme.link))
    } else {
        border_style(theme)
    }
}

/// The Composer: a top dash rule, the draft, then a bottom border (ADR-0048,
/// qwen `InputPrompt`). The draft is pre-wrapped by the pure [`composer::layout`]
/// (char-based, so the cursor cell below is exact - `Paragraph`'s word-wrap
/// points can't be queried). The FIRST row wears the `> ` prompt in
/// `accent_style`; every continuation row - hard-newline and wrapped alike -
/// indents 2 spaces to align under it. An empty draft shows the placeholder.
///
/// When the draft is taller than the box, the Composer scrolls internally
/// ([`composer::first_visible_row`]) so the cursor row stays visible, near the
/// bottom like a terminal. The REAL terminal cursor is placed at the cursor's
/// cell (shifted DOWN one row by the top rule) - except while the Approval modal
/// owns the keyboard, when a blinking composer cursor would misstate where keys
/// go.
pub fn render_composer(
    frame: &mut Frame,
    area: Rect,
    t: &Screen,
    layout: &ComposerLayout,
    theme: &Theme,
) {
    // Operation → Integration (IOSP): the pure [`composer_chrome`] carries the
    // fit decision (`None` = too small to draw), the border style, the bottom
    // rule Rect, and the terminal-cursor cell (`None` when the Approval owns
    // the keyboard); the slot below only issues the draw calls.
    render_composer_slot(frame, area, composer_chrome(area, t, theme), layout, theme);
}

/// The Composer's drawable chrome (the compute-plan behind [`render_composer`]):
/// the border style, the bottom-border Rect, and whether the terminal cursor is
/// parked (`focused` - false while the Approval modal owns the keyboard, when a
/// blinking composer cursor would misstate where keys go). Built by
/// [`composer_chrome`]; the cursor CELL is layout-dependent, computed at draw
/// time by [`composer_cursor`].
struct ComposerChrome {
    border: Style,
    bottom: Rect,
    focused: bool,
}

/// Operation (IOSP): the Composer's chrome for `area`, or `None` when the zone
/// is too small to hold the two chrome rows plus a draft column (measure ==
/// draw, ADR-0029). Pure: no frame access. The fit and the `focused` decision
/// are made here so [`render_composer`] never branches.
fn composer_chrome(area: Rect, t: &Screen, theme: &Theme) -> Option<ComposerChrome> {
    let fits = area.height as usize > COMPOSER_CHROME_ROWS && area.width >= 2;
    // The composer is focused when no modal holds the keyboard. A pending
    // question modal takes focus like an approval, EXCEPT while it is collecting
    // a free-form "Other" answer - then the composer is the interactive element
    // and stays focused (ADR-0057).
    let question_holds_focus = t
        .pending_question
        .as_ref()
        .is_some_and(|q| q.collecting_other.is_none());
    let focused = t.pending_approval.is_none() && !question_holds_focus;
    fits.then(|| ComposerChrome {
        border: composer_border_style(focused, theme),
        bottom: Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        },
        focused,
    })
}

/// The Composer slot: draws the body, the bottom rule, and (when focused) parks
/// the terminal cursor - but only when the plan says the zone fits (`Some`).
/// The presence + focus branches live HERE so [`render_composer`] is call-only
/// (IOSP).
fn render_composer_slot(
    frame: &mut Frame,
    area: Rect,
    chrome: Option<ComposerChrome>,
    layout: &ComposerLayout,
    theme: &Theme,
) {
    if let Some(chrome) = chrome {
        draw_composer(frame, area, &chrome, layout, theme);
    }
}

/// Draws a fitted Composer (call-only assembler): the body Paragraph, the
/// bottom rule, and the terminal cursor when the chrome carries a cell.
fn draw_composer(
    frame: &mut Frame,
    area: Rect,
    chrome: &ComposerChrome,
    layout: &ComposerLayout,
    theme: &Theme,
) {
    let rule_width = area.width as usize;
    frame.render_widget(
        Paragraph::new(composer_body_lines(
            layout,
            area.height,
            rule_width,
            chrome.border,
            theme,
        )),
        area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled("─".repeat(rule_width), chrome.border)),
        chrome.bottom,
    );
    place_composer_cursor(frame, chrome.focused, layout, area);
}

/// Parks the terminal cursor at the draft cell when the chrome is `focused`,
/// else leaves it (the Approval owns the keyboard). The focus branch lives HERE
/// (IOSP).
fn place_composer_cursor(frame: &mut Frame, focused: bool, layout: &ComposerLayout, area: Rect) {
    if focused {
        frame.set_cursor_position(composer_cursor(layout, area));
    }
}

/// Operation (IOSP): the Composer's body lines - the top dash rule (qwen's
/// hand-drawn `─`×`rule_width`; the `top_right_label` seam is deferred, no
/// session-name concept yet) then the draft rows (the `> ` prompt on row 0,
/// 2-space indent on continuations) or the placeholder when empty. The bottom
/// border is a separate draw (a different rect), so it is not in this list.
/// `zone_height` is the full composer zone height; the draft window is it less
/// the two chrome rows. Pure.
fn composer_body_lines(
    layout: &ComposerLayout,
    zone_height: u16,
    rule_width: usize,
    border: Style,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled("─".repeat(rule_width), border)];
    if composer_is_empty(layout) {
        lines.push(composer_placeholder_line(theme));
        return lines;
    }
    let visible = zone_height as usize - COMPOSER_CHROME_ROWS;
    let top = composer::first_visible_row(layout.cursor_row, visible);
    let prompt = accent_style(theme).add_modifier(Modifier::BOLD);
    lines.extend(
        layout
            .rows
            .iter()
            .enumerate()
            .skip(top)
            .take(visible)
            .map(|(i, row)| {
                let prefix = if i == 0 { "> " } else { "  " };
                Line::from(vec![Span::styled(prefix, prompt), Span::raw(row.clone())])
            }),
    );
    lines
}

/// Operation (IOSP): the real terminal cursor cell for the draft cursor - the
/// `> ` prompt shifts it right by [`PROMPT_GUTTER_COLS`], and the top dash rule
/// shifts it DOWN one row (the correctness-critical `+1`, Risk #1). `cursor_col <
/// width` by the layout contract, so the cell is always inside `area`. Pure.
fn composer_cursor(layout: &ComposerLayout, area: Rect) -> (u16, u16) {
    let visible = area.height as usize - COMPOSER_CHROME_ROWS;
    let top = composer::first_visible_row(layout.cursor_row, visible);
    (
        area.x + PROMPT_GUTTER_COLS as u16 + layout.cursor_col as u16,
        area.y + 1 + (layout.cursor_row - top) as u16,
    )
}

/// The width of the `> ` prompt gutter every draft row hangs under.
const PROMPT_GUTTER_COLS: usize = 2;

/// Whether the Composer draft is empty (one blank row, cursor at the origin) -
/// the placeholder condition. Pure over the layout.
fn composer_is_empty(layout: &ComposerLayout) -> bool {
    layout.cursor_row == 0 && layout.cursor_col == 0 && layout.rows.iter().all(|r| r.is_empty())
}

/// The placeholder line (qwen `InputPrompt`): the two-space-lead hint in
/// secondary, its FIRST glyph `REVERSED` so a resting block cursor sits where
/// typing begins.
fn composer_placeholder_line(theme: &Theme) -> Line<'static> {
    let secondary = secondary_style(theme);
    let mut chars = COMPOSER_PLACEHOLDER.chars();
    let first: String = chars.by_ref().take(1).collect();
    let rest: String = chars.collect();
    Line::from(vec![
        Span::styled(first, secondary.add_modifier(Modifier::REVERSED)),
        Span::styled(rest, secondary),
    ])
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

// Normalizes a `key_arg` for rendering: an absent OR empty arg both read as "no
// arg". The ONE place the display treats emptiness (the source rule lives in the
// core's `key_arg`, but a recovered call summary can still be empty).
fn present_arg(key_arg: Option<&str>) -> Option<&str> {
    key_arg.filter(|a| !a.is_empty())
}

/// A tool result row's dim `description` (qwen `ToolInfo` description, shown after
/// the bold name): the salient `key_arg` and the result summary joined `arg ·
/// result`, dropping to bare `result` when there is no arg. The tool NAME is NOT
/// repeated here - `tool_header_row` draws it bold ahead of this.
fn tool_desc(key_arg: Option<&str>, summary: &str) -> String {
    match present_arg(key_arg) {
        Some(arg) if summary.is_empty() => arg.to_string(),
        Some(arg) => format!("{arg} · {summary}"),
        None => summary.to_string(),
    }
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
#[path = "../../tests/ui/components.rs"]
mod tests;
