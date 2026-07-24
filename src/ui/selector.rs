//! Generic filterable selector - a PURE, reusable single-select list (ADR-0001's
//! TEA shape, ADR-0019: no ratatui/crossterm here). It is the one widget behind
//! both the Slash Command menu (ADR-0032) and any command's own list (the
//! `/model` model list, the `/theme` theme list): all "filter a list, pick
//! one" the same shape.
//!
//! The model is a cursor over [`SelectorRow`]s, each playing a [`RowRole`] in
//! a GROUPED list: a header row starts a group that runs until the next
//! header, and its notes and members travel with it under filtering. A list
//! with no headers (the slash menu) is the degenerate case - every row its
//! own group - and filters exactly as a flat list. Two independent facts fall
//! out of a row's role: a CURSOR STOP (member or note) is where Up/Down land,
//! PICKABLE (member only) is what Enter resolves - a note under the cursor is
//! a deliberate no-op stop, because the popup window ends at the highlight,
//! so a group's trailing note is the anchor that scrolls the whole group into
//! view. The selector does NOT own the filter text - the Composer drives it,
//! so the caller passes the current filter into every query.
//! [`Selector::filtered`] is the group-aware case-insensitive substring view
//! over `label` (collapsed reveals capped at [`COLLAPSED_REVEAL_CAP`], the
//! excess counted on the group's note); [`Selector::handle_nav`] folds a
//! navigation key against that filtered view and either moves the cursor
//! (`None`) or resolves with a [`SelectorOutcome`].

use crate::ui::screen::Key;

/// The most collapsed rows one group reveals under a filter; the matched rest
/// surfaces as the overflow count on the group's note ([`FilteredRow`]).
/// Sized to the `/model` popup: its window shows POPUP_MAX_ROWS (8, in
/// `components.rs`) body rows ending at the highlight, so a fully-capped
/// group - header, five collapsed rows, the trailing note under the cursor -
/// fits whole.
pub const COLLAPSED_REVEAL_CAP: usize = 5;

/// The part a row plays in the grouped list. It decides two independent
/// facts: whether the row is a cursor stop ([`SelectorRow::is_stop`]: member
/// or note) and whether Enter may pick it ([`SelectorRow::pickable`]: member
/// only) - and [`Selector::filtered`] shows each role by its own rule (see
/// there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowRole {
    /// Starts a group (a Provider's name): the rows after it, until the next
    /// header, belong to it. Not a stop, never picked.
    Header,
    /// An annotation that travels with its group's header (an "unavailable"
    /// note); on its own with no header before it (a broken theme in the
    /// `/theme` list), it is its own group and filters on its own label. A
    /// cursor stop that Enter refuses: landing on a group's trailing note
    /// anchors the popup window - which ends at the highlight - so the whole
    /// group above it scrolls into view.
    Note,
    /// A pickable row - the only role Enter resolves.
    Member,
    /// A greyed, unpickable member (a credential-less built-in's Catalog
    /// model): hidden at the empty filter, revealed when its label matches,
    /// at most [`COLLAPSED_REVEAL_CAP`] per group. Not a stop.
    Collapsed,
}

/// One row in a [`Selector`]: `value` is what a [`SelectorOutcome::Select`]
/// returns, `label` is shown and filtered on, and `hint` is optional secondary
/// text (a command's help, a "(current)" marker, a note's terse reason) that
/// never affects filtering. `role` places the row in its group and decides
/// stop-ness and pickability - see [`RowRole`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorRow {
    pub value: String,
    pub label: String,
    pub hint: Option<String>,
    pub role: RowRole,
}

impl SelectorRow {
    /// A pickable row from a value, label, and optional hint.
    pub fn new(value: impl Into<String>, label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow {
            value: value.into(),
            label: label.into(),
            hint,
            role: RowRole::Member,
        }
    }

    /// A group-starting header (a Provider's name): shown (and it shows its
    /// group) but never picked, never a stop. No hint of its own - a group's
    /// annotations belong to its note.
    pub fn header(label: impl Into<String>) -> Self {
        SelectorRow::unpickable(RowRole::Header, label, None)
    }

    /// A group's annotation (an "unavailable" note whose hint is the terse
    /// reason): a cursor stop that Enter refuses.
    pub fn note(label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow::unpickable(RowRole::Note, label, hint)
    }

