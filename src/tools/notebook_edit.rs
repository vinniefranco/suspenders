//! `notebook_edit(notebook_path, cell_id?, new_source?, cell_type?, edit_mode?)`:
//! edits a Jupyter notebook (`.ipynb`) safely at the cell level - the VERBATIM port of
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

use crate::tool::path::{
    FileError, PathReject, file_error, resolve_absolute_in, unescape_and_trim,
};
use crate::tool::read_cache::ReadState;
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

mod apply;

pub use apply::{CellType, EditMode, NotebookEditParams};

pub struct NotebookEdit;

const DESCRIPTION: &str = "\
Edits a Jupyter notebook (.ipynb) safely at the cell level. Use this instead of edit or \
write_file for notebook cells. Supports replacing, inserting, and deleting cells. Always read the \
notebook first with read_file; then use the cell IDs shown in that output.";

#[async_trait::async_trait]
impl Tool for NotebookEdit {
    // Mutator (qwen notebook-edit.ts:741 `Kind.Edit`): BLOCKED in plan mode.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Edit
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "notebook_edit".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "notebook_path": {
                        "type": "string",
                        "description": "Absolute path to the Jupyter notebook file to edit. \
                            Must end with .ipynb."
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
                "required": ["notebook_path"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // qwen's `unescapePath(params.notebook_path.trim())` (notebook-edit.ts:764),
        // applied BEFORE the absolute check and BEFORE any message echoes the path:
        // trim, then strip shell-escaping backslashes. The trimmed/unescaped form is
        // what is validated, resolved, and displayed.
        let path = unescape_and_trim(read_path(input)?);
        if !path.to_ascii_lowercase().ends_with(".ipynb") {
            return Err(
                "File must be a Jupyter notebook (.ipynb). Use the edit tool for other file types."
                    .to_string(),
            );
        }
        let params = decode_params(input)?;

        // qwen REQUIRES an absolute path and confines it to the workspace
        // (or the trusted managed-memory subtree). A relative path is refused
        // with qwen's verbatim message (notebook-edit.ts:771); an escaping
        // absolute path with the project-wide confinement message.
        let abs = resolve_absolute_in(&path, &ctx.root, ctx.memory_root.as_deref()).map_err(
            |reject| match reject {
                PathReject::Relative => format!("Notebook path must be absolute: {path}"),
                PathReject::Escapes => "path escapes project root".to_string(),
            },
        )?;
        let raw = read_notebook_text(&abs, &path)?;

        // Read-before-edit enforcement against the Run's file-read cache: the
        // notebook must have been FULLY read this session, and not changed on
        // disk since (F6, ADR-0060). VERBATIM qwen rejections.
        enforce_prior_read(ctx, &abs, &path)?;

        // Apply the pure cell edit, then write the result atomically. A normal
        // edit records the write back into the cache so a follow-up edit does
        // not see this tool's OWN write as a stale external change. A structural
        // edit that LOST stable cell ids instead INVALIDATES the cache entry
        // (qwen `requiresReadAfterWrite`): the `cell-N` fallback ids renumbered,
        // so the model must re-read before it can target a cell again, or a
        // second edit would land on the wrong cell.
        let result = apply::apply_notebook_edit(&raw, &params)?;
        write_notebook(&abs, &path, &result.updated_content)?;
        if result.requires_read_after_write {
            ctx.read_cache().invalidate(&abs);
        } else {
            record_write(ctx, &abs);
        }

        Ok(edit_summary(&path, &result, &params))
    }
}

// ---- param decoding ---------------------------------------------------------

fn read_path(input: &Value) -> Result<&str, String> {
    input
        .get("notebook_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            "invalid input: notebook_edit requires a string \"notebook_path\"".to_string()
        })
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
        // `cacheable = false`: the notebook writer produced a structured payload
        // the model must re-materialize before it can target a cell, so a repeat
        // read must NOT be short-circuited to the unchanged placeholder (qwen
        // `notebook-edit.ts recordWrite({ cacheable: false })`).
        ctx.read_cache()
            .record_write(abs.to_path_buf(), mtime_ms, size, false);
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
#[path = "../../tests/tools/notebook_edit.rs"]
mod tests;
