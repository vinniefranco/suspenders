//! File-edit diffs for `edit_file` and `write_file` (ADR-0007's diff behavior,
//! relocated into the tools that own it).
//!
//! A file-editing tool knows the before-content (what it read/replaced) and the
//! after-content (what it wrote), so it computes its own diff and attaches the
//! `diff` display Artifact (CONTEXT.md: Artifact) to its Tool Result. The
//! Artifact rides the `:tool_result` event to the Transcript store, which swaps
//! the one-line Tool Result summary for a first-class [`TranscriptItem::Diff`]
//! (the semantic display vocabulary, ADR-0008). write_file OVERWRITES (qwen's
//! contract): an overwrite renders an old->new diff; a fresh create renders an
//! all-added created-file diff from the written content alone.
//!
//! The model-facing content is left alone: edit_file replaces EXACT literal
//! text (qwen's contract) and write_file writes the model's content verbatim, so
//! what landed on disk is what the model sent - neither needs a grounding diff
//! appended to the Tool Result. The diff is purely for display.
//!
//! Pure: no ratatui (ADR-0019). The glyph/colour treatment lives in
//! `ui/components`; this module only carries the parsed hunks to Presentment.

pub mod display;
pub mod hunks;

use std::collections::HashMap;

use serde_json::Value;

use crate::view_model::TranscriptItem;
pub use display::Diff as DiffArtifact;

/// The Artifact key the file-diff behavior reserves, declared in one place: a
/// producer (the tool) and consumer (the Transcript store) that disagree fail
/// to *compile*, and a rename touches this module alone.
pub const DIFF: &str = "diff";

/// The tools whose Tool Result the Transcript store swaps for a Diff when a
/// `diff` Artifact rides it: `edit` (edit_file's wire name) and `write_file`.
pub const EDIT_TOOLS: [&str; 2] = ["edit", "write_file"];

/// The `diff` Artifact for a file edit, or `None` when there is no textual
/// difference (an edit that produced no change attaches nothing). `before` is
/// the pre-edit content (an in-place edit or a write_file overwrite) or `None`
/// for a fresh create - a created file is one all-added hunk, since diffing
/// against the empty string would show a phantom removed empty line. `path` is
/// the model-supplied path, echoed in the diff title. The returned `Value` is
/// the serialized [`DiffArtifact`], ready to attach to the Tool Result.
///
/// Fail-open (ADR-0007): a Diff artifact is a plain struct that always
/// serializes, but should that ever break, attach nothing rather than panic -
/// the Transcript store reads `None` and simply shows no diff.
pub fn artifact(before: Option<&str>, after: &str, path: &str) -> Option<Value> {
    let diff = match before {
        Some(before) => edit_diff(before, after, path)?,
        None => created_diff(after, path),
    };
    serde_json::to_value(&diff).ok()
}

// An old->new diff for an edit_file replacement or a write_file overwrite. Both
// land exactly what the model sent (edit_file replaces exact literal text,
// write_file writes verbatim), so the diff is purely for display - no grounding.
// A change that produced no textual difference attaches no artifact.
fn edit_diff(before: &str, after: &str, path: &str) -> Option<DiffArtifact> {
    let computed = hunks::compute(before, after);
    if computed.is_empty() {
        return None;
    }
    let stats = hunks::stats(&computed);
    Some(DiffArtifact {
        path: path.to_string(),
        hunks: computed,
        added: stats.added,
        removed: stats.removed,
        created: false,
    })
}

// A created file is one all-added hunk; write_file writes the model's content
// verbatim, so it never needs grounding.
fn created_diff(content: &str, path: &str) -> DiffArtifact {
    let computed = hunks::all_added(content);
    let stats = hunks::stats(&computed);
    DiffArtifact {
        path: path.to_string(),
        hunks: computed,
        added: stats.added,
        removed: 0,
        created: true,
    }
}

/// Reads the `diff` Artifact back out of a Tool Result's Artifacts, or `None`
/// when it is absent or malformed. Read by the Transcript store to decide
/// whether to swap the summary for a [`TranscriptItem::Diff`].
pub fn read_artifact(artifacts: &HashMap<String, Value>) -> Option<DiffArtifact> {
    let value = artifacts.get(DIFF)?;
    serde_json::from_value(value.clone()).ok()
}

/// Builds the first-class [`TranscriptItem::Diff`] for a diff Artifact under the
/// given tool `name` - the swap the Transcript store performs on a successful
/// edit_file/write_file Tool Result.
pub fn to_diff_item(name: &str, diff: &DiffArtifact) -> TranscriptItem {
    let (hunks, elided) = display::hunks(diff, display::DISPLAY_LINES);
    TranscriptItem::Diff {
        title: display::title(name, diff),
        lang: display::lang(&diff.path),
        hunks,
        elided,
    }
}

#[cfg(test)]
#[path = "../../tests/tools/file_diff.rs"]
mod tests;
