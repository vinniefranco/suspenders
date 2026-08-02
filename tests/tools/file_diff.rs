use super::*;
use crate::tool::{Tool, ToolCtx};
use crate::tools::edit_file::EditFile;
use crate::tools::write_file::WriteFile;
use crate::view_model::{DiffLine, DiffSide, TranscriptItem};
use serde_json::json;
use std::collections::HashMap;
use tempfile::TempDir;

fn ctx(root: &std::path::Path) -> ToolCtx {
    ToolCtx::for_test(root.to_path_buf(), 10_000)
}

// edit_file / write_file take an absolute `file_path` (qwen contract).
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

// The `diff` Artifact off a tool's rich output, decoded back into a DiffArtifact.
fn diff_of(output: &crate::tool::ToolOutput) -> DiffArtifact {
    read_artifact(&output.artifacts).expect("diff artifact present")
}

// ============================================================
// edit_file: the tool computes its own diff Artifact
// ============================================================

#[tokio::test]
async fn edit_file_exact_match_attaches_an_old_to_new_diff_and_stays_terse() {
    let tmp = TempDir::new().unwrap();
    let ctx = ctx(tmp.path());
    seed_read(&ctx, "sample.txt", "one\ntwo\nthree\n");
    let target = abs(tmp.path(), "sample.txt");

    let input = json!({"file_path": &target, "old_string": "two", "new_string": "TWO"});
    let output = EditFile.run_rich(&input, &ctx).await.unwrap();

    assert!(!output.is_error);
    // The model-facing content is edit_file's own success message; the diff is
    // display-only and appends no grounding.
    let text = crate::content::result_blocks_text(&output.blocks);
    assert!(text.starts_with(&format!("The file: {target} has been updated. Showing lines")));
    assert!(!text.contains("the match was fuzzy"));

    let diff = diff_of(&output);
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
async fn edit_file_run_projects_the_message_without_the_diff() {
    // `run` (the text projection) returns exactly edit_file's success message and
    // carries no Artifact - the diff lives on the `run_rich` path only.
    let tmp = TempDir::new().unwrap();
    let ctx = ctx(tmp.path());
    seed_read(&ctx, "code.ex", "def foo do\n  x = 1\nend\n");
    let target = abs(tmp.path(), "code.ex");

    let input = json!({"file_path": &target, "old_string": "  x = 1", "new_string": "  y = 2"});
    let message = EditFile.run(&input, &ctx).await.unwrap();

    assert!(message.starts_with(&format!("The file: {target} has been updated. Showing lines")));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("code.ex")).unwrap(),
        "def foo do\n  y = 2\nend\n"
    );
}

#[tokio::test]
async fn edit_file_failed_edit_is_an_err_and_carries_no_diff() {
    let tmp = TempDir::new().unwrap();
    let ctx = ctx(tmp.path());
    seed_read(&ctx, "sample.txt", "one\n");
    let target = abs(tmp.path(), "sample.txt");

    let input = json!({"file_path": &target, "old_string": "missing", "new_string": "x"});
    let result = EditFile.run_rich(&input, &ctx).await;

    assert!(result.is_err());
}

// ============================================================
// write_file: created vs overwrite diff Artifact
// ============================================================

