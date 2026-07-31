//! `notebook_edit(path, cell_id?, new_source?, cell_type?, edit_mode?)`: edits a
//! Jupyter notebook (`.ipynb`) safely at the cell level - the VERBATIM port of
//! qwen v0.16.0 `packages/core/src/tools/notebook-edit.ts` (`NotebookEditTool` /
//! `NotebookEditInvocation`), narrowed to Suspenders' shape.
//!
//! The pure cell edit (parse -> resolve target by display id -> replace / insert
//! / delete -> serialize preserving JSON format) lives in [`apply`]. This file is
//! the [`Tool`] wrapper: it enforces the read-before-edit contract against the
//! Run's file-read cache (F6, ADR-0060), applies the edit, and writes the result
//! atomically. A normal edit records the write back into the cache; a structural
//! edit that lost stable cell ids INVALIDATES the cache entry instead (qwen
//! `requiresReadAfterWrite`), forcing a re-read before the next cell-level edit.
//!
//! ## Read-before-edit enforcement (the F6 consumer)
//!
//! A cell-level edit needs the model to have actually SEEN the notebook this
//! session, and to have seen ALL of it (a cell id the model quotes must be one
//! from a real read, not a hallucination). So before mutating, the tool stats
//! the notebook and consults [`crate::tool::read_cache::FileReadCache::check`]:
//!
//! - not in the cache (never read this Run), or read but NOT fully -> the
//!   VERBATIM "has not been fully read in this session" rejection;
//! - read fully but the fingerprint drifted (changed on disk since) -> the
//!   VERBATIM "has been modified since you last read it" rejection;
//! - read but the rendered output was TRUNCATED (`last_read_was_full == false`
//!   on a Fresh entry) -> the VERBATIM "too large for cell-level editing"
//!   rejection.
//!
//! Only a Fresh entry whose last read was FULL passes. This is the sole P3 3c
//! consumer of the read cache; `edit_file`/`write_file` do not yet consult it
//! (DEFERRED, ADR-0060).

use crate::tool::path::{FileError, file_error};
use crate::tool::read_cache::ReadState;
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

mod apply;

pub use apply::{CellType, EditMode, NotebookEditParams};

pub struct NotebookEdit;

const DESCRIPTION: &str = "\
Edits a Jupyter notebook (.ipynb) safely at the cell level. Use this instead of edit_file or \
write_file for notebook cells. Supports replacing, inserting, and deleting cells. Always read the \
notebook first with read_file; then use the cell IDs shown in that output.";

#[async_trait::async_trait]
impl Tool for NotebookEdit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "notebook_edit".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The path of the Jupyter notebook (.ipynb) to edit, \
                            relative to the project root (e.g. \"notebooks/run.ipynb\"). Do not \
                            pass an absolute path. Must end with .ipynb."
                    },
                    "cell_id": {
                        "type": "string",
                        "description": "Target cell ID from read_file output, or cell-N 0-based \
                            fallback. Required for replace and delete. For insert, the new cell is \
                            inserted after this cell; if omitted, inserted at the beginning."
                    },
                    "new_source": {
                        "type": "string",
                        "description": "New source content for replace and insert operations. Not \
                            required for delete."
                    },
                    "cell_type": {
                        "type": "string",
                        "description": "Cell type for inserted cells or type conversion on \
                            replace.",
                        "enum": ["code", "markdown"]
                    },
                    "edit_mode": {
                        "type": "string",
                        "description": "Notebook edit operation. Defaults to replace.",
                        "enum": ["replace", "insert", "delete"]
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let path = read_path(input)?;
        if !path.to_ascii_lowercase().ends_with(".ipynb") {
            return Err(
                "File must be a Jupyter notebook (.ipynb). Use the edit tool for other file types."
                    .to_string(),
            );
        }
        let params = decode_params(input)?;

        // Resolve + confine the path to the Project Root, then read the raw text.
        let abs = crate::tool::path::resolve_path(path, &ctx.root)?;
        let raw = read_notebook_text(&abs, path)?;

        // Read-before-edit enforcement against the Run's file-read cache: the
        // notebook must have been FULLY read this session, and not changed on
        // disk since (F6, ADR-0060). VERBATIM qwen rejections.
        enforce_prior_read(ctx, &abs, path)?;

        // Apply the pure cell edit, then write the result atomically. A normal
        // edit records the write back into the cache so a follow-up edit does
        // not see this tool's OWN write as a stale external change. A structural
        // edit that LOST stable cell ids instead INVALIDATES the cache entry
        // (qwen `requiresReadAfterWrite`): the `cell-N` fallback ids renumbered,
        // so the model must re-read before it can target a cell again, or a
        // second edit would land on the wrong cell.
        let result = apply::apply_notebook_edit(&raw, &params)?;
        write_notebook(&abs, path, &result.updated_content)?;
        if result.requires_read_after_write {
            ctx.read_cache().invalidate(&abs);
        } else {
            record_write(ctx, &abs);
        }

        Ok(edit_summary(path, &result, &params))
    }
}