    /// A greyed member hidden until the filter matches its label (a
    /// credential-less built-in's Catalog model): never picked, never a stop.
    pub fn collapsed(label: impl Into<String>) -> Self {
        SelectorRow::unpickable(RowRole::Collapsed, label, None)
    }

    /// Whether Up/Down/Wheel may land here: members and notes.
    pub fn is_stop(&self) -> bool {
        matches!(self.role, RowRole::Member | RowRole::Note)
    }

    /// Whether Enter may resolve here: members only.
    pub fn pickable(&self) -> bool {
        self.role == RowRole::Member
    }

    // The shared base of every row Enter cannot pick: no value, role and
    // hint per constructor.
    fn unpickable(role: RowRole, label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow {
            value: String::new(),
            label: label.into(),
            hint,
            role,
        }
    }
}

/// One row of [`Selector::filtered`]'s view: the row itself, plus - on a
/// group's note - `overflow`, how many matched collapsed rows the
/// [`COLLAPSED_REVEAL_CAP`] truncated (what the render layer suffixes as
/// "· N more"). Truncation is filter-time knowledge, so it rides the view,
/// never the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredRow<'a> {
    pub row: &'a SelectorRow,
    pub overflow: Option<usize>,
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
    /// A selector over `rows`, cursor on the first CURSOR STOP of the
    /// empty-filter view - what first renders. The raw rows also hold
    /// collapsed rows the empty filter hides, so an index minted over them
    /// would drift off the view the cursor actually indexes.
    pub fn new(rows: Vec<SelectorRow>) -> Self {
        let mut selector = Selector { rows, cursor: 0 };
        selector.cursor = selector
            .filtered("")
            .iter()
            .position(|f| f.row.is_stop())
            .unwrap_or(0);
        selector
    }

    /// The group-aware filtered view, in the original stable order. Matching
    /// is a case-insensitive substring test on `label`; a group is a header
    /// plus the rows after it until the next header, and rows before the
    /// first header are each their own group (a headerless list filters as a
    /// flat one). Per role:
    /// - a member shows when the filter is empty or its label matches;
    /// - a collapsed member shows ONLY when its label matches a non-empty
    ///   filter - the empty filter hides it - and at most
    ///   [`COLLAPSED_REVEAL_CAP`] reveal per group, in order; the matched
    ///   rest becomes the `overflow` count on the group's note. Overflow
    ///   counts truncated MATCHES only: the empty filter reveals nothing, so
    ///   it truncates nothing;
    /// - a header and its notes show at the empty filter, and under a
    ///   non-empty one whenever ANY label in the group (their own included)
    ///   matches - they travel with their group's matches, and a group with
    ///   no match at all vanishes entirely.
    pub fn filtered(&self, filter: &str) -> Vec<FilteredRow<'_>> {
        let needle = filter.to_lowercase();
        let hits = |row: &SelectorRow| row.label.to_lowercase().contains(&needle);
        let mut view = Vec::new();
        let mut start = 0;
        while start < self.rows.len() {
            let end = if self.rows[start].role == RowRole::Header {
                self.rows[start + 1..]
                    .iter()
                    .position(|r| r.role == RowRole::Header)
                    .map(|offset| start + 1 + offset)
                    .unwrap_or(self.rows.len())
            } else {
                start + 1
            };
            let group = &self.rows[start..end];
            let group_hit = group.iter().any(&hits);
            let matched_collapsed = group
                .iter()
                .filter(|r| r.role == RowRole::Collapsed && hits(r))
                .count();
            // The truncation count, handed to the group's first note. Only a
            // non-empty filter reveals (and so truncates) collapsed rows.
            let mut overflow = (!filter.is_empty())
                .then(|| matched_collapsed.saturating_sub(COLLAPSED_REVEAL_CAP))
                .filter(|n| *n > 0);
            let mut revealed = 0;
            for row in group {
                let entry = match row.role {
                    RowRole::Header => (filter.is_empty() || group_hit).then_some(FilteredRow {
                        row,
                        overflow: None,
                    }),
                    RowRole::Note => (filter.is_empty() || group_hit).then(|| FilteredRow {
                        row,
                        overflow: overflow.take(),
                    }),
                    RowRole::Member => (filter.is_empty() || hits(row)).then_some(FilteredRow {
                        row,
                        overflow: None,
                    }),
                    RowRole::Collapsed => (!filter.is_empty()
                        && hits(row)
                        && revealed < COLLAPSED_REVEAL_CAP)
                        .then(|| {
                            revealed += 1;
                            FilteredRow {
                                row,
                                overflow: None,
                            }
                        }),
                };
                if let Some(entry) = entry {
                    view.push(entry);
                }
            }
            start = end;
        }
        view
    }

    /// The display highlight into the filtered view: the cursor snapped onto
    /// a cursor stop exactly as the next [`Selector::handle_nav`] fold will
    /// see it, so what renders reversed is where the cursor really is.
    pub fn highlight(&self, filter: &str) -> usize {
        snap(&self.stop_mask(filter), self.cursor)
    }

    /// Folds one navigation key against the FILTERED view: Up/WheelUp and
    /// Down/WheelDown move the cursor to the adjacent cursor stop (a member
    /// or a note; headers and collapsed rows are skipped), saturating at both
    /// ends; Enter resolves to [`SelectorOutcome::Select`] of the highlighted
    /// row's value when that row is pickable, and is a no-op (`None`) when it
    /// is not - a note under the cursor, or an empty filtered view; Escape
    /// resolves to [`SelectorOutcome::Cancel`]. Every other key is ignored.
    ///
    /// The cursor is snapped to a cursor stop first (clamped to the filtered
    /// length, then off any non-stop row), so a filter that shrank the list
    /// since the last fold cannot leave the cursor dangling or parked on a
    /// header.
    pub fn handle_nav(&mut self, key: Key, filter: &str) -> Option<SelectorOutcome> {
        let mask = self.stop_mask(filter);
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
            Key::Enter => self
                .filtered(filter)
                .get(self.cursor)
                .filter(|f| f.row.pickable())
                .map(|f| SelectorOutcome::Select(f.row.value.clone())),
            Key::Escape => Some(SelectorOutcome::Cancel),
            _ => None,
        }
    }

    // Which rows of the filtered view the cursor may land on (members and
    // notes).
    fn stop_mask(&self, filter: &str) -> Vec<bool> {
        self.filtered(filter)
            .iter()
            .map(|f| f.row.is_stop())
            .collect()
    }
}

