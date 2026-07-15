//! Generic filterable selector — a PURE, reusable single-select list (ADR-0001's
//! TEA shape, ADR-0019: no ratatui/crossterm here). It is the one widget behind
//! both the Slash Command menu (ADR-0032) and any command's own list (the
//! `/model` model list, a future `/theme` theme list): all "filter a list, pick
//! one" the same shape.
//!
//! The model is a cursor over [`SelectorRow`]s. The selector does NOT own the
//! filter text — the Composer drives it, so the caller passes the current
//! filter into every query. [`Selector::filtered`] is the case-insensitive
//! substring view over `label`; [`Selector::handle_nav`] folds a navigation key
//! against that filtered view and either moves the cursor (`None`) or resolves
//! with a [`SelectorOutcome`].

use crate::ui::transcript::Key;

/// One row in a [`Selector`]: `value` is what a [`SelectorOutcome::Select`]
/// returns, `label` is shown and filtered on, and `hint` is optional secondary
/// text (a command's help, a "(current)" marker) that never affects filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorRow {
    pub value: String,
    pub label: String,
    pub hint: Option<String>,
}

impl SelectorRow {
    /// A row from a value, label, and optional hint.
    pub fn new(value: impl Into<String>, label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow {
            value: value.into(),
            label: label.into(),
            hint,
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
/// `rows` — filtering narrows what the cursor may land on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub rows: Vec<SelectorRow>,
    pub cursor: usize,
}

impl Selector {
    /// A selector over `rows`, cursor on the first row.
    pub fn new(rows: Vec<SelectorRow>) -> Self {
        Selector { rows, cursor: 0 }
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

    /// Folds one navigation key against the FILTERED view: Up/WheelUp and
    /// Down/WheelDown move the cursor within that view, saturating at both ends;
    /// Enter resolves to [`SelectorOutcome::Select`] of the highlighted filtered
    /// row (a no-op / `None` on an empty filtered list); Escape resolves to
    /// [`SelectorOutcome::Cancel`]. Every other key is ignored.
    ///
    /// The cursor is clamped to the filtered length first, so a filter that
    /// shrank the list since the last fold cannot leave the cursor dangling
    /// past the end.
    pub fn handle_nav(&mut self, key: Key, filter: &str) -> Option<SelectorOutcome> {
        let len = self.filtered(filter).len();
        self.clamp(len);
        match key {
            Key::ArrowUp | Key::WheelUp => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            Key::ArrowDown | Key::WheelDown => {
                if self.cursor + 1 < len {
                    self.cursor += 1;
                }
                None
            }
            Key::Enter => self
                .filtered(filter)
                .get(self.cursor)
                .map(|row| SelectorOutcome::Select(row.value.clone())),
            Key::Escape => Some(SelectorOutcome::Cancel),
            _ => None,
        }
    }

    // Keep the cursor inside a filtered view of length `len`: clamp to the last
    // row, or to 0 when the filter matches nothing.
    fn clamp(&mut self, len: usize) {
        self.cursor = self.cursor.min(len.saturating_sub(1));
    }
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
