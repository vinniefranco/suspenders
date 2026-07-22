//! Generic filterable selector - a PURE, reusable single-select list (ADR-0001's
//! TEA shape, ADR-0019: no ratatui/crossterm here). It is the one widget behind
//! both the Slash Command menu (ADR-0032) and any command's own list (the
//! `/model` model list, a future `/theme` theme list): all "filter a list, pick
//! one" the same shape.
//!
//! The model is a cursor over [`SelectorRow`]s. The selector does NOT own the
//! filter text - the Composer drives it, so the caller passes the current
//! filter into every query. [`Selector::filtered`] is the case-insensitive
//! substring view over `label`; [`Selector::handle_nav`] folds a navigation key
//! against that filtered view and either moves the cursor (`None`) or resolves
//! with a [`SelectorOutcome`].

use crate::ui::screen::Key;

/// One row in a [`Selector`]: `value` is what a [`SelectorOutcome::Select`]
/// returns, `label` is shown and filtered on, and `hint` is optional secondary
/// text (a command's help, a "(current)" marker) that never affects filtering.
/// A non-`selectable` row (a Provider group header, an "unavailable" note)
/// renders and filters like any other but the cursor skips it and Enter never
/// picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorRow {
    pub value: String,
    pub label: String,
    pub hint: Option<String>,
    pub selectable: bool,
}

impl SelectorRow {
    /// A pickable row from a value, label, and optional hint.
    pub fn new(value: impl Into<String>, label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow {
            value: value.into(),
            label: label.into(),
            hint,
            selectable: true,
        }
    }

    /// A non-selectable row: shown (and filtered) but never picked - the
    /// navigation folds skip it. Carries no value; the optional hint rides
    /// dimmed like any other (an "unavailable" note's terse reason).
    pub fn header(label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow {
            value: String::new(),
            label: label.into(),
            hint,
            selectable: false,
        }
    }
}

/// How the selector resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorOutcome {
    /// Pick this row's `value`.
    Select(String),
    /// Leave without selecting (Escape).
    Cancel,
}

/// The selector's whole state: the rows and the highlighted index. The cursor
/// is an index into the FILTERED view (re-clamped on every fold), not into
/// `rows` - filtering narrows what the cursor may land on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub rows: Vec<SelectorRow>,
    pub cursor: usize,
}

impl Selector {
    /// A selector over `rows`, cursor on the first SELECTABLE row (the first
    /// row of a grouped list is a header).
    pub fn new(rows: Vec<SelectorRow>) -> Self {
        let cursor = rows.iter().position(|r| r.selectable).unwrap_or(0);
        Selector { rows, cursor }
    }

    /// The rows whose `label` contains `filter` (case-insensitive substring),
    /// in the original stable order. An empty filter shows every row.
    pub fn filtered(&self, filter: &str) -> Vec<&SelectorRow> {
        let needle = filter.to_lowercase();
        self.rows
            .iter()
            .filter(|row| row.label.to_lowercase().contains(&needle))
            .collect()
    }

    /// The display highlight into the filtered view: the cursor snapped onto a
    /// selectable row exactly as the next [`Selector::handle_nav`] fold will
    /// see it, so what renders reversed is what Enter picks.
    pub fn highlight(&self, filter: &str) -> usize {
        snap(&self.selectable_mask(filter), self.cursor)
    }

    /// Folds one navigation key against the FILTERED view: Up/WheelUp and
    /// Down/WheelDown move the cursor to the adjacent SELECTABLE row (headers
    /// and notes are skipped), saturating at both ends; Enter resolves to
    /// [`SelectorOutcome::Select`] of the highlighted filtered row (a no-op /
    /// `None` on an empty or all-header filtered list); Escape resolves to
    /// [`SelectorOutcome::Cancel`]. Every other key is ignored.
    ///
    /// The cursor is snapped to a selectable row first (clamped to the
    /// filtered length, then off any header), so a filter that shrank the
    /// list since the last fold cannot leave the cursor dangling or parked on
    /// a header.
    pub fn handle_nav(&mut self, key: Key, filter: &str) -> Option<SelectorOutcome> {
        let mask = self.selectable_mask(filter);
        self.cursor = snap(&mask, self.cursor);
        match key {
            Key::ArrowUp | Key::WheelUp => {
                if let Some(prev) = nearest(&mask, self.cursor, Direction::Up) {
                    self.cursor = prev;
                }
                None
            }
            Key::ArrowDown | Key::WheelDown => {
                if let Some(next) = nearest(&mask, self.cursor, Direction::Down) {
                    self.cursor = next;
                }
                None
            }
            Key::Enter => {
                if !mask.get(self.cursor).copied().unwrap_or(false) {
                    return None;
                }
                self.filtered(filter)
                    .get(self.cursor)
                    .map(|row| SelectorOutcome::Select(row.value.clone()))
            }
            Key::Escape => Some(SelectorOutcome::Cancel),
            _ => None,
        }
    }