// ---- param decoding ---------------------------------------------------------

fn read_path(input: &Value) -> Result<&str, String> {
    input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "invalid input: notebook_edit requires a string \"path\"".to_string())
}

/// Decode the edit params from the wire (the JSON Schema `enum`s are validated
/// upstream by `tool::validate`; here a bad enum value is a decode error). A
/// missing `edit_mode` defaults to replace.
fn decode_params(input: &Value) -> Result<NotebookEditParams, String> {
    let edit_mode = match input.get("edit_mode") {
        None | Some(Value::Null) => EditMode::Replace,
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|_| "edit_mode must be 'replace', 'insert', or 'delete'.".to_string())?,
    };
    let cell_type = match input.get("cell_type") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            serde_json::from_value::<CellType>(v.clone())
                .map_err(|_| "cell_type must be 'code' or 'markdown'.".to_string())?,
        ),
    };
    Ok(NotebookEditParams {
        cell_id: string_field(input, "cell_id"),
        new_source: string_field(input, "new_source"),
        cell_type,
        edit_mode,
    })
}

fn string_field(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(|v| v.as_str()).map(String::from)
}

// ---- read-before-edit enforcement (F6 consumer, ADR-0060) -------------------

/// Enforce the read-before-edit contract (qwen `checkPriorNotebookRead`): the
/// notebook must be a Fresh cache entry whose last read was FULL. Returns the
/// VERBATIM qwen rejection otherwise.
fn enforce_prior_read(ctx: &ToolCtx, abs: &std::path::Path, path: &str) -> Result<(), String> {
    let (mtime_ms, size) = stat_fingerprint(abs)?;
    let cache = ctx.read_cache();
    let state = cache.check(abs, mtime_ms, size);
    let entry = cache.entry(abs);

    // Fresh AND read this session (not a write-only entry): a full read passes;
    // a truncated read is refused for cell-level editing (qwen's two Fresh arms).
    if state == ReadState::Fresh
        && let Some(entry) = &entry
        && entry.last_read_at.is_some()
    {
        return if entry.last_read_was_full {
            Ok(())
        } else {
            Err(format!(
                "Notebook {path} is too large for cell-level editing because its rendered output \
was truncated when read. Reduce the notebook output size or split the notebook before editing \
cells."
            ))
        };
    }

    // Read this session but the on-disk fingerprint drifted: re-read first.
    if state == ReadState::Stale {
        return Err(format!(
            "Notebook {path} has been modified since you last read it. Re-read it with the \
read_file tool before editing it."
        ));
    }

    // Never read this session (Unknown), or a write-only entry: not fully read.
    Err(format!(
        "Notebook {path} has not been fully read in this session. Use the read_file tool first, \
without offset or limit, before editing cells."
    ))
}

/// The read cache needs a live `stat` for the current fingerprint. Unlike a
/// read (which tolerates a stat miss), a missing stat here means we cannot
/// verify the prior read, so the edit is refused - never silently allowed.
fn stat_fingerprint(abs: &std::path::Path) -> Result<(u128, u64), String> {
    let meta = std::fs::metadata(abs)
        .map_err(|err| file_error("read", &abs.to_string_lossy(), FileError::from_io(&err)))?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok((mtime_ms, meta.len()))
}

// ---- I/O --------------------------------------------------------------------

fn read_notebook_text(abs: &std::path::Path, path: &str) -> Result<String, String> {
    std::fs::read_to_string(abs).map_err(|err| file_error("read", path, FileError::from_io(&err)))
}

/// Write the updated notebook to disk (like write_file's atomic write, minus the
/// exists guard - the notebook must already exist to have been read).
fn write_notebook(abs: &std::path::Path, path: &str, content: &str) -> Result<(), String> {
    std::fs::write(abs, content).map_err(|err| file_error("write", path, FileError::from_io(&err)))
}

/// Record the write back into the read cache (F6): the model authored the
/// current bytes, so a follow-up notebook_edit sees a Fresh, full read rather
/// than its own write as a stale external change. A stat miss here is dropped -
/// the write already succeeded, and the next read re-stats.
fn record_write(ctx: &ToolCtx, abs: &std::path::Path) {
    if let Ok((mtime_ms, size)) = stat_fingerprint(abs) {
        ctx.read_cache()
            .record_write(abs.to_path_buf(), mtime_ms, size);
    }
}

// ---- result wording ---------------------------------------------------------

