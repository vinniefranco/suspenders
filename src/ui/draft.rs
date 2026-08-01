//! Draft cursor geometry - the ONE owner of where the cursor sits in the
//! Composer's draft (CONTEXT.md: the input area where the user authors the
//! next prompt; NOT a line - drafts may span many).
//!
//! Both sides that reason about the draft cursor read from here:
//!
//! * the EDIT path (`ui::composer`'s key fold) moves and mutates the cursor
//!   as the user types, and
//! * the RENDER path (`ui::composer`'s `layout`) wraps the draft to `width`
//!   and places a real terminal cell for the cursor.
//!
//! They previously computed equivalent line/column math independently and
//! stayed in sync only by hand; one diverging rule (how a trailing '\n', a
//! run of blank lines, or a cursor at the end is counted) would paint the
//! cursor in the wrong cell. This module makes that a single definition.
//!
//! # Units
//!
//! The cursor is a CHAR index, not a byte offset and not a grapheme cluster -
//! this is the convention the whole codebase measures text in. A LOGICAL line
//! is a hard line: the draft split on '\n'. Width-wrapping into VISUAL rows is
//! a rendering concern (`ui::composer`) BUILT ON this logical geometry, never
//! a re-derivation of it. No ratatui types cross this seam (ADR-0019): strings
//! and char indices in, plain numbers out.

/// The `(logical line, column)` of char index `cursor` - both counted in
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
/// empty draft is one zero-length line - the cursor still needs a line to sit
/// on.
pub fn line_lengths(value: &str) -> Vec<usize> {
    value.split('\n').map(|l| l.chars().count()).collect()
}

/// The char index of `(logical line, col)` - the inverse of [`line_col`],
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
#[path = "../../tests/ui/draft.rs"]
mod tests;