    // Which rows of the filtered view the cursor may land on.
    fn selectable_mask(&self, filter: &str) -> Vec<bool> {
        self.filtered(filter).iter().map(|r| r.selectable).collect()
    }
}

// The direction a skip searches in.
enum Direction {
    Up,
    Down,
}

// The nearest selectable index strictly before/after `from`, or `None` when
// no selectable row lies that way (the cursor saturates in place).
fn nearest(mask: &[bool], from: usize, direction: Direction) -> Option<usize> {
    match direction {
        Direction::Up => (0..from).rev().find(|&i| mask[i]),
        Direction::Down => ((from + 1)..mask.len()).find(|&i| mask[i]),
    }
}

// Puts the cursor on a selectable row of a view with selectability `mask`:
// clamp into the view, keep a selectable spot, else the nearest selectable
// below, else above. An empty or all-header view returns the clamped index
// unchanged - Enter guards on the mask, so nothing is pickable there.
fn snap(mask: &[bool], cursor: usize) -> usize {
    let clamped = cursor.min(mask.len().saturating_sub(1));
    if mask.get(clamped).copied().unwrap_or(false) {
        return clamped;
    }
    nearest(mask, clamped, Direction::Down)
        .or_else(|| nearest(mask, clamped, Direction::Up))
        .unwrap_or(clamped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: &str) -> SelectorRow {
        SelectorRow::new(value, value, Some(format!("help for {value}")))
    }

    fn selector(values: &[&str]) -> Selector {
        Selector::new(values.iter().map(|v| row(v)).collect())
    }

    fn labels(rows: Vec<&SelectorRow>) -> Vec<String> {
        rows.iter().map(|r| r.label.clone()).collect()
    }

    #[test]
    fn new_starts_on_the_first_row() {
        assert_eq!(selector(&["a", "b", "c"]).cursor, 0);
    }

    #[test]
    fn an_empty_filter_shows_every_row_in_order() {
        let s = selector(&["model", "theme", "compact"]);
        assert_eq!(labels(s.filtered("")), vec!["model", "theme", "compact"]);
    }

    #[test]
    fn filtering_is_case_insensitive_substring_and_stable() {
        let s = selector(&["model", "theme", "compact"]);
        // Substring, not prefix: "e" matches model and theme (both contain
        // 'e'), "compact" does not.
        assert_eq!(labels(s.filtered("e")), vec!["model", "theme"]);
        // Case-insensitive.
        assert_eq!(labels(s.filtered("MO")), vec!["model"]);
        // No match yields an empty view.
        assert_eq!(labels(s.filtered("zzz")), Vec::<String>::new());
    }

    #[test]
    fn arrows_move_the_cursor_saturating_at_both_ends() {
        let mut s = selector(&["a", "b", "c"]);

        assert_eq!(s.handle_nav(Key::ArrowUp, ""), None);
        assert_eq!(s.cursor, 0, "up at the top stays");

        assert_eq!(s.handle_nav(Key::ArrowDown, ""), None);
        assert_eq!(s.cursor, 1);
        s.handle_nav(Key::ArrowDown, "");
        assert_eq!(s.cursor, 2);
        s.handle_nav(Key::ArrowDown, "");
        assert_eq!(s.cursor, 2, "down at the bottom stays");

        s.handle_nav(Key::ArrowUp, "");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn the_wheel_moves_the_cursor_like_the_arrows() {
        let mut s = selector(&["a", "b"]);
        assert_eq!(s.handle_nav(Key::WheelDown, ""), None);
        assert_eq!(s.cursor, 1);
        assert_eq!(s.handle_nav(Key::WheelDown, ""), None);
        assert_eq!(s.cursor, 1);
        assert_eq!(s.handle_nav(Key::WheelUp, ""), None);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn down_saturates_within_the_filtered_view_not_the_full_list() {
        let mut s = selector(&["model", "theme", "compact"]);
        // "o" matches only "model" and "compact" (both contain 'o'); "theme"
        // does not.
        assert_eq!(labels(s.filtered("o")), vec!["model", "compact"]);
        s.handle_nav(Key::ArrowDown, "o");
        assert_eq!(s.cursor, 1);
        s.handle_nav(Key::ArrowDown, "o");
        assert_eq!(s.cursor, 1, "saturates at the filtered end, not row 2");
    }

    #[test]
    fn a_shrinking_filter_re_clamps_the_cursor() {
        let mut s = selector(&["model", "theme", "compact"]);
        // Move to the last row of the unfiltered view.
        s.handle_nav(Key::ArrowDown, "");
        s.handle_nav(Key::ArrowDown, "");
        assert_eq!(s.cursor, 2);
        // A filter that leaves one row must pull the cursor back to it.
        assert_eq!(s.handle_nav(Key::ArrowUp, "theme"), None);
        assert_eq!(s.cursor, 0);
        assert_eq!(
            s.handle_nav(Key::Enter, "theme"),
            Some(SelectorOutcome::Select("theme".into()))
        );
    }

    #[test]
    fn enter_selects_the_highlighted_filtered_rows_value() {
        let mut s = selector(&["model", "theme", "compact"]);
        s.handle_nav(Key::ArrowDown, "");
        assert_eq!(
            s.handle_nav(Key::Enter, ""),
            Some(SelectorOutcome::Select("theme".into()))
        );
    }

    #[test]
    fn enter_on_an_empty_filtered_list_is_none() {
        let mut s = selector(&["model", "theme"]);
        assert_eq!(s.handle_nav(Key::Enter, "zzz"), None);
    }

    #[test]
    fn escape_cancels() {
        let mut s = selector(&["model"]);
        assert_eq!(s.handle_nav(Key::Escape, ""), Some(SelectorOutcome::Cancel));
    }

    #[test]
    fn other_keys_are_ignored() {
        let mut s = selector(&["a", "b"]);
        assert_eq!(s.handle_nav(Key::Char('x'), ""), None);
        assert_eq!(s.handle_nav(Key::Backspace, ""), None);
        assert_eq!(s.handle_nav(Key::PageUp, ""), None);
        assert_eq!(s.cursor, 0);
    }

    // --- non-selectable rows (group headers, notes) --------------------------

    fn grouped() -> Selector {
        Selector::new(vec![
            SelectorRow::header("local", None),
            row("local/qwen"),
            SelectorRow::header("anthropic", None),
            row("anthropic/claude-fable-5"),
            row("anthropic/claude-haiku-4-5"),
        ])
    }

    #[test]
    fn a_grouped_selector_opens_on_the_first_selectable_row() {
        assert_eq!(grouped().cursor, 1, "row 0 is a header");
        assert_eq!(grouped().highlight(""), 1);
    }

    #[test]
    fn navigation_skips_headers_in_both_directions() {
        let mut s = grouped();
        // Down from local/qwen (1) skips the anthropic header (2) to 3.
        s.handle_nav(Key::ArrowDown, "");
        assert_eq!(s.cursor, 3);
        // Up from 3 skips the header back to 1.
        s.handle_nav(Key::ArrowUp, "");
        assert_eq!(s.cursor, 1);
        // Up at the first selectable stays - the header above is not a stop.
        s.handle_nav(Key::ArrowUp, "");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn enter_never_picks_a_header() {
        // Only headers survive this filter (no model label contains "thro"
        // beyond anthropic's rows; use a filter matching the header alone).
        let mut s = Selector::new(vec![SelectorRow::header("anthropic", None)]);
        assert_eq!(s.handle_nav(Key::Enter, ""), None);
        assert_eq!(s.handle_nav(Key::Escape, ""), Some(SelectorOutcome::Cancel));
    }

    #[test]
    fn a_filter_that_lands_the_cursor_on_a_header_snaps_to_a_selectable_row() {
        let mut s = grouped();
        // Move deep, then filter down to the anthropic group: the stale
        // cursor is re-snapped onto a selectable row and Enter picks it.
        s.handle_nav(Key::ArrowDown, "");
        s.handle_nav(Key::ArrowDown, "");
        assert_eq!(s.cursor, 4);
        // Filter "anthropic" keeps the header + its two models (indexes 0..3);
        // cursor 4 clamps to 2, a selectable row.
        assert_eq!(s.highlight("anthropic"), 2);
        assert_eq!(
            s.handle_nav(Key::Enter, "anthropic"),
            Some(SelectorOutcome::Select("anthropic/claude-haiku-4-5".into()))
        );
    }

    #[test]
    fn headers_filter_like_any_other_row() {
        let s = grouped();
        assert_eq!(
            labels(s.filtered("anthropic")),
            vec![
                "anthropic",
                "anthropic/claude-fable-5",
                "anthropic/claude-haiku-4-5"
            ]
        );
    }

    #[test]
    fn navigation_on_an_empty_list_never_moves_or_selects() {
        let mut s = selector(&[]);
        assert_eq!(s.handle_nav(Key::ArrowDown, ""), None);
        assert_eq!(s.handle_nav(Key::ArrowUp, ""), None);
        assert_eq!(s.cursor, 0);
        assert_eq!(s.handle_nav(Key::Enter, ""), None);
        assert_eq!(s.handle_nav(Key::Escape, ""), Some(SelectorOutcome::Cancel));
    }
}
