//! The first Extension (ADR-0007): diffs on file edits.
//!
//! Covers edit_file and write_file. For edit_file, [`pre_run`](Diff::pre_run)
//! snapshots the target file; [`post_run`](Diff::post_run) on a successful edit
//! re-reads the file, computes line hunks ([`hunks`]), and attaches the `diff`
//! Artifact. write_file only creates (it refuses to overwrite), so it needs no
//! snapshot: `post_run` on a successful write attaches an all-added created-file
//! diff from the written content alone. [`present`](Diff::present) replaces the
//! one-line Tool Result summary with a first-class [`TranscriptItem::Diff`] (the
//! semantic display vocabulary, ADR-0008).
//!
//! The model-facing content is left alone with one exception: when edit_file's
//! fuzzy re-indentation path landed something other than a naive exact
//! old_str→new_str replacement of the snapshot, the model's picture of the file
//! is wrong, so a compact unified diff is appended to the Tool Result. Detection
//! is semantic (recompute the exact replacement and compare), never by matching
//! the tool's message. write_file writes the model's content verbatim, so it
//! never needs grounding. This is the "only on fuzzy match" policy: Context
//! Budget is spent exactly where the model would otherwise be blind.
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
const TOOLS: [&str; 2] = ["edit_file", "write_file"];

/// Model-facing compact diff cap (lines); the Result Cap still applies on top.
/// Display cap is looser - Artifacts cost no Context Budget.
const MODEL_DIFF_LINES: usize = 40;

/// The Diff extension (ADR-0007).
pub struct Diff;

