//! Display formatting for diff artifacts (ADR-0008 semantic display
//! vocabulary).
//!
//! Produces a title, the source language, and the tagged [`DiffHunk`]s from a
//! diff artifact. Extracted from [`crate::extensions::diff`] so the rendering
//! logic is unit-testable without a Middleware lifecycle.
//!
//! Lines carry a [`DiffSide`] - `Added`, `Removed`, `Context` - and RAW code
//! text (no `+`/`-` marker); a later `ui/components` phase maps the side to a
//! background tint, adds the marker glyph, and layers the syntect foreground.
//! The Presenter decides WHAT to show (this language, these hunks, this much
//! elided); the adapter decides HOW.

use serde::{Deserialize, Serialize};

use crate::extensions::diff::hunks::{Hunk, Line, Tag};
use crate::view_model::{DiffHunk, DiffLine, DiffSide};

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
/// title("edit", %{path: "lib/foo.ex", added: 3, removed: 1, created: false})
/// // => "edit lib/foo.ex (+3 -1)"
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

/// The source-language token for a diff's path: the file extension (`rs`, `js`,
/// `json`, …), which the adapter resolves to a syntax. `None` when the path has
/// no extension - the core never names a syntect syntax, it carries the language
/// fact (ADR-0019). An unknown extension still rides through as `Some(ext)`; the
/// adapter falls back when it resolves no syntax.
pub fn lang(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_string)
}

/// Builds the tagged [`DiffHunk`]s for a diff, capped at `max_lines` code lines
/// across all hunks. Returns the hunks and the count of lines the cap elided
/// (`0` when nothing was cut) - the adapter renders that count as a muted
/// `… N more lines` tail.
///
/// Created files omit the hunk header (no `@@` line - it would be noise on an
/// all-added file); otherwise each hunk carries its unified-diff header. Every
/// line's text is RAW code with NO `+`/`-` marker: the adapter adds the marker
/// glyph and the tint, so the same text can also feed the syntect highlighter.
pub fn hunks(diff: &Diff, max_lines: usize) -> (Vec<DiffHunk>, usize) {
    // Counts CODE lines only (the elision tail reports elided code, not the
    // per-hunk `@@` headers, which the adapter always keeps for shown hunks).
    let total: usize = diff.hunks.iter().map(|hunk| hunk.lines.len()).sum();
    let mut budget = max_lines;
    let mut out = Vec::with_capacity(diff.hunks.len());

    for hunk in &diff.hunks {
        if budget == 0 {
            break;
        }
        let take = hunk.lines.len().min(budget);
        budget -= take;
        out.push(DiffHunk {
            header: hunk_header(hunk, diff.created),
            lines: hunk.lines[..take].iter().map(diff_line).collect(),
        });
    }

    (out, total.saturating_sub(max_lines))
}

// A created file is one all-added hunk; the @@ header would be noise.
fn hunk_header(hunk: &Hunk, created: bool) -> Option<String> {
    if created {
        None
    } else {
        Some(format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ))
    }
}

// The line's [`DiffSide`] over its RAW text (ADR-0008): the adapter adds the
// `+`/`-`/context marker and the color. This is an Extension display choice (the
// Extension decides WHAT to show; the adapter decides HOW).
fn diff_line(line: &Line) -> DiffLine {
    let side = match line.tag {
        Tag::Context => DiffSide::Context,
        Tag::Added => DiffSide::Added,
        Tag::Removed => DiffSide::Removed,
    };
    DiffLine::new(side, line.text.clone())
}

#[cfg(test)]
#[path = "../../../tests/extensions/diff/display.rs"]
mod tests;
