use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::plan::TodoItem;
use crate::ui::composer::{self, ComposerLayout, ComposerView, OverlayView};
use crate::ui::screen::{Screen, Status};
use crate::ui::theme::Theme;

use super::composer_input::{COMPOSER_CHROME_ROWS, render_composer};
use super::footer::{FooterCtx, render_footer};
use super::overlay::{plan_modal_lines, question_modal_lines, render_help_overlay};
use super::popup::render_composer_popup;
use super::render_cache::RenderCache;
use super::scroll::{ScrollIntent, draw_overflow_marker, scrolled_clip};
use super::spinner::{SpinnerState, live_thinking_lines, spinner_line};
use super::sticky::{
    render_sticky_todos, sticky_box_area, sticky_fits, sticky_todos, sticky_todos_height,
};
use super::tool_group::{
    Approving, CONTENT_MARGIN, GroupedRows, Toggles, content_width, grouped_rows_with_approval,
    newest_live_tool_index, wrapped_count,
};

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
    let sticky_items =
        (t.pending_approval.is_none() && t.pending_question.is_none() && t.pending_plan.is_none())
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
pub(super) fn render_pending_body(
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

    // The plan modal (ADR-0067, qwen `exit_plan_mode`): a standalone bordered box
    // appended BOTTOM-MOST, same as the question modal - the Run is waiting on the
    // USER, so the plan + its outcome rows are the salient content the top-clip
    // must never eat. Not tied to a transcript ToolCall, so it draws as its own
    // box. `None` when no plan is pending, so the pending body stays byte-identical
    // to the committed blit (which never carries it).
    if let Some(pending) = t.pending_plan.as_ref() {
        append_live(&mut lines, &plan_modal_lines(pending, width, theme));
    }
    lines
}

/// Draws the body starting AT an explicit item index `hw`: it emits the settled
/// tail `items[hw..]` plus the live stream, bottom-anchored and top-clipped
/// (ADR-0046). [`render_pending_body`] calls this with `hw = 0` so the WHOLE
/// transcript renders in the fullscreen viewport; the parameter is kept so tests
/// can render a partial tail.
pub(super) fn render_pending_body_at(
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
pub(super) fn append_live(lines: &mut Vec<Line<'static>>, entry: &[Line<'static>]) {
    if entry.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(Line::default());
    }
    lines.extend(entry.iter().cloned());
}
