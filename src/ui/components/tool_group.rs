//! Transcript-item + tool-group rendering (ADR-0047/0049): the grouped-rows
//! fold that boxes contiguous tool runs, the per-item `message_lines` dispatch,
//! the prefixed text/markdown builders, the tool status markers, and the inner
//! tool/diff/todo bodies. Split from the components god module by rendering
//! responsibility; shared primitives arrive via `use super::*`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::plan::{TodoItem, TodoStatus};
use crate::ui::screen::PendingApproval;
use crate::ui::theme::Theme;
use crate::view_model::{Tone, TranscriptItem};

use super::approval::approval_block_rows;
use super::box_draw::{box_bottom, box_row, box_top};
use super::diff::{diff_elided_tail, diff_lines, markdown_lines};
use super::header::{HeaderView, header_lines};
use super::render_cache::RenderCache;
use super::style::{
    accent_style, border_style, error_style, primary_style, secondary_style, success_style,
    symbol_style, warning_style,
};
use super::text::{push_cols, text_rows, wrap_words};

/// The blank `marginTop:1` separator row between committed items (qwen
/// `HistoryItemDisplay.tsx:64`; continuation types get `marginTop:0`). Emitted at
/// assembly by [`grouped_rows`], never cached.
pub(super) fn separator_row() -> Line<'static> {
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
pub(super) struct GroupCtx<'a> {
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
pub(super) fn render_segment(segment: Segment, ctx: &GroupCtx<'_>) -> Vec<Line<'static>> {
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
pub(super) fn render_tool_group(
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
    // The body rows are ALREADY border-wrapped by [`box_body_rows`] (a tool group
    // re-renders the confirming call fresh, so it can't go through [`frame_box`]);
    // stack the shared corner rows around them.
    let mut out = vec![box_top(inner, border)];
    out.extend(box_body_rows(&body));
    out.push(box_bottom(inner, border));
    out
}

/// The boxed-body render context (a Parameter Object for [`box_body_rows`]): the
/// tool items, their cached inner lines, the inner width, the border style, the
/// theme, and the optional confirming `(group-local index, pending)`. Bundled so
/// the body render takes one borrow.
pub(super) struct BoxBody<'a> {
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
pub(super) fn box_body_rows(body: &BoxBody<'_>) -> Vec<Line<'static>> {
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
pub(super) fn is_group_shell(item: &TranscriptItem) -> bool {
    match item {
        TranscriptItem::ToolCall { name, .. } | TranscriptItem::ToolResult { name, .. } => {
            is_shell_tool(name)
        }
        _ => false,
    }
}

/// The grey style settled Thinking draws in (qwen `ThinkMessage`
/// `text.secondary`). No italic - qwen thoughts read as plain grey markdown.
pub(super) fn thinking_style(theme: &Theme) -> Style {
    secondary_style(theme)
}

/// A settled Thinking item's lines (qwen `ThinkMessage`, ConversationMessages.tsx
/// :250): the grey `✦` U+2726 marker + grey markdown body, hung under the 2-col
/// prefix. qwen has NO per-thought collapse - a thought either shows in full or
/// is hidden entirely by compact mode (the show/hide decision is the caller's,
/// ADR-0052), so this always renders the full grey body.
pub(super) fn settled_thinking_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
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
pub(super) fn recolor_line(line: Line<'static>, style: Style) -> Line<'static> {
    Line::from(
        line.spans
            .into_iter()
            .map(|s| Span::styled(s.content, s.style.patch(style)))
            .collect::<Vec<_>>(),
    )
}

/// The lines one Transcript item renders as. `Diff` is the first-class rich item
/// of the semantic display vocabulary (ADR-0008): a titled diff whose lines take
/// a semantic tint from their [`DiffSide`]'s Theme slots and a syntect foreground.
/// `compact` (Ctrl+O, qwen `compactMode`, the core's `Screen::compact_mode`) hides
/// settled `Thinking` items ENTIRELY and folds a tool RESULT body (a multi-line
/// `Diff`, or a `Todo` checklist) to its header row - keeping the transcript terse
/// (ADR-0052). `content_width` is the `content_area` width the lines draw in.
pub(super) fn message_lines(
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
pub(super) fn marker_text(item: &TranscriptItem) -> &str {
    match item {
        TranscriptItem::Info { text } | TranscriptItem::Marker { text, .. } => text,
        _ => "",
    }
}

/// The 2-column prefix width every single-glyph committed prefix hangs under
/// (qwen `getPrefixWidth = stringWidth(prefix) + 1`, ConversationMessages.tsx:90
/// / StatusMessages.tsx:44): one glyph column plus one clear column so the body
/// never touches the marker. All Phase-2 prefixes (`>`,`✦`,`●`) are width-1.
pub(super) const PREFIX_WIDTH: usize = 2;

/// The row-0 prefix marker span: the `glyph` plus one clear column, styled by
/// `prefix_style` (qwen's `<prefix> ` lead). The ONE place the marker span is
/// built, so the two prefixed-line builders share its format (BP-010).
pub(super) fn prefix_marker_span(glyph: &str, prefix_style: Style) -> Span<'static> {
    Span::styled(format!("{glyph} "), prefix_style)
}

/// Lines for a prefixed PLAIN-TEXT item (qwen `PrefixedTextMessage`): the `glyph`
/// in `prefix_style` on row 0, then the wrapping text in `text_style` hung under
/// the [`PREFIX_WIDTH`] prefix column. Every produced [`Line`] is `<= content_width`
/// columns (the body wrapped to `content_width - PREFIX_WIDTH`, both prefix and
/// continuation padded to the prefix column), so the viewport's `Wrap` never
/// re-breaks it (measure==draw, ADR-0029).
pub(super) fn prefixed_text_lines(
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
                prefix_marker_span(glyph, prefix_style)
            } else {
                Span::raw(pad.clone())
            };
            out.push(Line::from(vec![lead, Span::styled(seg, text_style)]));
            first = false;
        }
    }
    if out.is_empty() {
        out.push(Line::from(prefix_marker_span(glyph, prefix_style)));
    }
    out
}

/// Lines for a prefixed MARKDOWN item (qwen `PrefixedMarkdownMessage`): the
/// `glyph` in `prefix_style` on the first body row, every row (row 0 and each
/// continuation) hung under the [`PREFIX_WIDTH`] prefix column. The markdown
/// `body` is already styled; this only prepends the marker/indent column. Because
/// the body was built at the reduced width by the cache, the prefixed lines stay
/// `<= content_width` (measure==draw, ADR-0029).
pub(super) fn prefixed_markdown_lines(
    glyph: &str,
    prefix_style: Style,
    body: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let pad = " ".repeat(PREFIX_WIDTH);
    let mut first = true;
    body.into_iter()
        .map(|line| {
            let lead = if first {
                prefix_marker_span(glyph, prefix_style)
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
pub(super) const STATUS_INDICATOR_WIDTH: usize = 3;

/// The rounded-box overhead subtracted from the box width to get the inner
/// content width: 1 border + 1 `paddingX` on each side (qwen `ToolMessage`
/// `paddingX={1}` inside a `borderStyle:"round"` box, ToolMessage.tsx:665). Four
/// columns total.
pub(super) const BOX_CHROME: usize = 4;

/// The inner content width tool items build at: the box width less the border +
/// padding chrome ([`BOX_CHROME`]), floored at 1.
pub(super) fn tool_inner_width(content_width: u16) -> u16 {
    content_width.saturating_sub(BOX_CHROME as u16).max(1)
}

/// Whether an item belongs to a tool group (grouped into the box at render):
/// ToolCall, ToolResult, a tool Diff, or a Todo list. The ONE membership
/// predicate the grouping fold ([`group_segments`]) keys on. A `Todo` is a
/// tool item so it renders INSIDE the same rounded box as the `todo_write` it
/// stands in for (ADR-0047/0048), the identity every consumer relies on:
/// committed and pending draw byte-identically down the same box path.
pub(super) fn is_tool_item(item: &TranscriptItem) -> bool {
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
pub(super) fn tool_inner_lines(
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
pub(super) fn confirming_inner_lines(
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
pub(super) fn tool_todo_lines(
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
pub(super) fn todo_item_rows(
    item: &TodoItem,
    content_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
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
pub(super) fn tool_diff_lines(
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
pub(super) fn indent_box_body(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
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
pub(super) fn tool_header_row(
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

/// A folded Diff's one-line row inside the box: the marker gutter, the title, and
/// the `· ^O expand` affordance, truncate-end at `inner_width`.
pub(super) fn tool_diff_fold_row(title: &str, inner_width: u16, theme: &Theme) -> Line<'static> {
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
pub(super) enum ToolMarker {
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
pub(super) fn tool_marker(marker: ToolMarker, name: &str, theme: &Theme) -> Span<'static> {
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
pub(super) fn is_shell_tool(name: &str) -> bool {
    matches!(name, "run_shell_command" | "shell" | "Shell")
}

/// A Marker's prefix glyph + style, chosen by its [`Tone`] (qwen `StatusMessages`
/// set): a Constrain marker (the loop-detector's run-close - a guard on the model)
/// reads the `△` U+25B3 warning status; a Steering marker the `●` info glyph in
/// the accent (the user's own voice reaching a running Run); everything else the
/// `●` info glyph, secondary/muted. Tone alone decides, never the text.
pub(super) fn marker_prefix_and_style(
    item: &TranscriptItem,
    theme: &Theme,
) -> (&'static str, Style) {
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

// Normalizes a `key_arg` for rendering: an absent OR empty arg both read as "no
// arg". The ONE place the display treats emptiness (the source rule lives in the
// core's `key_arg`, but a recovered call summary can still be empty).
pub(super) fn present_arg(key_arg: Option<&str>) -> Option<&str> {
    key_arg.filter(|a| !a.is_empty())
}

/// A tool result row's dim `description` (qwen `ToolInfo` description, shown after
/// the bold name): the salient `key_arg` and the result summary joined `arg ·
/// result`, dropping to bare `result` when there is no arg. The tool NAME is NOT
/// repeated here - `tool_header_row` draws it bold ahead of this.
pub(super) fn tool_desc(key_arg: Option<&str>, summary: &str) -> String {
    match present_arg(key_arg) {
        Some(arg) if summary.is_empty() => arg.to_string(),
        Some(arg) => format!("{arg} · {summary}"),
        None => summary.to_string(),
    }
}
