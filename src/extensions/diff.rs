//! The first Extension (ADR-0007): diffs on file edits.
//!
//! Covers edit_file and write_file. Both snapshot the target file in
//! [`pre_run`](Diff::pre_run); [`post_run`](Diff::post_run) on a successful
//! operation re-reads the file, computes line hunks ([`hunks`]), and attaches
//! the `diff` Artifact. write_file OVERWRITES (qwen's contract), so a write over
//! an existing file renders an old->new diff; a write that creates a fresh file
//! (no snapshot captured) renders an all-added created-file diff from the
//! written content alone. [`present`](Diff::present) replaces the one-line Tool
//! Result summary with a first-class [`TranscriptItem::Diff`] (the semantic
//! display vocabulary, ADR-0008).
//!
//! The model-facing content is left alone: edit_file replaces EXACT literal
//! text (qwen's contract) and write_file writes the model's content verbatim,
//! so what landed on disk is what the model sent - neither needs a grounding
//! diff appended to the Tool Result.
//!
//! This Extension composes both roles (ADR-0042): a Middleware
//! (`pre_run` snapshots, `post_run` computes hunks and attaches the Artifact)
//! and a Presenter (`present` replaces the summary with a [`TranscriptItem::Diff`]).

pub mod display;
pub mod hunks;

use std::collections::HashMap;

use serde_json::Value;

use crate::middleware::{Middleware, Token};
use crate::presenter::Presenter;
use crate::tool::path::resolve_path;
use crate::view_model::TranscriptItem;
use display::Diff as DiffArtifact;

/// The Token keys the Diff extension reserves, declared in one place.
///
/// `assigns` and `artifacts` are open `HashMap`s by design (ADR-0007:
/// Extensions are an open extension seam, so the core [`Token`] never closes
/// the key universe). Each Extension owns its own reserved keys instead; naming
/// them here once means a producer and consumer that disagree fail to *compile*
/// rather than silently missing the value - a rename touches this module alone.
mod keys {
    /// `assigns`: the pre-edit file snapshot [`super::Diff::pre_run`] captures,
    /// read back by [`super::Diff::post_run`] to compute the edit's hunks.
    pub const BEFORE: &str = "before";

    /// `artifacts`: the serialized [`super::DiffArtifact`] that rides the
    /// Tool Result to Presentment (CONTEXT.md: Artifact), read back by
    /// [`super::Diff::present`].
    pub const DIFF: &str = "diff";
}

/// The tools the Diff extension acts on.
const TOOLS: [&str; 2] = ["edit", "write_file"];

/// The Diff extension (ADR-0007).
pub struct Diff;

impl Middleware for Diff {
    fn pre_run(&self, token: Token, _opts: &Value) -> Token {
        // Both edit_file and write_file need a before-snapshot: write_file now
        // overwrites (qwen's contract), so an overwrite has an old->new diff. A
        // fresh create has no readable target here, so no snapshot is captured
        // and post_run renders the all-added created-file diff instead.
        if !TOOLS.contains(&token.tool.as_str()) {
            return token;
        }
        match target(&token) {
            Some(abs) => match std::fs::read_to_string(&abs) {
                Ok(content) => token.assign(keys::BEFORE, content),
                Err(_) => token,
            },
            None => token,
        }
    }

    fn post_run(&self, token: Token, _opts: &Value) -> Token {
        let is_error = token.result.as_ref().map(|r| r.is_error).unwrap_or(true);

        match token.tool.as_str() {
            "edit" if !is_error => {
                let abs = match target(&token) {
                    Some(abs) => abs,
                    None => return token,
                };
                let before = match token.assigns.get(keys::BEFORE).and_then(|v| v.as_str()) {
                    Some(b) => b.to_string(),
                    None => return token,
                };
                let after_content = match std::fs::read_to_string(&abs) {
                    Ok(c) => c,
                    Err(_) => return token,
                };
                edit_diff(token, &before, &after_content)
            }
            "write_file" if !is_error => {
                let abs = match target(&token) {
                    Some(abs) => abs,
                    None => return token,
                };
                let content = match std::fs::read_to_string(&abs) {
                    Ok(c) => c,
                    Err(_) => return token,
                };
                // An overwrite has a before-snapshot: render an old->new diff.
                // A fresh create has none: render the all-added created-file diff.
                // write_file writes the model's content verbatim either way, so
                // neither path needs grounding.
                let before = token
                    .assigns
                    .get(keys::BEFORE)
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                match before {
                    Some(before) => edit_diff(token, &before, &content),
                    None => created_diff(token, &content),
                }
            }
            _ => token,
        }
    }
}

impl Presenter for Diff {
    fn present(
        &self,
        item: TranscriptItem,
        artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        // Replace a successful edit_file/write_file Tool Result summary with a
        // first-class Diff item; everything else passes through.
        if let TranscriptItem::ToolResult {
            name,
            is_error: false,
            ..
        } = &item
            && TOOLS.contains(&name.as_str())
            && let Some(diff) = read_diff_artifact(artifacts)
        {
            let (hunks, elided) = display::hunks(&diff, display::DISPLAY_LINES);
            return TranscriptItem::Diff {
                title: display::title(name, &diff),
                lang: display::lang(&diff.path),
                hunks,
                elided,
            };
        }
        item
    }
}

// ---- post_run internals ----

// An old->new diff for an edit_file replacement or a write_file overwrite. Both
// land exactly what the model sent (edit_file replaces exact literal text,
// write_file writes verbatim), so the diff is purely for display - no grounding.
// A change that produced no textual difference attaches no artifact.
fn edit_diff(token: Token, before: &str, after_content: &str) -> Token {
    let computed = hunks::compute(before, after_content);
    if computed.is_empty() {
        return token;
    }
    let stats = hunks::stats(&computed);
    let artifact = DiffArtifact {
        path: path(&token),
        hunks: computed,
        added: stats.added,
        removed: stats.removed,
        created: false,
    };
    put_diff(token, &artifact)
}

// A created file is one all-added hunk; write_file writes the model's content
// verbatim, so it never needs grounding.
fn created_diff(token: Token, content: &str) -> Token {
    let computed = hunks::all_added(content);
    let stats = hunks::stats(&computed);
    let artifact = DiffArtifact {
        path: path(&token),
        hunks: computed,
        added: stats.added,
        removed: 0,
        created: true,
    };
    put_diff(token, &artifact)
}

// ---- artifact (de)serialization ----

// Artifacts ride the event as JSON (baud's map); serialize the Diff into the
// `diff` slot.
fn put_diff(token: Token, artifact: &DiffArtifact) -> Token {
    let value = serde_json::to_value(artifact).expect("Diff artifact serializes");
    token.put_artifact(keys::DIFF, value)
}

fn read_diff_artifact(artifacts: &HashMap<String, Value>) -> Option<DiffArtifact> {
    let value = artifacts.get(keys::DIFF)?;
    serde_json::from_value(value.clone()).ok()
}

// ---- shared ----

// Both edit_file and write_file name their path parameter `file_path` (qwen's
// absolute-path contract), so a single lookup serves both.
fn path_param(token: &Token) -> Option<&str> {
    token.input.get("file_path").and_then(|v| v.as_str())
}

fn target(token: &Token) -> Option<std::path::PathBuf> {
    // `resolve_path` normalizes the absolute `file_path` both tools carry.
    resolve_path(path_param(token)?, &token.ctx.root).ok()
}

fn path(token: &Token) -> String {
    path_param(token).unwrap_or("").to_string()
}

#[cfg(test)]
#[path = "../../tests/extensions/diff.rs"]
mod tests;
