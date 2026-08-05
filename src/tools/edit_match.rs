//! The edit-string matcher: the faithful port of qwen v0.16.0
//! `utils/editHelper.ts`'s edit-string normalization - `normalizeEditStrings` +
//! `maybeAugmentOldStringForDeletion` and the `findMatchedSlice` matcher they
//! lean on - plus the EXACT `countOccurrences` the edit applies afterwards. A
//! cohesive leaf consumed by `edit_file`; it holds no file I/O and no tool
//! contract, only the pure string pipeline.
//!
//! The pipeline stays deterministic and progressively relaxes: literal
//! substring, then character-equivalence (curly quotes / dashes / exotic
//! spaces), then line-based matching that tolerates trailing whitespace. A
//! relaxed hit returns the CANONICAL on-disk slice so the caller replaces real
//! bytes, not the model's approximation.

/// qwen `normalizeEditStrings`' return: the (possibly canonicalized) strings.
pub struct Normalized {
    pub old_string: String,
    pub new_string: String,
}

/// qwen `NormalizedEditStrings`-producing `normalizeEditStrings`: when the
/// literal `old_string` is not found verbatim but a relaxed match is,
/// substitute the on-disk slice as `old_string` and (if the relaxed match
/// dropped a trailing empty line) trim `new_string`'s trailing newline. An
/// empty `old_string` is returned untouched (it is the new-file sentinel).
pub fn edit_strings(file_content: &str, old_string: &str, new_string: &str) -> Normalized {
    if old_string.is_empty() {
        return Normalized {
            old_string: old_string.to_string(),
            new_string: new_string.to_string(),
        };
    }
    match find_matched_slice(file_content, old_string) {
        Some(matched) => Normalized {
            old_string: matched.slice,
            new_string: adjust_new_string_for_trailing_line(
                new_string,
                matched.removed_trailing_final_empty_line,
            ),
        },
        None => Normalized {
            old_string: old_string.to_string(),
            new_string: new_string.to_string(),
        },
    }
}

/// qwen `maybeAugmentOldStringForDeletion`: for a pure deletion
/// (`new_string == ""`) where `old_string` has no trailing newline but the
/// file holds `old_string + "\n"`, grow `old_string` by that newline so the
/// removal does not leave a blank line behind.
pub fn augment_old_string_for_deletion(
    file_content: &str,
    old_string: &str,
    new_string: &str,
) -> String {
    if old_string.is_empty() || !new_string.is_empty() || old_string.ends_with('\n') {
        return old_string.to_string();
    }
    let candidate = format!("{old_string}\n");
    if file_content.contains(&candidate) {
        candidate
    } else {
        old_string.to_string()
    }
}

/// Number of non-overlapping occurrences of `old_string` in `content` (qwen
/// `countOccurrences`).
pub fn count_occurrences(content: &str, old_string: &str) -> usize {
    if old_string.is_empty() {
        return 0;
    }
    content.matches(old_string).count()
}

/// qwen `MatchedSliceResult`.
struct MatchedSlice {
    slice: String,
    removed_trailing_final_empty_line: bool,
}

/// qwen `findMatchedSlice`: literal `indexOf`, then character-normalized
/// `indexOf`, then `findLineBasedMatch`. Returns the CANONICAL slice from the
/// ORIGINAL `haystack` in every case.
fn find_matched_slice(haystack: &str, needle: &str) -> Option<MatchedSlice> {
    if needle.is_empty() {
        return None;
    }

    // Literal match: the on-disk slice IS the needle.
    if let Some(byte_idx) = haystack.find(needle) {
        return Some(MatchedSlice {
            slice: haystack[byte_idx..byte_idx + needle.len()].to_string(),
            removed_trailing_final_empty_line: false,
        });
    }

    // Character-equivalence match: normalize both, find the char index, then
    // cut the ORIGINAL haystack at [idx, idx + needle_char_len). The map is
    // 1:1 on chars, so char indices line up between original and normalized.
    let haystack_chars: Vec<char> = haystack.chars().collect();
    let normalized_haystack: String = haystack_chars
        .iter()
        .map(|c| normalize_basic_char(*c))
        .collect();
    let normalized_needle: String = needle.chars().map(normalize_basic_char).collect();
    if let Some(char_idx) = char_index_of(&normalized_haystack, &normalized_needle) {
        let needle_char_len = needle.chars().count();
        let slice: String = haystack_chars[char_idx..char_idx + needle_char_len]
            .iter()
            .collect();
        return Some(MatchedSlice {
            slice,
            removed_trailing_final_empty_line: false,
        });
    }

    find_line_based_match(haystack, needle)
}

