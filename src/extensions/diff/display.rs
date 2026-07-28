//! Display formatting for diff artifacts (ADR-0008 semantic display
//! vocabulary).
//!
//! Produces a title string and a list of [`StyledLine`]s from a diff artifact.
//! Extracted from [`crate::extensions::diff`] so the rendering logic is
//! unit-testable without a Middleware lifecycle.
//!
//! Lines carry semantic styles - [`LineStyle::Added`], `Removed`, `Context`,
//! `Muted` - that a later `ui/components` phase maps to terminal colors.

use serde::{Deserialize, Serialize};

use crate::extensions::diff::hunks::{Hunk, Line, Tag};
use crate::view_model::{LineStyle, StyledLine};

/// The default display cap: hunks render at most this many lines before eliding
/// with a muted tail (baud's `lines/2` default). Artifacts cost no Context
/// Budget, so this cap is looser than the model-facing one.
pub const DISPLAY_LINES: usize = 60;

/// The diff artifact fields Display renders. Mirrors the `:diff` artifact map's
/// display-relevant shape (`path`, `hunks`, `added`, `removed`, `created`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub path: String,
    pub hunks: Vec<Hunk>,
    pub added: usize,
    pub removed: usize,
    pub created: bool,
}

/// Builds a human-readable title for a diff block.
///
/// ```text
/// title("edit_file", %{path: "lib/foo.ex", added: 3, removed: 1, created: false})
/// // => "edit_file lib/foo.ex (+3 -1)"
/// title("write_file", %{path: "new.ex", added: 5, removed: 0, created: true})
/// // => "write_file new.ex (new file, +5)"
/// ```
pub fn title(name: &str, diff: &Diff) -> String {
    if diff.created {
        format!("{name} {} (new file, +{})", diff.path, diff.added)
    } else {
        format!("{name} {} (+{} -{})", diff.path, diff.added, diff.removed)
    }
}

/// Builds a list of display lines for a diff block, capped at `max_lines`.
///
/// Created files omit the hunk header (no `@@` lines); otherwise each hunk
/// starts with a muted unified-diff header. Lines beyond `max_lines` are elided
/// with a trailing muted entry.
pub fn lines(diff: &Diff, max_lines: usize) -> Vec<StyledLine> {
    let all: Vec<StyledLine> = diff
        .hunks
        .iter()
        .flat_map(|hunk| hunk_lines(hunk, diff.created))
        .collect();

    if all.len() <= max_lines {
        all
    } else {
        let rest = all.len() - max_lines;
        let mut shown: Vec<StyledLine> = all.into_iter().take(max_lines).collect();
        shown.push(StyledLine::new(
            LineStyle::Muted,
            format!("… {rest} more lines"),
        ));
        shown
    }
}

// A created file is one all-added hunk; the @@ header would be noise.
fn hunk_lines(hunk: &Hunk, created: bool) -> Vec<StyledLine> {
    if created {
        hunk.lines.iter().map(display_line).collect()
    } else {
        let header = StyledLine::new(
            LineStyle::Muted,
            format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ),
        );
        std::iter::once(header)
            .chain(hunk.lines.iter().map(display_line))
            .collect()
    }
}

// Minimal diff lines (ADR-0040 Decision D): the `+`/`-`/context markers carry
// the change and the semantic [`LineStyle`] carries the color, while the
// `@@ … @@` hunk header carries the location - so the line-number gutter is
// dropped. This is a Extension display choice (ADR-0008: the Extension decides WHAT
// to show; the adapter maps the style to a color).
fn display_line(line: &Line) -> StyledLine {
    match line.tag {
        Tag::Context => StyledLine::new(LineStyle::Context, format!("  {}", line.text)),
        Tag::Added => StyledLine::new(LineStyle::Added, format!("+ {}", line.text)),
        Tag::Removed => StyledLine::new(LineStyle::Removed, format!("- {}", line.text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::diff::hunks;

    // ---- title/2 ----

    #[test]
    fn title_existing_file() {
        let diff = Diff {
            path: "lib/x.ex".to_string(),
            hunks: vec![],
            added: 3,
            removed: 1,
            created: false,
        };
        assert_eq!(title("edit_file", &diff), "edit_file lib/x.ex (+3 -1)");
    }

    #[test]
    fn title_new_file() {
        let diff = Diff {
            path: "new.ex".to_string(),
            hunks: vec![],
            added: 5,
            removed: 0,
            created: true,
        };
        assert_eq!(
            title("write_file", &diff),
            "write_file new.ex (new file, +5)"
        );
    }

    // ---- lines/2 ----

    #[test]
    fn existing_file_shows_hunk_headers() {
        let hunks = hunks::compute("a\nb\nc", "a\nB\nc");
        let diff = Diff {
            path: String::new(),
            hunks,
            added: 1,
            removed: 1,
            created: false,
        };
        let lines = lines(&diff, DISPLAY_LINES);
        assert!(lines.contains(&StyledLine::new(LineStyle::Muted, "@@ -1,3 +1,3 @@")));
    }

    #[test]
    fn created_file_skips_hunk_headers() {
        let hunks = hunks::all_added("a\n");
        let diff = Diff {
            path: String::new(),
            hunks,
            added: 1,
            removed: 0,
            created: true,
        };
        let lines = lines(&diff, DISPLAY_LINES);
        assert_eq!(lines, vec![StyledLine::new(LineStyle::Added, "+ a")]);
    }

    #[test]
    fn long_diffs_cap_with_a_muted_tail() {
        let content = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let hunks = hunks::all_added(&content);
        let diff = Diff {
            path: String::new(),
            hunks,
            added: 100,
            removed: 0,
            created: true,
        };
        let lines = lines(&diff, DISPLAY_LINES);
        assert_eq!(lines.len(), 61);
        assert_eq!(
            lines.last(),
            Some(&StyledLine::new(LineStyle::Muted, "… 40 more lines"))
        );
    }
}
