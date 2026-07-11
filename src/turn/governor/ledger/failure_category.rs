//! Classifies a failed Tool Result's content into a category, recorded on the
//! Turn [`Ledger`](super::Ledger) as each failure happens (baud:
//! `Baud.Turn.Nudges.FailureCategory`).
//!
//! Placement judgment (ADR-0026 Step 3): the category is a FACT about the
//! error — a deterministic classification of the result content, written once
//! at the firing site and never tuned (no setpoints; correct or incorrect,
//! like the Ledger itself) — so the classifier lives with the Ledger, not
//! with the failure Governor that JUDGES the tallies. The Governor reads
//! counts and recency; the categories ride the streak as recorded facts.
//!
//! The category tells the model *what kind* of failure pattern it is in, not
//! just how many. [`crate::voice::failure_nudge`] consumes these categories to
//! produce a human-readable description.
//!
//! The classification is a flat, ordered list of predicates: the first match
//! wins, falling through to [`FailureCategory::Unknown`] when nothing matches.
//! Kept as its own unit so the rules are testable and the categories form a
//! documented contract between the error surface (tool modules) and the Voice.

use crate::voice::FailureCategory;

/// Maps error content to a category. The heuristics check for known error
/// patterns in the content string; the first matching rule wins.
pub fn classify(content: &str) -> FailureCategory {
    // Ordered rules: the first matching predicate wins. enoent precedes
    // not_found because an ENOENT message ("... enoent (no such file)") often
    // also contains "not found", and the specific category should win.
    if content.contains("enoent") {
        FailureCategory::Enoent
    } else if is_input_error(content) {
        FailureCategory::InputError
    } else if content.contains("not found") {
        FailureCategory::NotFound
    } else if content.contains("timed out") {
        FailureCategory::Timeout
    } else if content.contains("denied") {
        FailureCategory::Denied
    } else if content.contains("path escapes") {
        FailureCategory::PathError
    } else if is_command_error(content) {
        FailureCategory::CommandError
    } else {
        FailureCategory::Unknown
    }
}

fn is_input_error(content: &str) -> bool {
    content.contains("unknown field")
        || content.contains("missing required field")
        || content.contains("should be a string")
        || content.contains("invalid input")
}

fn is_command_error(content: &str) -> bool {
    content.contains("exit code:") && !content.contains("exit code: 0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enoent() {
        assert_eq!(classify("file not found: enoent"), FailureCategory::Enoent);
        assert_eq!(
            classify("could not read x: enoent (no such file)"),
            FailureCategory::Enoent
        );
    }

    #[test]
    fn input_error() {
        assert_eq!(
            classify("unknown field: \"foo\""),
            FailureCategory::InputError
        );
        assert_eq!(
            classify("missing required field(s): \"path\""),
            FailureCategory::InputError
        );
        assert_eq!(
            classify("should be a string, got: 42"),
            FailureCategory::InputError
        );
        assert_eq!(
            classify("invalid input: read_file"),
            FailureCategory::InputError
        );
    }

    #[test]
    fn not_found() {
        assert_eq!(classify("old_str not found"), FailureCategory::NotFound);
    }

    #[test]
    fn timeout() {
        assert_eq!(classify("timed out"), FailureCategory::Timeout);
        assert_eq!(
            classify("command timed out after 120s"),
            FailureCategory::Timeout
        );
    }

    #[test]
    fn denied() {
        assert_eq!(classify("denied by user"), FailureCategory::Denied);
    }

    #[test]
    fn path_error() {
        assert_eq!(
            classify("path escapes project root"),
            FailureCategory::PathError
        );
    }

    #[test]
    fn command_error() {
        assert_eq!(classify("[exit code: 1]"), FailureCategory::CommandError);
        assert_eq!(classify("[exit code: 127]"), FailureCategory::CommandError);
    }

    #[test]
    fn exit_code_0_is_not_command_error() {
        assert_eq!(classify("[exit code: 0]"), FailureCategory::Unknown);
    }

    #[test]
    fn unknown() {
        assert_eq!(classify("some random error"), FailureCategory::Unknown);
        assert_eq!(classify(""), FailureCategory::Unknown);
    }
}
