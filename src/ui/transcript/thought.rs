//! The spinner's rolling thought SUBJECT (qwen `LoadingIndicator.tsx:72`
//! `thought?.subject || currentLoadingPhrase`): a pure text concern, split from
//! the [`Transcript`](super::Transcript) store so the store keeps only its
//! history-invariant responsibility. Given the live reasoning text, derive the
//! short head the spinner shows in place of the lull phrase.
//!
//! qwen's `parseThought` (core `thoughtUtils.ts`) is ported verbatim, but
//! suspenders' reasoning streams do NOT reliably emit `**bold**` subjects, so a
//! three-fallback ladder is used (the divergence recorded in ADR-0046). Pure: no
//! ratatui, no IO - a testable function over a `&str`.

/// The double-asterisk delimiter qwen's `parseThought` wraps a thought subject
/// in (`START_DELIMITER`/`END_DELIMITER`, `thoughtUtils.ts`).
const THOUGHT_DELIMITER: &str = "**";

/// The spinner's rolling thought subject over the live reasoning `thinking`
/// (qwen `thought?.subject || currentLoadingPhrase`). The three-fallback ladder:
/// (1) qwen's bold subject, else (2) the last non-empty reasoning line (the live
/// head), else (3) `None` (the divergence in ADR-0046 - suspenders' reasoning
/// does not reliably emit `**bold**` subjects, so the spinner falls back to the
/// lull phrase).
pub(super) fn thought_subject_of(thinking: &str) -> Option<String> {
    if let Some(subject) = parse_thought_subject(thinking) {
        return Some(subject);
    }
    thinking
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
}

/// qwen `parseThought` (core `thoughtUtils.ts`): the trimmed text between the
/// FIRST `**` and the NEXT `**` after it. `None` when there is no opening
/// delimiter, no closing delimiter, or the pair wraps only whitespace (an empty
/// subject is no subject - the caller falls back to the last-line head).
fn parse_thought_subject(text: &str) -> Option<String> {
    let start = text.find(THOUGHT_DELIMITER)?;
    let after_start = start + THOUGHT_DELIMITER.len();
    let end_rel = text[after_start..].find(THOUGHT_DELIMITER)?;
    let subject = text[after_start..after_start + end_rel].trim();
    (!subject.is_empty()).then(|| subject.to_string())
}

#[cfg(test)]
mod tests {
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
}