/// qwen `findLineBasedMatch`: line-window the haystack against the needle's
/// lines, trying identity, then `trimEnd`, then `normalizeBasicCharacters +
/// trimEnd` per line. If the needle's last line is empty and no match is
/// found, retry with that trailing empty line dropped
/// (`removedTrailingFinalEmptyLine = true`).
fn find_line_based_match(haystack: &str, needle: &str) -> Option<MatchedSlice> {
    let index = build_line_index(haystack);
    let pattern_lines: Vec<&str> = needle.split('\n').collect();
    let ends_with_newline = needle.ends_with('\n');
    if pattern_lines.is_empty() {
        return None;
    }

    if let Some(idx) = attempt_match(&index.lines, &pattern_lines) {
        return Some(MatchedSlice {
            slice: slice_from_lines(&index, idx, pattern_lines.len(), ends_with_newline),
            removed_trailing_final_empty_line: false,
        });
    }

    if pattern_lines.last() == Some(&"") {
        let trimmed = &pattern_lines[..pattern_lines.len() - 1];
        if trimmed.is_empty() {
            return None;
        }
        if let Some(idx) = attempt_match(&index.lines, trimmed) {
            return Some(MatchedSlice {
                slice: slice_from_lines(&index, idx, trimmed.len(), false),
                removed_trailing_final_empty_line: true,
            });
        }
    }
    None
}

/// One line-comparison pass in qwen's `attemptMatch` relaxation ladder: the
/// identity pass, the `trimEnd` pass, then the `normalizeBasicCharacters +
/// trimEnd` pass. A first-class enum (rather than a function-pointer table)
/// so each transform is reached through a plain method call.
#[derive(Clone, Copy)]
enum LinePass {
    Identity,
    TrimEnd,
    Normalize,
}

impl LinePass {
    /// The three passes in relaxation order.
    const LADDER: [LinePass; 3] = [LinePass::Identity, LinePass::TrimEnd, LinePass::Normalize];

    /// Apply this pass' transform to one line.
    fn apply(self, value: &str) -> String {
        match self {
            LinePass::Identity => value.to_string(),
            LinePass::TrimEnd => trim_end(value).to_string(),
            LinePass::Normalize => normalize_line_for_comparison(value),
        }
    }
}

/// qwen's `attemptMatch`: the identity and `trimEnd` passes, then the
/// character-normalizing + `trimEnd` pass, tried in order.
fn attempt_match(lines: &[&str], pattern: &[&str]) -> Option<usize> {
    LinePass::LADDER
        .into_iter()
        .find_map(|pass| seek_sequence_with_transform(lines, pattern, pass))
}

