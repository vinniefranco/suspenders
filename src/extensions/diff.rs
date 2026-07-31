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
mod tests {
    use super::*;
    use crate::extensions::{self, Registered};
    use crate::tool::ToolCtx;
    use crate::view_model::{DiffLine, DiffSide};
    use serde_json::json;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx::for_test(root.to_path_buf(), 10_000)
    }

    fn extensions() -> Vec<Registered> {
        vec![
            Registered::new("Diff", json!({}))
                .with_middleware(Box::new(Diff))
                .with_presenter(Box::new(Diff)),
        ]
    }

    // The lifecycle exactly as the Run runs it: pre_run, then execution with
    // post_run and Shaping inside extensions::execute.
    async fn run(name: &str, input: Value, ctx: &ToolCtx) -> extensions::PipelineResult {
        let regs = extensions();
        let (token, failures) = extensions::pre_run(&regs, Token::new(name, input, ctx.clone()));
        assert!(failures.is_empty());
        let (result, failures) = extensions::execute(&regs, token).await;
        assert!(failures.is_empty());
        result
    }

    fn diff_of(result: &extensions::PipelineResult) -> DiffArtifact {
        read_diff_artifact(&result.artifacts).expect("diff artifact present")
    }

    // edit_file / write_file now take an absolute `file_path` (qwen contract).
    fn abs(root: &std::path::Path, rel: &str) -> String {
        root.join(rel).to_string_lossy().into_owned()
    }

    // Write `body` to `rel` under `ctx.root` and record a prior read of it, so
    // edit_file's read-before-edit gate is satisfied.
    fn seed_read(ctx: &ToolCtx, rel: &str, body: &str) {
        let abs = ctx.root.join(rel);
        std::fs::write(&abs, body).unwrap();
        let meta = std::fs::metadata(&abs).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        ctx.read_cache()
            .record_read(abs, mtime, meta.len(), true, true);
    }

    // ============================================================
    // edit_file
    // ============================================================

    #[tokio::test]
    async fn edit_file_exact_match_diff_artifact_model_facing_content_stays_terse() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        seed_read(&ctx, "sample.txt", "one\ntwo\nthree\n");
        let target = abs(tmp.path(), "sample.txt");

        let input = json!({"file_path": &target, "old_string": "two", "new_string": "TWO"});
        let result = run("edit", input, &ctx).await;

        assert!(!result.is_error);
        // The model-facing content is qwen's base update line plus the
        // edited-region snippet; the Diff extension appends no grounding.
        assert!(result.text().starts_with(&format!(
            "The file: {target} has been updated. Showing lines"
        )));
        assert!(!result.text().contains("the match was fuzzy"));

        let diff = diff_of(&result);
        assert_eq!(diff.path, target);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        assert!(!diff.created);

        assert_eq!(diff.hunks.len(), 1);
        let lines = &diff.hunks[0].lines;
        assert!(lines.iter().any(|l| *l
            == hunks::Line {
                tag: hunks::Tag::Removed,
                old: Some(2),
                new: None,
                text: "two".to_string()
            }));
        assert!(lines.iter().any(|l| *l
            == hunks::Line {
                tag: hunks::Tag::Added,
                old: None,
                new: Some(2),
                text: "TWO".to_string()
            }));
    }

    #[tokio::test]
    async fn edit_file_replacement_content_is_not_grounded_by_the_diff_extension() {
        // The Diff extension no longer appends a "fuzzy" grounding block; the
        // model-facing content is exactly edit_file's own success message (base
        // update line + qwen's edited-region snippet).
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        seed_read(&ctx, "code.ex", "def foo do\n  x = 1\nend\n");
        let target = abs(tmp.path(), "code.ex");

        let input = json!({"file_path": &target, "old_string": "  x = 1", "new_string": "  y = 2"});
        let result = run("edit", input, &ctx).await;

        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("code.ex")).unwrap(),
            "def foo do\n  y = 2\nend\n"
        );
        // No grounding annotation from the Diff extension.
        assert!(!result.text().contains("the match was fuzzy"));
        assert!(result.text().starts_with(&format!(
            "The file: {target} has been updated. Showing lines"
        )));

        let diff = diff_of(&result);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
    }

    #[tokio::test]
    async fn edit_file_failed_edit_no_artifact_error_content_untouched() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        seed_read(&ctx, "sample.txt", "one\n");
        let target = abs(tmp.path(), "sample.txt");

        let input = json!({"file_path": &target, "old_string": "missing", "new_string": "x"});
        let result = run("edit", input, &ctx).await;

        assert!(result.is_error);
        assert!(result.artifacts.is_empty());
    }

    // ============================================================
    // write_file
    // ============================================================

    #[tokio::test]
    async fn write_file_created_file_all_added_diff_created_true() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());

        let target = abs(tmp.path(), "fresh.txt");
        let input = json!({"file_path": &target, "content": "a\nb\n"});
        let result = run("write_file", input, &ctx).await;

        assert!(!result.is_error);
        assert!(
            result
                .text()
                .contains("Successfully created and wrote to new file:")
        );

        let diff = diff_of(&result);
        assert_eq!(diff.path, target);
        assert_eq!(diff.added, 2);
        assert_eq!(diff.removed, 0);
        assert!(diff.created);
        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(
            diff.hunks[0].lines,
            vec![
                hunks::Line {
                    tag: hunks::Tag::Added,
                    old: None,
                    new: Some(1),
                    text: "a".to_string()
                },
                hunks::Line {
                    tag: hunks::Tag::Added,
                    old: None,
                    new: Some(2),
                    text: "b".to_string()
                },
            ]
        );
    }

    #[tokio::test]
    async fn write_file_creation_takes_no_snapshot_and_renders_a_created_diff() {
        // A fresh create has no readable target, so pre_run captures no snapshot;
        // the created diff is computed from the written file alone.
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let target = abs(tmp.path(), "fresh.txt");

        let regs = extensions();
        let (token, failures) = extensions::pre_run(
            &regs,
            Token::new(
                "write_file",
                json!({"file_path": &target, "content": "x"}),
                ctx.clone(),
            ),
        );
        assert!(failures.is_empty());
        assert!(!token.assigns.contains_key(keys::BEFORE));
    }

    #[tokio::test]
    async fn write_file_overwrite_snapshots_and_renders_an_old_to_new_diff() {
        // write_file now OVERWRITES (qwen's contract): pre_run snapshots the
        // existing file and post_run renders an old->new diff, created:false.
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let target = abs(tmp.path(), "config.txt");
        std::fs::write(tmp.path().join("config.txt"), "keep\nold\n").unwrap();

        let input = json!({"file_path": &target, "content": "keep\nnew\n"});
        let result = run("write_file", input, &ctx).await;

        assert!(!result.is_error);
        assert!(result.text().contains("Successfully overwrote file:"));
        // The file was overwritten.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("config.txt")).unwrap(),
            "keep\nnew\n"
        );

        let diff = diff_of(&result);
        assert_eq!(diff.path, target);
        assert!(!diff.created);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        let lines = &diff.hunks[0].lines;
        assert!(lines.iter().any(|l| *l
            == hunks::Line {
                tag: hunks::Tag::Removed,
                old: Some(2),
                new: None,
                text: "old".to_string()
            }));
        assert!(lines.iter().any(|l| *l
            == hunks::Line {
                tag: hunks::Tag::Added,
                old: None,
                new: Some(2),
                text: "new".to_string()
            }));
    }

    // ============================================================
    // other tools
    // ============================================================

    #[tokio::test]
    async fn other_tools_read_file_passes_through_untouched() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        std::fs::write(tmp.path().join("r.txt"), "hello").unwrap();

        // read_file now takes an absolute `file_path` (qwen contract).
        let target = tmp.path().join("r.txt").to_string_lossy().into_owned();
        let result = run("read_file", json!({"file_path": target}), &ctx).await;

        assert!(!result.is_error);
        assert!(result.artifacts.is_empty());
    }

    // ============================================================
    // present/3
    // ============================================================

    fn diff_artifact(overrides: impl FnOnce(&mut DiffArtifact)) -> HashMap<String, Value> {
        let hunks = hunks::compute("a\nb\nc", "a\nB\nc");
        let mut artifact = DiffArtifact {
            path: "lib/x.ex".to_string(),
            hunks,
            added: 1,
            removed: 1,
            created: false,
        };
        overrides(&mut artifact);
        let mut map = HashMap::new();
        map.insert(
            keys::DIFF.to_string(),
            serde_json::to_value(&artifact).unwrap(),
        );
        map
    }

    #[test]
    fn present_replaces_a_successful_tool_result_with_a_diff_item() {
        let item = TranscriptItem::ToolResult {
            name: "edit".to_string(),
            summary: "edited lib/x.ex".to_string(),
            is_error: false,
            key_arg: None,
        };

        let presented = Diff.present(item, &diff_artifact(|_| {}), &json!({}));

        let TranscriptItem::Diff {
            title,
            lang,
            hunks,
            elided,
        } = presented
        else {
            panic!("expected a diff");
        };
        assert_eq!(title, "edit lib/x.ex (+1 -1)");
        // "lib/x.ex" resolves its language from the extension.
        assert_eq!(lang.as_deref(), Some("ex"));
        assert_eq!(elided, 0);
        assert_eq!(hunks.len(), 1);
        // The `@@ … @@` hunk header is kept as structure; the lines are RAW code
        // with no +/-/context marker (the adapter adds it).
        assert_eq!(hunks[0].header.as_deref(), Some("@@ -1,3 +1,3 @@"));
        let lines = &hunks[0].lines;
        assert!(lines.contains(&DiffLine::new(DiffSide::Removed, "b")));
        assert!(lines.contains(&DiffLine::new(DiffSide::Added, "B")));
        assert!(lines.contains(&DiffLine::new(DiffSide::Context, "a")));
    }

    #[test]
    fn present_a_created_file_titles_as_new_and_skips_the_hunk_header() {
        let artifacts = diff_artifact(|a| {
            a.hunks = hunks::all_added("a\n");
            a.added = 1;
            a.removed = 0;
            a.created = true;
        });
        let item = TranscriptItem::ToolResult {
            name: "write_file".to_string(),
            summary: "created lib/x.ex".to_string(),
            is_error: false,
            key_arg: None,
        };

        let presented = Diff.present(item, &artifacts, &json!({}));

        let TranscriptItem::Diff { title, hunks, .. } = presented else {
            panic!("expected a diff");
        };
        assert_eq!(title, "write_file lib/x.ex (new file, +1)");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].header, None);
        assert_eq!(hunks[0].lines, vec![DiffLine::new(DiffSide::Added, "a")]);
    }

    #[test]
    fn present_long_diffs_cap_and_report_the_elided_count() {
        let content = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let artifacts = diff_artifact(|a| {
            a.hunks = hunks::all_added(&content);
            a.added = 100;
            a.removed = 0;
            a.created = true;
        });
        let item = TranscriptItem::ToolResult {
            name: "write_file".to_string(),
            summary: "created big".to_string(),
            is_error: false,
            key_arg: None,
        };

        let presented = Diff.present(item, &artifacts, &json!({}));

        let TranscriptItem::Diff { hunks, elided, .. } = presented else {
            panic!("expected a diff");
        };
        let shown: usize = hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(shown, display::DISPLAY_LINES);
        assert_eq!(elided, 40);
    }

    #[test]
    fn present_errors_other_items_and_missing_artifacts_pass_through() {
        let error_item = TranscriptItem::ToolResult {
            name: "edit".to_string(),
            summary: "old_str not found".to_string(),
            is_error: true,
            key_arg: None,
        };
        let call_item = TranscriptItem::ToolCall {
            id: "t1".to_string(),
            name: "edit".to_string(),
            summary: "path=x".to_string(),
        };
        let plain_item = TranscriptItem::ToolResult {
            name: "edit".to_string(),
            summary: "edited x".to_string(),
            is_error: false,
            key_arg: None,
        };

        assert_eq!(
            Diff.present(error_item.clone(), &diff_artifact(|_| {}), &json!({})),
            error_item
        );
        assert_eq!(
            Diff.present(call_item.clone(), &HashMap::new(), &json!({})),
            call_item
        );
        assert_eq!(
            Diff.present(plain_item.clone(), &HashMap::new(), &json!({})),
            plain_item
        );
    }
}
