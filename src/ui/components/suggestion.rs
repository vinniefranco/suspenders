use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::ui::completion;
use crate::ui::theme::Theme;

use super::popup::{MAX_SUGGESTIONS, MenuCursor};
use super::style::{accent_style, primary_style, secondary_style, tui_color};
use super::text::push_cols;

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
    // The command column must fit the name AND its ` <argument-hint>` (a skill's
    // hint renders inside this column, after the name), so a hinted row's width
    // includes the leading space + the hint.
    let max_label = suggestions
        .iter()
        .map(|s| s.label.width() + s.argument_hint.as_deref().map_or(0, |h| 1 + h.width()))
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
    // The skill command's `argument-hint` (ADR-0058, qwen `/<name>
    // <argument-hint>`): rendered muted right after the name, inside the command
    // column, so the palette advertises what the command takes. Display-only.
    if let Some(hint) = &s.argument_hint {
        used = push_cols(&mut spans, " ", Style::default(), used, width);
        used = push_cols(&mut spans, hint, secondary_style(theme), used, width);
    }
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
