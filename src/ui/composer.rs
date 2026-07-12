//! UI Composer layout - the pure wrapping and cursor-position math behind the
//! growing Composer (CONTEXT.md: the input area where the user authors the
//! next prompt; NOT a line - drafts may span many).
//!
//! Lives in its own module for the same reason `ui::viewport` does: `ui.rs`
//! and `ui::components` are untested-by-design adapters (ADR-0001's split),
//! so the math they draw with is plain strings and `usize` in/out - no
//! ratatui types (ADR-0019) - and all unit-tested here.
//!
//! The wrapping is CHAR-based, not word-based, on purpose: the view places a
//! REAL terminal cursor (`frame.set_cursor_position`) at the exact cell of
//! the draft cursor, which needs row/column math the renderer can reproduce
//! exactly - `Paragraph`'s word-wrap points cannot be queried cheaply.
//! Char-per-cell is also how the rest of the codebase measures text.
//!
//! The contract:
//!
//! * **Rows** are the draft split on hard '\n', each hard line then chunked
//!   into `width`-char rows. A hard line whose length is an exact multiple of
//!   `width` (the empty line included) yields one EXTRA empty row - the cell
//!   the cursor occupies at that line's end, exactly like a terminal that has
//!   just wrapped. So `cursor_row/cursor_col` are total functions of the
//!   cursor: `offset / width` and `offset % width` within the hard line.
//! * **Every cursor position is a real cell**: `cursor_col < width` always,
//!   so the view never places the terminal cursor outside the Composer.
//! * **Height is capped** at `min(8, terminal_height / 3)` rows - never
//!   below one - so a tall draft never starves the transcript viewport; when
//!   the draft overflows the cap, [`first_visible_row`] scrolls the Composer
//!   internally so the cursor row stays visible, pinned to the BOTTOM of the
//!   box like a terminal.

/// The most rows the Composer ever occupies, however tall the terminal.
pub const MAX_ROWS: usize = 8;

/// The Composer's draft, wrapped: the display rows (hard newlines AND
/// width-wrapping both split) and the `(row, col)` cell the cursor occupies
/// within them. Plain data - the view adds the gutter and colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerLayout {
    /// The wrapped display rows, top first. Never empty: an empty draft is
    /// one empty row (the cursor needs a cell to sit in).
    pub rows: Vec<String>,
    /// The row index (into `rows`) the cursor sits on.
    pub cursor_row: usize,
    /// The column (in chars, `< width`) the cursor sits at within its row.
    pub cursor_col: usize,
}

/// Wraps the draft at `width` chars per row and locates the cursor (a CHAR
/// index into `value`, clamped to its length). `width` is the text width the
/// view will draw the rows at - the same for every row, first and
/// continuation alike, since the "› " gutter and the 2-space indent are the
/// same 2 cells. A degenerate `width` of 0 is treated as 1.
pub fn layout(value: &str, cursor: usize, width: usize) -> ComposerLayout {
    let width = width.max(1);
    let cursor = cursor.min(value.chars().count());

    let mut rows = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_col = 0;
    // The char index the current hard line starts at (its '\n' excluded).
    let mut line_start = 0;

    for line in value.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        // `len / width + 1` rows: the extra row on an exact multiple is the
        // cell the cursor occupies at the line's end (see the module doc).
        let row_count = chars.len() / width + 1;
        let base_row = rows.len();
        for r in 0..row_count {
            let end = ((r + 1) * width).min(chars.len());
            rows.push(chars[r * width..end].iter().collect());
        }
        // The cursor belongs to this line when it sits between the line's
        // first char and the position just past its last (ON the '\n' counts
        // as end-of-line, matching the core's `line_col`).
        if cursor >= line_start && cursor <= line_start + chars.len() {
            let offset = cursor - line_start;
            cursor_row = base_row + offset / width;
            cursor_col = offset % width;
        }
        line_start += chars.len() + 1;
    }

    ComposerLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// The most rows the Composer may occupy in a `terminal_height`-row terminal:
/// `min(8, terminal_height / 3)`, but never below 1 - the transcript viewport
/// keeps the lion's share, and the Composer never vanishes.
pub fn max_visible_rows(terminal_height: usize) -> usize {
    (terminal_height / 3).clamp(1, MAX_ROWS)
}

