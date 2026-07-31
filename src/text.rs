//! Shared text-normalisation helpers - a std-only crate-root leaf, so both the
//! notebook read projection and the background-shell capture file import the same
//! [`strip_ansi`] with no tool-graph cycle (mirrors the `notebook` leaf's role).
//!
//! Ported from qwen v0.16.0's `stripAnsi` (used by the notebook formatter and the
//! background-shell output stream): terminal control codes a subprocess writes
//! (dev servers, build tools, ipykernel) must not leak into the prompt or into a
//! capture file the model reads as plain text.
//!
//! Covers the four escape families qwen matches - CSI, OSC, DCS/APC/PM/SOS, and
//! lone C1 Fe two-byte escapes. Each escape family is skipped by a helper that
//! returns the index just past the sequence; a plain char passes through.

/// Strip ANSI escape sequences from `input`, returning the cleaned string.
pub fn strip_ansi(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(next) = escape_skip(&chars, i) {
            i = next;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// If an escape sequence starts at `i`, the index just past it; else `None` (a
/// plain char the caller emits). Dispatches on the byte after `ESC` to the right
/// family skipper.
fn escape_skip(chars: &[char], i: usize) -> Option<usize> {
    if chars[i] != '\u{1b}' || i + 1 >= chars.len() {
        return None;
    }
    match chars[i + 1] {
        '[' => Some(skip_csi(chars, i)),
        ']' => Some(skip_osc(chars, i)),
        'P' | '_' | '^' | 'X' => Some(skip_string_terminated(chars, i)),
        '\u{40}'..='\u{5a}' | '\u{5c}'..='\u{5f}' => Some(skip_c1(i)),
        _ => None,
    }
}

/// CSI: `ESC [` params intermediates final. Returns the index past the final
/// byte (`0x40..=0x7e`), consuming the whole sequence.
fn skip_csi(chars: &[char], i: usize) -> usize {
    let mut j = i + 2;
    while j < chars.len() && matches!(chars[j], '\u{30}'..='\u{3f}') {
        j += 1;
    }
    while j < chars.len() && matches!(chars[j], '\u{20}'..='\u{2f}') {
        j += 1;
    }
    if j < chars.len() && matches!(chars[j], '\u{40}'..='\u{7e}') {
        j += 1;
    }
    j
}

/// OSC: `ESC ]` ... terminated by BEL (`0x07`) or ST (`ESC \`). Returns the
/// index past whichever terminator ended it (or end-of-input if unterminated).
fn skip_osc(chars: &[char], i: usize) -> usize {
    let mut j = i + 2;
    while j < chars.len() && chars[j] != '\u{07}' && chars[j] != '\u{1b}' {
        j += 1;
    }
    if j < chars.len() && chars[j] == '\u{07}' {
        j + 1
    } else if j + 1 < chars.len() && chars[j] == '\u{1b}' && chars[j + 1] == '\\' {
        j + 2
    } else {
        j
    }
}

/// DCS / APC / PM / SOS: `ESC P/_/^/X` ... terminated by ST (`ESC \`). Returns
/// the index past the ST, or end-of-input when the sequence is unterminated.
fn skip_string_terminated(chars: &[char], i: usize) -> usize {
    let mut j = i + 2;
    while j + 1 < chars.len() && !(chars[j] == '\u{1b}' && chars[j + 1] == '\\') {
        j += 1;
    }
    if j + 1 < chars.len() {
        j + 2
    } else {
        chars.len()
    }
}

/// A lone two-byte C1 Fe escape (`ESC` + a `0x40..=0x5f` byte, minus `[` which
/// the CSI arm owns): always exactly two chars wide.
fn skip_c1(i: usize) -> usize {
    i + 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_color_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        // A bare ESC with no recognized family passes through untouched.
        assert_eq!(strip_ansi("a\u{1b}"), "a\u{1b}");
    }

    #[test]
    fn strip_ansi_removes_osc_with_bel_and_st_terminators() {
        // OSC terminated by BEL (0x07): a window-title set.
        assert_eq!(strip_ansi("\u{1b}]0;title\u{07}text"), "text");
        // OSC terminated by ST (ESC \\): a hyperlink open.
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{1b}\\link"), "link");
        // An unterminated OSC consumes to end-of-input.
        assert_eq!(strip_ansi("keep\u{1b}]0;never-ends"), "keep");
    }

    #[test]
    fn strip_ansi_removes_dcs_apc_pm_sos_string_sequences() {
        // DCS (ESC P) terminated by ST.
        assert_eq!(strip_ansi("\u{1b}Pq...data...\u{1b}\\after"), "after");
        // APC (ESC _) terminated by ST.
        assert_eq!(strip_ansi("pre\u{1b}_payload\u{1b}\\post"), "prepost");
        // An unterminated DCS consumes to end-of-input.
        assert_eq!(strip_ansi("head\u{1b}Pno-end"), "head");
    }

    #[test]
    fn strip_ansi_removes_a_lone_c1_fe_escape() {
        // ESC + a 0x40..=0x5f byte (here `M`, reverse line feed) is a two-char
        // C1 escape, stripped whole.
        assert_eq!(strip_ansi("a\u{1b}Mb"), "ab");
    }
}