// The direction a skip searches in.
enum Direction {
    Up,
    Down,
}

// The nearest cursor-stop index strictly before/after `from`, or `None` when
// no stop lies that way (the cursor saturates in place).
fn nearest(mask: &[bool], from: usize, direction: Direction) -> Option<usize> {
    match direction {
        Direction::Up => (0..from).rev().find(|&i| mask[i]),
        Direction::Down => ((from + 1)..mask.len()).find(|&i| mask[i]),
    }
}

// Puts the cursor on a stop of a view with stop-ness `mask`: clamp into the
// view, keep a stop, else the nearest stop below, else above. A view with no
// stop at all returns the clamped index unchanged - Enter guards on
// pickability, so nothing resolves there.
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

    fn labels(view: Vec<FilteredRow<'_>>) -> Vec<String> {
        view.iter().map(|f| f.row.label.clone()).collect()
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

    // --- grouped lists (headers, notes as stops) -----------------------------

    fn grouped() -> Selector {
        Selector::new(vec![
            SelectorRow::header("local"),
            row("local/qwen"),
            SelectorRow::header("anthropic"),
            row("anthropic/claude-fable-5"),
            row("anthropic/claude-haiku-4-5"),
        ])
    }

    #[test]
    fn a_grouped_selector_opens_on_the_first_cursor_stop() {
        assert_eq!(grouped().cursor, 1, "row 0 is a header");
        assert_eq!(grouped().highlight(""), 1);
    }

    #[test]
    fn the_initial_cursor_comes_from_the_empty_filter_view_not_the_raw_rows() {
        // A greyed group's collapsed rows are hidden at the empty filter, so
        // an index minted over the raw rows (where they exist) would point
        // past the stop it means. The first stop of the empty-filter view
        // [openrouter, "  unavailable", local, local/qwen] is the note at 1.
        let s = Selector::new(vec![
            SelectorRow::header("openrouter"),
            SelectorRow::collapsed("openrouter/a"),
            SelectorRow::collapsed("openrouter/b"),
            SelectorRow::collapsed("openrouter/c"),
            SelectorRow::note("  unavailable", Some("set OPENROUTER_API_KEY".into())),
            SelectorRow::header("local"),
            row("local/qwen"),
        ]);
        assert_eq!(s.cursor, 1);
        assert_eq!(s.highlight(""), 1);
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
        // Up at the first stop stays - the header above is not a stop.
        s.handle_nav(Key::ArrowUp, "");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn enter_never_picks_a_header() {
        let mut s = Selector::new(vec![SelectorRow::header("anthropic")]);
        assert_eq!(s.handle_nav(Key::Enter, ""), None);
        assert_eq!(s.handle_nav(Key::Escape, ""), Some(SelectorOutcome::Cancel));
    }

    #[test]
    fn a_filter_that_lands_the_cursor_on_a_header_snaps_to_a_cursor_stop() {
        let mut s = grouped();
        // Move deep, then filter down to the anthropic group: the stale
        // cursor is re-snapped onto a stop and Enter picks it.
        s.handle_nav(Key::ArrowDown, "");
        s.handle_nav(Key::ArrowDown, "");
        assert_eq!(s.cursor, 4);
        // Filter "anthropic" keeps the header + its two models (indexes 0..3);
        // cursor 4 clamps to 2, a member.
        assert_eq!(s.highlight("anthropic"), 2);
        assert_eq!(
            s.handle_nav(Key::Enter, "anthropic"),
            Some(SelectorOutcome::Select("anthropic/claude-haiku-4-5".into()))
        );
    }

    #[test]
    fn a_provider_name_filter_keeps_the_header_and_its_scoped_members() {
        let s = grouped();
        // The header matches on its own label AND the scoped members match on
        // theirs; the unmatched "local" group vanishes entirely.
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
    fn a_header_and_its_notes_travel_when_only_a_member_matches() {
        let s = grouped();
        // "qwen" matches no header, only the local group's member: the
        // header shows the way to it, and the other group vanishes.
        assert_eq!(labels(s.filtered("qwen")), vec!["local", "local/qwen"]);
    }

    #[test]
    fn a_header_match_alone_shows_only_the_header_and_its_notes() {
        // A group whose member labels do NOT contain the header's name: the
        // header (and its note) show on the header's own match, but the
        // unmatched member stays hidden - a header match reveals no members
        // by itself.
        let s = Selector::new(vec![
            SelectorRow::header("acme"),
            SelectorRow::note("  beta", None),
            row("claude-fable-5"),
        ]);
        assert_eq!(labels(s.filtered("acme")), vec!["acme", "  beta"]);
    }

    #[test]
    fn a_group_with_no_match_vanishes_entirely() {
        assert_eq!(labels(grouped().filtered("zzz")), Vec::<String>::new());
    }

    // --- collapsed members (a credential-less built-in's greyed catalog) -----

    // A greyed group carries its note LAST - the /model shape - so cursoring
    // onto it anchors the popup window (which ends at the highlight) below
    // the whole revealed group.
    fn with_collapsed() -> Selector {
        Selector::new(vec![
            SelectorRow::header("local"),
            row("local/qwen"),
            SelectorRow::header("openrouter"),
            SelectorRow::collapsed("openrouter/qwen3.5-27b"),
            SelectorRow::collapsed("openrouter/kimi-k2"),
            SelectorRow::note("  unavailable", Some("set OPENROUTER_API_KEY".into())),
        ])
    }

    #[test]
    fn collapsed_members_hide_at_the_empty_filter() {
        assert_eq!(
            labels(with_collapsed().filtered("")),
            vec!["local", "local/qwen", "openrouter", "  unavailable"]
        );
    }

    #[test]
    fn a_matching_collapsed_member_is_revealed_between_header_and_note() {
        // The `/model kimi` case: the exact greyed row surfaces above the
        // note's env-key hint, and the unmatched "local" group vanishes.
        assert_eq!(
            labels(with_collapsed().filtered("kimi")),
            vec!["openrouter", "openrouter/kimi-k2", "  unavailable"]
        );
    }

    #[test]
    fn a_provider_name_query_reveals_its_scoped_catalog_up_to_the_cap() {
        // The `/model openrouter` case: the header matches, and every
        // collapsed label - scoped as `openrouter/...` - matches too, so the
        // greyed catalog (here, under the cap) shows above the note.
        let s = with_collapsed();
        let view = s.filtered("openrouter");
        assert_eq!(
            view.iter().map(|f| f.row.label.clone()).collect::<Vec<_>>(),
            vec![
                "openrouter",
                "openrouter/qwen3.5-27b",
                "openrouter/kimi-k2",
                "  unavailable"
            ]
        );
        assert!(
            view.iter().all(|f| f.overflow.is_none()),
            "nothing truncated below the cap"
        );
    }

    #[test]
    fn the_cursor_skips_collapsed_rows_but_stops_on_the_note() {
        let mut s = with_collapsed();
        // "qwen" reveals local/qwen (pickable), the collapsed
        // openrouter/qwen3.5-27b, and the trailing note: the view is
        // [local, local/qwen, openrouter, openrouter/qwen3.5-27b, note].
        assert_eq!(s.highlight("qwen"), 1);
        s.handle_nav(Key::ArrowDown, "qwen");
        assert_eq!(
            s.highlight("qwen"),
            4,
            "the collapsed row is skipped; the note is the next stop"
        );
        assert_eq!(
            s.handle_nav(Key::Enter, "qwen"),
            None,
            "Enter refuses a note"
        );
        s.handle_nav(Key::ArrowUp, "qwen");
        assert_eq!(
            s.handle_nav(Key::Enter, "qwen"),
            Some(SelectorOutcome::Select("local/qwen".into()))
        );
    }

    #[test]
    fn the_cursor_walks_onto_a_trailing_note_at_the_empty_filter() {
        // Every greyed provider's hint is reachable without typing: from the
        // last member, Down lands on the note ["local", "local/qwen",
        // "openrouter", "  unavailable"] and Enter is a no-op there.
        let mut s = with_collapsed();
        assert_eq!(s.highlight(""), 1);
        s.handle_nav(Key::ArrowDown, "");
        assert_eq!(s.highlight(""), 3);
        assert_eq!(s.handle_nav(Key::Enter, ""), None);
    }

    // --- the reveal cap and its overflow count -------------------------------

    fn big_greyed_group(models: usize) -> Selector {
        let mut rows = vec![SelectorRow::header("openrouter")];
        rows.extend((0..models).map(|i| SelectorRow::collapsed(format!("openrouter/m{i:02}"))));
        rows.push(SelectorRow::note(
            "  unavailable",
            Some("set OPENROUTER_API_KEY".into()),
        ));
        Selector::new(rows)
    }

    #[test]
    fn a_reveal_is_capped_and_the_note_carries_the_overflow_count() {
        let s = big_greyed_group(8);
        let view = s.filtered("openrouter");
        // Header, the first COLLAPSED_REVEAL_CAP matches in order, the note.
        assert_eq!(view.len(), 1 + COLLAPSED_REVEAL_CAP + 1);
        assert_eq!(view[1].row.label, "openrouter/m00");
        assert_eq!(view[COLLAPSED_REVEAL_CAP].row.label, "openrouter/m04");
        let note = view.last().unwrap();
        assert_eq!(note.row.role, RowRole::Note);
        assert_eq!(note.overflow, Some(3), "8 matched, 5 shown");
        assert!(
            view.iter().rev().skip(1).all(|f| f.overflow.is_none()),
            "the count rides the note alone"
        );
    }

    #[test]
    fn the_empty_filter_reveals_nothing_so_the_note_carries_no_overflow() {
        // Overflow counts truncated MATCHES: with every collapsed row hidden
        // at the empty filter, nothing is truncated and the note's hint must
        // stay bare - no "· N more" against an unrevealed catalog.
        let s = big_greyed_group(8);
        let view = s.filtered("");
        assert_eq!(labels(view.clone()), vec!["openrouter", "  unavailable"]);
        assert!(view.iter().all(|f| f.overflow.is_none()));
    }

    #[test]
    fn a_reveal_within_the_cap_carries_no_overflow() {
        let s = big_greyed_group(COLLAPSED_REVEAL_CAP);
        let view = s.filtered("openrouter");
        assert_eq!(view.len(), 1 + COLLAPSED_REVEAL_CAP + 1);
        assert!(view.iter().all(|f| f.overflow.is_none()));
    }

    #[test]
    fn a_headerless_note_filters_on_its_own_label() {
        // The `/theme` broken-file row: a note with no header before it is
        // its own group - shown at the empty filter, shown when its own
        // label matches, and NOT dragged in by its neighbors' matches.
        let s = Selector::new(vec![
            row("dark"),
            SelectorRow::note("typo", Some("colors.added: bad".into())),
            row("zebra"),
        ]);
        assert_eq!(labels(s.filtered("")), vec!["dark", "typo", "zebra"]);
        assert_eq!(labels(s.filtered("typo")), vec!["typo"]);
        assert_eq!(labels(s.filtered("zebra")), vec!["zebra"]);
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