/// The model-facing summary (qwen's `llmContent`): the mode + edited cell id, and
/// for a non-delete edit the updated source echoed back.
fn edit_summary(
    path: &str,
    result: &apply::NotebookEditResult,
    params: &NotebookEditParams,
) -> String {
    let mode = result.mode.as_str();
    let head = format!(
        "Notebook {path} has been updated. {mode} cell {}.",
        result.edited_cell_id
    );
    if result.mode == EditMode::Delete {
        head
    } else {
        let source = params.new_source.as_deref().unwrap_or("");
        format!("{head}\n\nUpdated source:\n\n---\n\n{source}")
    }
}

#[cfg(test)]
mod tests {
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
    fn spec_requires_only_the_path() {
        let spec = NotebookEdit.spec();
        assert_eq!(spec.name, "notebook_edit");
        assert_eq!(spec.input_schema["required"], json!(["path"]));
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
        let err = run(
            json!({"path": "notes.txt", "cell_id": "c", "new_source": "x"}),
            &ctx_with_cache(tmp.path(), cache),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Jupyter notebook (.ipynb)"));
    }

    #[tokio::test]
    async fn editing_an_unread_notebook_is_the_verbatim_rejection() {
        let tmp = TempDir::new().unwrap();
        write_fixture(&tmp);
        let cache = Arc::new(FileReadCache::new());
        let err = run(
            json!({"path": "nb.ipynb", "cell_id": "run", "new_source": "print('bye')\n"}),
            &ctx_with_cache(tmp.path(), cache),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            "Notebook nb.ipynb has not been fully read in this session. Use the read_file tool \
first, without offset or limit, before editing cells."
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
            json!({"path": "nb.ipynb", "cell_id": "run", "new_source": "print('bye')\n"}),
            &ctx_with_cache(tmp.path(), cache),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            "Notebook nb.ipynb has been modified since you last read it. Re-read it with the \
read_file tool before editing it."
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
            json!({"path": "nb.ipynb", "cell_id": "run", "new_source": "print('bye')\n"}),
            &ctx_with_cache(tmp.path(), cache),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            "Notebook nb.ipynb is too large for cell-level editing because its rendered output \
was truncated when read. Reduce the notebook output size or split the notebook before editing \
cells."
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
            json!({"path": "nb.ipynb", "cell_id": "run", "new_source": "print('bye')\n"}),
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
        write_fixture(&tmp);
        let cache = Arc::new(FileReadCache::new());
        let ctx = ctx_with_cache(tmp.path(), cache);

        // A full read_file records a Fresh, full entry for the notebook.
        crate::tools::read_file::ReadFile
            .run_rich(&json!({"path": "nb.ipynb"}), &ctx)
            .await
            .unwrap();

        // The edit now applies without a prior-read rejection.
        let msg = run(
            json!({"path": "nb.ipynb", "cell_id": "run", "new_source": "print('bye')\n"}),
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
        write_fixture(&tmp);
        let cache = Arc::new(FileReadCache::new());
        let ctx = ctx_with_cache(tmp.path(), cache);
        let err = run(
            json!({"path": "nb.ipynb", "cell_id": "run", "new_source": "x\n"}),
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
            json!({"path": "nb.ipynb", "cell_id": "run", "edit_mode": "delete"}),
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
            json!({"path": "nb.ipynb", "cell_id": "cell-0", "new_source": "inserted\n", "edit_mode": "insert"}),
            &ctx,
        )
        .await
        .unwrap();

        // The cache entry was invalidated (not recorded as a fresh write): the
        // path now reads Unknown, so a second edit is rejected pending a re-read.
        let (mtime2, size2) = fingerprint(&abs);
        assert_eq!(cache.check(&abs, mtime2, size2), ReadState::Unknown);
        let err = run(
            json!({"path": "nb.ipynb", "cell_id": "cell-0", "new_source": "again\n"}),
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
            json!({"path": "nb.ipynb", "cell_id": "run", "new_source": "x\n", "edit_mode": "insert"}),
            &ctx,
        )
        .await
        .unwrap();

        // The write was recorded (Fresh), so a follow-up edit applies.
        let (mtime2, size2) = fingerprint(&abs);
        assert_eq!(cache.check(&abs, mtime2, size2), ReadState::Fresh);
        run(
            json!({"path": "nb.ipynb", "cell_id": "run", "new_source": "y\n"}),
            &ctx,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_path_escaping_the_root_is_refused() {
        let tmp = TempDir::new().unwrap();
        let cache = Arc::new(FileReadCache::new());
        let err = run(
            json!({"path": "../escape.ipynb", "cell_id": "c", "new_source": "x"}),
            &ctx_with_cache(tmp.path(), cache),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "path escapes project root");
    }
}
