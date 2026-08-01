
use super::*;
use crate::notebook::Notebook;
use crate::tool::caps::Capabilities;
use crate::tool::read_cache::FileReadCache;
use std::sync::Arc;
use tempfile::TempDir;

const FIXTURE: &str = r#"{
 "cells": [
  {
   "cell_type": "code",
   "id": "run",
   "execution_count": 1,
   "metadata": {},
   "outputs": [],
   "source": [
    "print('hi')\n"
   ]
  }
 ],
 "metadata": {},
 "nbformat": 4,
 "nbformat_minor": 5
}
"#;

// A ctx sharing a caller-supplied read cache, so a test can pre-populate the
// cache (or share it with read_file) and drive the enforcement.
fn ctx_with_cache(root: &std::path::Path, cache: Arc<FileReadCache>) -> ToolCtx {
    let caps = Capabilities::for_test_with_read_cache(cache);
    ToolCtx {
        root: root.to_path_buf(),
        result_cap: 100_000,
        command_timeout_ms: 120_000,
        input_modalities: crate::content::Modalities::default(),
        memory_root: None,
        session_dir: std::env::temp_dir(),
        caps,
    }
}

fn write_fixture(dir: &TempDir) -> std::path::PathBuf {
    let abs = dir.path().join("nb.ipynb");
    std::fs::write(&abs, FIXTURE).unwrap();
    abs
}

// Stat the file for its current (mtime, size) fingerprint.
fn fingerprint(abs: &std::path::Path) -> (u128, u64) {
    let meta = std::fs::metadata(abs).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    (mtime, meta.len())
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    NotebookEdit.run(&input, ctx).await
}

#[test]
fn spec_requires_only_the_notebook_path() {
    let spec = NotebookEdit.spec();
    assert_eq!(spec.name, "notebook_edit");
    assert_eq!(spec.input_schema["required"], json!(["notebook_path"]));
    assert_eq!(
        spec.input_schema["properties"]["notebook_path"]["description"],
        "Absolute path to the Jupyter notebook file to edit. Must end with .ipynb."
    );
    assert_eq!(
        spec.input_schema["properties"]["cell_type"]["enum"],
        json!(["code", "markdown"])
    );
    assert_eq!(
        spec.input_schema["properties"]["edit_mode"]["enum"],
        json!(["replace", "insert", "delete"])
    );
}

#[tokio::test]
async fn a_non_ipynb_path_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let abs = tmp.path().join("notes.txt");
    let err = run(
        json!({"notebook_path": abs.to_string_lossy(), "cell_id": "c", "new_source": "x"}),
        &ctx_with_cache(tmp.path(), cache),
    )
    .await
    .unwrap_err();
    assert!(err.contains("Jupyter notebook (.ipynb)"));
}

#[tokio::test]
async fn editing_an_unread_notebook_is_the_verbatim_rejection() {
    let tmp = TempDir::new().unwrap();
    let abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    let err = run(
            json!({"notebook_path": abs.to_string_lossy(), "cell_id": "run", "new_source": "print('bye')\n"}),
            &ctx_with_cache(tmp.path(), cache),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        format!(
            "Notebook {} has not been fully read in this session. Use the read_file tool \
first, without offset or limit, before editing cells.",
            abs.display()
        )
    );
}

#[tokio::test]
async fn editing_a_stale_notebook_is_the_verbatim_rejection() {
    let tmp = TempDir::new().unwrap();
    let abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    // Record a FULL read at a DIFFERENT fingerprint than the file now has.
    cache.record_read(abs.clone(), 1, 1, true, true);
    let err = run(
            json!({"notebook_path": abs.to_string_lossy(), "cell_id": "run", "new_source": "print('bye')\n"}),
            &ctx_with_cache(tmp.path(), cache),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        format!(
            "Notebook {} has been modified since you last read it. Re-read it with the \
read_file tool before editing it.",
            abs.display()
        )
    );
}

#[tokio::test]
async fn editing_a_truncated_read_notebook_is_the_verbatim_rejection() {
    let tmp = TempDir::new().unwrap();
    let abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    let (mtime, size) = fingerprint(&abs);
    // A read at the CURRENT fingerprint but NOT full (rendered output was
    // truncated when read).
    cache.record_read(abs.clone(), mtime, size, false, true);
    let err = run(
            json!({"notebook_path": abs.to_string_lossy(), "cell_id": "run", "new_source": "print('bye')\n"}),
            &ctx_with_cache(tmp.path(), cache),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        format!(
            "Notebook {} is too large for cell-level editing because its rendered output \
was truncated when read. Reduce the notebook output size or split the notebook before editing \
cells.",
            abs.display()
        )
    );
}

