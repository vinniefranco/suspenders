
use super::*;

// qwen's bold subject wins: the trimmed text between the FIRST `**...**`
// pair, description discarded (spinner-only).
#[test]
fn parses_the_bold_subject_when_present() {
    assert_eq!(
        thought_subject_of("**Refactoring the parser** now let me look at the tokens"),
        Some("Refactoring the parser".to_string())
    );
    // Only the FIRST pair matters; later `**` are part of the description.
    assert_eq!(
        thought_subject_of("**First** then **Second** thing"),
        Some("First".to_string())
    );
}

// No bold subject: fall back to the LAST non-empty line of the reasoning
// (the live head), trimmed.
#[test]
fn falls_back_to_the_last_nonempty_reasoning_line() {
    assert_eq!(
        thought_subject_of("first line\nsecond line\n  third line  "),
        Some("third line".to_string())
    );
    // Trailing blank lines are skipped to reach the real head.
    assert_eq!(
        thought_subject_of("only line\n\n   \n"),
        Some("only line".to_string())
    );
}

// Empty / whitespace-only reasoning has no subject -> None (the spinner
// falls back to the lull phrase).
#[test]
fn is_none_for_empty_or_whitespace_reasoning() {
    assert_eq!(thought_subject_of(""), None);
    assert_eq!(thought_subject_of("   \n  \n"), None);
}

// A `**` with no closing pair, or a pair wrapping only whitespace, is NOT a
// subject: the bold branch declines and the last-line fallback takes over.
#[test]
fn declines_a_malformed_or_empty_bold_pair() {
    // Unterminated `**` -> not a subject.
    assert_eq!(
        parse_thought_subject("**unterminated subject and more"),
        None
    );
    // End-to-end: the unterminated bold declines, so the last-line fallback
    // returns the whole (single) line rather than dropping to None.
    assert_eq!(
        thought_subject_of("**unterminated subject and more"),
        Some("**unterminated subject and more".to_string())
    );
    // A pair wrapping only whitespace is an empty subject -> None.
    assert_eq!(parse_thought_subject("**   ** rest"), None);
    // The end-to-end read then falls back to the last non-empty line.
    assert_eq!(
        thought_subject_of("**   ** rest"),
        Some("**   ** rest".to_string())
    );
}
