
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
