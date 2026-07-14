//! Draft cursor geometry — the ONE owner of where the cursor sits in the
//! Composer's draft (CONTEXT.md: the input area where the user authors the
//! next prompt; NOT a line — drafts may span many).
//!
//! Both sides that reason about the draft cursor read from here:
//!
//! * the EDIT path (`ui::transcript`) moves and mutates the cursor as the
//!   user types, and
//! * the RENDER path (`ui::composer`) wraps the draft to `width` and places a
//!   real terminal cell for the cursor.
//!
//! They previously computed equivalent line/column math independently and
//! stayed in sync only by hand; one diverging rule (how a trailing '\n', a
//! run of blank lines, or a cursor at the end is counted) would paint the
//! cursor in the wrong cell. This module makes that a single definition.
//!
//! # Units
//!
//! The cursor is a CHAR index, not a byte offset and not a grapheme cluster —
//! this is the convention the whole codebase measures text in. A LOGICAL line
//! is a hard line: the draft split on '\n'. Width-wrapping into VISUAL rows is
//! a rendering concern (`ui::composer`) BUILT ON this logical geometry, never
//! a re-derivation of it. No ratatui types cross this seam (ADR-0019): strings
//! and char indices in, plain numbers out.

/// The `(logical line, column)` of char index `cursor` — both counted in
/// chars. A logical line is a hard line (split on '\n'); the column is the
/// char offset from that line's start. A cursor sitting ON a '\n' counts as
/// the end of the line before it (its column equals that line's length).
///
/// Clamped: a `cursor` past the last char resolves to the end of the last
/// line.
pub fn line_col(value: &str, cursor: usize) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for c in value.chars().take(cursor) {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Each logical (hard) line's length in chars, top first. Never empty: an
/// empty draft is one zero-length line — the cursor still needs a line to sit
/// on.
pub fn line_lengths(value: &str) -> Vec<usize> {
    value.split('\n').map(|l| l.chars().count()).collect()
}

/// The char index of `(logical line, col)` — the inverse of [`line_col`],
/// counting one char per '\n' between lines. `col` must already be clamped to
/// the line's length by the caller.
pub fn cursor_at(value: &str, line: usize, col: usize) -> usize {
    line_lengths(value)
        .iter()
        .take(line)
        // +1 for the '\n' that terminates each preceding line.
        .map(|len| len + 1)
        .sum::<usize>()
        + col
}

/// The byte offset of char index `cursor` in `value` (the string's byte
/// length when the cursor sits past the last char). String surgery translates
/// the char-index cursor to a byte offset exactly once, here, so multi-byte
/// input never splits a char or panics.
pub fn byte_of(value: &str, cursor: usize) -> usize {
    value
        .char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(value.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `line_col` and `cursor_at` are exact inverses on every clamped cursor.
    #[test]
    fn line_col_and_cursor_at_round_trip() {
        for value in ["", "abcd", "ab\ncd\nef", "a\n\nb", "x\n", "\n\n\n"] {
            let chars = value.chars().count();
            for cursor in 0..=chars {
                let (line, col) = line_col(value, cursor);
                assert_eq!(
                    cursor_at(value, line, col),
                    cursor,
                    "round trip failed for value={value:?} cursor={cursor}"
                );
            }
        }
    }

    #[test]
    fn a_cursor_on_a_newline_is_the_end_of_the_line_before() {
        // "ab\ncd": index 2 sits on the '\n'; it is column 2 of line 0.
        assert_eq!(line_col("ab\ncd", 2), (0, 2));
        assert_eq!(line_col("ab\ncd", 3), (1, 0));
    }

    #[test]
    fn line_lengths_never_empty_and_counts_blank_lines() {
        assert_eq!(line_lengths(""), vec![0]);
        assert_eq!(line_lengths("a\n\nb"), vec![1, 0, 1]);
        assert_eq!(line_lengths("x\n"), vec![1, 0]);
    }

    #[test]
    fn byte_of_translates_char_index_past_multibyte() {
        // 'é' is two bytes; char index 2 is the byte after "hé".
        assert_eq!(byte_of("héllo", 0), 0);
        assert_eq!(byte_of("héllo", 2), 3);
        assert_eq!(byte_of("héllo", 99), "héllo".len());
    }
}
