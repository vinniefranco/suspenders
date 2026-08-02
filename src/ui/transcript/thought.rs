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
    if let Some(subject) = bold_subject_of(thinking) {
        return Some(subject);
    }
    thinking
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
}

/// The spinner's thought subject RESTRICTED to a DISTINCT bold `**subject**`
/// (qwen `parseThought`), with NO last-line head fallback. Used when the live
/// `✦ Thinking` tail is on screen (non-compact): the tail already shows the
/// reasoning head, so the head fallback [`thought_subject_of`] uses would render
/// the SAME text twice - once in the tail, once on the spinner line. `None` when
/// there is no bold subject, so the spinner falls back to the lull phrase rather
/// than echoing the tail.
pub(super) fn bold_subject_of(thinking: &str) -> Option<String> {
    parse_thought_subject(thinking)
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
#[path = "../../../tests/ui/transcript/thought.rs"]
mod tests;
