use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::plan::{TodoItem, TodoStatus};
use crate::ui::theme::Theme;

use super::box_draw::{box_bottom, box_row, box_top};
use super::style::{border_style, primary_style, secondary_style, success_style};
use super::text::truncate_visual;
use super::tool_group::CONTENT_MARGIN;

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
pub(super) fn ordered_sticky_todos(items: &[TodoItem]) -> Vec<(usize, &TodoItem)> {
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
pub(super) fn sticky_todos(
    latest: Option<(usize, &[TodoItem])>,
    total: usize,
) -> Option<&[TodoItem]> {
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
pub(super) fn sticky_todos_height(count: usize) -> usize {
    let visible = count.min(STICKY_TODO_MAX_VISIBLE);
    let overflow = usize::from(count > STICKY_TODO_MAX_VISIBLE);
    2 + 1 + visible + overflow
}

/// The minimum body height the Pending tail keeps when the sticky box shows:
/// one row (`Constraint::Min(1)`) so the live tail never fully collapses.
const STICKY_MIN_BODY_ROWS: usize = 1;

/// Whether a `sticky_height`-row "Current tasks" box fits this frame alongside
/// the status row (1), the composer, and at least one body row. Pure predicate -
/// the show/hide guard so a short terminal drops the box rather than letting
/// Layout squeeze its zone below the measured height (ADR-0029 measure==draw).
pub(super) fn sticky_fits(
    frame_height: usize,
    sticky_height: usize,
    composer_height: usize,
) -> bool {
    let reserved = sticky_height
        .saturating_add(1) // status bar
        .saturating_add(composer_height)
        .saturating_add(STICKY_MIN_BODY_ROWS);
    reserved <= frame_height
}

/// The sticky box's draw rect inside its zone: the zone inset by the marginX 2
/// gutter (qwen `marginX={2}`) so the box aligns under the [`CONTENT_MARGIN`]
/// pending body. Pure.
pub(super) fn sticky_box_area(zone: Rect) -> Rect {
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
pub(super) fn render_sticky_todos(
    frame: &mut Frame,
    area: Rect,
    items: &[TodoItem],
    theme: &Theme,
) {
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
