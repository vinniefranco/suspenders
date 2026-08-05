use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::plan::{TodoItem, TodoStatus};
use crate::ui::screen::PendingApproval;
use crate::ui::theme::Theme;
use crate::view_model::{Tone, TranscriptItem};

use super::approval::approval_block_rows;
use super::diff::{diff_elided_tail, diff_lines};
use super::picker::tool_desc;
use super::style::{
    accent_style, error_style, primary_style, secondary_style, success_style, symbol_style,
    warning_style,
};
use super::text::{push_cols, wrap_words};

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
/// a clean `✓ todo_write` header row with an EMPTY description - the Transcript
/// store dropped the raw JSON args when it swapped the Tool Result for a
/// [`Todo`], so there is nothing to leak - then one circle-glyph row per item
/// indented under the 3-wide marker column. The glyph is
/// [`crate::plan::TodoStatus::glyph`]
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
        // Housekeeping/Plain both read the quiet `●` info glyph, secondary.
        _ => ("●", secondary_style(theme)),
    }
}
