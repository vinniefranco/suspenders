//! The inline PENDING region (ADR-0046): the uncommitted transcript tail
//! rendering (bottom-anchored + top-clipped), the flat-footer/composer/overlay
//! assembly of `render_pending`, the live reasoning tail + spinner, and the
//! committed-slice blit + Ctrl-S peek that freeze rows into native scrollback.
//! Split from the components god module; the shared cache infra (`RenderCache`,
//! `Toggles`, `wrapped_count`, `CONTENT_MARGIN`) stays in the parent and arrives
//! via `use super::*`.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::plan::TodoItem;
use crate::ui::composer::{self, ComposerLayout, ComposerView, OverlayView};
use crate::ui::lull;
use crate::ui::screen::{Screen, Status};
use crate::ui::theme::Theme;
use crate::view_model::TranscriptItem;

use super::approval::question_modal_lines;
use super::composer_input::{COMPOSER_CHROME_ROWS, render_composer};
use super::footer::{FooterCtx, render_footer};
use super::overlay::{render_help_overlay, render_mcp_dialog};
use super::popup::render_composer_popup;
use super::render_cache::RenderCache;
use super::sticky::{
    render_sticky_todos, sticky_box_area, sticky_fits, sticky_todos, sticky_todos_height,
};
use super::style::{secondary_style, tui_color};
use super::text::{text_rows, truncate_visual};
use super::tool_group::{
    Approving, GroupedRows, grouped_rows, grouped_rows_with_approval, newest_live_tool_index,
};
use super::{CONTENT_MARGIN, Toggles, wrapped_count};

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
pub(super) fn frame_chunks(
    area: Rect,
    sticky_rows: usize,
    composer_rows: usize,
) -> std::rc::Rc<[Rect]> {
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
pub(super) fn capped_composer_height(layout: &ComposerLayout, frame_height: usize) -> usize {
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
pub(super) struct PendingLayout<'a> {
    pub(super) body: Rect,
    pub(super) sticky_box: Rect,
    pub(super) sticky_items: Option<&'a [TodoItem]>,
    pub(super) status: Rect,
    pub(super) composer: Rect,
    pub(super) draft: ComposerLayout,
    pub(super) popup_top: u16,
}

/// Operation (IOSP): the pending region's geometry for this frame. Wraps the
/// draft, caps the composer zone, decides the sticky "Current tasks" box
/// (ADR-0048) - reserved only when it fits alongside the status row, composer,
/// and one body row (ADR-0029 measure == draw) - and splits `area` into the
/// four zones. Pure: no frame access, no drawing.
pub(super) fn pending_layout<'a>(
    area: Rect,
    view: &ComposerView<'_>,
    t: &'a Screen,
) -> PendingLayout<'a> {
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
pub(super) fn render_sticky_slot(
    frame: &mut Frame,
    area: Rect,
    items: Option<&[TodoItem]>,
    theme: &Theme,
) {
    if let Some(items) = items {
        render_sticky_todos(frame, area, items, theme);
    }
}

