//! The inline Composer overlay popups (ADR-0051): the fuzzy '/' palette (System B) + committed-command numbered dialog (System A), their suggestion/label rendering (qwen SuggestionsDisplay/PrepareLabel), and the popup box sizing.
//!
//! Split from the components god module by rendering responsibility; shared
//! primitives arrive via `use super::*`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::ui::completion;
use crate::ui::composer::{self, OverlayStatus, OverlayView};
use crate::ui::slash;
use crate::ui::theme::Theme;
use crate::view_model::{RowRole, SelectorRow};

use super::approval::{SELECTION_GUTTER_WIDTH, SELECTION_MARKER};
use super::style::{accent_style, primary_style, secondary_style, success_style, tui_color};
use super::text::{padded, push_cols};

/// Computes the bounding rect for the Composer overlay popup: body rows plus
/// top/bottom border, capped at `body_cap` so a long list never eats the
/// screen, positioned just above `anchor_y` and horizontally centered within
/// `area`. Pure - no frame access. `body_len` is the number of content lines
/// the popup will hold; `body_cap` is the most body rows it may occupy (System
/// A dialogs cap at [`POPUP_MAX_ROWS`] and scroll; System B is already windowed
/// to its suggestions + `▲/▼`/counter chrome, so it caps higher).
pub(super) fn popup_rect(anchor_y: u16, area: Rect, body_len: usize, body_cap: u16) -> Rect {
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
pub(super) fn selector_popup_title(command: &str) -> String {
    slash::lookup(command)
        .map(|c| c.list_title.to_string())
        .unwrap_or_else(|| command.to_string())
}

/// The body lines a status line spells: a Loading or Failed dialog draws one
/// muted/error line. `Ready` returns `None` (the caller draws the rows). Pure.
pub(super) fn dialog_status_line(
    status: &OverlayStatus,
    title: &str,
    theme: &Theme,
) -> Option<Line<'static>> {
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
pub(super) struct PopupDraw {
    title: String,
    lines: Vec<Line<'static>>,
    /// `Some` for System A (the box re-scrolls to this active row); `None` for
    /// System B (already windowed).
    scroll_active: Option<usize>,
    body_cap: u16,
}

/// The System B palette body cap: the [`MAX_SUGGESTIONS`] window plus the three
/// chrome rows (`▲`, `▼`, and the `(n/m)` counter) it may add.
pub(super) const MENU_BODY_CAP: u16 = MAX_SUGGESTIONS as u16 + 3;

/// The System B palette's cursor state (Parameter Object): the highlighted row,
/// the scroll-window top, and whether the active row is expanded (`←/→`).
/// Bundled so the palette draw pipeline stays integration steps.
#[derive(Debug, Clone, Copy)]
pub(super) struct MenuCursor {
    active: usize,
    scroll: usize,
    expanded: bool,
}

impl MenuCursor {
    /// A cursor at `active`/`scroll` that is NOT expanded (the AT file picker,
    /// which has no `←/→` expand). The single shared constructor so the two
    /// palette systems build the cursor one way (BP-009: no duplicated struct
    /// literal).
    fn collapsed(active: usize, scroll: usize) -> Self {
        MenuCursor {
            active,
            scroll,
            expanded: false,
        }
    }

    /// A cursor at `active`/`scroll` with an explicit `expanded` flag (the `/`
    /// palette, which honours `←/→`). Struct-update over [`MenuCursor::collapsed`]
    /// so the shared fields live in one place.
    fn with_expanded(active: usize, scroll: usize, expanded: bool) -> Self {
        MenuCursor {
            expanded,
            ..MenuCursor::collapsed(active, scroll)
        }
    }
}

// The System B (`/` palette) draw plan: the color-only suggestion rows +
// windowing chrome; the box never re-scrolls (`scroll_active` None).
pub(super) fn menu_draw(
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
pub(super) fn at_draw(
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
pub(super) fn searching_line(theme: &Theme) -> Line<'static> {
    Line::styled(
        "searching…",
        Style::default()
            .fg(tui_color(theme.muted))
            .add_modifier(Modifier::ITALIC),
    )
}

/// The System A (numbered `›` dialog) content spec (Parameter Object): the
/// committed command's name, its load [`OverlayStatus`], the [`SelectorRow`]s to
/// list, and the highlighted `active` row. Bundled so [`dialog_draw`] takes one
/// borrow rather than a four-wide data clump (the `OverlayView::Dialog` fields
/// travel together at the one call site).
pub(super) struct DialogSpec<'a> {
    command: &'a str,
    status: &'a OverlayStatus,
    rows: &'a [SelectorRow],
    active: usize,
}

// The System A (numbered `›` dialog) draw plan: a status line, or the numbered
// rows the box scrolls to keep the active row visible.
pub(super) fn dialog_draw(spec: &DialogSpec<'_>, inner_width: u16, theme: &Theme) -> PopupDraw {
    let title = selector_popup_title(spec.command);
    let lines = match dialog_status_line(spec.status, &title, theme) {
        Some(line) => vec![line],
        None => dialog_rows(spec.rows, spec.active, inner_width, theme),
    };
    PopupDraw {
        title,
        lines,
        scroll_active: Some(spec.active),
        body_cap: POPUP_MAX_ROWS,
    }
}

/// The columns the Composer overlay popup spends on box chrome: the two `│`
/// border columns plus one `paddingX` column on each side, so the inner content
/// width is the popup width less this. Four columns total.
pub(super) const POPUP_BOX_CHROME: u16 = 4;

/// The inline Composer overlay popup (ADR-0051): a compact bordered list
/// anchored just above `anchor_y`. The TWO systems render distinctly - `Menu`
/// (System B) is the fuzzy `/` palette ([`menu_draw`]), `Dialog` (System A) a
/// committed command's numbered `›` list ([`dialog_draw`]). This is the one
/// integration step: pick the draw plan, size the box, blit the scrolled
/// window (IOSP). Inline and height-bounded - never the full screen.
pub(super) fn render_composer_popup(
    frame: &mut Frame,
    anchor_y: u16,
    area: Rect,
    view: &OverlayView,
    theme: &Theme,
) {
    let inner_width = area.width.saturating_sub(POPUP_BOX_CHROME).max(1);
    let plan = match view {
        OverlayView::Menu {
            suggestions,
            active,
            scroll,
            query: _,
            expanded,
        } => menu_draw(
            suggestions,
            MenuCursor::with_expanded(*active, *scroll, *expanded),
            inner_width,
            theme,
        ),
        OverlayView::Dialog {
            command,
            status,
            rows,
            active,
            detail: _,
        } => dialog_draw(
            &DialogSpec {
                command,
                status,
                rows,
                active: *active,
            },
            inner_width,
            theme,
        ),
        OverlayView::AtFiles {
            suggestions,
            active,
            scroll,
            query: _,
            loading,
        } => at_draw(
            suggestions,
            MenuCursor::collapsed(*active, *scroll),
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
pub(super) fn suggestion_rows(
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
pub(super) fn up_arrow_line(frame: &SuggestionFrame, theme: &Theme) -> Option<Line<'static>> {
    frame
        .show_up
        .then(|| Line::styled("▲", primary_style(theme)))
}

/// The windowed suggestion rows, each with its active flag resolved against
/// `frame.start` (a pure row builder).
pub(super) fn suggestion_body_lines(
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
pub(super) fn down_arrow_line(frame: &SuggestionFrame, theme: &Theme) -> Option<Line<'static>> {
    frame
        .show_down
        .then(|| Line::styled("▼", secondary_style(theme)))
}

/// The trailing `(active+1/total)` counter, present only when the list
/// overflows the window (a one-branch pure row builder).
pub(super) fn counter_line(
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
pub(super) struct SuggestionFrame {
    start: usize,
    end: usize,
    cmd_col: usize,
    show_up: bool,
    show_down: bool,
    show_counter: bool,
}

pub(super) fn suggestion_frame(
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
pub(super) fn no_matches_line(theme: &Theme) -> Line<'static> {
    Line::styled(
        "no matches",
        Style::default()
            .fg(tui_color(theme.muted))
            .add_modifier(Modifier::ITALIC),
    )
}

/// The width of the ` → `/` ← ` expand affordance (qwen SuggestionsDisplay), so
/// the label column can reserve room for it when a long row would show it.
pub(super) const EXPAND_AFFORDANCE_COLS: usize = 3;

/// The command column width (qwen `commandColumnWidth`): the widest label,
/// floored at one column. Capped at HALF the popup width when a second
/// (description) column shares the row - the slash palette - to leave that
/// column room. When every suggestion's description is EMPTY (the AT file
/// picker: paths, no descriptions), there is no second column to reserve for, so
/// the label column uses the FULL inner width and a long path renders whole
/// instead of chopped at width/2 - minus the ` → ` affordance's columns when a
/// long row could show it, so the affordance never falls off the row's end. Pure.
pub(super) fn command_column_width(suggestions: &[completion::Suggestion], width: usize) -> usize {
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
pub(super) struct RowState {
    active: bool,
    expanded: bool,
}

/// One System B suggestion row: the label (fuzzy match inverted) in the command
/// column, padded to the boundary, then the description in the second column.
/// The active row reads `text.accent`, the rest `text.secondary`.
pub(super) fn suggestion_row(
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
pub(super) fn label_is_long(label: &str) -> bool {
    label.chars().count() >= completion::MAX_WIDTH
}

/// Pushes a suggestion's label with its fuzzy match window drawn INVERTED (qwen
/// `PrepareLabel`: the match substring reversed against the row color). Returns
/// the new used-column count. The match window is `[start, end)` char indices
/// over the label; when absent the label draws plain. When the label is long
/// (`>= MAX_WIDTH`) and NOT `is_expanded`, it collapses to a truncated window
/// (qwen `PrepareLabel` cases 1-3), so the row fits; `is_expanded` shows it in
/// full.
pub(super) fn push_label_with_match(
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
pub(super) fn prepare_label(
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
            with_ellipsis_suffix(&slice(0, completion::MAX_WIDTH))
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
            with_ellipsis_suffix(&slice(m_start, cut)),
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

/// The qwen `PrepareLabel` ASCII ellipsis (three dots, NOT the single `…` glyph
/// the width-clip path uses): the mid-label elision marker a collapsed fuzzy
/// window shows at its clipped ends. Its char length is [`ELLIPSIS_LEN`].
pub(super) const ELLIPSIS: &str = "...";

/// The char length of the ASCII [`ELLIPSIS`]: the count of chars a prefix/suffix
/// elision replaces (qwen's `.slice(3)` / `.slice(0, -3)`).
pub(super) const ELLIPSIS_LEN: usize = ELLIPSIS.len();

/// `s` with the ASCII [`ELLIPSIS`] appended (qwen's `<slice> + '...'`).
pub(super) fn with_ellipsis_suffix(s: &str) -> String {
    format!("{s}{ELLIPSIS}")
}

// Replaces the first [`ELLIPSIS_LEN`] chars of `s` with `...` (qwen `'...' +
// before.slice(3)`), or `...` when shorter than that.
pub(super) fn elide_prefix(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= ELLIPSIS_LEN {
        format!(
            "{ELLIPSIS}{}",
            chars[ELLIPSIS_LEN..].iter().collect::<String>()
        )
    } else {
        ELLIPSIS.to_string()
    }
}

// Replaces the last [`ELLIPSIS_LEN`] chars of `s` with `...` (qwen `after.slice(0,
// -3) + '...'`), or `...` when shorter than that.
pub(super) fn elide_suffix(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= ELLIPSIS_LEN {
        with_ellipsis_suffix(
            &chars[..chars.len() - ELLIPSIS_LEN]
                .iter()
                .collect::<String>(),
        )
    } else {
        ELLIPSIS.to_string()
    }
}

/// A numbered-row label `N.` (qwen `showNumbers`): the dot-suffixed number every
/// selection/dialog/sticky row draws in its number column. The ONE place the
/// label format lives, so the number-field width math and the row draw agree
/// (BP-010).
pub(super) fn number_label(n: usize) -> String {
    format!("{n}.")
}

/// The System A (numbered `›` dialog) rows for a committed command's list
/// (ADR-0051): the `selection_rows` shape, but over [`SelectorRow`]s whose role
/// decides the disabled mask (headers/greyed rows are dim + unnavigable, the
/// active navigable row is the `›`-marked success-green one) and whose hint
/// trails dimmed. Numbered per the row's position, matching the digit
/// quick-select.
pub(super) fn dialog_rows(
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
    let num_field = number_label(rows.len()).width();
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
                let num = number_label(i + 1);
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
pub(super) const MAX_SUGGESTIONS: usize = completion::MAX_SUGGESTIONS_TO_SHOW;

/// The most body rows the Slash popup shows before it scrolls internally - keeps
/// the overlay compact even against a long model list.
pub(super) const POPUP_MAX_ROWS: u16 = 8;