#[tokio::test]
async fn a_full_fresh_read_lets_the_edit_apply_and_records_the_write() {
    let tmp = TempDir::new().unwrap();
    let abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    let (mtime, size) = fingerprint(&abs);
    cache.record_read(abs.clone(), mtime, size, true, true);

    let ctx = ctx_with_cache(tmp.path(), Arc::clone(&cache));
    let msg = run(
            json!({"notebook_path": abs.to_string_lossy(), "cell_id": "run", "new_source": "print('bye')\n"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(msg.contains("replace cell run"));
    assert!(msg.contains("Updated source:"));

    // The file was actually rewritten.
    let on_disk = std::fs::read_to_string(&abs).unwrap();
    let nb = Notebook::parse(&on_disk).unwrap();
    assert_eq!(nb.cells[0].source.normalize(), "print('bye')\n");

    // The write was recorded: a follow-up edit sees a Fresh, full read
    // (the tool's own write is not a stale external change).
    let (mtime2, size2) = fingerprint(&abs);
    assert_eq!(cache.check(&abs, mtime2, size2), ReadState::Fresh);
    assert!(cache.entry(&abs).unwrap().last_read_was_full);
}

#[tokio::test]
async fn read_file_then_notebook_edit_shares_the_cache_and_passes() {
    // Drive read_file first (which records into the shared cache), then edit:
    // the read-before-edit contract is satisfied through the shared cache.
    let tmp = TempDir::new().unwrap();
    let nb_abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);

    // A full read_file records a Fresh, full entry for the notebook.
    // read_file now takes an absolute `file_path` (qwen contract).
    crate::tools::read_file::ReadFile
        .run_rich(&json!({"file_path": nb_abs.to_string_lossy()}), &ctx)
        .await
        .unwrap();

    // The edit now applies without a prior-read rejection.
    let msg = run(
            json!({"notebook_path": nb_abs.to_string_lossy(), "cell_id": "run", "new_source": "print('bye')\n"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(msg.contains("replace cell run"));
}

#[tokio::test]
async fn an_edit_without_a_read_fails_even_though_read_file_shares_the_cache() {
    // The inverse of the sharing test: without driving read_file, the shared
    // cache is empty, so the edit is rejected.
    let tmp = TempDir::new().unwrap();
    let abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    let ctx = ctx_with_cache(tmp.path(), cache);
    let err = run(
        json!({"notebook_path": abs.to_string_lossy(), "cell_id": "run", "new_source": "x\n"}),
        &ctx,
    )
    .await
    .unwrap_err();
    assert!(err.contains("has not been fully read in this session"));
}

#[tokio::test]
async fn delete_summary_omits_the_updated_source() {
    let tmp = TempDir::new().unwrap();
    let abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    let (mtime, size) = fingerprint(&abs);
    cache.record_read(abs.clone(), mtime, size, true, true);
    let ctx = ctx_with_cache(tmp.path(), cache);
    let msg = run(
        json!({"notebook_path": abs.to_string_lossy(), "cell_id": "run", "edit_mode": "delete"}),
        &ctx,
    )
    .await
    .unwrap();
    assert!(msg.contains("delete cell run"));
    assert!(!msg.contains("Updated source:"));
}

// A no-stable-id notebook (cells rely on the `cell-N` fallback), nbformat
// 4.4 so an insert does NOT generate an id either - a structural edit here
// renumbers the fallback ids and must force a re-read.
const NO_STABLE_IDS: &str = r#"{
 "cells": [
  {"cell_type": "code", "source": ["a\n"]},
  {"cell_type": "code", "source": ["b\n"]}
 ],
 "metadata": {},
 "nbformat": 4,
 "nbformat_minor": 4
}
"#;

#[tokio::test]
async fn an_insert_that_loses_stable_ids_invalidates_the_cache() {
    // Insert into a no-stable-id notebook: the `cell-N` fallbacks renumber,
    // so the tool INVALIDATES the read-cache entry (qwen
    // requiresReadAfterWrite) rather than recording its own write. A second
    // edit against the freshly renumbered ids is then rejected until a
    // re-read, matching qwen: the model must requote from fresh output.
    let tmp = TempDir::new().unwrap();
    let abs = tmp.path().join("nb.ipynb");
    std::fs::write(&abs, NO_STABLE_IDS).unwrap();
    let cache = Arc::new(FileReadCache::new());
    let (mtime, size) = fingerprint(&abs);
    cache.record_read(abs.clone(), mtime, size, true, true);
    let ctx = ctx_with_cache(tmp.path(), Arc::clone(&cache));

    // First edit: insert after cell-0.
    run(
            json!({"notebook_path": abs.to_string_lossy(), "cell_id": "cell-0", "new_source": "inserted\n", "edit_mode": "insert"}),
            &ctx,
        )
        .await
        .unwrap();

    // The cache entry was invalidated (not recorded as a fresh write): the
    // path now reads Unknown, so a second edit is rejected pending a re-read.
    let (mtime2, size2) = fingerprint(&abs);
    assert_eq!(cache.check(&abs, mtime2, size2), ReadState::Unknown);
    let err = run(
            json!({"notebook_path": abs.to_string_lossy(), "cell_id": "cell-0", "new_source": "again\n"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.contains("has not been fully read in this session"));
}

#[tokio::test]
async fn an_insert_that_keeps_stable_ids_records_the_write() {
    // The FIXTURE carries stable ids and is nbformat 4.5, so an inserted
    // cell gets a generated id: the notebook keeps stable ids through the
    // edit, so the write is RECORDED (no forced re-read) and a follow-up
    // edit passes.
    let tmp = TempDir::new().unwrap();
    let abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    let (mtime, size) = fingerprint(&abs);
    cache.record_read(abs.clone(), mtime, size, true, true);
    let ctx = ctx_with_cache(tmp.path(), Arc::clone(&cache));

    run(
            json!({"notebook_path": abs.to_string_lossy(), "cell_id": "run", "new_source": "x\n", "edit_mode": "insert"}),
            &ctx,
        )
        .await
        .unwrap();

    // The write was recorded (Fresh), so a follow-up edit applies.
    let (mtime2, size2) = fingerprint(&abs);
    assert_eq!(cache.check(&abs, mtime2, size2), ReadState::Fresh);
    run(
        json!({"notebook_path": abs.to_string_lossy(), "cell_id": "run", "new_source": "y\n"}),
        &ctx,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn an_absolute_path_escaping_the_root_is_refused() {
    // An absolute path outside the Project Root is the escapes case.
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let err = run(
        json!({"notebook_path": "/etc/escape.ipynb", "cell_id": "c", "new_source": "x"}),
        &ctx_with_cache(tmp.path(), cache),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "path escapes project root");
}

#[tokio::test]
async fn a_relative_notebook_path_is_refused_with_the_verbatim_absolute_message() {
    // qwen REQUIRES an absolute notebook_path (notebook-edit.ts:770-772): a
    // relative path is refused with the verbatim message, echoing the
    // trimmed/unescaped value.
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(FileReadCache::new());
    let err = run(
        json!({"notebook_path": "nb.ipynb", "cell_id": "run", "new_source": "x"}),
        &ctx_with_cache(tmp.path(), cache),
    )
    .await
    .unwrap_err();
    assert_eq!(err, "Notebook path must be absolute: nb.ipynb");
}

#[tokio::test]
async fn a_padded_notebook_path_is_trimmed_before_resolution() {
    // qwen's `unescapePath(notebook_path.trim())` (notebook-edit.ts:764) runs
    // first: a surrounding-whitespace path resolves to the same file, so a
    // full prior read lets the padded edit apply.
    let tmp = TempDir::new().unwrap();
    let abs = write_fixture(&tmp);
    let cache = Arc::new(FileReadCache::new());
    let (mtime, size) = fingerprint(&abs);
    cache.record_read(abs.clone(), mtime, size, true, true);
    let ctx = ctx_with_cache(tmp.path(), cache);

    let padded = format!("  {}  ", abs.to_string_lossy());
    let msg = run(
        json!({"notebook_path": padded, "cell_id": "run", "new_source": "print('bye')\n"}),
        &ctx,
    )
    .await
    .unwrap();
    assert!(msg.contains("replace cell run"));
}
