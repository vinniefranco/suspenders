//! Line-level diff hunks for [`crate::extensions::diff`], computed with a
//! Myers/LCS line diff - no dependency.
//!
//! A hunk is a run of changed lines plus up to 3 context lines on each side;
//! changed runs closer than a context gap merge into one hunk, the classic
//! unified-diff grouping. Every changed line lands in exactly one hunk, so
//! [`stats`] over the hunks is complete.
//!
//! ## The line diff (judgment call)
//!
//! baud computes the script with Elixir's stdlib `List.myers_difference/2`,
//! which yields an ordered edit script of `{:eq | :del | :ins, lines}` chunks.
//! Rust has no stdlib equivalent, so [`myers_difference`] reimplements it: a
//! classic Myers shortest-edit-script over the two line lists, walked back into
//! the same `Eq | Del | Ins` chunk sequence. The chunk order matches
//! `List.myers_difference` for the tested inputs - within a change region all
//! deletions precede all insertions (a `Del` chunk before its paired `Ins`
//! chunk), which the `number`/`group` passes and every ported hunks test rely
//! on.

use serde::{Deserialize, Serialize};

mod assemble;
mod myers;

const CONTEXT: usize = 3;

/// The tag of one diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tag {
    Context,
    Added,
    Removed,
}

/// One diff line: its tag, old-file line number, new-file line number, text.
/// `old`/`new` are `None` where the line has no counterpart on that side
/// (mirrors baud's `nil` line numbers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub tag: Tag,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub text: String,
}

impl Line {
    pub(super) fn new(
        tag: Tag,
        old: Option<usize>,
        new: Option<usize>,
        text: impl Into<String>,
    ) -> Self {
        Line {
            tag,
            old,
            new,
            text: text.into(),
        }
    }
}

/// A diff hunk: a slice of tagged lines with old/new start line numbers and
/// counts. Mirrors baud's `hunk` map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<Line>,
}

/// Total added/removed line counts across the hunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub added: usize,
    pub removed: usize,
}

/// Diffs two file contents into hunks. Identical contents yield `[]`.
pub fn compute(before: &str, after: &str) -> Vec<Hunk> {
    let before_lines: Vec<String> = before.split('\n').map(str::to_string).collect();
    let after_lines: Vec<String> = after.split('\n').map(str::to_string).collect();

    let script = myers::difference(&before_lines, &after_lines);
    assemble::hunks(&script)
}

/// A created file as one all-added hunk. Diffing against the empty string would
/// show a phantom removed empty line (the empty file IS one empty line to a line
/// differ); a new file has no old side at all.
pub fn all_added(content: &str) -> Vec<Hunk> {
    let trimmed = content.strip_suffix('\n').unwrap_or(content);
    let lines: Vec<Line> = trimmed
        .split('\n')
        .enumerate()
        .map(|(i, text)| Line::new(Tag::Added, None, Some(i + 1), text))
        .collect();
    let count = lines.len();

    vec![Hunk {
        old_start: 0,
        old_count: 0,
        new_start: 1,
        new_count: count,
        lines,
    }]
}

/// Total added/removed line counts across the hunks.
pub fn stats(hunks: &[Hunk]) -> Stats {
    let mut added = 0;
    let mut removed = 0;
    for hunk in hunks {
        for line in &hunk.lines {
            match line.tag {
                Tag::Added => added += 1,
                Tag::Removed => removed += 1,
                Tag::Context => {}
            }
        }
    }
    Stats { added, removed }
}

#[cfg(test)]
#[path = "../../../tests/extensions/diff/hunks.rs"]
mod tests;