/// qwen `seekSequenceWithTransform`: first window index where every
/// pass-transformed pattern line equals the transformed haystack line.
fn seek_sequence_with_transform(lines: &[&str], pattern: &[&str], pass: LinePass) -> Option<usize> {
    if pattern.is_empty() {
        return Some(0);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    'outer: for i in 0..=(lines.len() - pattern.len()) {
        for (p, pat) in pattern.iter().enumerate() {
            if pass.apply(lines[i + p]) != pass.apply(pat) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

/// qwen `buildLineIndex`'s output as a first-class value: the text, its
/// `split('\n')` lines, and the byte offset each line starts at (`offsets`
/// has `lines.len() + 1` entries; the last is the content length). Groups the
/// `(text, lines, offsets)` clump `slice_from_lines` needed as three separate
/// positional arguments into one Parameter Object.
struct LineIndex<'a> {
    text: &'a str,
    lines: Vec<&'a str>,
    offsets: Vec<usize>,
}

/// qwen `buildLineIndex`.
fn build_line_index(text: &str) -> LineIndex<'_> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut offsets = vec![0usize; lines.len() + 1];
    let mut cursor = 0usize;
    for (i, line) in lines.iter().enumerate() {
        offsets[i] = cursor;
        cursor += line.len();
        if i < lines.len() - 1 {
            cursor += 1; // the '\n' that split() removed.
        }
    }
    offsets[lines.len()] = text.len();
    LineIndex {
        text,
        lines,
        offsets,
    }
}

/// qwen `sliceFromLines`: reconstruct the original bytes for
/// `[start_line, start_line + line_count)`, optionally including the newline
/// after the final line. Takes the [`LineIndex`] Parameter Object so its
/// `(text, lines, offsets)` data clump travels as one argument.
fn slice_from_lines(
    index: &LineIndex<'_>,
    start_line: usize,
    line_count: usize,
    include_trailing_newline: bool,
) -> String {
    if line_count == 0 {
        return if include_trailing_newline {
            "\n".to_string()
        } else {
            String::new()
        };
    }
    let start_index = index.offsets.get(start_line).copied().unwrap_or(0);
    let last_line_index = start_line + line_count - 1;
    let last_line_start = index.offsets.get(last_line_index).copied().unwrap_or(0);
    let mut end_index = last_line_start + index.lines.get(last_line_index).map_or(0, |l| l.len());

    if include_trailing_newline {
        if let Some(next_line_start) = index.offsets.get(start_line + line_count).copied() {
            end_index = next_line_start;
        } else if index.text.ends_with('\n') {
            end_index = index.text.len();
        }
    }
    index.text[start_index..end_index].to_string()
}

/// qwen `adjustNewStringForTrailingLine`.
fn adjust_new_string_for_trailing_line(new_string: &str, removed_trailing_line: bool) -> String {
    if removed_trailing_line {
        remove_trailing_newline(new_string).to_string()
    } else {
        new_string.to_string()
    }
}

/// qwen `removeTrailingNewline`: strip a single trailing CRLF, LF, or CR.
fn remove_trailing_newline(text: &str) -> &str {
    if let Some(stripped) = text.strip_suffix("\r\n") {
        stripped
    } else if let Some(stripped) = text.strip_suffix('\n') {
        stripped
    } else if let Some(stripped) = text.strip_suffix('\r') {
        stripped
    } else {
        text
    }
}

/// qwen `normalizeLineForComparison`: `normalizeBasicCharacters` then
/// `trimEnd`.
fn normalize_line_for_comparison(value: &str) -> String {
    let normalized: String = value.chars().map(normalize_basic_char).collect();
    trim_end(&normalized).to_string()
}

/// JS `String.prototype.trimEnd`: trim trailing whitespace only. Rust's
/// `trim_end` trims the same Unicode whitespace set for the characters we
/// care about here (ASCII spaces/tabs and the mapped exotic spaces once
/// normalized).
fn trim_end(value: &str) -> &str {
    value.trim_end()
}

/// First CHAR index of `needle` in `haystack` (JS `indexOf` on strings whose
/// normalization is char-for-char 1:1, so a char index is well-defined).
fn char_index_of(haystack: &str, needle: &str) -> Option<usize> {
    let byte_idx = haystack.find(needle)?;
    Some(haystack[..byte_idx].chars().count())
}

/// qwen `UNICODE_EQUIVALENT_MAP`: curly quotes to straight, several dash
/// variants to ASCII hyphen-minus, and exotic spaces to a normal space.
fn normalize_basic_char(c: char) -> char {
    match c {
        // Hyphen / dash variations -> ASCII hyphen-minus.
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        // Curly single quotes -> straight apostrophe.
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        // Curly double quotes -> straight double quote.
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        // Whitespace variants -> normal space.
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
        | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
        | '\u{3000}' => ' ',
        other => other,
    }
}

#[cfg(test)]
#[path = "../../tests/tools/edit_match.rs"]
mod tests;
