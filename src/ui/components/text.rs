use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncates `text` to at most `width` CHARS, replacing the trimmed tail with a
/// single `…`. Char-based (not column-based); the caller's glyphs must be width-1.
pub(super) fn truncate_visual(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

/// Greedy word-wrap of `text` into segments each at most `width` chars, char
/// based (consistent with `truncate_visual`; no `unicode-width`, so the caller's
/// glyphs must be width-1 - the machinery/marker text is). Words are broken on
/// ASCII spaces; a single word longer than `width` is HARD-SPLIT across rows so
/// no segment ever exceeds `width` (the invariant `indented_lines` relies on to
/// keep measure==draw). A `width` of 0 is treated as 1. An empty input yields
/// one empty segment so a blank line survives as a blank row.
pub(super) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_len = 0usize;
    for word in text.split(' ') {
        let mut word = word;
        // Hard-split a word wider than the whole line before it ever tries to
        // sit on one: peel `width`-char chunks until the remainder fits.
        while word.chars().count() > width {
            if line_len > 0 {
                out.push(std::mem::take(&mut line));
                line_len = 0;
            }
            let head: String = word.chars().take(width).collect();
            let consumed = head.len();
            out.push(head);
            word = &word[consumed..];
        }
        let wlen = word.chars().count();
        // +1 for the space that would join this word to the current line.
        let needed = if line_len == 0 {
            wlen
        } else {
            line_len + 1 + wlen
        };
        if needed > width && line_len > 0 {
            out.push(std::mem::take(&mut line));
            line_len = 0;
        }
        if line_len > 0 {
            line.push(' ');
            line_len += 1;
        }
        line.push_str(word);
        line_len += wlen;
    }
    out.push(line);
    out
}

/// Splits multi-line text into one string per source row: ratatui does not break
/// a single `Line` on an embedded '\n', so multi-line messages must become
/// multiple `Line`s or they collapse into one blob. Tabs become two spaces; `\r`
/// is stripped; empty lines survive as blank rows. Width-wrapping is the
/// Paragraph's job (`Wrap`), so this only handles hard line breaks.
pub(super) fn text_rows(text: &str) -> Vec<String> {
    text.replace('\r', "")
        .split('\n')
        .map(|row| row.replace('\t', "  "))
        .collect()
}

/// Truncates `text` to at most `width` DISPLAY COLUMNS (a wide glyph counts 2),
/// replacing the trimmed tail with a single `…`. The diff path's chrome uses
/// this (not the char-based [`truncate_visual`]) so a CJK/emoji title or header
/// still occupies `<= width` columns and the viewport never re-wraps it
/// (measure==draw, ADR-0029).
pub(super) fn truncate_cols(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    // Leave one column for the ellipsis; stop before a wide glyph would straddle.
    let (mut out, _) = clip_to_cols(text, width.saturating_sub(1));
    out.push('…');
    out
}

/// The longest char-boundary prefix of `text` that fits in `max` DISPLAY COLUMNS,
/// with its column width. A wide glyph that would straddle the cap is dropped
/// (never half-drawn), so the returned width is always `<= max`. The one place
/// the diff path's column clipping lives ([`truncate_cols`] and [`push_cols`]).
pub(super) fn clip_to_cols(text: &str, max: usize) -> (String, usize) {
    let mut out = String::new();
    let mut cols = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if cols + w > max {
            break;
        }
        out.push(ch);
        cols += w;
    }
    (out, cols)
}

/// Pushes `text` styled onto `spans`, truncated so the row stays within `width`
/// DISPLAY COLUMNS. Returns the new used-column count. A wide glyph that would
/// straddle the cap is dropped (never half-drawn), so `used <= width` always and
/// the produced [`Line`] occupies `<= width` columns - what keeps every diff row
/// from soft-wrapping (measure==draw, ADR-0029).
pub(super) fn push_cols(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    used: usize,
    width: usize,
) -> usize {
    if used >= width {
        return used;
    }
    let room = width - used;
    if text.width() <= room {
        let w = text.width();
        spans.push(Span::styled(text.to_string(), style));
        return used + w;
    }
    let (clipped, cols) = clip_to_cols(text, room);
    spans.push(Span::styled(clipped, style));
    used + cols
}