/// The first row a `visible`-row Composer box shows so the cursor row is
/// always inside it, preferring the cursor at the BOTTOM of the box (like a
/// terminal): rows above scroll away first, and only a draft shorter than the
/// window shows its tail below the cursor.
pub fn first_visible_row(cursor_row: usize, visible: usize) -> usize {
    cursor_row.saturating_sub(visible.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(value: &str, width: usize) -> Vec<String> {
        layout(value, 0, width).rows
    }

    fn cursor(value: &str, cursor: usize, width: usize) -> (usize, usize) {
        let l = layout(value, cursor, width);
        (l.cursor_row, l.cursor_col)
    }

    // --- wrapping ------------------------------------------------------------

    #[test]
    fn an_empty_draft_is_one_empty_row_with_the_cursor_at_the_origin() {
        let l = layout("", 0, 10);
        assert_eq!(l.rows, vec![String::new()]);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 0));
    }

    #[test]
    fn a_short_draft_is_a_single_row() {
        assert_eq!(rows("hello", 10), vec!["hello"]);
    }

    #[test]
    fn hard_newlines_split_rows_and_empty_lines_survive_as_blank_rows() {
        assert_eq!(rows("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn a_long_line_wraps_at_the_width_char_by_char() {
        assert_eq!(rows("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn an_exact_multiple_line_gains_the_empty_row_the_cursor_lands_on() {
        // "abcd" at width 4 fills its row exactly; the cursor at its end (4)
        // needs the next row's first cell, so that row exists.
        assert_eq!(rows("abcd", 4), vec!["abcd", ""]);
        assert_eq!(cursor("abcd", 4, 4), (1, 0));
    }

    #[test]
    fn hard_newlines_and_wrapping_compose() {
        assert_eq!(rows("abcdef\nxy", 4), vec!["abcd", "ef", "xy"]);
    }

    #[test]
    fn multibyte_chars_wrap_by_char_count_not_bytes() {
        assert_eq!(rows("héllo wörld", 6), vec!["héllo ", "wörld"]);
        assert_eq!(cursor("héllo wörld", 11, 6), (1, 5));
    }

    #[test]
    fn a_degenerate_zero_width_is_treated_as_one() {
        assert_eq!(rows("ab", 0), vec!["a", "b", ""]);
    }

    // --- cursor position -------------------------------------------------------

    #[test]
    fn the_cursor_at_start_middle_and_end_of_one_row() {
        assert_eq!(cursor("hello", 0, 10), (0, 0));
        assert_eq!(cursor("hello", 3, 10), (0, 3));
        assert_eq!(cursor("hello", 5, 10), (0, 5));
    }

    #[test]
    fn the_cursor_on_a_wrapped_continuation_row() {
        // "abcdefghij" at width 4: rows abcd / efgh / ij.
        assert_eq!(cursor("abcdefghij", 4, 4), (1, 0));
        assert_eq!(cursor("abcdefghij", 7, 4), (1, 3));
        assert_eq!(cursor("abcdefghij", 10, 4), (2, 2));
    }

    #[test]
    fn the_cursor_on_hard_newline_rows() {
        // "ab\ncd\nef": the cursor ON the '\n' is the end of the line before.
        assert_eq!(cursor("ab\ncd\nef", 2, 10), (0, 2));
        assert_eq!(cursor("ab\ncd\nef", 3, 10), (1, 0));
        assert_eq!(cursor("ab\ncd\nef", 5, 10), (1, 2));
        assert_eq!(cursor("ab\ncd\nef", 8, 10), (2, 2));
    }

    #[test]
    fn a_wide_draft_of_hard_lines_and_wraps_places_the_cursor_exactly() {
        // width 4: "abcdef" wraps to abcd/ef (rows 0-1), "" is row 2,
        // "ghijklm" wraps to ghij/klm (rows 3-4).
        let value = "abcdef\n\nghijklm";
        assert_eq!(rows(value, 4), vec!["abcd", "ef", "", "ghij", "klm"]);
        assert_eq!(cursor(value, 6, 4), (1, 2)); // end of "abcdef"
        assert_eq!(cursor(value, 7, 4), (2, 0)); // the empty line
        assert_eq!(cursor(value, 12, 4), (4, 0)); // start of "klm"
        assert_eq!(cursor(value, 15, 4), (4, 3)); // end of the draft
    }

    #[test]
    fn the_cursor_column_is_always_inside_the_width() {
        for cur in 0..=12 {
            let l = layout("abcd\nefghijkl", cur, 4);
            assert!(
                l.cursor_col < 4,
                "cursor {cur} produced col {}",
                l.cursor_col
            );
            assert!(l.cursor_row < l.rows.len());
        }
    }

    #[test]
    fn a_cursor_past_the_draft_clamps_to_the_end() {
        assert_eq!(cursor("hi", 99, 10), (0, 2));
    }

    // --- height cap and internal scroll ---------------------------------------

    #[test]
    fn max_visible_rows_is_a_third_of_the_terminal_capped_at_eight() {
        assert_eq!(max_visible_rows(60), 8); // 60/3 = 20, capped
        assert_eq!(max_visible_rows(24), 8); // 24/3 = 8, exactly the cap
        assert_eq!(max_visible_rows(12), 4);
        assert_eq!(max_visible_rows(3), 1);
        assert_eq!(max_visible_rows(0), 1); // never starves to zero
    }

    #[test]
    fn first_visible_row_pins_the_cursor_to_the_bottom_of_the_box() {
        assert_eq!(first_visible_row(9, 4), 6); // cursor on the box's last row
        assert_eq!(first_visible_row(6, 4), 3);
    }

    #[test]
    fn first_visible_row_shows_the_top_while_the_cursor_fits() {
        assert_eq!(first_visible_row(0, 4), 0);
        assert_eq!(first_visible_row(3, 4), 0);
        assert_eq!(first_visible_row(0, 0), 0); // degenerate box
    }
}