/// The Composer overlay slot: draws the popup when an overlay is open, else
/// nothing. The presence branch lives HERE, so [`render_pending`] calls it
/// unconditionally (IOSP).
pub(super) fn render_composer_popup_slot(
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
pub(super) fn render_pending_body(
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
pub(super) fn pending_body_lines(
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
pub(super) fn render_pending_body_at(
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
pub(super) fn append_live(lines: &mut Vec<Line<'static>>, entry: &[Line<'static>]) {
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
pub(super) struct PendingClip {
    /// The stack's total wrapped rows, echoed back for the caller's return value.
    pub(super) total_lines: usize,
    /// Content Paragraph scroll offset (the top-clipped row count, saturated).
    pub(super) scroll: u16,
    pub(super) content_draw: Rect,
    /// The `… Ctrl-S to show more` marker row, present only on overflow.
    pub(super) marker_draw: Option<Rect>,
}

/// Operation (IOSP): the pure anchor/clip math for a pending body of
/// `total_lines` wrapped rows in a `content_area` inside the zone `area`. When the
/// stack overflows, keep the LAST `height` rows (drop from the top, qwen's
/// `overflowDirection:"top"`) and reserve the top row for the overflow marker; when
/// it fits, bottom-anchor it via `pad_top`. No frame access, no side effects.
pub(super) fn anchor_clip(total_lines: usize, area: Rect, content_area: Rect) -> PendingClip {
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
pub(super) fn draw_overflow_marker(frame: &mut Frame, area: Rect, theme: &Theme) {
    let marker_style = Style::default()
        .fg(tui_color(theme.muted))
        .add_modifier(Modifier::ITALIC);
    frame.render_widget(
        Paragraph::new(Line::styled("… Ctrl-S to show more", marker_style)),
        area,
    );
}

/// The milliseconds-per-second divisor used when converting `quiet_ticks` (each
/// tick is `TICK_MS` ms) into an elapsed-seconds figure for the lull timer.
pub(super) const MILLIS_PER_SEC: u64 = 1_000;

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
pub(super) fn blit_body_lines(buf: &mut Buffer, lines: Vec<Line<'static>>) {
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
pub(super) fn live_thinking_lines(
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
pub(super) const TOKEN_K: u64 = 1_000;
/// The `m` (million) grouping unit: at/above it, `format_token_count` renders
/// `N.Nm` (qwen `value >= 1_000_000 -> (value/1_000_000).toFixed(1) + "m"`).
pub(super) const TOKEN_M: u64 = 1_000_000;
/// The threshold at/above which `format_token_count` drops the decimal (`Nk`),
/// and below which it shows one decimal (`N.Nk`).
pub(super) const TOKEN_K_DECIMAL_LIMIT: u64 = 10_000;
/// The hundredths divisor used to round a token count to one decimal `k`: `count
/// / 100` rounded, then `/ 10`, matches JS `(count/1000).toFixed(1)`.
pub(super) const TOKEN_HUNDREDTHS: f64 = 100.0;
/// The tenths divisor completing the one-decimal `k` rounding.
pub(super) const TOKEN_TENTHS: f64 = 10.0;

/// A compact token count (qwen `formatTokenCount`, statusLinePresets.ts:217): the
/// bare number under 1000, `N.Nk` (one decimal, rounded) from 1000 to 9999, `Nk`
/// (floored) from 10000 to 999999, and `N.Nm` (one decimal, rounded) at 1000000
/// and above (qwen `2_400_000 -> "2.4m"`). Used by the spinner's `↑ 1.2k tokens`
/// figure.
pub(super) fn format_token_count(count: u64) -> String {
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
pub(super) struct SpinnerState<'a> {
    pub(super) subject: Option<&'a str>,
    pub(super) tokens: Option<u64>,
    pub(super) receiving: bool,
}

/// The running spinner line (qwen `LoadingIndicator.tsx`, ADR-0041/0048): a
/// braille [`SPINNER`] frame, the phrase (the current lull scene content - a
/// deliberate divergence from qwen's `usePhraseCycler`, kept for the whimsy; the
/// [`SpinnerState::subject`] wins when `Some`), then the cancel group
/// `(<elapsed> [· <arrow> <tokens> tokens] · esc to cancel)` in secondary.
/// paddingLeft 2. Every produced row is truncated to `width` so it stays one
/// visual row (measure==draw, ADR-0029). Empty when the lull is still settling
/// (no phrase yet).
pub(super) fn spinner_line(
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

/// The running-spinner animation frames (braille), advanced by the adapter's
/// animation tick while a Run is running.
pub(super) const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How many source rows of the live reasoning the rolling tail shows under the
/// `✦ Thinking` header (the short reasoning tail). Tunable.
pub(super) const THINKING_TAIL_ROWS: usize = 3;
