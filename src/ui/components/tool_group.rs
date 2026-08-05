use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::ui::screen::PendingApproval;
use crate::ui::theme::Theme;
use crate::view_model::TranscriptItem;

use super::RenderCache;
use super::box_draw::box_row;
use super::style::{border_style, symbol_style, warning_style};
use super::tool_body::{confirming_inner_lines, is_shell_tool, is_tool_item};

/// The content side margin (columns): qwen `HistoryItemDisplay` wraps every item
/// in `marginLeft:2, marginRight:2` (HistoryItemDisplay.tsx:64), so content is
/// the frame width minus a 2-col left AND 2-col right margin. `pub(crate)` so the
/// adapter shares the same margin the Pending-tail render uses.
pub(crate) const CONTENT_MARGIN: u16 = 2;

/// The widest readable content is drawn (columns), matching qwen's
/// `mainAreaWidth = min(terminalWidth - 4, 100)` (AppContainer.tsx): on an
/// ultrawide terminal, prose/diffs/tool output stay legible left-aligned at 100
/// columns instead of stretching edge to edge. Full-width chrome (the footer rule)
/// is sized separately and is NOT bound by this cap.
pub(super) const MAX_CONTENT_WIDTH: u16 = 100;

/// The readable-content width for a zone `area_width`: the frame width minus both
/// [`CONTENT_MARGIN`]s, capped at [`MAX_CONTENT_WIDTH`] (qwen `mainAreaWidth`).
/// The ONE place the cap lives, so a zone's measure and draw agree (measure==draw,
/// ADR-0029). Below the cap it is exactly `area_width - 2*CONTENT_MARGIN`, so
/// narrow terminals are unchanged.
pub(super) fn content_width(area_width: u16) -> u16 {
    area_width
        .saturating_sub(2 * CONTENT_MARGIN)
        .min(MAX_CONTENT_WIDTH)
}

/// The blank `marginTop:1` separator row between committed items (qwen
/// `HistoryItemDisplay.tsx:64`; continuation types get `marginTop:0`). Emitted at
/// assembly by [`grouped_rows`], never cached.
pub(super) fn separator_row() -> Line<'static> {
    Line::default()
}

/// Folds the settled items `[hw..]` into the flat body via the collapsed-run fold
/// with NO open approval - the convenience wrapper the assembly tests measure
/// against. The production path is [`grouped_rows_with_approval`] (the pending
/// body always passes its approving state); this drops that arg so a test can
/// render a plain item slice. `items` is the FULL item list; only `[hw..]` is
/// emitted. `width` is the content width the cache was synced at.
#[cfg(test)]
pub(super) fn grouped_rows(
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
pub(super) struct Approving<'a> {
    pub(super) pending: &'a PendingApproval,
    /// The index into `items` of the confirming ToolCall.
    pub(super) call_index: usize,
}

/// The full input to the grouped-rows fold (a Parameter Object): the cache, the
/// item list + high-water mark, the content width, the theme, and the optional
/// inline approval. Bundled so [`grouped_rows_with_approval`] takes one borrow.
pub(super) struct GroupedRows<'a> {
    pub(super) cache: &'a RenderCache,
    pub(super) items: &'a [TranscriptItem],
    pub(super) hw: usize,
    pub(super) width: u16,
    pub(super) theme: &'a Theme,
    pub(super) approving: Option<&'a Approving<'a>>,
}

/// [`grouped_rows`] with an optional inline approval (ADR-0049): when `approving`
/// names a confirming ToolCall inside a tool group, that group renders with a
/// `warning` border, a `?` marker on the confirming call, and the approval block
/// (question + radio) appended inside its box.
pub(super) fn grouped_rows_with_approval(spec: &GroupedRows<'_>) -> Vec<Line<'static>> {
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
pub(super) enum Segment {
    /// A single non-tool item at this index (passes through as cached lines).
    Item(usize),
    /// A `[start, end)` run of tool items (rendered as one box).
    ToolGroup(usize, usize),
}

/// Operation (IOSP): segments the settled tail `[hw..]` into passthrough items
/// and maximal tool-runs (ADR-0047). Pure over the item sequence - no cache, no
/// draw - so the grouping rule is asserted without a frame.
pub(super) fn group_segments(items: &[TranscriptItem], hw: usize) -> Vec<Segment> {
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
pub(super) fn group_border_style(
    items: &[TranscriptItem],
    confirming: bool,
    theme: &Theme,
) -> Style {
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

/// The index of the newest live ToolCall (ADR-0049): the last
/// `TranscriptItem::ToolCall` still awaiting its result (a ToolResult supersedes
/// the call, so any surviving ToolCall item is unresolved). The confirming
/// Approval attaches here. `None` when no call is live.
pub(super) fn newest_live_tool_index(items: &[TranscriptItem]) -> Option<usize> {
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

/// The rows `lines` wrap to at `width`, measured by a throwaway `Paragraph`
/// with the SAME `Wrap { trim: false }` the viewport draws with - the window
/// math is only correct if measuring and drawing agree exactly.
pub(super) fn wrapped_count(lines: Vec<Line<'static>>, width: u16) -> usize {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width)
}
