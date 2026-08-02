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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
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

/// Splits the fullscreen frame `area` into the three vertical zones the body
/// draws into: `[transcript_body, footer, composer]` (ADR-0046). The body is
/// bottom-anchored + top-clipped in the top zone, lifted by the app-owned scroll
/// intent when the view is detached from the tail (Stage 2, [`scrolled_clip`]).
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

/// Renders the FULLSCREEN frame (ADR-0046): the WHOLE transcript body (every
/// settled item plus the live reasoning tail, streaming answer, and lull row),
/// the flat footer, the Composer, and any open overlay/approval. The app owns
/// the entire alt-screen viewport and redraws it from the model each frame, so
/// nothing lives in native scrollback and resize simply re-wraps everything.
///
/// The transcript body is BOTTOM-ANCHORED in its zone and TOP-CLIPPED on
/// overflow (qwen's `MaxSizedBox overflowDirection:"top"`): the newest rows
/// always show, older rows drop off the top. The app owns scrolling (Stage 2): a
/// detached view ([`Screen::follow_tail`] false) lifts the window up through the
/// clipped rows, clamped to the live viewport each frame ([`scrolled_clip`]).
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
    if t.help_open {
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
    // right, the AutoAcceptIndicator or `? for shortcuts` on the left.
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
    // The sticky box DERIVES from the latest Todo item once it is no longer the
    // transcript tail; a tail, all-completed, or empty list, or a frame too short
    // to also hold the box, drops it (costs no rows). The `.filter` is the measure
    // == draw guard: reserving a
    // zone we cannot fully draw would paint a headless fragment over the
    // composer.
    //
    // An OPEN approval (ADR-0049) also drops the box: the approval renders inside
    // the pending body, and the informational sticky box would starve that body
    // (`Constraint::Min(1)`) and top-clip the "Apply this change?" question out of
    // view. A visible approval takes priority over the sticky list, so we reserve
    // NO sticky zone while `pending_approval.is_some()`.
    let sticky_items = (t.pending_approval.is_none() && t.pending_question.is_none())
        .then(|| sticky_todos(t.transcript().latest_todo(), t.transcript().items().len()))
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

/// The transcript BODY zone height for a frame of size `area` (ADR-0046, Stage
/// 2): the wrapped-row page the pure [`Screen`]'s PageUp/PageDown step by. Runs
/// the same pure [`pending_layout`] the render uses, so the page matches the drawn
/// body exactly (measure == draw). The adapter calls this each frame and records
/// it via [`crate::ui::screen::Screen::note_body_height`]; the core stays
/// geometry-free.
pub fn body_height(area: Rect, t: &Screen) -> usize {
    let view = t.composer().view();
    pending_layout(area, &view, t).body.height as usize
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
    if let Some(overlay) = overlay {
        render_composer_popup(frame, popup_top, area, overlay, theme);
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

/// Draws the whole transcript body into `area`, bottom-anchored and top-clipped
/// (ADR-0046). Returns the total wrapped-row count of the stack (before clipping)
/// so the caller can label the status bar. The assembly is the body pipeline -
/// cache sync, the collapsed-run fold over the full items, the three live entries
/// - starting at item 0 and anchored to the bottom of the zone.
fn render_pending_body(
    frame: &mut Frame,
    area: Rect,
    params: &mut PendingBodyParams<'_>,
    theme: &Theme,
) -> usize {
    // Fullscreen renderer (ADR-0046): the whole transcript renders each frame,
    // so the body starts at item 0 (no committed prefix in native scrollback).
    render_pending_body_at(frame, area, params, theme, 0)
}

/// Assembles the FULL, UNCLAMPED body line set at content `width` starting from
/// item `hw` (ADR-0046): the settled tail `items[hw..]` through [`grouped_rows`],
/// then the live entries newest-last (the reasoning tail, the streaming answer,
/// the spinner). The fullscreen renderer passes `hw = 0` so the WHOLE transcript
/// renders. This is the PRE-CLIP line set `render_pending_body_at` anchors/clips
/// to the body zone. Syncs the cache as a side effect (the settled tail's lines
/// come from it); no frame access, no anchor/clip math (IOSP).
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
    // FULL-CONTENT body (ADR-0046): the whole settled transcript renders through
    // the [`grouped_rows`] fold (qwen's `<Static>` prints history un-clamped; the
    // ONLY overflow reduction is the bottom-anchor + top-clip the caller applies).
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

/// Draws the body starting AT an explicit item index `hw`: it emits the settled
/// tail `items[hw..]` plus the live stream, bottom-anchored and top-clipped
/// (ADR-0046). [`render_pending_body`] calls this with `hw = 0` so the WHOLE
/// transcript renders in the fullscreen viewport; the parameter is kept so tests
/// can render a partial tail.
fn render_pending_body_at(
    frame: &mut Frame,
    area: Rect,
    params: &mut PendingBodyParams<'_>,
    theme: &Theme,
    hw: usize,
) -> usize {
    let content_area = Rect {
        x: area.x + CONTENT_MARGIN,
        width: content_width(area.width),
        ..area
    };
    // Operation (IOSP): assemble the FULL, unclamped body once; the draw below
    // only anchors/clips it (ADR-0046, [`pending_body_lines`]).
    let lines = pending_body_lines(params, theme, hw, content_area.width);

    // Integration (IOSP): compute the anchor/clip geometry in the pure
    // [`scrolled_clip`] operation against the app-owned scroll intent (ADR-0046,
    // Stage 2), then only issue the draw calls. Following the tail is byte-identical
    // to Stage 1's bottom-anchor; a detached view lifts the window UP, clamped there.
    let total = wrapped_count(lines.clone(), content_area.width);
    let clip = scrolled_clip(
        total,
        area,
        content_area,
        ScrollIntent {
            follow_tail: params.screen.follow_tail,
            lines: params.screen.scroll_lines,
        },
    );

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
///
/// Bottom-anchored (Stage 1 / help) - equivalent to [`scrolled_clip`] with a zero
/// scroll intent. Kept as the thin wrapper the help overlay draws through.
fn anchor_clip(total_lines: usize, area: Rect, content_area: Rect) -> PendingClip {
    scrolled_clip(total_lines, area, content_area, ScrollIntent::FOLLOW)
}

/// The transcript's app-owned scroll INTENT (ADR-0046, Stage 2), passed from the
/// pure [`Screen`] to the render clamp: `follow_tail` pins to the bottom (the
/// Stage 1 default), else `lines` is how many wrapped rows the view is scrolled UP
/// from the bottom (`usize::MAX` = as far up as possible). Geometry-free - the
/// clamp turns it into a valid top-clip against the live viewport each frame.
#[derive(Clone, Copy)]
struct ScrollIntent {
    follow_tail: bool,
    lines: usize,
}

impl ScrollIntent {
    /// The bottom-anchored, tail-following default (Stage 1 behavior).
    const FOLLOW: ScrollIntent = ScrollIntent {
        follow_tail: true,
        lines: 0,
    };
}

/// Operation (IOSP): the anchor/clip math generalized to an app-owned scroll
/// INTENT (ADR-0046, Stage 2). Following the tail bottom-anchors exactly as Stage
/// 1 did; a detached `intent` lifts the window UP by `intent.lines`, CLAMPED here
/// to the valid range so the pure core stays geometry-free: `max_scroll =
/// total - height` (0 when the stack fits, so scroll is a no-op), and the effective
/// lift is `min(intent.lines, max_scroll)` - an over-scroll or `usize::MAX` (Home)
/// simply pins to the oldest row, and a grown terminal auto-re-clamps. No frame
/// access, no side effects.
fn scrolled_clip(
    total_lines: usize,
    area: Rect,
    content_area: Rect,
    intent: ScrollIntent,
) -> PendingClip {
    let height = area.height as usize;
    let overflowed = total_lines > height;

    // The rows scrolled up from the bottom, clamped to what the stack allows.
    // `follow_tail` (or a stack that fits) means no lift.
    let max_scroll = total_lines.saturating_sub(height);
    let effective = if intent.follow_tail {
        0
    } else {
        intent.lines.min(max_scroll)
    };
    // The rows still hidden ABOVE the viewport once the lift is applied: the
    // overflow marker (`…`) shows only while some remain (so Home, fully scrolled
    // up, reveals the oldest row instead of hiding it under the marker).
    let clipped_above = max_scroll - effective;
    let has_marker = overflowed && clipped_above > 0;

    let (top, drawn_rows, pad_top) = if overflowed {
        // Bottom-origin top-clip lifted by `effective` (qwen's
        // `overflowDirection:"top"`): drop `clipped_above` rows from the top, plus
        // one more for the marker row when it shows.
        (clipped_above + has_marker as usize, height, 0)
    } else {
        (0, total_lines, height - total_lines)
    };

    // When the marker shows, the top visible row is it, so the content starts one
    // row down and loses that row of height.
    let content_top_pad: u16 = if has_marker { 1 } else { 0 };
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
        marker_draw: has_marker.then_some(Rect {
            y: area.y + pad_top as u16,
            height: 1,
            ..area
        }),
    }
}

/// Draws the `…` overflow marker on the reserved top row when rows are clipped
/// ABOVE the viewport (ADR-0046): the "more above" affordance the app-owned scroll
/// (Stage 2) reveals - wheel/PageUp/Ctrl-S walk up into those rows, and the marker
/// clears once the view reaches the very top.
fn draw_overflow_marker(frame: &mut Frame, area: Rect, theme: &Theme) {
    let marker_style = Style::default()
        .fg(tui_color(theme.muted))
        .add_modifier(Modifier::ITALIC);
    frame.render_widget(Paragraph::new(Line::styled("…", marker_style)), area);
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
        // The `/mcp` management wizard (ADR-0065 Phase E): a heterogeneous
        // multi-step surface (grouped server list, key/value detail with a radio
        // action list, tool list, tool-schema detail, OAuth progress). Its active
        // row is already baked into each row's semantic `McpStyle`, so it renders
        // as the flattened header/content/footer the fold minted - no re-scroll.
        OverlayView::McpDialog(dialog) => mcp_dialog_draw(dialog, theme),
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

/// The `/mcp` dialog's popup plan (ADR-0065 Phase E): the current step's
/// [`McpDialogView`] flattened to lines - the header rows, a blank, the content
/// rows, a blank, then the footer - titled `/mcp` and capped like a System A
/// dialog. The popup Block draws the border, so these rows carry none. Pure over
/// the view + Theme; the active row's highlight already rides each row's
/// [`McpStyle`].
fn mcp_dialog_draw(dialog: &McpDialogView, theme: &Theme) -> PopupDraw {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.extend(dialog.header.iter().map(|r| mcp_row_line(r, theme)));
    lines.push(Line::default());
    lines.extend(dialog.content.iter().map(|r| mcp_row_line(r, theme)));
    lines.push(Line::default());
    lines.push(mcp_row_line(&dialog.footer, theme));
    PopupDraw {
        title: "/mcp".to_string(),
        lines,
        scroll_active: None,
        body_cap: POPUP_MAX_ROWS,
    }
}

/// One rendered [`McpRow`] as a borderless [`Line`]: each [`McpSpan`] mapped from
/// its semantic [`McpStyle`] to the active Theme, plus [`Modifier::BOLD`] when the
/// span asserts `bold` (qwen's orthogonal `<Text bold>` emphasis on header titles,
/// group headings, and TOOL_DETAIL labels).
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
// ADR-0048). A LIVE overlay (uncached, never in grouped_rows - like the lull
// row): it DERIVES from the Transcript's latest `Todo` item, so it and the
// inline copy in the body read one source of truth.
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
/// `Todo`'s items show iff the list is NON-EMPTY, NOT all-completed, AND the item
/// is NOT the newest transcript item (`latest_index + 1 < total`). In the
/// fullscreen model everything renders inline, so the "not the newest item" gate
/// stands in for qwen's pending/recent guards: while the todo IS the tail it
/// renders inline just above the composer and the sticky box would double it;
/// once newer content follows, the inline copy scrolls up under the anchor and
/// the sticky box takes over. Pure - a testable predicate, no frame.
fn sticky_todos(latest: Option<(usize, &[TodoItem])>, total: usize) -> Option<&[TodoItem]> {
    let (index, items) = latest?;
    let non_empty = !items.is_empty();
    let all_completed = non_empty && items.iter().all(|i| i.status == TodoStatus::Completed);
    let not_the_tail = index + 1 < total;
    (non_empty && !all_completed && not_the_tail).then_some(items)
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
/// adapter shares the same margin the pending region uses.
pub(crate) const CONTENT_MARGIN: u16 = 2;

/// The widest readable content is drawn (columns), matching qwen's
/// `mainAreaWidth = min(terminalWidth - 4, 100)` (AppContainer.tsx): on an
/// ultrawide terminal, prose/diffs/tool output stay legible left-aligned at 100
/// columns instead of stretching edge to edge. Full-width chrome (the footer rule)
/// is sized separately and is NOT bound by this cap.
const MAX_CONTENT_WIDTH: u16 = 100;

/// The readable-content width for a zone `area_width`: the frame width minus both
/// [`CONTENT_MARGIN`]s, capped at [`MAX_CONTENT_WIDTH`] (qwen `mainAreaWidth`).
/// The ONE place the cap lives, so a zone's measure and draw agree (measure==draw,
/// ADR-0029). Below the cap it is exactly `area_width - 2*CONTENT_MARGIN`, so
/// narrow terminals are unchanged.
fn content_width(area_width: u16) -> u16 {
    area_width
        .saturating_sub(2 * CONTENT_MARGIN)
        .min(MAX_CONTENT_WIDTH)
}

/// The blank `marginTop:1` separator row between committed items (qwen
/// `HistoryItemDisplay.tsx:64`; continuation types get `marginTop:0`). Emitted at
/// assembly by [`grouped_rows`], never cached.
fn separator_row() -> Line<'static> {
    Line::default()
}

/// Folds the settled items `[hw..]` into the flat body via the collapsed-run fold
/// with NO open approval - the convenience wrapper the assembly tests measure
/// against. The production path is [`grouped_rows_with_approval`] (the pending
/// body always passes its approving state); this drops that arg so a test can
/// render a plain item slice. `items` is the FULL item list; only `[hw..]` is
/// emitted. `width` is the content width the cache was synced at.
#[cfg(test)]
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
mod render_cache {
    use ratatui::text::Line;

    use super::{Toggles, markdown_lines, message_lines, wrapped_count};
    use crate::ui::theme::{self, Theme};
    use crate::ui::transcript::Transcript;

    /// Per-item render state for the fullscreen transcript body (ADR-0046), owned
    /// by the adapter's run loop and threaded through [`super::render_pending`].
    /// Holds ratatui [`Line`]s, so it lives HERE, not in the pure modules
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
                let lines = message_lines(item, toggles.compact, width, theme);
                // Per-item separators are added at assembly (`grouped_rows`
                // interleaves a blank `separator_row`, qwen `marginTop:1`), not
                // baked into each cached item - so the cache holds only the
                // item's own body lines (ADR-0046).
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
            assert_eq!(line_text(&cache.items[1].lines[0]), "● appended");
        }

        #[test]
        fn cache_sync_rebuilds_when_the_revision_moves() {
            let mut t = fresh_transcript();
            t.steering_queued("check");
            // The delivered steering removes its pending marker - a structural
            // edit that bumps the store's revision - so the cache rebuilds
            // from scratch: the sentinel is gone and the promoted user line is
            // seen. The `>` caret prefix is baked into the cached User line now
            // (ADR-0046 qwen chrome), so the cached first span is `> check`.
            let mut cache = seeded_cache(&t);
            t.steering_delivered("check");
            cache.sync(&t, Toggles::default(), 80, theme::dark());
            assert_eq!(cache.items.len(), 1);
            assert_eq!(line_text(&cache.items[0].lines[0]), "> check");
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
            assert_eq!(line_text(&cache.items[0].lines[0]), "● replacement");
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
    ("Ctrl+S", "Scroll up a page through the transcript"),
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
        width: content_width(area.width),
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
}

impl FooterItem {
    /// This item's display text (no separator, no padding). Semantics-in,
    /// text-out - the seam ADR-0019 wants.
    fn text(&self) -> String {
        match self {
            FooterItem::Model { model } => format!("model {model}"),
            FooterItem::Context { label, .. } => label.clone(),
            FooterItem::Cost { label } => label.clone(),
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

    Footer { left, right }.fit(width, footer_left_cells(left))
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
    // The spinner line / LoadingIndicator (ADR-0048): the running spinner that
    // subsumed the old lull "waiting" row - it keeps the lull scene as its
    // phrase content and carries the elapsed + esc-to-cancel affordance.
    // -----------------------------------------------------------------------

    fn spinner_text(lines: &[Line<'static>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    // format_token_count (qwen formatTokenCount): bare under 1000, N.Nk rounded
    // 1000..9999, Nk floored 10000..999999, N.Nm rounded at 1000000+.
    #[test]
    fn format_token_count_matches_qwens_k_thresholds() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(847), "847");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1.0k");
        assert_eq!(format_token_count(5400), "5.4k");
        assert_eq!(format_token_count(9999), "10.0k");
        assert_eq!(format_token_count(10000), "10k");
        assert_eq!(format_token_count(100000), "100k");
        assert_eq!(format_token_count(999999), "999k");
        // The megatoken branch (qwen `2_400_000 -> "2.4m"`): one decimal, rounded.
        assert_eq!(format_token_count(1_000_000), "1.0m");
        assert_eq!(format_token_count(1_200_000), "1.2m");
        assert_eq!(format_token_count(2_400_000), "2.4m");
        assert_eq!(format_token_count(2_450_000), "2.5m");
    }

    // While the lull settles (`quiet_ticks` under SETTLE_TICKS) there is no
    // phrase yet, so the spinner waits - unless a subject overrides.
    #[test]
    fn spinner_line_is_empty_within_the_settle_window_without_a_subject() {
        let lines = spinner_line(Anim::default(), SpinnerState::default(), 60, theme::dark());
        assert!(lines.is_empty(), "no phrase yet, no spinner row");
    }

    // At the settle close the spinner shows one row carrying the phrase, the
    // elapsed timer (opens at 5s), and `esc to cancel`. No token figure when
    // `tokens` is None.
    #[test]
    fn spinner_line_shows_phrase_elapsed_and_esc_to_cancel() {
        let anim = Anim {
            quiet_ticks: lull::SETTLE_TICKS,
            ..Default::default()
        };
        let lines = spinner_line(anim, SpinnerState::default(), 80, theme::dark());
        assert_eq!(lines.len(), 1, "one visual row");
        let text = spinner_text(&lines);
        assert!(text.contains("5s"), "elapsed opens at 5s: {text:?}");
        assert!(
            text.contains("esc to cancel"),
            "cancel affordance: {text:?}"
        );
        assert!(
            !text.contains("tokens"),
            "no token figure when None: {text:?}"
        );
    }

    // The subject wins over the lull phrase (the Phase-6 thought-subject seam,
    // qwen `thought?.subject || currentLoadingPhrase`) and shows even inside the
    // settle window.
    #[test]
    fn spinner_line_subject_wins_over_the_lull_phrase() {
        let lines = spinner_line(
            Anim::default(),
            SpinnerState {
                subject: Some("Refactoring the parser"),
                ..SpinnerState::default()
            },
            80,
            theme::dark(),
        );
        assert!(spinner_text(&lines).contains("Refactoring the parser"));
    }

    // The token figure shows with the arrow: `↑` when NOT receiving (sending),
    // `↓` while streaming text arrives; the count is formatTokenCount'd.
    #[test]
    fn spinner_line_shows_the_arrow_and_token_figure_when_present() {
        let anim = Anim {
            quiet_ticks: lull::SETTLE_TICKS,
            ..Default::default()
        };
        let sending = spinner_line(
            anim,
            SpinnerState {
                tokens: Some(1234),
                ..SpinnerState::default()
            },
            80,
            theme::dark(),
        );
        let text = spinner_text(&sending);
        assert!(text.contains("↑ 1.2k tokens"), "sending arrow up: {text:?}");

        let receiving = spinner_line(
            anim,
            SpinnerState {
                tokens: Some(1234),
                receiving: true,
                ..SpinnerState::default()
            },
            80,
            theme::dark(),
        );
        assert!(
            spinner_text(&receiving).contains("↓ 1.2k tokens"),
            "receiving arrow down"
        );
    }

    // A narrow width truncates the whole line to one visual row (measure==draw,
    // ADR-0029).
    #[test]
    fn spinner_line_truncates_to_one_row_at_a_narrow_width() {
        let width = 24u16;
        let anim = Anim {
            quiet_ticks: lull::SETTLE_TICKS,
            ..Default::default()
        };
        let lines = spinner_line(
            anim,
            SpinnerState {
                subject: Some("a very long thought subject that overflows"),
                ..SpinnerState::default()
            },
            width,
            theme::dark(),
        );
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).chars().count() <= width as usize);
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
    /// block with a blank row above and below. This finds
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
    // The flat footer assembly (ADR-0053).
    // -----------------------------------------------------------------------

    /// A [`FigureView`] with no token estimate and a zero (hidden) cost.
    fn no_figures() -> FigureView {
        FigureView {
            tokens: None,
            session_cost: 0.0,
            context_budget: None,
        }
    }

    /// A [`FigureView`] carrying only the token estimate, cost hidden.
    fn tokens_only(estimate: u64) -> FigureView {
        FigureView {
            tokens: Some(estimate),
            session_cost: 0.0,
            context_budget: None,
        }
    }

    /// The pure footer assembly at `width` with the default fixtures (idle-ish,
    /// tokens present with a budget, a positive cost) - the frame-free seam the
    /// footer tests assert against.
    fn footer_at(width: usize) -> Footer {
        footer(
            width,
            FooterView {
                conn: ConnectionView {
                    base_url: "http://localhost:8080",
                    model: "qwen3-coder",
                },
                figures: FigureView {
                    tokens: Some(2500),
                    session_cost: 0.42,
                    context_budget: Some(10_000),
                },
                approval_mode: ApprovalMode::Default,
            },
        )
    }

    /// The footer at `width` with a given approval mode (default fixtures else).
    fn footer_with_mode(mode: ApprovalMode) -> Footer {
        footer(
            200,
            FooterView {
                conn: ConnectionView {
                    base_url: "http://localhost:8080",
                    model: "qwen3-coder",
                },
                figures: no_figures(),
                approval_mode: mode,
            },
        )
    }

    #[test]
    fn a_wide_footer_shows_model_then_context_then_cost_in_order() {
        let bar = footer_at(200);
        assert_eq!(bar.left, FooterLeft::Shortcuts);
        assert_eq!(
            bar.right,
            vec![
                FooterItem::Model {
                    model: "qwen3-coder".into()
                },
                FooterItem::Context {
                    label: "25.0% context used".into(),
                    over_limit: false,
                },
                FooterItem::Cost {
                    label: "$0.42".into(),
                },
            ]
        );
    }

    #[test]
    fn a_narrow_footer_sheds_cost_then_model_keeping_context() {
        // Cost drops first, then model; context% survives longest (qwen's sole
        // figure). Widths chosen against the fixture: left `? for shortcuts` (15),
        // right `model qwen3-coder` (17) + `25.0% used` (10) + `$0.42` (5), each
        // ` | ` join (3), padding (4), min gap (1). Full = 4+15+1+(17+10+5+6)=58.
        let full = footer_at(60);
        assert_eq!(
            full.right,
            vec![
                FooterItem::Model {
                    model: "qwen3-coder".into()
                },
                FooterItem::Context {
                    label: "25.0% used".into(),
                    over_limit: false,
                },
                FooterItem::Cost {
                    label: "$0.42".into(),
                },
            ]
        );

        // Tighter (52): cost sheds (drops to 50), model + context survive.
        let no_cost = footer_at(52);
        assert_eq!(
            no_cost.right,
            vec![
                FooterItem::Model {
                    model: "qwen3-coder".into()
                },
                FooterItem::Context {
                    label: "25.0% used".into(),
                    over_limit: false,
                },
            ]
        );

        // Tightest: model sheds too, context% NEVER drops.
        let context_only = footer_at(20);
        assert_eq!(
            context_only.right,
            vec![FooterItem::Context {
                label: "25.0% used".into(),
                over_limit: false,
            }]
        );
    }

    #[test]
    fn the_footer_joins_the_right_group_with_a_grey_pipe_no_leading_sep() {
        // No powerline separators anywhere; the ` | ` join is the ONLY chrome
        // between items and there is none before the first.
        let bar = footer_at(200);
        assert!(!FOOTER_SEP.contains('\u{e0b0}'));
        assert!(!FOOTER_SEP.contains('\u{e0b2}'));
        assert_eq!(FOOTER_SEP, " | ");
        // right_cells = texts + one join per adjacent pair.
        let texts: usize = bar.right.iter().map(FooterItem::cells).sum();
        assert_eq!(
            bar.right_cells(),
            texts + (bar.right.len() - 1) * FOOTER_SEP.chars().count()
        );
    }

    #[test]
    fn default_mode_shows_the_shortcuts_hint() {
        let bar = footer_with_mode(ApprovalMode::Default);
        assert_eq!(bar.left, FooterLeft::Shortcuts);
    }

    #[test]
    fn each_non_default_mode_shows_the_autoaccept_label_and_hint() {
        for mode in [
            ApprovalMode::Plan,
            ApprovalMode::AutoEdit,
            ApprovalMode::Auto,
            ApprovalMode::Yolo,
        ] {
            let bar = footer_with_mode(mode);
            assert_eq!(bar.left, FooterLeft::AutoAccept(mode), "{mode:?}");
            assert!(!approval_mode_label(mode).is_empty(), "{mode:?}");
        }
    }

    #[test]
    fn the_autoaccept_labels_match_qwen_verbatim() {
        assert_eq!(approval_mode_label(ApprovalMode::Plan), "plan mode");
        assert_eq!(
            approval_mode_label(ApprovalMode::AutoEdit),
            "auto-accept edits"
        );
        assert_eq!(
            approval_mode_label(ApprovalMode::Auto),
            "auto mode (classifier-evaluated)"
        );
        assert_eq!(approval_mode_label(ApprovalMode::Yolo), "YOLO mode");
        assert_eq!(approval_mode_label(ApprovalMode::Default), "");
        assert_eq!(CYCLE_HINT, " (shift + tab to cycle)");
        assert_eq!(SHORTCUTS_HINT, "? for shortcuts");
    }

    #[test]
    fn the_autoaccept_label_colour_matches_the_mode() {
        // plan → success, auto-edit/auto → warning, yolo → error (qwen).
        let theme = theme::dark();
        assert_eq!(
            approval_mode_style(ApprovalMode::Plan, theme).fg,
            success_style(theme).fg
        );
        assert_eq!(
            approval_mode_style(ApprovalMode::AutoEdit, theme).fg,
            warning_style(theme).fg
        );
        assert_eq!(
            approval_mode_style(ApprovalMode::Auto, theme).fg,
            warning_style(theme).fg
        );
        assert_eq!(
            approval_mode_style(ApprovalMode::Yolo, theme).fg,
            error_style(theme).fg
        );
    }

    #[test]
    fn the_cost_item_is_absent_at_zero_and_present_when_priced() {
        let zero = footer(
            200,
            FooterView {
                conn: ConnectionView {
                    base_url: "u",
                    model: "m",
                },
                figures: no_figures(),
                approval_mode: ApprovalMode::Default,
            },
        );
        assert!(
            !zero
                .right
                .iter()
                .any(|i| matches!(i, FooterItem::Cost { .. }))
        );

        let priced = footer(
            200,
            FooterView {
                conn: ConnectionView {
                    base_url: "u",
                    model: "m",
                },
                figures: FigureView {
                    tokens: None,
                    session_cost: 0.42,
                    context_budget: None,
                },
                approval_mode: ApprovalMode::Default,
            },
        );
        assert_eq!(
            priced
                .right
                .iter()
                .find(|i| matches!(i, FooterItem::Cost { .. })),
            Some(&FooterItem::Cost {
                label: "$0.42".into()
            })
        );
    }

    #[test]
    fn cost_label_shows_two_decimals_and_a_sub_cent_floor() {
        assert_eq!(cost_label(0.42), "$0.42");
        assert_eq!(cost_label(0.01), "$0.01");
        assert_eq!(cost_label(12.3), "$12.30");
        assert_eq!(cost_label(1234.567), "$1234.57");
        assert_eq!(cost_label(0.0099), "<$0.01");
        assert_eq!(cost_label(0.0001), "<$0.01");
    }

    #[test]
    fn context_percent_label_formats_the_ratio_flat_and_labels_by_width() {
        // FLAT now (no block padding, ADR-0053): the leading `%` is part of the
        // label; `% used` under 100 cols, `% context used` at/above.
        assert_eq!(
            context_percent_label(2500, 10_000, 80).as_deref(),
            Some("25.0% used")
        );
        assert_eq!(
            context_percent_label(2500, 10_000, 120).as_deref(),
            Some("25.0% context used")
        );
        assert_eq!(
            context_percent_label(15_000, 10_000, 120).as_deref(),
            Some(">100% context used")
        );
        assert_eq!(context_percent_label(2500, 0, 80), None);
    }

    #[test]
    fn context_over_limit_flags_usage_past_the_budget() {
        assert!(!context_over_limit(2500, 10_000));
        assert!(!context_over_limit(10_000, 10_000));
        assert!(context_over_limit(10_001, 10_000));
        assert!(!context_over_limit(1, 0));
    }

    #[test]
    fn the_footer_carries_a_context_item_only_when_a_budget_exists() {
        let with_budget = footer(
            120,
            FooterView {
                conn: ConnectionView {
                    base_url: "u",
                    model: "m",
                },
                figures: FigureView {
                    tokens: Some(2500),
                    session_cost: 0.0,
                    context_budget: Some(10_000),
                },
                approval_mode: ApprovalMode::Default,
            },
        );
        assert_eq!(
            with_budget
                .right
                .iter()
                .find(|i| matches!(i, FooterItem::Context { .. })),
            Some(&FooterItem::Context {
                label: "25.0% context used".into(),
                over_limit: false,
            })
        );

        let no_budget = footer(
            120,
            FooterView {
                conn: ConnectionView {
                    base_url: "u",
                    model: "m",
                },
                figures: tokens_only(2500),
                approval_mode: ApprovalMode::Default,
            },
        );
        assert!(
            !no_budget
                .right
                .iter()
                .any(|i| matches!(i, FooterItem::Context { .. }))
        );
    }

    #[test]
    fn an_over_budget_context_item_flags_over_limit() {
        let bar = footer(
            120,
            FooterView {
                conn: ConnectionView {
                    base_url: "u",
                    model: "m",
                },
                figures: FigureView {
                    tokens: Some(15_000),
                    session_cost: 0.0,
                    context_budget: Some(10_000),
                },
                approval_mode: ApprovalMode::Default,
            },
        );
        assert_eq!(
            bar.right
                .iter()
                .find(|i| matches!(i, FooterItem::Context { .. })),
            Some(&FooterItem::Context {
                label: ">100% context used".into(),
                over_limit: true,
            })
        );
    }

    #[test]
    fn a_footer_item_cells_match_its_text() {
        let item = FooterItem::Model {
            model: "qwen3-coder".into(),
        };
        assert_eq!(item.cells(), item.text().chars().count());
        assert_eq!(item.text(), "model qwen3-coder");
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
        // The `>` caret prefix is baked into the cached User line (ADR-0046 qwen
        // chrome), so the 16-char word hangs under the 2-col prefix and wraps at
        // the reduced width (10 - 2 = 8) to 2 rows.
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
    fn cache_sync_rebuilds_when_compact_hides_a_thought() {
        // Compact mode (Ctrl+O) HIDES a settled Thinking item entirely (qwen
        // `!compactMode`, ADR-0052); the default shows the full grey body.
        let mut t = fresh_transcript();
        t.push(TranscriptItem::Thinking {
            text: "line one\nline two".to_string(),
        });
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        // Default (compact=false): the grey `✦`-prefixed markdown body (two
        // source rows → two rows).
        assert_eq!(cache.settled().next().unwrap().0.len(), 2);
        cache.sync(&t, Toggles { compact: true }, 80, theme::dark());
        // Compact: the thought is hidden entirely - zero lines.
        assert_eq!(cache.settled().next().unwrap().0.len(), 0);
    }

    #[test]
    fn cache_sync_rebuilds_when_compact_folds_a_tool_body() {
        // Compact folds a multi-line Diff to a single title line (qwen
        // `!compactMode || forceShowResult`); the default shows the full body.
        // Flipping the toggle clears the cache so the change takes effect.
        // Separators are added at assembly, not baked per item.
        let mut t = fresh_transcript();
        t.push(diff_item(
            "edit src/foo.rs",
            vec![
                DiffLine::new(DiffSide::Added, "added line"),
                DiffLine::new(DiffSide::Removed, "removed line"),
            ],
        ));
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles { compact: true }, 80, theme::dark());
        // Compact: one fold row (3-wide marker gutter + title + affordance).
        let collapsed = cache.settled().next().unwrap().0;
        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            line_text(&collapsed[0]).trim_start(),
            "edit src/foo.rs · ^O expand"
        );
        // The default (compact=false) shows the full body.
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        // The tool header row + both diff body rows.
        let expanded = cache.settled().next().unwrap().0;
        assert_eq!(expanded.len(), 3);
        assert!(line_text(&expanded[0]).contains("edit src/foo.rs"));
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
        // recolors `muted`, the settled thought (grey = muted) carries the new
        // color. A Marker's `●` prefix reads its tone slot (Plain = muted).
        let mut t = fresh_transcript();
        t.marker("a marker", Tone::Plain);
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), 80, theme::dark());
        assert_eq!(settled_span_fgs(&cache)[1], Some(Color::DarkGray));

        let recolored = themed("[colors]\nmuted = \"#ff00ff\"\n");
        cache.sync(&t, Toggles::default(), 80, &recolored);
        assert_eq!(settled_span_fgs(&cache)[1], Some(Color::Rgb(255, 0, 255)));
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
        // The INNER box content (qwen `ToolInfo`): the 3-wide `✓` marker gutter,
        // the bold name, then the dim `arg · result` description.
        let lines = message_lines(&item, false, 80, theme::dark());
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "✓  read_file src/foo.rs · 340 lines");
    }

    #[test]
    fn an_unpaired_result_shows_only_the_summary() {
        // No key_arg (governor-injected result): the description is the bare
        // summary (no arg to set off).
        let item = TranscriptItem::ToolResult {
            name: "run_shell_command".to_string(),
            summary: "injected".to_string(),
            is_error: false,
            key_arg: None,
        };
        let lines = message_lines(&item, false, 80, theme::dark());
        assert_eq!(line_text(&lines[0]), "✓  run_shell_command injected");
    }

    // --- The startup Header banner (qwen `AppHeader`) ----------------------

    // Tier 1 (side-by-side): at a WIDE content width (>= 129) the 83-col logo
    // draws to the LEFT of the bordered info panel, and the last row is the
    // `Tips:` line. Every drawn row stays within the content width (measure==draw).
    #[test]
    fn a_header_at_a_wide_width_shows_the_logo_beside_the_boxed_panel() {
        let item = TranscriptItem::Header {
            title: "suspenders".into(),
            version: "1.2.3".into(),
            model: "openrouter/qwen3-coder".into(),
            cwd: "/tmp/proj".into(),
            tip: "Type / to see all available commands.".into(),
        };
        let width = 140;
        let lines = message_lines(&item, false, width, theme::dark());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        // The logo drew (its first row's block glyphs are present) beside the box.
        assert!(text.contains("███████╗"), "the logo drew:\n{text}");
        assert!(
            text.contains("╭") && text.contains("╮"),
            "the box drew:\n{text}"
        );
        assert!(text.contains(">_ suspenders"), "the brand title:\n{text}");
        assert!(text.contains("(v1.2.3)"), "the version:\n{text}");
        assert!(
            text.contains("openrouter/qwen3-coder"),
            "the scoped model:\n{text}"
        );
        assert!(
            text.contains("(/model to change)"),
            "the model hint fits at this width:\n{text}"
        );
        // The Tips line is the final row.
        assert_eq!(
            line_text(lines.last().unwrap()),
            "Tips: Type / to see all available commands."
        );
        // No row soft-wraps: each is within the content width (measure==draw).
        for row in &lines {
            assert!(
                row_display_width(row) <= width as usize,
                "row exceeds content width: {:?}",
                line_text(row)
            );
        }
    }

    // The WIDTH GATE (qwen `showLogo`): a NARROW content width cannot fit the
    // 83-col logo + gap + a minimum info panel, so the logo is hidden and the
    // panel (+ tips) render alone. The block glyphs never appear.
    #[test]
    fn a_header_at_a_narrow_width_hides_the_logo() {
        let item = TranscriptItem::Header {
            title: "suspenders".into(),
            version: "1.2.3".into(),
            model: "openrouter/qwen3-coder".into(),
            cwd: "/tmp/proj".into(),
            tip: "Type / to see all available commands.".into(),
        };
        let width = 50;
        let lines = message_lines(&item, false, width, theme::dark());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !text.contains("███████╗"),
            "the logo is hidden on a narrow terminal:\n{text}"
        );
        // The bordered panel + brand + tips still render.
        assert!(text.contains(">_ suspenders"), "the brand title:\n{text}");
        assert!(text.contains("╭"), "the box top border:\n{text}");
        assert!(text.contains("Tips:"), "the tips line:\n{text}");
        for row in &lines {
            assert!(
                row_display_width(row) <= width as usize,
                "row exceeds content width: {:?}",
                line_text(row)
            );
        }
    }

    // Tier 2 (stacked): a MID content width (83 <= W < 129) that fits the logo but
    // not the side-by-side panel draws the full-width logo banner ON TOP, then the
    // bordered info panel BELOW it. Both the block glyphs AND the box appear, and
    // no row exceeds the content width (measure==draw).
    #[test]
    fn a_header_at_a_mid_width_stacks_the_logo_above_the_boxed_panel() {
        let item = TranscriptItem::Header {
            title: "suspenders".into(),
            version: "1.2.3".into(),
            model: "openrouter/qwen3-coder".into(),
            cwd: "/tmp/proj".into(),
            tip: "Type / to see all available commands.".into(),
        };
        let width = 100;
        let lines = message_lines(&item, false, width, theme::dark());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        // The logo banner AND the box both drew.
        assert!(text.contains("███████╗"), "the logo banner drew:\n{text}");
        assert!(
            text.contains("╭") && text.contains("╮"),
            "the box drew below the logo:\n{text}"
        );
        assert!(text.contains(">_ suspenders"), "the brand title:\n{text}");
        assert!(text.contains("Tips:"), "the tips line:\n{text}");
        // The logo is a full-width TOP banner: the first 6 rows are pure logo (no
        // box border yet), then the box begins. Row 0 is a logo row, and the box
        // top border appears only AFTER the 6 logo rows.
        assert!(
            line_text(&lines[0]).contains("███████╗"),
            "row 0 is the logo banner top:\n{text}"
        );
        let box_top = lines
            .iter()
            .position(|l| line_text(l).contains('╭'))
            .expect("the box top border row");
        assert!(
            box_top >= 6,
            "the box begins after the 6 logo rows (stacked), got row {box_top}"
        );
        // No row soft-wraps: each is within the content width (measure==draw).
        for row in &lines {
            assert!(
                row_display_width(row) <= width as usize,
                "row exceeds content width: {:?}",
                line_text(row)
            );
        }
    }

    // The tier gate is the ONE place the boundary math lives: side-by-side at
    // >= 129, stacked at 83..129, no-logo below 83.
    #[test]
    fn header_tier_boundaries_are_exact() {
        assert_eq!(header_tier(129), HeaderTier::SideBySide);
        assert_eq!(header_tier(128), HeaderTier::Stacked);
        assert_eq!(header_tier(83), HeaderTier::Stacked);
        assert_eq!(header_tier(82), HeaderTier::NoLogo);
        assert_eq!(header_tier(0), HeaderTier::NoLogo);
    }

    // Measure==draw (ADR-0029): every Header row is `<= content width` across
    // ALL three tiers (NoLogo, Stacked, SideBySide), so the startup banner never
    // soft-wraps a row - the same width-sweep guard the Help panel carries.
    #[test]
    fn header_rows_never_exceed_the_content_width() {
        let item = TranscriptItem::Header {
            title: "suspenders".into(),
            version: "1.2.3".into(),
            model: "openrouter/qwen3-coder".into(),
            cwd: "/home/vinnie/Projects/suspenders".into(),
            tip: "Use @path/to/file to add files as context.".into(),
        };
        for width in [40u16, 60, 82, 83, 100, 128, 129, 160, 220] {
            let lines = message_lines(&item, false, width, theme::dark());
            for row in &lines {
                assert!(
                    row_display_width(row) <= width as usize,
                    "row exceeds width {width}: {:?}",
                    line_text(row)
                );
            }
        }
    }

    // The model hint is dropped when the scoped id + ` (/model to change)` would
    // overflow the info panel's inner width (qwen `showModelHint`), so the panel
    // never soft-wraps.
    #[test]
    fn a_header_drops_the_model_hint_when_it_would_not_fit() {
        let item = TranscriptItem::Header {
            title: "suspenders".into(),
            version: "1.2.3".into(),
            model: "some-very-long-provider/a-very-long-model-identifier-name".into(),
            cwd: "/tmp/proj".into(),
            tip: "tip".into(),
        };
        // A width wide enough for the panel but not the model + hint together.
        let width = 50;
        let lines = message_lines(&item, false, width, theme::dark());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            !text.contains("(/model to change)"),
            "the hint is dropped when it would overflow:\n{text}"
        );
    }

    // tildeify_with_home abbreviates a leading home to `~`, and leaves other
    // paths (and the no-home case) untouched. Pure - no process env is touched.
    #[test]
    fn tildeify_abbreviates_the_home_prefix() {
        let home = Some("/home/dev");
        assert_eq!(tildeify_with_home("/home/dev/proj/src", home), "~/proj/src");
        assert_eq!(tildeify_with_home("/home/dev", home), "~");
        assert_eq!(tildeify_with_home("/etc/hosts", home), "/etc/hosts");
        // A path merely PREFIXED by the home string but not on a path boundary is
        // left alone (no `~/develop` from `/home/develop`).
        assert_eq!(tildeify_with_home("/home/develop", home), "/home/develop");
        // No home / empty home: pass through.
        assert_eq!(tildeify_with_home("/home/dev/x", None), "/home/dev/x");
        assert_eq!(tildeify_with_home("/home/dev/x", Some("")), "/home/dev/x");
    }

    #[test]
    fn a_failing_result_shows_the_error_marker() {
        // A failed result reads the `x` U+0078 ERROR marker (0.16.0 ASCII, NOT
        // `✗`), then the name + `arg · summary` description.
        let item = TranscriptItem::ToolResult {
            name: "run_shell_command".to_string(),
            summary: "exit 1".to_string(),
            is_error: true,
            key_arg: Some("cargo test".to_string()),
        };
        let lines = message_lines(&item, false, 80, theme::dark());
        assert_eq!(
            line_text(&lines[0]),
            "x  run_shell_command cargo test · exit 1"
        );
    }

    #[test]
    fn a_failing_result_without_an_arg_shows_the_bare_summary() {
        let item = TranscriptItem::ToolResult {
            name: "edit".to_string(),
            summary: "old_str not found".to_string(),
            is_error: true,
            key_arg: Some("src/foo.rs".to_string()),
        };
        let lines = message_lines(&item, false, 80, theme::dark());
        assert_eq!(
            line_text(&lines[0]),
            "x  edit src/foo.rs · old_str not found"
        );
    }

    // A Marker's prefix glyph + tint are chosen by its Tone (qwen StatusMessages
    // set): a Constrain marker reads the `△` warning, everything else the `●` info
    // glyph. Identical text under two tones tints differently, proving the adapter
    // never sniffs the line.
    #[test]
    fn a_marker_tints_by_its_tone_not_its_text() {
        let theme = theme::dark();
        for (tone, glyph, expected) in [
            (Tone::Housekeeping, "● ", theme.muted),
            (Tone::Aid, "● ", theme.muted),
            (Tone::Constrain, "△ ", theme.warning),
            (Tone::Steering, "● ", theme.accent),
            (Tone::Plain, "● ", theme.muted),
        ] {
            let item = TranscriptItem::Marker {
                // Same text for every tone: the tint cannot be coming from it.
                text: "harness marker".to_string(),
                tone,
            };
            let lines = message_lines(&item, false, 80, theme);
            assert_eq!(lines.len(), 1);
            assert_eq!(
                line_text(&lines[0]),
                format!("{glyph}harness marker"),
                "{tone:?}"
            );
            // The prefix span carries the tone color.
            assert_eq!(
                lines[0].spans[0].style.fg,
                Some(tui_color(expected)),
                "{tone:?}"
            );
        }
    }

    #[test]
    fn has_foldable_body_is_true_only_for_a_non_empty_diff() {
        // A non-empty Diff folds under Ctrl-O.
        let diff = diff_item("edit x", vec![DiffLine::new(DiffSide::Added, "a")]);
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
            "edit src/foo.rs (+1 -1)",
            vec![
                DiffLine::new(DiffSide::Added, "new"),
                DiffLine::new(DiffSide::Removed, "old"),
            ],
        );
        // Compact (folds the body): one fold row (3-wide marker gutter + the
        // title and the `· ^O expand` affordance).
        let collapsed = message_lines(&diff, true, 80, theme::dark());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(
            line_text(&collapsed[0]).trim_start(),
            "edit src/foo.rs (+1 -1) · ^O expand"
        );
        // Default (compact=false): the tool header row + both body rows.
        let expanded = message_lines(&diff, false, 80, theme::dark());
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
            title: "edit foo".to_string(),
            lang: lang.map(str::to_string),
            hunks: vec![DiffHunk {
                header: header.map(str::to_string),
                lines,
            }],
            elided: 0,
        }
    }

    // The rendered diff code rows for `item` (bypassing the tool box): the
    // line-number-gutter + marker + code rows [`diff_lines`] produces, the direct
    // seam these diff-internals tests inspect (the box only adds an outer 3-col
    // indent). Panics on a non-Diff item.
    fn diff_rows_of(item: &TranscriptItem, width: u16) -> Vec<Line<'static>> {
        match item {
            TranscriptItem::Diff { lang, hunks, .. } => {
                diff_lines(lang.as_deref(), hunks, width, theme::dark())
            }
            _ => panic!("not a Diff item"),
        }
    }

    // A created-file Diff (one all-added hunk, `header: None`) of `content` in
    // `lang`, rendered to its code rows.
    fn created_diff_rows(lang: &str, content: &[&str], width: u16) -> Vec<Line<'static>> {
        let lines = content
            .iter()
            .map(|t| DiffLine::new(DiffSide::Added, *t))
            .collect();
        let item = diff_of(Some(lang), None, lines);
        diff_rows_of(&item, width)
    }

    // The distinct syntect foregrounds of a diff code row: the fgs AFTER the
    // line-number gutter + marker glyph, dropping the trailing full-width pad (bg
    // only, no fg). Used to compare the color a line's code was highlighted with.
    fn code_fgs(row: &Line<'static>) -> Vec<Color> {
        row.spans
            .iter()
            .skip(2) // the line-number gutter + the marker glyph
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
        // The 4 comment lines are rows[0..=3]; row[4] is the trailing code line.
        let comment_fg = code_fgs(&rows[0]);
        assert!(
            comment_fg.iter().all(|c| matches!(c, Color::Rgb(..))),
            "the comment's first line is syntect-colored: {comment_fg:?}"
        );
        let first = comment_fg[0];
        for row in &rows[0..=3] {
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
        let code_fg = code_fgs(&rows[4]);
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
        let rows = diff_rows_of(&item, 80);
        // rows: removed, added (the `@@` header is parsed for line numbers, not
        // drawn as a row).
        let removed = &rows[0];
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
        let row = &rows[0];
        let added_bg = Some(tui_color(theme::dark().added_bg));
        // The marker glyph (after the line-number gutter) carries the tint and the
        // semantic (green) fg.
        assert_eq!(row.spans[1].content.as_ref(), "+ ");
        assert_eq!(row.spans[1].style.bg, added_bg);
        assert_eq!(row.spans[1].style.fg, Some(tui_color(theme::dark().added)));
        // Every span (gutter, marker, code, pad) carries the tint (band-wide).
        for span in &row.spans {
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
        let rows = diff_rows_of(&item, 40);
        let ctx = &rows[0];
        // The context marker (after the gutter) is two blanks and NO span carries
        // a background.
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
        let rows = diff_rows_of(&item, 40);
        let row = &rows[0];
        // The code span (after gutter + marker) carries the semantic added fg
        // (green), not a syntect Rgb.
        let code = &row.spans[2];
        assert_eq!(code.content.as_ref(), "just text");
        assert_eq!(code.style.fg, Some(tui_color(theme::dark().added)));
    }

    #[test]
    fn the_elided_tail_renders_as_a_muted_count() {
        let tail_rows = diff_elided_tail(40, 40, theme::dark());
        let tail = &tail_rows[0];
        assert_eq!(line_text(tail).trim_end(), "... last 40 lines hidden ...");
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
        let rows = diff_rows_of(&item, 80);
        // rows: the 6 code rows in file order (no title/header row).
        let code = &rows[..];
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
        let rows = diff_rows_of(&item, 80);
        let removed_bg = Some(tui_color(theme::dark().removed_bg));
        for row in &rows[..] {
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
        // A tab becomes two spaces; the common leading indentation (here the whole
        // tab) is then stripped per-hunk (qwen DiffRenderer), so the code reads at
        // the box edge. The key invariant: no raw tab survives the render path.
        let rows = created_diff_rows("rs", &["\tlet x = 1;"], 80);
        let row = &rows[0];
        let text = line_text(row);
        assert!(
            text.starts_with("1 + let x = 1;"),
            "tab expanded + common indent stripped: {text:?}"
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
        let row = &rows[0];
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
            let row = &rows[0];
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

    #[test]
    fn parse_hunk_header_reads_the_old_and_new_start_line_numbers() {
        // The MEDIUM-risk data path (Phase 2 risk #2): the `@@ -old,_ +new,_ @@`
        // header is parsed render-side for the two 1-based start numbers.
        assert_eq!(parse_hunk_header(Some("@@ -12,4 +30,5 @@")), (12, 30));
        // A single-line hunk omits the count (`@@ -a +b @@`); the starts still parse.
        assert_eq!(parse_hunk_header(Some("@@ -7 +9 @@")), (7, 9));
        // A trailing section label after the second `@@` does not confuse the
        // parse: the STARTS are `-1`/`+1` (the `,3`/`,4` are line counts, not starts).
        assert_eq!(
            parse_hunk_header(Some("@@ -1,3 +1,4 @@ fn main() {")),
            (1, 1)
        );
        // Distinct non-1 starts survive a trailing label too.
        assert_eq!(
            parse_hunk_header(Some("@@ -40,3 +52,4 @@ impl Foo {")),
            (40, 52)
        );
        // A created file carries no header; both sides start at line 1.
        assert_eq!(parse_hunk_header(None), (1, 1));
        // A malformed header falls back to (1, 1) rather than panicking.
        assert_eq!(parse_hunk_header(Some("not a header")), (1, 1));
    }

    #[test]
    fn hunk_line_numbers_advances_each_side_by_line_kind() {
        // The exact per-row gutter numbers a mixed hunk draws (qwen DiffRenderer
        // :279-301): a context row shows its NEW number and advances BOTH sides; an
        // added row shows NEW and advances new only; a removed row shows OLD and
        // advances old only. Header `@@ -10,_ +20,_ @@` starts old at 10, new at 20.
        let hunk = DiffHunk {
            header: Some("@@ -10,3 +20,4 @@".to_string()),
            lines: vec![
                DiffLine::new(DiffSide::Context, "ctx a"), // old 10 / new 20 -> shows 20
                DiffLine::new(DiffSide::Removed, "gone"),  // old 11          -> shows 11
                DiffLine::new(DiffSide::Added, "new one"), // new 21          -> shows 21
                DiffLine::new(DiffSide::Added, "new two"), // new 22          -> shows 22
                DiffLine::new(DiffSide::Context, "ctx b"), // old 12 / new 23 -> shows 23
            ],
        };
        // Context: 20 (new). Removed: 11 (old, new stays 21). Added: 21, 22 (new).
        // Context: 23 (new; old is now 12).
        assert_eq!(hunk_line_numbers(&hunk), vec![20, 11, 21, 22, 23]);
    }

    // (The Ctrl-O viewport-stability test is retired: the app now owns the whole
    // fullscreen viewport and redraws the transcript from the model each frame, so
    // there is no frozen scrollback prefix to keep stable. Ctrl-O's effect on the
    // cached line counts is still covered by the cache toggle tests.)

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
                .map(|item| wrapped_count(message_lines(item, false, width, theme::dark()), width))
                .sum();
            let whole: Vec<Line> = items
                .iter()
                .flat_map(|item| message_lines(item, false, width, theme::dark()))
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
    use ratatui::buffer::Buffer;
    use ratatui::widgets::Widget;

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

    /// A whole [`Buffer`] as newline-joined rows of symbols (the `Buffer`
    /// counterpart of [`buffer_text`], used by the assembly tests that render
    /// `grouped_rows` into a bare buffer).
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

    // The first char of a test-buffer cell (for gutter/marker assertions).
    fn cell_char(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> char {
        terminal
            .backend()
            .buffer()
            .cell((x, y))
            .expect("cell in test buffer")
            .symbol()
            .chars()
            .next()
            .unwrap_or(' ')
    }

    // The foreground color of a test-buffer cell (for accent/secondary checks).
    fn cell_fg(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> Option<Color> {
        terminal
            .backend()
            .buffer()
            .cell((x, y))
            .expect("cell in test buffer")
            .style()
            .fg
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
        let content_width = width.saturating_sub(2 * CONTENT_MARGIN);
        cache.sync(
            screen.transcript(),
            Toggles {
                compact: screen.compact_mode,
            },
            content_width,
            theme,
        );
        let items = screen.transcript().items();
        // hw = 0: measure the WHOLE settled transcript (the test helper draws it
        // all top-aligned) through the SAME grouped fold the body draws (ADR-0046),
        // including the inline approval block when one is open (ADR-0049).
        let approving = screen.pending_approval.as_ref().and_then(|pending| {
            newest_live_tool_index(items).map(|call_index| Approving {
                pending,
                call_index,
            })
        });
        let mut lines = grouped_rows_with_approval(&GroupedRows {
            cache,
            items,
            hw: 0,
            width: content_width,
            theme,
            approving: approving.as_ref(),
        });
        // Add the live stream rows the body would append. Compact suppresses the
        // live thinking tail (matching `render_pending_body_at`).
        let thinking = screen.transcript().streaming_thinking();
        let thinking_lines = if screen.compact_mode {
            Vec::new()
        } else {
            live_thinking_lines(&thinking, 0, content_width, theme)
        };
        append_live(&mut lines, &thinking_lines);
        if let Some((tail, _)) = cache.streaming_tail() {
            append_live(&mut lines, tail);
        }
        wrapped_count(lines, content_width)
    }

    // --- tool-group box (ADR-0047): grouping fold + border rigidity ----------

    /// A store carrying an assistant line, a two-tool run (a call + a result),
    /// then another assistant line - the shape the grouping fold boxes in the
    /// middle only.
    fn store_with_a_tool_run() -> crate::ui::transcript::Transcript {
        let mut t = crate::ui::transcript::Transcript::new(Vec::new());
        t.user("do it");
        t.push(TranscriptItem::Assistant {
            text: "on it".into(),
        });
        t.push(TranscriptItem::ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            summary: "src/foo.rs".into(),
        });
        t.push(TranscriptItem::ToolResult {
            name: "read_file".into(),
            summary: "340 lines".into(),
            is_error: false,
            key_arg: Some("src/foo.rs".into()),
        });
        t.push(TranscriptItem::Assistant {
            text: "done".into(),
        });
        t
    }

    #[test]
    fn a_contiguous_tool_run_renders_as_one_rounded_box() {
        // The grouping fold (ADR-0047): the two tool items between the assistant
        // lines are ONE box - a single top border, a single bottom border.
        let t = store_with_a_tool_run();
        let items: Vec<TranscriptItem> = t.items().to_vec();
        let width: u16 = 60;
        let content_width = width - 2 * CONTENT_MARGIN;
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), content_width, theme::dark());
        let lines = grouped_rows(&cache, &items, 0, content_width, theme::dark());
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            text.iter().filter(|r| r.starts_with('╭')).count(),
            1,
            "exactly one box top: {text:?}"
        );
        assert_eq!(
            text.iter().filter(|r| r.starts_with('╰')).count(),
            1,
            "exactly one box bottom: {text:?}"
        );
        // The two tool rows are inside the border (start with `│`), the assistant
        // lines are not.
        assert!(
            text.iter()
                .any(|r| r.contains("read_file") && r.starts_with('│'))
        );
        assert!(text.iter().any(|r| r.starts_with("✦ done")));
    }

    // --- Todo render (ADR-0048, the committed defect fix) --------------------

    fn todo(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
        }
    }

    fn todo_item(items: Vec<TodoItem>) -> TranscriptItem {
        TranscriptItem::Todo { items }
    }

    // The span carrying `needle` (used to assert its style).
    fn span_with<'a>(line: &'a Line<'static>, needle: &str) -> &'a Span<'static> {
        line.spans
            .iter()
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| panic!("no span contains {needle:?} in {:?}", line_text(line)))
    }

    #[test]
    fn tool_todo_lines_draws_a_clean_header_then_the_circle_list() {
        use TodoStatus::{Completed, InProgress, Pending};
        let items = vec![
            todo("read the file", Completed),
            todo("edit the file", InProgress),
            todo("build", Pending),
        ];
        let lines = tool_todo_lines(&items, false, 40, theme::dark());
        let text: Vec<String> = lines.iter().map(line_text).collect();

        // A clean `✓ todo_write` header with NO raw JSON args (the key_arg is
        // gone structurally - a Todo carries no summary).
        assert_eq!(text[0], "✓  todo_write");
        assert!(
            !text.iter().any(|r| r.contains('{') || r.contains("status")),
            "no raw JSON args leak: {text:?}"
        );
        // The circle glyphs, in order, one row per item.
        assert!(text[1].starts_with("● "), "completed circle: {:?}", text[1]);
        assert!(text[1].contains("read the file"));
        assert!(
            text[2].starts_with("◐ "),
            "in_progress circle: {:?}",
            text[2]
        );
        assert!(text[3].starts_with("○ "), "pending circle: {:?}", text[3]);
    }

    #[test]
    fn tool_todo_lines_folds_to_header_only_under_compact() {
        // Compact (Ctrl+O) folds the checklist body away (qwen `!compactMode`,
        // ADR-0052): the header stays, the circle rows are gone. This pins the
        // one display-hide branch of `tool_todo_lines` (compact=true), where the
        // non-compact tests above exercise compact=false's full list.
        use TodoStatus::{Completed, InProgress, Pending};
        let items = vec![
            todo("read the file", Completed),
            todo("edit the file", InProgress),
            todo("build", Pending),
        ];
        let lines = tool_todo_lines(&items, true, 40, theme::dark());
        assert_eq!(lines.len(), 1, "only the header row survives under compact");
        assert_eq!(line_text(&lines[0]), "✓  todo_write");
    }

    #[test]
    fn tool_todo_lines_colours_in_progress_green_and_strikes_completed() {
        use TodoStatus::{Completed, InProgress, Pending};
        let items = vec![
            todo("done item", Completed),
            todo("active item", InProgress),
            todo("later item", Pending),
        ];
        let lines = tool_todo_lines(&items, false, 40, theme::dark());

        // in_progress reads success (green); completed is CROSSED_OUT and NOT
        // green (qwen colours completed Foreground); pending is plain.
        let done = span_with(&lines[1], "done item");
        assert!(
            done.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "completed is struck through"
        );
        assert_eq!(
            done.style.fg,
            primary_style(theme::dark()).fg,
            "completed is Foreground, not green"
        );

        let active = span_with(&lines[2], "active item");
        assert_eq!(active.style.fg, success_style(theme::dark()).fg);
        assert!(!active.style.add_modifier.contains(Modifier::CROSSED_OUT));

        let later = span_with(&lines[3], "later item");
        assert_eq!(later.style.fg, primary_style(theme::dark()).fg);
        assert!(!later.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn tool_todo_lines_wraps_long_content_and_every_row_fits_the_inner_width() {
        // Long content word-wraps under the 3-wide gutter; no produced row
        // exceeds the inner width (measure==draw, ADR-0029).
        let inner: u16 = 24;
        let items = vec![todo(
            "a rather long todo item that must wrap onto several rows",
            TodoStatus::Pending,
        )];
        let lines = tool_todo_lines(&items, false, inner, theme::dark());
        assert!(lines.len() > 2, "the long item wrapped: {}", lines.len());
        for line in &lines {
            assert!(
                line_text(line).width() <= inner as usize,
                "row exceeds inner width: {:?}",
                line_text(line)
            );
        }
    }

    // A Todo item renders as a bordered circle list through the SAME grouped fold
    // the fullscreen body draws (ADR-0048): one rounded box, the clean header, and
    // the three status glyphs. The prefix line ahead of it proves the box lands
    // below settled content, not just at row 0.
    #[test]
    fn a_todo_renders_as_a_bordered_circle_list() {
        use TodoStatus::{Completed, InProgress, Pending};
        let mut t = crate::ui::transcript::Transcript::new(Vec::new());
        t.info("a prefix line");
        t.push(todo_item(vec![
            todo("read", Completed),
            todo("edit", InProgress),
            todo("ship", Pending),
        ]));
        let items: Vec<TranscriptItem> = t.items().to_vec();
        let width: u16 = 50;
        let height: u16 = 12;
        let content_width = width - 2 * CONTENT_MARGIN;
        let mut cache = RenderCache::new();
        cache.sync(&t, Toggles::default(), content_width, theme::dark());

        // Draw the WHOLE transcript (info + separator + Todo box) as the fullscreen
        // body does: grouped_rows from hw=0 + the margin-inset Paragraph.
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        let lines = grouped_rows(&cache, &items, 0, content_width, theme::dark());
        let content_area = Rect {
            x: CONTENT_MARGIN,
            y: 0,
            width: content_width,
            height,
        };
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(content_area, &mut buf);

        let text = commit_buffer_text(&buf);
        assert_eq!(text.matches('╭').count(), 1, "one box:\n{text}");
        assert!(text.contains("todo_write"), "clean header:\n{text}");
        assert!(text.contains("●  read"), "completed glyph:\n{text}");
        assert!(text.contains("◐  edit"), "in_progress glyph:\n{text}");
        assert!(text.contains("○  ship"), "pending glyph:\n{text}");
    }

    // --- sticky "Current tasks" box (ADR-0048) -------------------------------

    #[test]
    fn ordered_sticky_todos_sorts_by_priority_stable_and_keeps_original_index() {
        use TodoStatus::{Completed, InProgress, Pending};
        let items = vec![
            todo("a-completed", Completed),   // 0
            todo("b-pending", Pending),       // 1
            todo("c-inprogress", InProgress), // 2
            todo("d-pending", Pending),       // 3
        ];
        let ordered = ordered_sticky_todos(&items);
        let seq: Vec<(usize, &str)> = ordered
            .iter()
            .map(|(i, item)| (*i, item.content.as_str()))
            .collect();
        // in_progress first, then pending (stable: index 1 before 3), then completed.
        assert_eq!(
            seq,
            vec![
                (2, "c-inprogress"),
                (1, "b-pending"),
                (3, "d-pending"),
                (0, "a-completed"),
            ]
        );
    }

    #[test]
    fn sticky_todos_shows_only_a_non_tail_non_empty_incomplete_list() {
        use TodoStatus::{Completed, InProgress, Pending};
        let items = vec![todo("read", InProgress), todo("edit", Pending)];

        // Non-empty, incomplete, NOT the tail (index 2 of 4 items): shows.
        assert_eq!(sticky_todos(Some((2, &items)), 4), Some(items.as_slice()));

        // IS the tail (index 3 is the last of 4 items): the inline copy is the
        // active tail on screen, so the sticky box defers.
        assert_eq!(sticky_todos(Some((3, &items)), 4), None);

        // No todo at all: nothing.
        assert_eq!(sticky_todos(None, 4), None);

        // Empty list: nothing.
        let empty: Vec<TodoItem> = vec![];
        assert_eq!(sticky_todos(Some((0, &empty)), 4), None);

        // All completed: the run is done, so the box hides.
        let done = vec![todo("read", Completed), todo("edit", Completed)];
        assert_eq!(sticky_todos(Some((0, &done)), 4), None);
    }

    #[test]
    fn sticky_todos_height_caps_at_five_and_adds_an_overflow_row() {
        // borders(2) + header(1) + visible + overflow?1.
        assert_eq!(sticky_todos_height(0), 3);
        assert_eq!(sticky_todos_height(3), 6);
        assert_eq!(sticky_todos_height(5), 8);
        // Six items: five shown + one overflow row.
        assert_eq!(sticky_todos_height(6), 9);
    }

    #[test]
    fn render_sticky_todos_draws_the_header_glyphs_and_overflow_and_fits_width() {
        use TodoStatus::{Completed, InProgress, Pending};
        let items = vec![
            todo("one", InProgress),
            todo("two", Pending),
            todo("three", Pending),
            todo("four", Pending),
            todo("five", Pending),
            todo("six", Completed),
        ];
        let width: u16 = 40;
        let height = sticky_todos_height(items.len()) as u16;
        let terminal = draw_frame(width, height, |f| {
            render_sticky_todos(f, Rect::new(0, 0, width, height), &items, theme::dark());
        });
        let text = buffer_text(&terminal);

        assert!(text.contains("Current tasks"), "header present:\n{text}");
        assert!(text.contains("◐"), "in_progress glyph present:\n{text}");
        assert!(text.contains("○"), "pending glyph present:\n{text}");
        // Six items > cap 5, so exactly one item is hidden with the overflow row.
        assert!(text.contains("... and 1 more"), "overflow row:\n{text}");
        // The rounded box corners are drawn.
        assert!(
            text.contains('╭') && text.contains('╰'),
            "box borders:\n{text}"
        );
        // Every drawn row is exactly `width` cells (the box never overflows -
        // measure==draw, ADR-0029): the TestBackend rows are width-padded, so
        // asserting the buffer drew without panicking on an oversize line suffices
        // here; the box_row funnel guarantees the width.
    }

    #[test]
    fn sticky_fits_drops_the_box_when_the_frame_cannot_hold_it_plus_composer() {
        // sticky(6) + status(1) + composer(3) + body(1) = 11.
        assert!(sticky_fits(11, 6, 3));
        assert!(sticky_fits(20, 6, 3));
        // One row short: no room for the body -> hide.
        assert!(!sticky_fits(10, 6, 3));
        // A very short frame with any composer hides the box.
        assert!(!sticky_fits(4, 6, 3));
    }

    #[test]
    fn render_sticky_todos_clamps_its_lines_to_a_squeezed_zone_without_panicking() {
        use TodoStatus::{InProgress, Pending};
        let items = vec![
            todo("one", InProgress),
            todo("two", Pending),
            todo("three", Pending),
        ];
        let full = sticky_todos_height(items.len()) as u16; // 6
        let width: u16 = 30;
        // Draw into a zone SHORTER than the measured box height: the clamp keeps
        // the draw within the zone (no over-draw, no panic).
        let squeezed = full - 3;
        let terminal = draw_frame(width, squeezed, |f| {
            render_sticky_todos(f, Rect::new(0, 0, width, squeezed), &items, theme::dark());
        });
        let text = buffer_text(&terminal);
        // The top of the box still draws; the truncated tail simply drops.
        assert!(text.contains("Current tasks"), "header present:\n{text}");
        assert_eq!(
            text.lines().count(),
            squeezed as usize,
            "drew exactly the zone height, no over-draw:\n{text}"
        );
    }

    // --- group_segments: the pure boundary fold (ADR-0047) -------------------

    // The item constructors the pure `group_segments` tests route through, so the
    // `TranscriptItem` literals stay in one place. No frame, no cache.
    fn assistant(text: &str) -> TranscriptItem {
        TranscriptItem::Assistant { text: text.into() }
    }
    fn tool_call(name: &str) -> TranscriptItem {
        TranscriptItem::ToolCall {
            id: "id".into(),
            name: name.into(),
            summary: "arg".into(),
        }
    }
    fn tool_result(name: &str) -> TranscriptItem {
        TranscriptItem::ToolResult {
            name: name.into(),
            summary: "ok".into(),
            is_error: false,
            key_arg: None,
        }
    }
    fn info(text: &str) -> TranscriptItem {
        TranscriptItem::Info { text: text.into() }
    }

    #[test]
    fn group_segments_boxes_maximal_tool_runs_between_prose() {
        // A single lone tool item is one group; the surrounding prose passes
        // through. A run at the START (index 0) and one at the very END both box.
        let items = [
            tool_call("read_file"),           // 0  (run at slice start)
            assistant("prose"),               // 1
            tool_call("run_shell_command"),   // 2  (run at the very end)
            tool_result("run_shell_command"), // 3
        ];
        assert_eq!(
            group_segments(&items, 0),
            vec![
                Segment::ToolGroup(0, 1),
                Segment::Item(1),
                Segment::ToolGroup(2, 4),
            ]
        );
    }

    #[test]
    fn group_segments_splits_a_run_around_a_mid_batch_info() {
        // The ADR-0047 behavior pinned: an `Info` interleaved between two tool
        // results (a mid-batch extension error / standing approval) SPLITS the run
        // into two tool groups with the Info as its own singleton between them.
        let items = [
            assistant("on it"),               // 0
            tool_call("edit"),                // 1
            tool_result("edit"),              // 2
            info("auto-approved"),            // 3  (splits the batch)
            tool_result("run_shell_command"), // 4
            assistant("done"),                // 5
        ];
        assert_eq!(
            group_segments(&items, 0),
            vec![
                Segment::Item(0),
                Segment::ToolGroup(1, 3),
                Segment::Item(3),
                Segment::ToolGroup(4, 5),
                Segment::Item(5),
            ]
        );
    }

    #[test]
    fn group_segments_keeps_a_diff_inside_the_tool_run() {
        // A `Diff` is a tool item, so it stays in the surrounding run's box rather
        // than breaking it (the diff renders inside the group box).
        let items = [
            tool_call("edit"),
            diff_item("edit foo", vec![DiffLine::new(DiffSide::Added, "a")]),
            tool_result("edit"),
        ];
        assert_eq!(group_segments(&items, 0), vec![Segment::ToolGroup(0, 3)]);
    }

    #[test]
    fn group_segments_groups_an_orphaned_result_like_any_tool_item() {
        // An orphaned `ToolResult` (no preceding call - e.g. after a supersede
        // moved the result to the tail) is still a tool item and boxes normally.
        let items = [tool_result("read_file")];
        assert_eq!(group_segments(&items, 0), vec![Segment::ToolGroup(0, 1)]);
    }

    #[test]
    fn group_segments_only_emits_the_settled_tail_from_the_high_water_mark() {
        // The fold starts at `hw`: items below the high-water mark are frozen into
        // scrollback and never re-emitted.
        let items = [
            assistant("frozen"),      // 0  (below hw)
            tool_call("read_file"),   // 1
            tool_result("read_file"), // 2
        ];
        assert_eq!(group_segments(&items, 1), vec![Segment::ToolGroup(1, 3)]);
    }

    // The box-rigidity assertion (ADR-0029, the HIGH risk): EVERY row `grouped_rows`
    // produces for this store - borders, tool rows, the gap, a boxed diff's rows - is
    // EXACTLY `width` display columns (so the right `│`/`╮`/`╯` corners always align in
    // one column) AND stays ONE visual row (the viewport's own `Wrap` agrees).
    fn assert_every_box_row_is_exactly_width(
        t: &crate::ui::transcript::Transcript,
        toggles: Toggles,
        widths: &[u16],
    ) {
        let items: Vec<TranscriptItem> = t.items().to_vec();
        for &width in widths {
            let mut cache = RenderCache::new();
            cache.sync(t, toggles, width, theme::dark());
            let lines = grouped_rows(&cache, &items, 0, width, theme::dark());
            for line in &lines {
                let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
                assert_eq!(
                    w,
                    width as usize,
                    "box row not exactly {width} cols (right border misaligned): {:?}",
                    line_text(line)
                );
                assert_eq!(
                    wrapped_count(vec![line.clone()], width),
                    1,
                    "box row is not one visual row at {width}: {:?}",
                    line_text(line)
                );
            }
        }
    }

    // A single-`ToolResult` transcript with the given name/desc - the shared
    // builder the box-rigidity goldens route through, so the `ToolResult { … }`
    // literal lives in one place (and a Diff/multi-tool case builds its own).
    fn store_with_one_result(name: &str, summary: String) -> crate::ui::transcript::Transcript {
        let mut t = crate::ui::transcript::Transcript::new(Vec::new());
        t.push(TranscriptItem::ToolResult {
            name: name.into(),
            summary,
            is_error: false,
            key_arg: Some(name.into()),
        });
        t
    }

    #[test]
    fn every_boxed_row_is_exactly_the_box_width_the_right_border_aligns() {
        // A long tool description (truncate-end) must not spill the right border.
        let t = store_with_one_result("run_shell_command", "a very long description ".repeat(10));
        assert_every_box_row_is_exactly_width(&t, Toggles::default(), &[30, 50, 72]);
    }

    #[test]
    fn a_cjk_and_emoji_tool_header_keeps_the_box_rigid() {
        // Widths are DISPLAY COLUMNS, not char counts: a CJK+emoji tool name and
        // description (each ideograph 2 cols, the emoji 2 cols) must still pad/
        // truncate to exactly the box width - the char-count trap the review flagged.
        let t = store_with_one_result("你好🎉世界", "説明 ".repeat(12));
        assert_every_box_row_is_exactly_width(&t, Toggles::default(), &[20, 33, 50]);
    }

    #[test]
    fn a_boxed_diff_with_a_wide_glyph_line_keeps_the_box_rigid() {
        // A tool group whose result is a Diff: the diff renders INSIDE the box
        // (tools_expanded), so every diff-in-box row - including a wide-glyph diff
        // line - must be exactly the box width, not just the header/border rows.
        let mut t = crate::ui::transcript::Transcript::new(Vec::new());
        t.push(TranscriptItem::Diff {
            title: "edit 世界.rs".into(),
            lang: Some("txt".into()),
            hunks: vec![DiffHunk {
                header: Some("@@ -1,2 +1,2 @@".into()),
                lines: vec![
                    DiffLine::new(DiffSide::Context, "let 語 = 実装; // 説明"),
                    DiffLine::new(DiffSide::Added, "let x = 1; 🎉"),
                    DiffLine::new(DiffSide::Removed, "let x = 0;"),
                ],
            }],
            elided: 0,
        });
        // Not compact: the diff body shows in full.
        let expanded = Toggles::default();
        assert_every_box_row_is_exactly_width(&t, expanded, &[24, 40, 60]);
    }

    #[test]
    fn a_multi_tool_box_pads_its_gap_row_to_the_width() {
        // Two tools in one box exercise the `gap:1` blank row between them; the gap
        // row (and every other row) must still be exactly the box width.
        let mut t = crate::ui::transcript::Transcript::new(Vec::new());
        t.push(TranscriptItem::ToolResult {
            name: "read_file".into(),
            summary: "340 lines".into(),
            is_error: false,
            key_arg: Some("src/foo.rs".into()),
        });
        t.push(TranscriptItem::ToolResult {
            name: "run_shell_command".into(),
            summary: "ok".into(),
            is_error: false,
            key_arg: Some("cargo build".into()),
        });
        assert_every_box_row_is_exactly_width(&t, Toggles::default(), &[30, 50, 72]);
    }

    #[test]
    fn a_tool_name_on_the_inner_width_boundary_keeps_the_box_rigid() {
        // A name+desc whose display width lands EXACTLY on the inner-width boundary
        // (no truncation, no leftover pad): the edge case between "fits" and "spills".
        // At width 30 the inner content width is 30 - BOX_CHROME(4) = 26; the header
        // is a 3-col marker + name + space + desc, so name+desc filling 23 cols lands
        // the header exactly on the inner width.
        let inner = 30usize - BOX_CHROME;
        let budget = inner - STATUS_INDICATOR_WIDTH; // marker column
        let name = "n".repeat(8);
        let desc = "d".repeat(budget - name.len() - 1); // -1 for the space
        let mut t = crate::ui::transcript::Transcript::new(Vec::new());
        t.push(TranscriptItem::ToolResult {
            name,
            summary: desc,
            is_error: false,
            key_arg: None,
        });
        assert_every_box_row_is_exactly_width(&t, Toggles::default(), &[30]);
    }

    // The fg colour of an item's FIRST span (the prefix glyph): the role a Phase-2
    // committed prefix wears. Asserted against the style HELPERS (not raw hexes) so
    // the Phase-7 slot remap moves in lockstep.
    fn first_span_fg(item: &TranscriptItem) -> Option<Color> {
        message_lines(item, false, 40, theme::dark())[0].spans[0]
            .style
            .fg
    }

    #[test]
    fn each_committed_prefix_wears_its_colour_role() {
        let theme = theme::dark();
        // User `>` and Assistant `✦` are both `text.accent` (accent_style).
        assert_eq!(
            first_span_fg(&TranscriptItem::User { text: "hi".into() }),
            accent_style(theme).fg
        );
        assert_eq!(
            first_span_fg(&TranscriptItem::Assistant { text: "hi".into() }),
            accent_style(theme).fg
        );
        // Info `●` is `text.primary` (primary_style - the `foreground` slot, ADR-0008).
        assert_eq!(
            first_span_fg(&TranscriptItem::Info {
                text: "note".into()
            }),
            primary_style(theme).fg
        );
        // Settled Thinking `✦` is grey `text.secondary` (thinking/secondary_style).
        assert_eq!(
            first_span_fg(&TranscriptItem::Thinking {
                text: "pondering".into()
            }),
            secondary_style(theme).fg
        );
        // And the roles are genuinely distinct where the slots differ: accent != grey.
        assert_ne!(accent_style(theme).fg, secondary_style(theme).fg);
    }

    #[test]
    fn the_phase_7_role_helpers_read_their_dedicated_slots() {
        // Phase 7 (ADR-0008/0053) carved four qwen roles into real slots; each
        // helper now reads its OWN slot, no longer a borrowed neighbour. Pinned
        // against the slot value (a hex the theme drift test also guards), so a
        // regression to the old borrow surfaces here.
        let theme = theme::dark();
        assert_eq!(accent_style(theme).fg, Some(tui_color(theme.accent)));
        assert_eq!(success_style(theme).fg, Some(tui_color(theme.success)));
        assert_eq!(warning_style(theme).fg, Some(tui_color(theme.warning)));
        assert_eq!(primary_style(theme).fg, Some(tui_color(theme.foreground)));
        // Purple accent, lime success, gold warning, real foreground - the
        // QwenDark hexes, and distinct from the slots they used to borrow.
        assert_eq!(accent_style(theme).fg, Some(Color::Rgb(0xD2, 0xA6, 0xFF)));
        assert_eq!(success_style(theme).fg, Some(Color::Rgb(0xAA, 0xD9, 0x4C)));
        assert_eq!(warning_style(theme).fg, Some(Color::Rgb(0xFF, 0xD7, 0x00)));
        assert_eq!(primary_style(theme).fg, Some(Color::Rgb(0xbf, 0xbd, 0xb6)));
        assert_ne!(accent_style(theme).fg, Some(tui_color(theme.prompt_gutter)));
        assert_ne!(success_style(theme).fg, Some(tui_color(theme.added)));
        assert_ne!(warning_style(theme).fg, Some(tui_color(theme.marker_aid)));
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

    // --- composer chrome (ADR-0048): top rule, prompt, bottom border, cursor --

    // The correctness-critical +1 (Risk #1): the draft sits BELOW the top dash
    // rule, so the terminal cursor y is the zone top plus one rule row plus the
    // cursor row offset. A one-line draft at column 3 lands at y = zone.y + 1.
    #[test]
    fn composer_cursor_sits_one_row_below_the_top_rule() {
        // A zone of height 4 (top rule + 1 draft row + bottom border + spare):
        // "abc" with the cursor after it.
        let layout = composer::layout("abc", 3, 40);
        assert_eq!(layout.cursor_row, 0);
        let zone = Rect::new(0, 5, 40, 4);
        let screen = Screen::new(ScreenOpts::default());
        let mut terminal = draw_frame(40, 12, |f| {
            render_composer(f, zone, &screen, &layout, theme::dark());
        });
        let cursor = terminal.get_cursor_position().expect("cursor placed");
        // x: zone.x + 2 (the `> ` prompt) + cursor_col(3) = 5.
        assert_eq!(cursor.x, 5, "cursor x past the `> ` prompt");
        // y: zone.y(5) + 1 (top rule) + cursor_row(0) = 6.
        assert_eq!(cursor.y, 6, "cursor y is one row below the top rule");
    }

    #[test]
    fn composer_draws_the_top_rule_prompt_and_bottom_border() {
        let layout = composer::layout("hello", 5, 40);
        let zone = Rect::new(0, 0, 40, 4);
        let screen = Screen::new(ScreenOpts::default());
        let terminal = draw_frame(40, 4, |f| {
            render_composer(f, zone, &screen, &layout, theme::dark());
        });
        // Row 0 is the full-width top dash rule.
        assert_eq!(row_text(&terminal, 0), "─".repeat(40));
        // Row 1 is the draft under the `> ` prompt.
        assert!(row_text(&terminal, 1).starts_with("> hello"));
        // The last row is the bottom border.
        assert_eq!(row_text(&terminal, 3), "─".repeat(40));
    }

    #[test]
    fn composer_shows_the_placeholder_when_the_draft_is_empty() {
        let layout = composer::layout("", 0, 40);
        assert!(composer_is_empty(&layout));
        let zone = Rect::new(0, 0, 40, 4);
        let screen = Screen::new(ScreenOpts::default());
        let terminal = draw_frame(40, 4, |f| {
            render_composer(f, zone, &screen, &layout, theme::dark());
        });
        assert!(
            row_text(&terminal, 1).contains("Type your message or @path/to/file"),
            "placeholder row: {:?}",
            row_text(&terminal, 1)
        );
    }

    // The zone height accounts for the two chrome rows on top of the draft rows
    // (Risk #1: the +2 must match `render_composer`'s reservations exactly).
    #[test]
    fn capped_composer_height_reserves_the_two_chrome_rows() {
        let one_line = composer::layout("hi", 2, 40);
        assert_eq!(one_line.rows.len(), 1);
        // 1 draft row + 2 chrome = 3.
        assert_eq!(capped_composer_height(&one_line, 30), 3);
    }

    // The fullscreen viewport hands `capped_composer_height` the WHOLE terminal
    // height, so a tall draft grows the Composer zone toward its 8-row cap on a
    // taller terminal instead of the fixed ~5 the old inline cap allowed. The
    // `max_visible_rows` third-of-the-terminal rule lifts as the frame grows.
    #[test]
    fn capped_composer_height_grows_with_the_full_screen_height() {
        // A draft taller than the 8-row cap so the frame height is the binding
        // constraint (12 hard-wrapped rows).
        let tall = composer::layout("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl", 0, 40);
        assert_eq!(tall.rows.len(), 12);
        // Short terminal: max_visible_rows(12) = 4 draft rows + 2 chrome.
        assert_eq!(capped_composer_height(&tall, 12), 4 + COMPOSER_CHROME_ROWS);
        // Tall terminal: max_visible_rows(60) saturates at 8 draft rows + 2 chrome.
        assert_eq!(capped_composer_height(&tall, 60), 8 + COMPOSER_CHROME_ROWS);
    }

    // A short pending stack is anchored to the BOTTOM of its zone: the top rows
    // of the body zone are blank and the content sits just above the status bar.
    #[test]
    fn render_pending_bottom_anchors_a_short_stack() {
        // A fresh screen: only the startup Header banner is pending.
        let screen = Screen::new(ScreenOpts::default());
        let terminal = draw_pending(60, 12, &screen);

        // The banner spans a few rows and anchors to the BOTTOM of the body
        // zone, so the top rows are blank and the content sits low. Find the
        // first non-blank body row: it must be past the top of the zone.
        let first_content = (0..10)
            .find(|&y| !row_text(&terminal, y).trim().is_empty())
            .expect("some content drew");
        assert!(
            first_content > 0,
            "the top of the body zone is blank (bottom-anchored); first content at row {first_content}"
        );
        // The header brand + tips drew in the body.
        let body = buffer_text(&terminal);
        assert!(
            body.contains(">_ suspenders"),
            "the header brand drew in the body:\n{body}"
        );
        assert!(body.contains("Tips:"), "the tips line drew:\n{body}");
    }

    // Readable content caps at MAX_CONTENT_WIDTH so an ultrawide terminal keeps
    // prose legible (qwen `mainAreaWidth = min(width - 4, 100)`); the footer rule
    // is sized separately and still spans the full width.
    #[test]
    fn readable_content_caps_at_100_columns_but_the_footer_stays_full_width() {
        // The helper: clamps at the cap, plain margin below it (narrow unchanged).
        assert_eq!(content_width(200), MAX_CONTENT_WIDTH);
        assert_eq!(content_width(104), MAX_CONTENT_WIDTH); // 104 - 4 == 100, the cap
        assert_eq!(content_width(103), 99); // below the cap: width - 2*margin
        assert_eq!(content_width(50), 46);

        // End to end on a 200-col terminal: the header content is capped at 100, so
        // the SideBySide tier (needs >= 129) can't apply - the header STACKS and its
        // info-panel brand sits near the left margin instead of to the right of the
        // 83-col logo.
        let screen = Screen::new(ScreenOpts::default());
        let terminal = draw_pending(200, 30, &screen);
        let brand_row = (0..30)
            .map(|y| row_text(&terminal, y))
            .find(|r| r.contains(">_ suspenders"))
            .expect("the header brand drew");
        // Byte index -> column: every cell left of the brand is a width-1 box/space
        // glyph, so the char count is the column.
        let brand_col = brand_row
            .find(">_")
            .map(|b| brand_row[..b].chars().count())
            .unwrap();
        assert!(
            brand_col < 10,
            "capped body: the brand is left-aligned (stacked), not beside the 83-col logo; col {brand_col} in {brand_row:?}"
        );

        // The footer is NOT bound by the cap: its row carries content past the
        // 102-col content edge (right-aligned segments reach toward the terminal
        // edge), while the body above never does.
        let footer_row = (0..30)
            .map(|y| row_text(&terminal, y))
            .find(|r| r.contains("shortcuts"))
            .expect("the footer drew");
        assert!(
            footer_row.trim_end().chars().count() > 102,
            "footer spans past the 100-col content cap: {footer_row:?}"
        );
    }

    // --- scrolled_clip: the app-owned scroll clamp (ADR-0046, Stage 2) -------
    //
    // `scrolled_clip` is the render-time clamp: it turns the pure Screen's scroll
    // INTENT into a valid top-clip against the live viewport. A body zone `area`
    // of height 10 with a 40-row stack has `max_scroll = 30`.

    // Following the tail is byte-identical to Stage 1's bottom-anchor: the window
    // sits at the bottom (content scroll = max_scroll + 1, the +1 for the marker),
    // with the overflow marker present because rows remain clipped above.
    #[test]
    fn scrolled_clip_following_the_tail_bottom_anchors() {
        let area = Rect::new(0, 0, 40, 10);
        let content = Rect::new(0, 0, 40, 10);
        let clip = scrolled_clip(40, area, content, ScrollIntent::FOLLOW);
        // max_scroll(30) + 1 marker row = 31.
        assert_eq!(clip.scroll, 31);
        assert!(clip.marker_draw.is_some(), "rows remain clipped above");
        // Identical to the bottom-anchoring `anchor_clip` wrapper.
        assert_eq!(clip.scroll, anchor_clip(40, area, content).scroll);
    }

    // Scrolling UP lifts the window: each scrolled row drops one from the content
    // offset, revealing older rows. The marker stays while any remain above.
    #[test]
    fn scrolled_clip_scrolls_up_and_clamps_below_the_top() {
        let area = Rect::new(0, 0, 40, 10);
        let content = Rect::new(0, 0, 40, 10);
        let intent = ScrollIntent {
            follow_tail: false,
            lines: 5,
        };
        let clip = scrolled_clip(40, area, content, intent);
        // clipped_above = 30 - 5 = 25, + 1 marker = 26.
        assert_eq!(clip.scroll, 26);
        assert!(clip.marker_draw.is_some(), "25 rows still clipped above");
    }

    // An over-scroll (or Home's `usize::MAX`) CLAMPS to the very top: the oldest
    // row shows and the "more above" marker clears (nothing is clipped above).
    #[test]
    fn scrolled_clip_over_scroll_pins_to_the_top_with_no_marker() {
        let area = Rect::new(0, 0, 40, 10);
        let content = Rect::new(0, 0, 40, 10);
        for lines in [30, 999, usize::MAX] {
            let intent = ScrollIntent {
                follow_tail: false,
                lines,
            };
            let clip = scrolled_clip(40, area, content, intent);
            assert_eq!(clip.scroll, 0, "the top row (0) is shown for lines={lines}");
            assert!(
                clip.marker_draw.is_none(),
                "no more-above marker at the very top for lines={lines}"
            );
        }
    }

    // When the transcript is SHORTER than the viewport the scroll is a no-op: the
    // stack bottom-anchors with no clip and no marker, whatever the intent.
    #[test]
    fn scrolled_clip_is_a_no_op_when_the_stack_fits() {
        let area = Rect::new(0, 0, 40, 10);
        let content = Rect::new(0, 0, 40, 10);
        let intent = ScrollIntent {
            follow_tail: false,
            lines: 5,
        };
        let clip = scrolled_clip(6, area, content, intent);
        assert_eq!(clip.scroll, 0, "a fitting stack never clips");
        assert!(clip.marker_draw.is_none(), "nothing overflows, no marker");
    }

    // An overflowing pending stack is top-clipped: the NEWEST rows survive and
    // the oldest drop off the top (qwen's overflowDirection:"top"), with the `…`
    // overflow marker on the top row.
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
            text.lines().next().map(str::trim) == Some("…"),
            "the overflow marker draws on the top row:\n{text}"
        );
    }

    // 4-zone starvation (Risk #1): a frame so short that a VISIBLE sticky box +
    // the status row + the composer cannot all fit leaves the body `Min(1)` with
    // no room. `frame_chunks` must still return four rects that tile the area
    // without over-running it, and Layout keeps at least the `Min(1)` body at the
    // expense of the fixed zones - the layout never panics or produces an
    // off-frame rect.
    #[test]
    fn frame_chunks_starved_by_a_sticky_box_never_over_runs_the_frame() {
        // A 6-row frame with a 6-row sticky box + 1 status + 3 composer wants 10
        // rows: the fixed zones alone exceed the frame, so Layout must shrink them.
        let area = Rect::new(0, 0, 40, 6);
        let chunks = frame_chunks(area, 6, 3);
        assert_eq!(chunks.len(), 4);
        // Every zone stays inside the frame (no off-frame y/height).
        for zone in chunks.iter() {
            assert!(
                zone.y >= area.y && zone.bottom() <= area.bottom(),
                "zone {zone:?} ran off the frame {area:?}"
            );
        }
        // The zones tile the frame top-to-bottom with no gap or overlap.
        assert_eq!(chunks[0].y, area.y);
        assert_eq!(chunks[0].bottom(), chunks[1].y);
        assert_eq!(chunks[1].bottom(), chunks[2].y);
        assert_eq!(chunks[2].bottom(), chunks[3].y);
        assert_eq!(chunks[3].bottom(), area.bottom());
        // The body keeps at least its `Min(1)` row (Layout honours Min over the
        // Length zones when starved).
        assert!(chunks[0].height >= 1, "the body kept >= 1 row: {chunks:?}");
    }

    // The full 4-zone render at tiny heights must never panic (the composer's
    // `area.height - COMPOSER_CHROME_ROWS` underflow is the trap) and must keep
    // any placed cursor ON the frame. Sweeps the smallest heights where the fixed
    // zones fight the body.
    #[test]
    fn render_pending_at_tiny_heights_never_panics_and_keeps_the_cursor_on_frame() {
        let screen = Screen::new(ScreenOpts {
            notices: (1..=6).map(|i| format!("notice-{i}")).collect(),
            ..ScreenOpts::default()
        });
        for height in 1..=8 {
            let mut terminal = draw_pending(40, height, &screen);
            // If a cursor was placed, it must be within the frame bounds.
            if let Ok(pos) = terminal.get_cursor_position() {
                assert!(
                    pos.x < 40 && pos.y < height,
                    "cursor {pos:?} off a {height}-row frame"
                );
            }
        }
    }

    // --- the Help overlay (qwen `Help`, the `?` affordance) ------------------

    // A Screen whose Help overlay is open, for the render assertions below.
    fn screen_with_help_open() -> Screen {
        let mut screen = Screen::new(ScreenOpts::default());
        screen.help_open = true;
        screen
    }

    // The open Help overlay draws the bordered panel in the body region: the
    // title, a shortcut row, a built-in command, and the `Esc to close` footer all
    // land, and it replaces the transcript body (no header/tips row shows through).
    #[test]
    fn help_overlay_shows_shortcuts_commands_and_the_close_hint() {
        let screen = screen_with_help_open();
        // Tall enough that the whole one-column panel fits above the composer
        // (title through footer) without top-clipping.
        let terminal = draw_pending(80, 32, &screen);
        let text = buffer_text(&terminal);
        assert!(
            text.contains("keyboard shortcuts"),
            "title present: {text:?}"
        );
        assert!(text.contains("Shortcuts"), "Shortcuts heading present");
        assert!(text.contains("Show this help"), "a shortcut row present");
        assert!(text.contains("Commands"), "Commands heading present");
        // A built-in command from the registry, derived (not hardcoded).
        assert!(text.contains("/model"), "the /model command is listed");
        assert!(
            text.contains("choose the model for this session"),
            "the /model help is listed"
        );
        assert!(text.contains("Esc to close"), "the close hint present");
        // The panel is bordered (box-drawing chars from the same helpers).
        assert!(
            text.contains('╭') && text.contains('╰'),
            "the panel is bordered"
        );
    }

    // Measure==draw (ADR-0029): every emitted panel Line is `<= content width`, so
    // the viewport never soft-wraps a row. Spans the one-column widths AND past the
    // two-column threshold (~116 content cols) so both layouts are exercised.
    #[test]
    fn help_panel_rows_never_exceed_the_content_width() {
        for width in [40u16, 60, 80, 100, 120, 140, 200] {
            let lines = help_panel_lines(width, theme::dark());
            for line in &lines {
                let cols: usize = line.spans.iter().map(|s| s.content.width()).sum();
                assert!(
                    cols <= width as usize,
                    "a Help row is {cols} cols, over the {width}-col width"
                );
            }
        }
    }

    // At a common width (100) the panel is ONE clean column: no row carries the
    // second column's key, and the longest description renders in FULL with no
    // ellipsis (the mid-word truncation bug the two-column layout caused is gone).
    #[test]
    fn help_defaults_to_a_single_untruncated_column_at_width_100() {
        let lines = help_panel_lines(100, theme::dark());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let longest = "Scroll up a page through the transcript";
        assert!(
            texts.iter().any(|t| t.contains(longest)),
            "the longest description renders in full at width 100: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains('…')),
            "no row is truncated with an ellipsis at width 100: {texts:?}"
        );
        // A single column: no shortcut row pairs two keys (e.g. `/` … `Ctrl+O`).
        assert!(
            !texts
                .iter()
                .any(|t| t.contains('/') && t.contains("Ctrl+O")),
            "no row carries a second column at width 100: {texts:?}"
        );
    }

    // Past the threshold (content width >= 2*(key_col + longest_desc) + gap, ~116)
    // the panel goes two-column: a row pairs the first-half and second-half keys,
    // the right column's descriptions stay untruncated, and every row still fits.
    #[test]
    fn help_goes_two_column_only_at_wide_widths_with_aligned_untruncated_cells() {
        let width = 140u16;
        let lines = help_panel_lines(width, theme::dark());
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // The first shortcut row pairs the first-half `/` key with the second-half
        // key (the split puts `Ctrl+O` at the top of the right column).
        assert!(
            texts
                .iter()
                .any(|t| t.contains('/') && t.contains("Ctrl+O")),
            "a two-column row pairs left+right keys at width {width}: {texts:?}"
        );
        // No ellipsis: both columns' full descriptions fit.
        assert!(
            !texts.iter().any(|t| t.contains('…')),
            "two columns never truncate at width {width}: {texts:?}"
        );
        // The right column aligns: every paired row starts its right key at the
        // SAME column (the fixed left-cell width). Find the column of `Ctrl+O`.
        let right_key_col = |t: &str| t.find("Ctrl+O").map(|b| t[..b].chars().count());
        let cols: Vec<usize> = texts.iter().filter_map(|t| right_key_col(t)).collect();
        assert!(!cols.is_empty(), "found the right column key");
        assert!(
            cols.iter().all(|&c| c == cols[0]),
            "the right column aligns to a fixed left-cell width: {cols:?}"
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

    // --- render_composer_popup: System B (`/` palette) ----------------------

    fn suggestion(
        label: &str,
        value: &str,
        desc: &str,
        matched: Option<(usize, usize)>,
    ) -> completion::Suggestion {
        completion::Suggestion {
            label: label.to_string(),
            value: value.to_string(),
            description: desc.to_string(),
            matched,
        }
    }

    fn palette(suggestions: Vec<completion::Suggestion>, active: usize) -> OverlayView {
        palette_expanded(suggestions, active, false)
    }

    fn palette_expanded(
        suggestions: Vec<completion::Suggestion>,
        active: usize,
        expanded: bool,
    ) -> OverlayView {
        OverlayView::Menu {
            suggestions,
            active,
            scroll: 0,
            query: "m".to_string(),
            expanded,
        }
    }

    #[test]
    fn the_palette_lists_two_columns_command_and_description() {
        let view = palette(
            vec![
                suggestion("model", "model", "pick the model", Some((0, 1))),
                suggestion("theme", "theme", "pick the theme", None),
            ],
            0,
        );
        let text = buffer_text(&draw_popup(&view));
        assert!(text.contains(" commands "), "bordered title:\n{text}");
        assert!(text.contains("model"));
        assert!(text.contains("pick the model"), "description column");
        assert!(text.contains("theme"));
    }

    // --- PrepareLabel: MAX_WIDTH collapse/expand (qwen) ---------------------

    #[test]
    fn prepare_label_leaves_a_short_label_whole() {
        // Below MAX_WIDTH the label is untouched (no match).
        let (before, matched, after) = prepare_label("model", None, false);
        assert_eq!(
            (before, matched, after),
            ("model".into(), "".into(), "".into())
        );
    }

    #[test]
    fn prepare_label_splits_a_short_label_at_its_match() {
        // A contiguous match window splits into before/matched/after (qwen
        // Case 1 - the label already fits).
        let (before, matched, after) = prepare_label("model", Some((0, 3)), false);
        assert_eq!(before, "");
        assert_eq!(matched, "mod");
        assert_eq!(after, "el");
    }

    #[test]
    fn prepare_label_truncates_a_long_label_until_expanded() {
        let long: String = std::iter::repeat_n('x', completion::MAX_WIDTH + 40).collect();
        // Collapsed: truncated to MAX_WIDTH chars + "..." (qwen no-match branch).
        let (before, _, _) = prepare_label(&long, None, false);
        assert_eq!(before.chars().count(), completion::MAX_WIDTH + 3);
        assert!(before.ends_with("..."));
        // Expanded: the full label, untouched.
        let (before, _, _) = prepare_label(&long, None, true);
        assert_eq!(before, long);
    }

    #[test]
    fn prepare_label_windows_a_long_label_around_its_match_with_elisions() {
        // A long label with a short match mid-string collapses to a window
        // centred on the match, `...`-elided at the clipped ends (qwen Case 3).
        let long: String = std::iter::repeat_n('x', completion::MAX_WIDTH * 2).collect();
        let mid = completion::MAX_WIDTH; // match sits in the middle
        let (before, matched, after) = prepare_label(&long, Some((mid, mid + 3)), false);
        assert_eq!(matched, "xxx", "the match stays whole");
        assert!(before.starts_with("..."), "left elision: {before:?}");
        assert!(after.ends_with("..."), "right elision: {after:?}");
        // The whole window fits MAX_WIDTH.
        let total = before.chars().count() + matched.chars().count() + after.chars().count();
        assert!(total <= completion::MAX_WIDTH, "window bounded: {total}");
    }

    #[test]
    fn a_long_active_row_shows_the_expand_affordance() {
        let long: String = std::iter::repeat_n('x', completion::MAX_WIDTH + 5).collect();
        // Collapsed active long row: ` → ` affordance.
        let collapsed = palette_expanded(vec![suggestion(&long, "cmd", "", None)], 0, false);
        assert!(
            buffer_text(&draw_popup(&collapsed)).contains('→'),
            "collapsed long active row shows →"
        );
        // Expanded active long row: ` ← ` affordance.
        let expanded = palette_expanded(vec![suggestion(&long, "cmd", "", None)], 0, true);
        assert!(
            buffer_text(&draw_popup(&expanded)).contains('←'),
            "expanded long active row shows ←"
        );
        // A SHORT active row shows no affordance.
        let short = palette_expanded(vec![suggestion("model", "model", "", None)], 0, false);
        let text = buffer_text(&draw_popup(&short));
        assert!(
            !text.contains('→') && !text.contains('←'),
            "short row: no affordance"
        );
    }

    #[test]
    fn the_palette_active_row_is_accent_others_secondary_no_marker() {
        let view = palette(
            vec![
                suggestion("model", "model", "", None),
                suggestion("theme", "theme", "", None),
            ],
            1,
        );
        let terminal = draw_popup(&view);
        // 2 body rows + borders = height 4, anchored above row 10 → rows 7-8,
        // text from x = 2. System B has NO `›` marker and NO number, so the
        // first cell IS the label's first glyph.
        assert_eq!(
            cell_char(&terminal, 2, 8),
            't',
            "no gutter before the label"
        );
        let accent = tui_color(theme::dark().accent);
        let secondary = tui_color(theme::dark().muted);
        assert_eq!(cell_fg(&terminal, 2, 8), Some(accent), "active row accent");
        assert_eq!(
            cell_fg(&terminal, 2, 7),
            Some(secondary),
            "inactive secondary"
        );
    }

    #[test]
    fn the_palette_match_substring_is_inverted() {
        // "/m" matches "model" at [0,1): the leading 'm' draws REVERSED.
        let view = palette(vec![suggestion("model", "model", "", Some((0, 1)))], 0);
        let terminal = draw_popup(&view);
        // One body row + borders = height 3 above row 10 → row 8, text x = 2.
        assert!(
            cell_modifier(&terminal, 2, 8).contains(Modifier::REVERSED),
            "the match is inverted"
        );
        assert!(
            !cell_modifier(&terminal, 3, 8).contains(Modifier::REVERSED),
            "the rest is not"
        );
    }

    #[test]
    fn an_empty_palette_shows_no_matches() {
        let view = palette(vec![], 0);
        assert!(buffer_text(&draw_popup(&view)).contains("no matches"));
    }

    #[test]
    fn the_palette_shows_arrows_and_a_counter_when_it_overflows() {
        // More than MAX_SUGGESTIONS rows, scrolled one down: a ▲ above, a ▼
        // below, and an (active/total) counter.
        let suggestions: Vec<_> = (0..MAX_SUGGESTIONS + 3)
            .map(|i| suggestion(&format!("cmd{i}"), &format!("cmd{i}"), "", None))
            .collect();
        let view = OverlayView::Menu {
            suggestions,
            active: 3,
            scroll: 1,
            query: "c".to_string(),
            expanded: false,
        };
        let terminal = draw_frame(40, 20, |f| {
            render_composer_popup(f, 18, f.area(), &view, theme::dark())
        });
        let text = buffer_text(&terminal);
        assert!(text.contains('▲'), "scroll-up arrow:\n{text}");
        assert!(text.contains('▼'), "scroll-down arrow");
        assert!(
            text.contains(&format!("(4/{})", MAX_SUGGESTIONS + 3)),
            "the (n/m) counter:\n{text}"
        );
    }

    // --- render_composer_popup: AT file picker (Phase C2) -------------------

    fn at_files(suggestions: Vec<completion::Suggestion>, loading: bool) -> OverlayView {
        OverlayView::AtFiles {
            suggestions,
            active: 0,
            scroll: 0,
            query: "co".to_string(),
            loading,
        }
    }

    #[test]
    fn the_at_picker_lists_repo_relative_paths_titled_files() {
        let view = at_files(
            vec![
                suggestion("src/ui/composer.rs", "src/ui/composer.rs", "", Some((7, 9))),
                suggestion("src/config.rs", "src/config.rs", "", Some((4, 6))),
            ],
            false,
        );
        let text = buffer_text(&draw_popup(&view));
        assert!(text.contains(" files "), "bordered 'files' title:\n{text}");
        assert!(text.contains("composer.rs"), "a path row is shown:\n{text}");
        assert!(text.contains("config.rs"));
    }

    #[test]
    fn a_long_at_path_renders_in_full_not_chopped_at_half_width() {
        // A path longer than HALF the inner width (36/2 = 18) must render WHOLE:
        // AT rows carry no description, so the label column uses the full inner
        // width, not the slash palette's width/2 cap. Before the fix
        // "src/ui/components.rs" (20 cols) chopped to "src/ui/components." (18).
        let view = at_files(
            vec![suggestion(
                "src/ui/components.rs",
                "src/ui/components.rs",
                "",
                Some((7, 9)),
            )],
            false,
        );
        let text = buffer_text(&draw_popup(&view));
        assert!(
            text.contains("src/ui/components.rs"),
            "the full path renders (no width/2 truncation):\n{text}"
        );
    }

    #[test]
    fn the_at_picker_match_substring_is_inverted() {
        // "co" matches "src/config.rs" at [4,6): the 'co' draws REVERSED.
        let view = at_files(
            vec![suggestion(
                "src/config.rs",
                "src/config.rs",
                "",
                Some((4, 6)),
            )],
            false,
        );
        let terminal = draw_popup(&view);
        // One body row + borders = height 3 above row 10 → row 8, text x = 2.
        // "src/" is x=2..6, so the 'c' at char 4 is cell x=6.
        assert!(
            cell_modifier(&terminal, 6, 8).contains(Modifier::REVERSED),
            "the match is inverted"
        );
    }

    #[test]
    fn an_empty_at_search_still_loading_shows_searching() {
        // A fetch in flight with no rows yet draws the subtle "searching…" line.
        let view = at_files(vec![], true);
        assert!(buffer_text(&draw_popup(&view)).contains("searching…"));
    }

    #[test]
    fn an_empty_at_search_that_finished_shows_no_matches() {
        // Not loading + no rows: the "no matches" placeholder (a real empty walk).
        let view = at_files(vec![], false);
        assert!(buffer_text(&draw_popup(&view)).contains("no matches"));
    }

    // --- render_composer_popup: System A (numbered `›` dialog) --------------

    fn dialog(
        command: &str,
        status: OverlayStatus,
        rows: Vec<SelectorRow>,
        active: usize,
    ) -> OverlayView {
        OverlayView::Dialog {
            command: command.to_string(),
            status,
            rows,
            active,
            detail: None,
        }
    }

    #[test]
    fn a_loading_dialog_shows_the_loading_line() {
        let view = dialog("model", OverlayStatus::Loading, vec![], 0);
        let text = buffer_text(&draw_popup(&view));
        assert!(text.contains(" models "), "dialog title:\n{text}");
        assert!(text.contains("loading models…"));
    }

    #[test]
    fn a_failed_dialog_shows_the_failure_message() {
        let view = dialog(
            "model",
            OverlayStatus::Failed("connection refused".to_string()),
            vec![],
            0,
        );
        assert!(buffer_text(&draw_popup(&view)).contains("failed: connection refused"));
    }

    #[test]
    fn a_ready_dialog_lists_numbered_rows_with_the_marker() {
        let view = dialog(
            "model",
            OverlayStatus::Ready,
            vec![
                SelectorRow::new("a", "qwen/qwen3-30b", None),
                SelectorRow::new("b", "meta/llama-3.1", None),
            ],
            0,
        );
        let text = buffer_text(&draw_popup(&view));
        assert!(text.contains(" models "));
        assert!(
            text.contains(SELECTION_MARKER),
            "the `›` marker on the active row"
        );
        assert!(text.contains("1."), "numbered rows");
        assert!(text.contains("qwen/qwen3-30b"));
        assert!(text.contains("meta/llama-3.1"));
    }

    #[test]
    fn a_dialog_header_row_is_dim_and_unnumbered() {
        let view = dialog(
            "model",
            OverlayStatus::Ready,
            vec![
                SelectorRow::header("openrouter"),
                SelectorRow::new("openrouter/kimi-k2", "openrouter/kimi-k2", None),
            ],
            1,
        );
        let terminal = draw_popup(&view);
        // Body rows 7 (header) and 8 (member), inset text from x = 2. The
        // header draws a blank gutter + blank number field, so its label sits
        // further right; the active member (row 1) carries the `›` marker.
        assert_ne!(cell_char(&terminal, 2, 7), '›', "the header has no marker");
        assert_eq!(
            cell_char(&terminal, 2, 8),
            '›',
            "the active member is marked"
        );
        // The header label ("openrouter") reads dim (secondary) wherever it
        // starts - find its first glyph on the row.
        let header_row = row_text(&terminal, 7);
        let col = header_row.find('o').expect("the header label") as u16;
        let secondary = tui_color(theme::dark().muted);
        assert_eq!(
            cell_fg(&terminal, col, 7),
            Some(secondary),
            "the header is dim"
        );
    }

    #[test]
    fn the_dialog_titles_itself_after_its_command() {
        // The title pluralizes the opaque command name, so /theme's dialog
        // reads " themes " without the renderer knowing any command.
        let view = dialog("theme", OverlayStatus::Loading, vec![], 0);
        let text = buffer_text(&draw_popup(&view));
        assert!(text.contains(" themes "), "dialog title:\n{text}");
        assert!(text.contains("loading themes…"));
    }

    #[test]
    fn the_dialog_scrolls_the_active_row_into_view() {
        // 20 rows against the POPUP_MAX_ROWS cap: an active row at the bottom
        // scrolls the top rows out and brings it on screen.
        let rows: Vec<SelectorRow> = (0..20)
            .map(|i| SelectorRow::new(format!("m{i}"), format!("model-{i:02}"), None))
            .collect();
        let view = dialog("model", OverlayStatus::Ready, rows, 19);
        let terminal = draw_frame(40, 14, |f| {
            render_composer_popup(f, 12, f.area(), &view, theme::dark())
        });
        let text = buffer_text(&terminal);
        assert!(
            text.contains("model-19"),
            "the active row scrolled into view"
        );
        assert!(!text.contains("model-00"), "the top rows scrolled out");
    }

    // --- pending body: layout, margins, streaming ---------------------------

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
            // Bracket the leftmost 2 margin columns (qwen `marginLeft:2`,
            // ADR-0046) so the left margin is unambiguous, then the content.
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
    // the default EXPANDED body (qwen `!compactMode`, the app's default) and the
    // compact fold (Ctrl+O / the binary's `o` key: each diff a fold-title
    // one-liner). The binary only adds the terminal lifecycle on top of this.
    #[test]
    fn the_diff_demo_screen_renders_its_diffs_without_panicking() {
        // Default (compact=false): the titles AND the code rows / elided tail.
        let expanded = buffer_text(&draw_viewport(100, 70, &Screen::demo_diffs()));
        assert!(
            expanded.contains("clean up the tokenizer"),
            "the request:\n{expanded}"
        );
        for title in [
            "edit src/lexer.rs",
            "src/greet.js",
            "package.json",
            "src/generated.js",
        ] {
            assert!(expanded.contains(title), "the {title} title:\n{expanded}");
        }
        assert!(
            expanded.contains("split_whitespace"),
            "the rust hunk body:\n{expanded}"
        );
        assert!(
            expanded.contains("Greets a user by name"),
            "the jsdoc body:\n{expanded}"
        );
        assert!(
            expanded.contains("37 lines hidden"),
            "the elided tail:\n{expanded}"
        );

        // Compact (Ctrl+O): the diff bodies fold away to their fold titles - the
        // code rows are gone, but the titles stay.
        let (compact_screen, _) = Screen::demo_diffs().handle_key(Key::ToggleCompact);
        let folded = buffer_text(&draw_viewport(100, 70, &compact_screen));
        assert!(
            folded.contains("edit src/lexer.rs"),
            "the fold title stays:\n{folded}"
        );
        assert!(
            !folded.contains("split_whitespace"),
            "the rust hunk body folds away under compact:\n{folded}"
        );
    }

    /// The whole demo run rendered through the FULL grouped fold (no top-clip,
    /// qwen `<Static>`), as newline-joined rows - the golden-shape body these
    /// tests inspect without the pending body's overflow clip.
    fn demo_committed_text(width: u16) -> String {
        let screen = Screen::demo();
        let content_width = width - 2 * CONTENT_MARGIN;
        let mut cache = RenderCache::new();
        cache.sync(
            screen.transcript(),
            Toggles::default(),
            content_width,
            theme::dark(),
        );
        let items: Vec<TranscriptItem> = screen.transcript().items().to_vec();
        let lines = grouped_rows(&cache, &items, 0, content_width, theme::dark());
        let height = wrapped_count(lines.clone(), content_width).max(1) as u16;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        Paragraph::new(lines).wrap(Wrap { trim: false }).render(
            Rect::new(CONTENT_MARGIN, 0, content_width, height),
            &mut buf,
        );
        commit_buffer_text(&buf)
    }

    #[test]
    fn the_demo_render_matches_the_confirmed_full_content_run_shape() {
        // The demo is the living spec (ADR-0046 + qwen v0.16.0 chrome): the
        // committed slice renders each item in FULL. Pin the load-bearing qwen
        // prefixes so a reskin regression trips here, not only in a manual dump.
        let text = demo_committed_text(100);
        // The user prompt wears the `>` caret; the assistant the `✦` marker.
        assert!(
            text.contains("> evaluate this project"),
            "user caret:\n{text}"
        );
        assert!(
            text.contains("✦ I'll evaluate this project"),
            "assistant marker:\n{text}"
        );
        // The first thought shows (grey `✦`, one-line collapsed).
        assert!(
            text.contains("✦ The user wants me to evaluate"),
            "the first thought shows:\n{text}"
        );
        // Tool work is inside a rounded box (qwen `ToolGroupMessage`).
        assert!(
            text.contains('╭') && text.contains('╰'),
            "tool box:\n{text}"
        );
        assert!(text.contains("list_directory"), "a tool row:\n{text}");
        // The error tool result reads the `x` ERROR marker.
        assert!(
            text.contains("run_shell_command") && text.contains("command denied"),
            "the error result:\n{text}"
        );
        // Assistant closing text + code fence.
        assert!(
            text.contains("The project is a well-structured"),
            "closing text:\n{text}"
        );
        assert!(text.contains("fn tokenize"), "code fence:\n{text}");
    }

    #[test]
    fn committed_content_stays_within_the_left_margin() {
        // qwen `marginLeft:2`: every non-blank committed demo row draws two columns
        // in - no content touches column 0/1 (the retired lane spine is gone,
        // ADR-0046). Rendered through the full committed slice so the pending
        // body's overflow marker (which sits at column 0) never confounds it.
        for (y, row) in demo_committed_text(100).lines().enumerate() {
            if row.trim().is_empty() {
                continue;
            }
            let cols: Vec<char> = row.chars().collect();
            assert_eq!(
                (cols[0], cols[1]),
                (' ', ' '),
                "row {y} bled into the 2-col left margin: {row:?}"
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
        assert!(text.contains(">_ suspenders"), "the header brand:\n{text}");
        assert!(text.contains("a launch notice"));
    }

    // The spinner line draws through the pending render path (ADR-0048): a
    // Running Run with nothing streaming, quiet past the settle window, paints
    // the elapsed timer + `esc to cancel` into the buffer as a live entry.
    #[test]
    fn the_pending_body_draws_the_spinner_line_when_running() {
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
        assert!(text.contains("5s"), "the elapsed opens at 5s:\n{text}");
        assert!(
            text.contains("esc to cancel"),
            "the cancel affordance:\n{text}"
        );
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

    // ---- wrap_words: the greedy word wrap ----------------------------------

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

    // ---- settled Thinking: the full grey body (qwen `ThinkMessage`) ---------

    #[test]
    fn settled_thinking_prefixes_the_first_body_row_with_the_grey_marker() {
        // qwen has no collapsed one-liner (ADR-0052): a shown thought renders its
        // full grey markdown body, the `✦` marker hung on the first row.
        let lines = settled_thinking_lines("one\ntwo\nthree", theme::dark());
        let text = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.starts_with("✦ one"), "grey thought marker: {text:?}");
    }

    #[test]
    fn settled_thinking_renders_every_source_row() {
        // The full body: three source lines render as three (or more) rows, not a
        // single collapsed line.
        let lines = settled_thinking_lines("one\ntwo\nthree", theme::dark());
        assert!(
            lines.len() >= 3,
            "the full body keeps every source row: {}",
            lines.len()
        );
    }

    #[test]
    fn the_reserved_gutter_forces_wrapping_at_the_reduced_content_width() {
        // RED-1 (ADR-0029): the 2-col left+right margins (qwen `marginLeft:2,
        // marginRight:2`) are carved off, so content wraps in the narrower
        // `content_area` and is DRAWN two columns in. A 39-char word fits in the
        // full 40 cols but must wrap at the reduced content width.
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
            40 - 2 * CONTENT_MARGIN,
            theme::dark(),
        );

        // (1) The notice is drawn CONTENT_MARGIN columns in (qwen `marginLeft:2`):
        // the first row carrying the word begins with the `●` info prefix two
        // columns in from the frame edge.
        let word_row = (0..20)
            .map(|y| row_text(&terminal, y))
            .find(|r| r.contains('x'))
            .expect("the notice row");
        let indent = word_row.chars().take_while(|c| *c == ' ').count();
        assert_eq!(
            indent, CONTENT_MARGIN as usize,
            "content draws at the 2-col margin, not column 0: {word_row:?}"
        );

        // (2) The 39-char word wrapped to more than one visual content row, which
        // happens ONLY at the reduced content width (frame 40 − two 2-col margins
        // − the 2-col info prefix = 34). At the full 40 cols it would be one row.
        let word_rows: usize = cache
            .settled()
            .find_map(|(lines, wrapped)| {
                lines
                    .iter()
                    .any(|l| l.spans.iter().any(|s| s.content.contains('x')))
                    .then_some(wrapped)
            })
            .expect("the notice's cached entry");
        assert!(
            word_rows >= 2,
            "the word wrapped at the reduced width, got {word_rows}"
        );
    }

    #[test]
    fn a_user_prompt_shows_the_caret_prefix_and_the_agent_the_marker() {
        // qwen chrome: an Info line (a launch notice) reads `●`; a User prompt
        // the `>` caret; the agent's answer the `✦` marker. All baked into the
        // content, 2-col margin.
        let screen = screen_with_notices(vec!["a launch notice".to_string()]);
        let (screen, _) = screen.submitted("do the thing", Ok(()));
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_end(
            vec![ContentBlock::text("done")],
            StopReason::EndTurn,
        ));
        let terminal = draw_viewport(40, 20, &screen);
        let text = buffer_text(&terminal);
        assert!(
            text.contains("> do the thing"),
            "user caret prefix:\n{text}"
        );
        assert!(text.contains("✦ done"), "assistant marker prefix:\n{text}");
        assert!(text.contains("● a launch notice"), "info prefix:\n{text}");
    }

    #[test]
    fn the_pending_body_top_clips_to_the_newest_rows() {
        // ADR-0046 inline top-clip: a tall agent answer that OVERFLOWS the pending
        // body keeps the NEWEST rows and drops the oldest.
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.submitted("the question", Ok(()));
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
        let text = buffer_text(&terminal);
        assert!(text.contains("ANSWER-13"), "the tail (newest) is kept");
        assert!(!text.contains("ANSWER-0"), "the oldest clipped off the top");
    }

    /// Vinnie's `evaluate this project` shape (~60 cols): a User prompt, a long
    /// settled thought that would wrap, a wrapping marker, and a tool
    /// call. Returns the rendered terminal.
    fn evaluate_project_screen(width: u16, height: u16) -> Terminal<TestBackend> {
        // A long Compaction marker (`⟨ compaction: ... → summary ⟩`) that soft-
        // wraps at 60 cols, standing in for the wrapping marker this
        // shape exercises.
        let status = "reading the manifest and the entry point and every other file \
                     that could plausibly appear so the marker wraps across several \
                     visual rows here";
        let thinking = "I should read the manifest and the entry point and the tests \
                        and then form a plan about what to evaluate first here";
        let screen = screen_with_thinking("evaluate this project", thinking);
        // Settle the thought (empty final content → thinking materializes).
        let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));
        // A wrapping Housekeeping marker (the `⟨ compaction: ... ⟩` line).
        let (screen, _) = screen.apply_event(Event::compaction_progress(status));
        let (screen, _) = screen.apply_event(Event::tool_call(
            "id1",
            "read_file",
            serde_json::json!({"path": "Cargo.toml"}),
        ));
        draw_viewport(width, height, &screen)
    }

    // A 400-`z` reasoning line (far wider than any terminal) must land as EXACTLY
    // one visual row ending in the `…` truncation marker - the "folds/truncates,
    // never balloons" guarantee both the settled and the streaming path make. Pins
    // that the z-carrying rows are a single truncated row.
    fn assert_z_line_folds_to_one_truncated_row(terminal: &Terminal<TestBackend>, height: u16) {
        let z_rows: Vec<String> = (0..height)
            .map(|y| row_text(terminal, y))
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
    fn compact_mode_hides_a_settled_thought_entirely() {
        // qwen has NO collapsed one-liner for a settled thought (ADR-0052): it
        // either shows in full or, under compact mode (Ctrl+O), is HIDDEN
        // entirely. A long thought shows its z's when NOT compact...
        let long = "z".repeat(400);
        let screen = screen_with_thinking("q", long);
        let (screen, _) = screen.apply_event(Event::message_end(vec![], StopReason::EndTurn));
        let shown = draw_viewport(60, 20, &screen);
        assert!(
            (0..20).any(|y| row_text(&shown, y).contains('z')),
            "the settled thought shows in full when not compact"
        );

        // ...and vanishes under compact mode.
        let (screen, _) = screen.handle_key(crate::ui::screen::Key::ToggleCompact);
        let hidden = draw_viewport(60, 20, &screen);
        assert!(
            (0..20).all(|y| !row_text(&hidden, y).contains('z')),
            "compact mode hides the settled thought entirely"
        );
    }

    #[test]
    fn settled_thinking_uses_the_star_glyph_not_the_brain_emoji() {
        // Symptom 3: settled thinking unifies on the `✦` family with the live
        // tail, and drops the width-2 `🧠` emoji. Shown (compact=false) the grey
        // `✦` marker prefixes the first markdown body row.
        let shown = message_lines(
            &TranscriptItem::Thinking {
                text: "line one\nline two".into(),
            },
            false,
            80,
            theme::dark(),
        );
        assert!(line_text(&shown[0]).starts_with("✦ line one"));
        assert!(!line_text(&shown[0]).contains('🧠'));

        // Compact (Ctrl+O) HIDES the thought entirely (qwen `!compactMode`,
        // ADR-0052): no rows at all.
        let hidden = message_lines(
            &TranscriptItem::Thinking {
                text: "a short thought".into(),
            },
            true,
            80,
            theme::dark(),
        );
        assert!(hidden.is_empty());
    }

    #[test]
    fn the_evaluate_shape_renders_its_content_within_the_margins() {
        // The `evaluate this project` shape at 60 cols - a wrapped thought, a
        // wrapped marker, a tool call - all draw two columns in (qwen `marginLeft:
        // 2`) and never touch column 0 (the retired lane spine is gone, ADR-0046).
        let terminal = evaluate_project_screen(60, 24);
        let mut saw_content = false;
        for y in 0..24 {
            let row = row_text(&terminal, y);
            if row.trim().is_empty() {
                continue;
            }
            saw_content = true;
            // No content column 0/1 (the 2-col left margin stays clear).
            let cols: Vec<char> = row.chars().collect();
            assert_eq!(
                (cols[0], cols[1]),
                (' ', ' '),
                "row {y} bled into the left margin: {row:?}"
            );
        }
        assert!(saw_content, "the shape drew content");
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
        // Live reasoning is content, not a metric: the animated
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
    fn compact_suppresses_the_live_thinking_tail_but_keeps_the_spinner_subject() {
        // qwen gates the pending `gemini_thought` under `!compactMode`
        // (`HistoryItemDisplay.tsx:155`) but does NOT compact-gate the
        // LoadingIndicator. So under compact the animated `✦ Thinking` tail is
        // ABSENT while the spinner SUBJECT (the thought-subject seam,
        // `thought?.subject || currentLoadingPhrase`) is still PRESENT.
        // RunStarted puts the Run into Running so the spinner (LoadingIndicator)
        // draws - the seam the subject fills; then stream a thought.
        let screen = screen_with_notices(vec![]);
        let (screen, _) = screen.apply_event(Event::run_started("r1"));
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Thinking("pondering".to_string()),
            vec![ContentBlock::Thinking {
                text: "weighing the tradeoffs".to_string(),
            }],
        ));
        assert_eq!(screen.status, Status::Running, "the Run is Running");
        let (screen, _) = screen.handle_key(crate::ui::screen::Key::ToggleCompact);
        assert!(screen.compact_mode, "toggled into compact");

        let terminal = draw_viewport(80, 20, &screen);
        let text = buffer_text(&terminal);
        // The live thinking tail's animated header is gone under compact.
        assert!(
            !text.contains("✦ Thinking"),
            "the live thinking tail must be suppressed under compact:\n{text}"
        );
        // ...but the spinner still shows, and its subject is the live reasoning
        // head (the last non-empty line fallback), not a lull phrase.
        assert!(
            text.contains("weighing the tradeoffs"),
            "the spinner subject line stays present under compact:\n{text}"
        );
    }

    /// Renders the FULL pending body for `screen` into a `width`x`height` buffer
    /// at the whole area (no zone re-measure, so the spinner line is never
    /// top-clipped out): the shape the duplication tests need to see the tail AND
    /// the spinner line together.
    fn draw_full_body(width: u16, height: u16, screen: &Screen) -> String {
        let mut cache = RenderCache::new();
        let terminal = draw_frame(width, height, |f| {
            render_pending_body_at(
                f,
                f.area(),
                &mut PendingBodyParams {
                    screen,
                    cache: &mut cache,
                    anim: Anim::default(),
                },
                theme::dark(),
                0,
            );
        });
        buffer_text(&terminal)
    }

    // REGRESSION (bug: "the colored thinking indicator has the thought go through
    // it"): while the model reasons, the live `✦ Thinking` tail shows the
    // reasoning head AND the spinner line's subject fell back to that SAME head,
    // so the exact line painted TWICE - once in the tail, once on the colored
    // spinner line. With no distinct bold `**subject**`, the spinner must NOT echo
    // the tail: the reasoning appears EXACTLY ONCE.
    #[test]
    fn the_spinner_does_not_duplicate_the_thinking_tail_head() {
        let reasoning = "Evaluate the whole project structure carefully";
        let (screen, _) = screen_with_notices(vec![]).apply_event(Event::run_started("r1"));
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Thinking("t".to_string()),
            vec![ContentBlock::Thinking {
                text: reasoning.to_string(),
            }],
        ));
        assert_eq!(screen.status, Status::Running, "the Run is Running");
        assert!(!screen.compact_mode, "non-compact: the tail is visible");

        let text = draw_full_body(80, 20, &screen);
        let occurrences = text.matches(reasoning).count();
        assert_eq!(
            occurrences, 1,
            "the reasoning head must render ONCE (tail only), not echoed on the \
             spinner line:\n{text}"
        );
        // The tail is the surface that keeps it (the `✦ Thinking` header sits above).
        assert!(
            text.contains("✦ Thinking"),
            "the thinking tail header:\n{text}"
        );
    }

    // A DISTINCT bold `**subject**` is NOT the tail's head, so it still belongs on
    // the spinner line even while the tail shows the fuller reasoning (qwen
    // `parseThought`): the short subject shows, the tail keeps the raw line.
    #[test]
    fn a_distinct_bold_subject_still_shows_on_the_spinner() {
        let (screen, _) = screen_with_notices(vec![]).apply_event(Event::run_started("r1"));
        let (screen, _) = screen.apply_event(Event::message_start(1));
        let (screen, _) = screen.apply_event(Event::message_update(
            Delta::Thinking("t".to_string()),
            vec![ContentBlock::Thinking {
                text: "**Mapping the tree** now I walk each crate".to_string(),
            }],
        ));
        let text = draw_full_body(80, 20, &screen);
        // The bold subject shows on the spinner; the raw reasoning stays in the tail.
        assert!(
            text.contains("Mapping the tree"),
            "the distinct bold subject shows on the spinner:\n{text}"
        );
        assert!(
            text.contains("now I walk each crate"),
            "the tail keeps the raw reasoning line:\n{text}"
        );
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
        assert_z_line_folds_to_one_truncated_row(&terminal, 20);
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
    // popup_rect: the popup geometry is the only pure math that was tangled
    // into render_composer_popup; tested here at the calculation level.
    // -----------------------------------------------------------------------

    #[test]
    fn popup_rect_height_is_body_plus_two_borders() {
        let area = Rect::new(0, 0, 80, 24);
        let r = popup_rect(10, area, 3, POPUP_MAX_ROWS); // 3 body rows -> height 5
        assert_eq!(r.height, 5);
    }

    #[test]
    fn popup_rect_height_is_capped_at_popup_max_plus_two() {
        let area = Rect::new(0, 0, 80, 24);
        // 100 body rows would be POPUP_MAX_ROWS + 2 once capped.
        let r = popup_rect(20, area, 100, POPUP_MAX_ROWS);
        assert_eq!(r.height, POPUP_MAX_ROWS + 2);
    }

    #[test]
    fn popup_rect_is_anchored_above_anchor_y() {
        let area = Rect::new(0, 0, 80, 24);
        let r = popup_rect(10, area, 3, POPUP_MAX_ROWS); // height 5, y = 10 - 5 = 5
        assert_eq!(r.y, 5);
    }

    // --- inline approval (ADR-0049) ----------------------------------------

    // A Screen with a live ToolCall `name`(input) gated by a pending Approval,
    // built through the real event path so the confirming call + pending state
    // are exactly what production produces.
    fn screen_confirming(name: &str, input: serde_json::Value, command: &str) -> Screen {
        let screen = Screen::new(ScreenOpts::default());
        let (screen, _) = screen.apply_event(Event::run_started("r1"));
        let (screen, _) = screen.apply_event(Event::tool_call("t1", name, input));
        let (screen, _) =
            screen.apply_event(Event::approval_request("approval-0", command.to_string()));
        screen
    }

    // A Screen carrying a COMMITTED Todo list (so the sticky "Current tasks" box
    // would show) AND an open approval on a live `run_command` ToolCall. The Todo
    // rides in through the real event path (a `todo_write` result with the todos
    // artifact, promoted by the registered todo Extension), is frozen with
    // `mark_committed`, then a second Run opens the confirming call + approval.
    fn screen_committed_todo_then_confirming() -> Screen {
        let opts = ScreenOpts {
            extensions: crate::extensions::configured(&["todo".to_string()]),
            ..ScreenOpts::default()
        };
        let screen = Screen::new(opts);
        let todos = serde_json::json!({
            "todos": [
                {"content": "read the file", "status": "in_progress"},
                {"content": "edit the file", "status": "pending"},
            ]
        });
        let (s, _) = screen.apply_event(Event::run_started("r1"));
        let (s, _) = s.apply_event(Event::tool_call("todo-call", "todo_write", todos));
        // The todos artifact rides the result so the todo Extension promotes it
        // to a first-class Todo item (ADR-0048).
        let mut artifacts = std::collections::HashMap::new();
        artifacts.insert(
            "todos".to_string(),
            serde_json::json!({
                "items": [
                    {"content": "read the file", "status": "in_progress"},
                    {"content": "edit the file", "status": "pending"},
                ]
            }),
        );
        let (s, _) = s.apply_event(Event::ToolResult {
            id: "todo-call".into(),
            name: "todo_write".into(),
            content: "ok".into(),
            is_error: false,
            artifacts,
        });
        // Confirm a Todo item landed. The second run_command below appends newer
        // content after it, so the Todo is no longer the transcript tail and the
        // sticky box qualifies (ADR-0048, fullscreen: not-the-tail gate).
        assert!(
            s.transcript().latest_todo().is_some(),
            "the todo Extension promoted a Todo item"
        );
        // Now a second, live run_command gated on an open approval.
        let (s, _) = s.apply_event(Event::tool_call(
            "t1",
            "run_shell_command",
            serde_json::json!({"command": "cargo test"}),
        ));
        let (s, _) = s.apply_event(Event::approval_request(
            "approval-0",
            "cargo test".to_string(),
        ));
        s
    }

    // BUG 2 (live-vet): with a committed Todo present, the sticky "Current tasks"
    // box must NOT hide the open approval. The approval renders inside the pending
    // body; reserving the sticky zone starves that body (Min(1)) and top-clips the
    // question out of view. The fix drops the sticky box while an approval is open
    // -- a visible approval wins over the informational list.
    #[test]
    fn an_open_approval_drops_the_sticky_box_so_the_question_stays_visible() {
        let screen = screen_committed_todo_then_confirming();
        // A full pending frame (render_pending -> pending_layout): the sticky box
        // WOULD show here (committed, non-empty, incomplete list) if not for the
        // open approval.
        let terminal = draw_pending(60, 24, &screen);
        let text = buffer_text(&terminal);
        assert!(
            text.contains("Allow execution of: 'cargo test'?"),
            "the approval question is visible:\n{text}"
        );
        assert!(
            text.contains("Yes, allow once"),
            "the first radio option is visible:\n{text}"
        );
        assert!(
            !text.contains("Current tasks"),
            "the sticky box is dropped while the approval is open:\n{text}"
        );
    }

    // The pending_layout-level guard: an open approval reserves NO sticky zone
    // (`sticky_items` is None), even though `latest_todo` would otherwise qualify.
    #[test]
    fn pending_layout_reserves_no_sticky_zone_while_an_approval_is_open() {
        let screen = screen_committed_todo_then_confirming();
        // Sanity: the committed list DOES qualify for a sticky box on its own.
        assert!(
            sticky_todos(
                screen.transcript().latest_todo(),
                screen.transcript().items().len(),
            )
            .is_some(),
            "the non-tail Todo would reserve a sticky box absent an approval"
        );
        // But with the approval open, pending_layout drops it.
        let view = screen.composer().view();
        let plan = pending_layout(Rect::new(0, 0, 60, 24), &view, &screen);
        assert!(
            plan.sticky_items.is_none(),
            "an open approval yields no sticky zone"
        );
        assert_eq!(
            plan.sticky_box.height, 0,
            "the sticky zone reserves zero rows"
        );
    }

    // A Screen carrying a COMMITTED Todo list (so the sticky "Current tasks" box
    // would show) with NO approval open - the regression counterpart to
    // [`screen_committed_todo_then_confirming`]. Same Todo setup, then a settling
    // assistant answer follows it so the Todo is no longer the transcript tail
    // (the fullscreen not-the-tail gate) - the sticky box is the only thing
    // driving the frame.
    fn screen_committed_todo_no_approval() -> Screen {
        let opts = ScreenOpts {
            extensions: crate::extensions::configured(&["todo".to_string()]),
            ..ScreenOpts::default()
        };
        let screen = Screen::new(opts);
        let todos = serde_json::json!({
            "todos": [
                {"content": "read the file", "status": "in_progress"},
                {"content": "edit the file", "status": "pending"},
            ]
        });
        let (s, _) = screen.apply_event(Event::run_started("r1"));
        let (s, _) = s.apply_event(Event::tool_call("todo-call", "todo_write", todos));
        let mut artifacts = std::collections::HashMap::new();
        artifacts.insert(
            "todos".to_string(),
            serde_json::json!({
                "items": [
                    {"content": "read the file", "status": "in_progress"},
                    {"content": "edit the file", "status": "pending"},
                ]
            }),
        );
        let (s, _) = s.apply_event(Event::ToolResult {
            id: "todo-call".into(),
            name: "todo_write".into(),
            content: "ok".into(),
            is_error: false,
            artifacts,
        });
        // A settling answer follows the Todo, so the Todo is no longer the tail.
        let (s, _) = s.apply_event(Event::message_start(1));
        let (s, _) = s.apply_event(Event::message_update(
            Delta::Text("done".into()),
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
        ));
        let (s, _) = s.apply_event(Event::message_end(
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
            StopReason::EndTurn,
        ));
        s
    }

    // The MEDIUM finding: on a SHORT terminal the pending body TOP-clips (last N
    // rows kept, [`anchor_clip`]). Before the fix the spinner/tails were appended
    // BELOW the inline approval block, so the top-clip ate the "Allow execution
    // of..." question first - the user could not see what they were approving. The
    // fix suppresses every trailing LIVE row while an approval is open, so the
    // approval block is the BOTTOM-most content and survives the clip. This test
    // renders the real production frame ([`draw_pending`]) with a live spinner
    // anim on a short terminal and asserts BOTH the question and the first radio
    // option survive, and that the spinner's cancel affordance is gone (the
    // approval block owns the bottom).
    #[test]
    fn short_terminal_keeps_the_approval_question_visible() {
        let screen = screen_confirming(
            "run_shell_command",
            serde_json::json!({"command": "cargo test"}),
            "cargo test",
        );
        // The FULL, unclamped confirming body: the approval box must be bottom-
        // most, so the LAST non-blank row is its `╰` border, NOT a spinner row.
        // This is what makes the top-clip keep the question. (Anim past the lull
        // settle window, so a spinner WOULD render absent the suppression.)
        let anim = Anim {
            quiet_ticks: lull::SETTLE_TICKS + 4,
            ..Anim::default()
        };
        let mut cache = RenderCache::new();
        let mut params = PendingBodyParams {
            screen: &screen,
            cache: &mut cache,
            anim,
        };
        let body = pending_body_lines(&mut params, theme::dark(), 0, 56);
        let last_non_blank = body
            .iter()
            .rev()
            .map(line_text)
            .find(|t| !t.trim().is_empty())
            .unwrap_or_default();
        assert!(
            last_non_blank.trim_start().starts_with('╰'),
            "the approval box is bottom-most (last row is its border), not a \
             spinner row: {last_non_blank:?}"
        );
        assert!(
            !body.iter().any(|l| line_text(l).contains("esc to cancel")),
            "the spinner is suppressed while the approval is open"
        );

        // And on a SHORT real frame the top-clip therefore keeps the question and
        // the first option (they ride at the bottom of the pending body).
        let terminal = draw_pending(60, 14, &screen);
        let text = buffer_text(&terminal);
        assert!(
            text.contains("Allow execution of: 'cargo test'?"),
            "the approval question survives top-clip on a short terminal:\n{text}"
        );
        assert!(
            text.contains("Yes, allow once"),
            "the first radio option is visible:\n{text}"
        );
    }

    // Fix-1 regression guard: dropping the sticky box (BUG 2) and suppressing the
    // trailing live rows (this finding) both key off `pending_approval.is_some()`.
    // With NO approval open, a committed Todo must STILL render the sticky
    // "Current tasks" box - the approval-only guards did not over-suppress.
    #[test]
    fn no_approval_still_renders_the_sticky_current_tasks_box() {
        let screen = screen_committed_todo_no_approval();
        // Sanity: the non-tail, non-empty, incomplete list qualifies for a box.
        assert!(
            sticky_todos(
                screen.transcript().latest_todo(),
                screen.transcript().items().len(),
            )
            .is_some(),
            "the non-tail Todo qualifies for a sticky box"
        );
        let terminal = draw_pending(60, 24, &screen);
        let text = buffer_text(&terminal);
        assert!(
            text.contains("Current tasks"),
            "the sticky box renders when no approval is open:\n{text}"
        );
    }

    // The measure-level counterpart to the short-terminal test: an OPEN approval
    // suppresses the spinner/lull row from the pending body, while an equivalent
    // Running-but-no-approval frame KEEPS it. Driven at [`pending_body_lines`] with
    // `quiet_ticks` past the lull settle window so the spinner has a phrase to draw
    // (default anim is still settling, so it would be empty regardless).
    #[test]
    fn an_open_approval_suppresses_the_spinner_row_that_a_bare_run_keeps() {
        let width: u16 = 60;
        // A settled lull (quiet_ticks past SETTLE_TICKS) so the spinner has a scene.
        let anim = Anim {
            quiet_ticks: 60,
            ..Anim::default()
        };
        let has_cancel =
            |lines: &[Line<'static>]| lines.iter().any(|l| line_text(l).contains("esc to cancel"));

        // Approval OPEN (Running, gated run_command): the spinner is suppressed.
        let confirming = screen_confirming(
            "run_shell_command",
            serde_json::json!({"command": "cargo test"}),
            "cargo test",
        );
        let mut cache = RenderCache::new();
        let mut params = PendingBodyParams {
            screen: &confirming,
            cache: &mut cache,
            anim,
        };
        let under_approval = pending_body_lines(&mut params, theme::dark(), 0, width);
        assert!(
            !has_cancel(&under_approval),
            "the spinner is suppressed while an approval is open"
        );

        // Running, NO approval: the same anim now DOES render the spinner row.
        let running = Screen::new(ScreenOpts::default());
        let (running, _) = running.apply_event(Event::run_started("r1"));
        assert_eq!(running.status, Status::Running);
        assert!(running.pending_approval.is_none());
        let mut cache = RenderCache::new();
        let mut params = PendingBodyParams {
            screen: &running,
            cache: &mut cache,
            anim,
        };
        let bare_run = pending_body_lines(&mut params, theme::dark(), 0, width);
        assert!(
            has_cancel(&bare_run),
            "the spinner renders on an equivalent Running frame with no approval"
        );
    }

    // selection_rows: the active row wears the `›` marker + green label; inactive
    // rows show two gutter spaces; numbers are 1-indexed `N.`.
    #[test]
    fn selection_rows_marks_the_active_row_and_numbers_the_options() {
        let items = ["Yes, allow once", "Always", "No"];
        let rows = selection_rows(&items, 1, true, 40, theme::dark());
        let text = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<String>()
        };
        assert!(text(&rows[0]).contains("1. Yes, allow once"));
        // Row 1 is active: the `›` marker leads.
        assert!(text(&rows[1]).starts_with("›"));
        assert!(text(&rows[1]).contains("2. Always"));
        // Row 0 is inactive: no marker, two leading spaces.
        assert!(text(&rows[0]).starts_with("  "));

        // The `N.` number turns success-green on the ACTIVE row (qwen
        // `BaseSelectionList.tsx:113-118`) and secondary elsewhere. Find the span
        // that carries the trailing `.` of the number.
        let theme = theme::dark();
        let number_span = |l: &Line| {
            l.spans
                .iter()
                .find(|s| s.content.ends_with('.'))
                .expect("a number span")
                .style
        };
        assert_eq!(
            number_span(&rows[1]).fg,
            success_style(theme).fg,
            "active-row number is success-green"
        );
        assert_eq!(
            number_span(&rows[0]).fg,
            secondary_style(theme).fg,
            "inactive-row number is secondary"
        );
    }

    // Render-buffer proof (P2) that the `›` marker lands on the ACTIVE option
    // row and the inactive rows carry two gutter spaces - not just via a direct
    // `selection_rows` call but through the real `draw_viewport` inline block.
    // The default active row is option 0 (`Yes, allow once`).
    #[test]
    fn drawn_inline_approval_marks_only_the_active_option_row() {
        let screen = screen_confirming(
            "run_shell_command",
            serde_json::json!({"command": "cargo test"}),
            "cargo test",
        );
        let terminal = draw_viewport(60, 24, &screen);
        // Exactly one active row, so exactly one `›` marker in the whole block.
        let full = buffer_text(&terminal);
        assert_eq!(
            full.matches('›').count(),
            1,
            "one active marker only\n{full}"
        );

        // Locate each option row and inspect its gutter directly in the buffer.
        let row_of = |needle: &str| {
            (0..24)
                .map(|y| row_text(&terminal, y))
                .find(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("row for {needle:?} not found\n{full}"))
        };
        // The active row (option 0) leads with the `›` marker (inside its box,
        // so after the border cell + padding); assert the marker is present and
        // the number `1.` follows.
        let active = row_of("Yes, allow once");
        assert!(
            active.contains('›'),
            "active row shows the marker: {active:?}"
        );
        assert!(active.contains("1."), "active row is numbered: {active:?}");
        // The inactive rows carry NO marker - two gutter spaces stand where the
        // `›` would be, ahead of their number.
        let inactive_always = row_of("Always allow in this project");
        let inactive_deny = row_of("No, suggest changes (esc)");
        assert!(!inactive_always.contains('›'), "{inactive_always:?}");
        assert!(!inactive_deny.contains('›'), "{inactive_deny:?}");
        assert!(inactive_always.contains("2."), "{inactive_always:?}");
        assert!(inactive_deny.contains("3."), "{inactive_deny:?}");
    }

    // The inline block appends the question + all three verbatim options, and the
    // exec question embeds the command.
    #[test]
    fn inline_exec_approval_renders_the_question_and_all_three_options() {
        let screen = screen_confirming(
            "run_shell_command",
            serde_json::json!({"command": "cargo test"}),
            "cargo test",
        );
        let terminal = draw_viewport(60, 24, &screen);
        let text = buffer_text(&terminal);
        assert!(text.contains("Allow execution of: 'cargo test'?"), "{text}");
        assert!(text.contains("Yes, allow once"));
        assert!(text.contains("Always allow in this project"));
        assert!(text.contains("No, suggest changes (esc)"));
    }

    // A web_fetch (Info) approval reads the generic proceed question.
    #[test]
    fn inline_info_approval_renders_the_proceed_question() {
        let screen = screen_confirming(
            "web_fetch",
            serde_json::json!({"url": "https://x"}),
            "https://x",
        );
        let terminal = draw_viewport(60, 24, &screen);
        let text = buffer_text(&terminal);
        assert!(text.contains("Do you want to proceed?"), "{text}");
    }

    // The question modal (ADR-0057, ask_user_question): a bordered box with the
    // VERBATIM title, the `[header]` chip + question text, the option labels, and
    // the auto-appended "Other" row.
    fn screen_with_question() -> Screen {
        use crate::tool::caps::{Question, QuestionOption};
        let screen = Screen::new(ScreenOpts::default());
        let (screen, _) = screen.apply_event(Event::run_started("r1"));
        let questions = vec![Question {
            question: "Which library should we use?".into(),
            header: "Library".into(),
            options: vec![
                QuestionOption {
                    label: "serde".into(),
                    description: "the standard".into(),
                },
                QuestionOption {
                    label: "miniserde".into(),
                    description: "smaller".into(),
                },
            ],
            multi_select: false,
        }];
        let (screen, _) = screen.apply_event(Event::question_request("q-1", questions));
        screen
    }

    #[test]
    fn question_modal_renders_title_question_options_and_the_other_row() {
        let screen = screen_with_question();
        // The bordered modal draws bottom-most in the pending body (top-clipped
        // like the approval), so a short viewport clips the title first. Assert
        // the interactive rows are visible in the live viewport...
        let terminal = draw_viewport(60, 40, &screen);
        let text = buffer_text(&terminal);
        assert!(text.contains("[Library]"), "the header chip shows: {text}");
        assert!(text.contains("Which library should we use?"), "{text}");
        assert!(text.contains("serde"), "{text}");
        assert!(text.contains("miniserde"), "{text}");
        // qwen ALWAYS appends an "Other" row.
        assert!(text.contains("Other"), "the auto-Other row shows: {text}");

        // ...and the VERBATIM title is the box's first content row (checked on the
        // pure line set so the top-clip does not hide it).
        let pending = screen.pending_question.as_ref().unwrap();
        let lines = question_modal_lines(pending, 58, theme::dark());
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(
            rendered
                .iter()
                .any(|row| row.contains("Please answer the following question(s):")),
            "the VERBATIM title row: {rendered:?}"
        );
    }

    // ADR-0029 rigidity: every rendered question-modal row is exactly the box
    // width, so the right border aligns (the box wrapper padded each row).
    #[test]
    fn question_modal_rows_are_rigid_to_the_box_width() {
        use crate::tool::caps::Question;
        let theme = theme::dark();
        let pending = crate::ui::screen::PendingQuestion::new(
            "q-1".into(),
            vec![Question {
                question: "Pick?".into(),
                header: "Lib".into(),
                options: vec![
                    crate::tool::caps::QuestionOption {
                        label: "a".into(),
                        description: "d".into(),
                    },
                    crate::tool::caps::QuestionOption {
                        label: "b".into(),
                        description: "d".into(),
                    },
                ],
                multi_select: false,
            }],
        );
        let width: u16 = 50;
        let lines = question_modal_lines(&pending, width, theme);
        for line in &lines {
            let cols: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert_eq!(cols, width as usize, "every row spans the full box width");
        }
    }

    // The confirming call's marker is `?` (warning), not the executing `⊷`.
    #[test]
    fn the_confirming_call_wears_the_question_marker() {
        let screen = screen_confirming(
            "web_fetch",
            serde_json::json!({"url": "https://x"}),
            "https://x",
        );
        let terminal = draw_viewport(60, 24, &screen);
        let text = buffer_text(&terminal);
        assert!(text.contains("?"), "the confirming marker shows");
        assert!(!text.contains("⊷"), "the executing marker is gone");
    }

    // A non-shell confirming group (web_fetch) borders warning-yellow; a shell
    // one (run_command) keeps the grey symbol border (shell precedence).
    #[test]
    fn group_border_precedence_is_shell_then_confirming_then_default() {
        let call = |name: &str| TranscriptItem::ToolCall {
            id: "t1".into(),
            name: name.into(),
            summary: String::new(),
        };
        let theme = theme::dark();
        // Default: a non-shell, non-confirming group.
        assert_eq!(
            group_border_style(&[call("web_fetch")], false, theme),
            border_style(theme)
        );
        // Confirming, non-shell: warning wins.
        assert_eq!(
            group_border_style(&[call("web_fetch")], true, theme),
            warning_style(theme)
        );
        // Shell always wins, even confirming.
        assert_eq!(
            group_border_style(&[call("run_shell_command")], true, theme),
            symbol_style(theme)
        );
    }

    // ADR-0029 rigidity: every rendered approval row is exactly the box inner
    // width (the box wrapper padded it to `inner`), so the right border aligns.
    #[test]
    fn inline_approval_rows_respect_the_box_inner_width() {
        let screen = screen_confirming(
            "web_fetch",
            serde_json::json!({"url": "https://x"}),
            "https://x",
        );
        let width = 50u16;
        let terminal = draw_viewport(width, 24, &screen);
        // The boxed rows carry the `│` border at both edges of the content zone.
        // Every row that has a left `│` must have a right `│` at the same column,
        // proving the width is rigid (measure==draw).
        let _ = width;
        let rows = terminal.backend().buffer().area.height;
        // Every boxed row of ONE box must place its two `│` borders at the SAME
        // two columns (the box is rigid: measure==draw, ADR-0029). The startup
        // Header's own info panel is a SEPARATE box at its own inner width, so
        // rigidity is asserted per contiguous box-row run (a blank / non-bordered
        // row ends a box), then a bordered box must have been seen.
        let mut border_cols: Option<(usize, usize)> = None;
        let mut saw_box = false;
        for y in 0..rows {
            let row: Vec<char> = row_text(&terminal, y).chars().collect();
            let left = row.iter().position(|&c| c == '│');
            let right = row.iter().rposition(|&c| c == '│');
            match (left, right) {
                (Some(l), Some(r)) if l != r => {
                    saw_box = true;
                    match border_cols {
                        None => border_cols = Some((l, r)),
                        Some((el, er)) => assert_eq!(
                            (l, r),
                            (el, er),
                            "row {y} borders at ({l},{r}) not the rigid ({el},{er}) of its box"
                        ),
                    }
                }
                // A row with no border pair ends the current box run.
                _ => border_cols = None,
            }
        }
        assert!(saw_box, "the approval box drew bordered rows");
    }

    // committed==pending identity (ADR-0049): once the Approval resolves and the
    // ToolResult supersedes the call, the committed slice carries NO approval
    // rows - the question/options are gone.
    #[test]
    fn a_resolved_approval_commits_with_no_approval_rows() {
        let screen = screen_confirming(
            "web_fetch",
            serde_json::json!({"url": "https://x"}),
            "https://x",
        );
        // Resolve + supersede the call with its result.
        let (screen, _) = screen.apply_event(Event::approval_resolved("approval-0", true));
        let (screen, _) = screen.apply_event(Event::tool_result(
            "t1",
            "web_fetch",
            "ok",
            false,
            std::collections::HashMap::new(),
        ));
        assert_eq!(screen.pending_approval, None);
        let terminal = draw_viewport(60, 24, &screen);
        let text = buffer_text(&terminal);
        assert!(!text.contains("Do you want to proceed?"), "{text}");
        assert!(!text.contains("Yes, allow once"), "{text}");
        assert!(!text.contains("?"), "no confirming marker after resolve");
    }
}