#[tokio::test]
async fn write_file_created_file_attaches_an_all_added_diff_created_true() {
    let tmp = TempDir::new().unwrap();
    let ctx = ctx(tmp.path());

    let target = abs(tmp.path(), "fresh.txt");
    let input = json!({"file_path": &target, "content": "a\nb\n"});
    let output = WriteFile.run_rich(&input, &ctx).await.unwrap();

    assert!(!output.is_error);
    let text = crate::content::result_blocks_text(&output.blocks);
    assert!(text.contains("Successfully created and wrote to new file:"));

    let diff = diff_of(&output);
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
async fn write_file_overwrite_snapshots_and_attaches_an_old_to_new_diff() {
    // write_file OVERWRITES (qwen's contract): the pre-write snapshot yields an
    // old->new diff, created:false.
    let tmp = TempDir::new().unwrap();
    let ctx = ctx(tmp.path());
    let target = abs(tmp.path(), "config.txt");
    std::fs::write(tmp.path().join("config.txt"), "keep\nold\n").unwrap();

    let input = json!({"file_path": &target, "content": "keep\nnew\n"});
    let output = WriteFile.run_rich(&input, &ctx).await.unwrap();

    assert!(!output.is_error);
    let text = crate::content::result_blocks_text(&output.blocks);
    assert!(text.contains("Successfully overwrote file:"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("config.txt")).unwrap(),
        "keep\nnew\n"
    );

    let diff = diff_of(&output);
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
// artifact(): the pure producer
// ============================================================

#[test]
fn artifact_is_none_when_an_edit_produces_no_textual_change() {
    // A before == after edit attaches nothing.
    assert!(artifact(Some("a\nb\n"), "a\nb\n", "x.txt").is_none());
}

#[test]
fn artifact_for_a_fresh_create_is_an_all_added_created_diff() {
    let value = artifact(None, "a\nb\n", "new.txt").expect("created diff");
    let diff: DiffArtifact = serde_json::from_value(value).unwrap();
    assert!(diff.created);
    assert_eq!(diff.added, 2);
    assert_eq!(diff.removed, 0);
    assert_eq!(diff.path, "new.txt");
}

// ============================================================
// to_diff_item(): the Transcript store swap
// ============================================================

fn diff_artifact(overrides: impl FnOnce(&mut DiffArtifact)) -> DiffArtifact {
    let hunks = hunks::compute("a\nb\nc", "a\nB\nc");
    let mut artifact = DiffArtifact {
        path: "lib/x.ex".to_string(),
        hunks,
        added: 1,
        removed: 1,
        created: false,
    };
    overrides(&mut artifact);
    artifact
}

#[test]
fn to_diff_item_builds_a_diff_with_a_header_and_raw_marker_free_lines() {
    let item = to_diff_item("edit", &diff_artifact(|_| {}));

    let TranscriptItem::Diff {
        title,
        lang,
        hunks,
        elided,
    } = item
    else {
        panic!("expected a diff");
    };
    assert_eq!(title, "edit lib/x.ex (+1 -1)");
    assert_eq!(lang.as_deref(), Some("ex"));
    assert_eq!(elided, 0);
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].header.as_deref(), Some("@@ -1,3 +1,3 @@"));
    let lines = &hunks[0].lines;
    assert!(lines.contains(&DiffLine::new(DiffSide::Removed, "b")));
    assert!(lines.contains(&DiffLine::new(DiffSide::Added, "B")));
    assert!(lines.contains(&DiffLine::new(DiffSide::Context, "a")));
}

#[test]
fn to_diff_item_for_a_created_file_titles_as_new_and_skips_the_hunk_header() {
    let artifact = diff_artifact(|a| {
        a.hunks = hunks::all_added("a\n");
        a.added = 1;
        a.removed = 0;
        a.created = true;
    });

    let TranscriptItem::Diff { title, hunks, .. } = to_diff_item("write_file", &artifact) else {
        panic!("expected a diff");
    };
    assert_eq!(title, "write_file lib/x.ex (new file, +1)");
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].header, None);
    assert_eq!(hunks[0].lines, vec![DiffLine::new(DiffSide::Added, "a")]);
}

#[test]
fn to_diff_item_caps_long_diffs_and_reports_the_elided_count() {
    let content = (1..=100)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let artifact = diff_artifact(|a| {
        a.hunks = hunks::all_added(&content);
        a.added = 100;
        a.removed = 0;
        a.created = true;
    });

    let TranscriptItem::Diff { hunks, elided, .. } = to_diff_item("write_file", &artifact) else {
        panic!("expected a diff");
    };
    let shown: usize = hunks.iter().map(|h| h.lines.len()).sum();
    assert_eq!(shown, display::DISPLAY_LINES);
    assert_eq!(elided, 40);
}

// ============================================================
// read_artifact(): the Transcript store reader
// ============================================================

#[test]
fn read_artifact_is_none_without_the_diff_key() {
    assert!(read_artifact(&HashMap::new()).is_none());
}