impl Middleware for Diff {
    fn pre_run(&self, token: Token, _opts: &Value) -> Token {
        // Only edit_file needs a before-snapshot; write_file only creates.
        if token.tool != "edit_file" {
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
            "edit_file" if !is_error => {
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
                diff(token, &before, &after_content)
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
                created_diff(token, &content)
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

fn diff(token: Token, before: &str, after_content: &str) -> Token {
    let computed = hunks::compute(before, after_content);
    if computed.is_empty() {
        return token;
    }

    let stats = hunks::stats(&computed);
    let artifact = DiffArtifact {
        path: path(&token),
        hunks: computed.clone(),
        added: stats.added,
        removed: stats.removed,
        created: false,
    };

    let token = put_diff(token, &artifact);
    maybe_ground_model(token, before, after_content, &computed)
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

// Only edit_file's fuzzy re-indentation path can land something the model did
// not send; when the applied edit differs from a naive exact replacement,
// ground the model with a compact diff of what was written.
fn maybe_ground_model(
    mut token: Token,
    before: &str,
    after_content: &str,
    computed: &[hunks::Hunk],
) -> Token {
    if exact_apply(before, &token.input).as_deref() == Some(after_content) {
        return token;
    }
    let compact = hunks::to_unified(computed, MODEL_DIFF_LINES);
    if let Some(result) = token.result.as_mut() {
        let annotated = format!(
            "{}\n[the match was fuzzy - what was actually written:]\n{}",
            result.text_of(),
            compact
        );
        result.set_text(annotated);
    }
    token
}

// What an exact single-occurrence replacement would have produced. A successful
// edit with zero or 2+ exact occurrences can only have gone through the fuzzy
// pass (edit_file errors on ambiguity), so `None` means fuzzy.
fn exact_apply(before: &str, input: &Value) -> Option<String> {
    let old_str = input.get("old_str").and_then(|v| v.as_str())?;
    let new_str = input.get("new_str").and_then(|v| v.as_str())?;
    if old_str.is_empty() {
        return None;
    }
    let parts: Vec<&str> = before.split(old_str).collect();
    if parts.len() == 2 {
        Some(format!("{}{}{}", parts[0], new_str, parts[1]))
    } else {
        None
    }
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

fn target(token: &Token) -> Option<std::path::PathBuf> {
    let path = token.input.get("path").and_then(|v| v.as_str())?;
    resolve_path(path, &token.ctx.root).ok()
}

fn path(token: &Token) -> String {
    token
        .input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
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

    // ============================================================
    // edit_file
    // ============================================================

    #[tokio::test]
    async fn edit_file_exact_match_diff_artifact_model_facing_content_stays_terse() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let path = "sample.txt";
        std::fs::write(tmp.path().join(path), "one\ntwo\nthree\n").unwrap();

        let input = json!({"path": path, "old_str": "two", "new_str": "TWO"});
        let result = run("edit_file", input, &ctx).await;

        assert!(!result.is_error);
        assert_eq!(result.text(), format!("edited {path}"));

        let diff = diff_of(&result);
        assert_eq!(diff.path, path);
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
    async fn edit_file_fuzzy_match_a_compact_diff_grounds_the_model() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let path = "code.ex";
        std::fs::write(tmp.path().join(path), "def foo do\n  x = 1\nend\n").unwrap();

        // Wrong indentation forces the whitespace-normalized pass; the file keeps
        // its real 2-space indent, which the model never sent.
        let input = json!({"path": path, "old_str": "      x = 1", "new_str": "      y = 2"});
        let result = run("edit_file", input, &ctx).await;

        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(path)).unwrap(),
            "def foo do\n  y = 2\nend\n"
        );

        assert!(
            result
                .text()
                .contains("[the match was fuzzy - what was actually written:]")
        );
        assert!(result.text().contains("-  x = 1"));
        assert!(result.text().contains("+  y = 2"));

        let diff = diff_of(&result);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
    }

    #[tokio::test]
    async fn edit_file_failed_edit_no_artifact_error_content_untouched() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let path = "sample.txt";
        std::fs::write(tmp.path().join(path), "one\n").unwrap();

        let input = json!({"path": path, "old_str": "missing", "new_str": "x"});
        let result = run("edit_file", input, &ctx).await;

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

        let input = json!({"path": "fresh.txt", "content": "a\nb\n"});
        let result = run("write_file", input, &ctx).await;

        assert!(!result.is_error);
        assert!(result.text().contains("created fresh.txt"));

        let diff = diff_of(&result);
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
    async fn write_file_pre_run_takes_no_snapshot_creation_only_nothing_to_diff_against() {
        // write_file never overwrites, so there is no before-content to snapshot;
        // the created diff is computed from the written file alone.
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let path = "existing.txt";
        std::fs::write(tmp.path().join(path), "old\n").unwrap();

        let regs = extensions();
        let (token, failures) = extensions::pre_run(
            &regs,
            Token::new(
                "write_file",
                json!({"path": path, "content": "x"}),
                ctx.clone(),
            ),
        );
        assert!(failures.is_empty());
        assert!(!token.assigns.contains_key(keys::BEFORE));
    }

    #[tokio::test]
    async fn write_file_refused_overwrite_error_result_no_diff_artifact() {
        // write_file refuses to overwrite an existing file, so the Diff extension
        // has nothing to diff - the error passes through with no artifact.
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        let path = "config.txt";
        std::fs::write(tmp.path().join(path), "keep\nold\n").unwrap();

        let input = json!({"path": path, "content": "keep\nnew\n"});
        let result = run("write_file", input, &ctx).await;

        assert!(result.is_error);
        assert!(result.text().contains("edit_file"));
        assert!(result.artifacts.is_empty());
        // The file is untouched.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(path)).unwrap(),
            "keep\nold\n"
        );
    }

    // ============================================================
    // other tools
    // ============================================================

    #[tokio::test]
    async fn other_tools_read_file_passes_through_untouched() {
        let tmp = TempDir::new().unwrap();
        let ctx = ctx(tmp.path());
        std::fs::write(tmp.path().join("r.txt"), "hello").unwrap();

        let result = run("read_file", json!({"path": "r.txt"}), &ctx).await;

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
            name: "edit_file".to_string(),
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
        assert_eq!(title, "edit_file lib/x.ex (+1 -1)");
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
            name: "edit_file".to_string(),
            summary: "old_str not found".to_string(),
            is_error: true,
            key_arg: None,
        };
        let call_item = TranscriptItem::ToolCall {
            id: "t1".to_string(),
            name: "edit_file".to_string(),
            summary: "path=x".to_string(),
        };
        let plain_item = TranscriptItem::ToolResult {
            name: "edit_file".to_string(),
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
