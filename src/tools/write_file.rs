//! `write_file(file_path, content)`: writes `content` to a file in the local
//! filesystem, creating parent directories as needed. A FAITHFUL port of qwen
//! v0.16.0 `tools/write-file.ts`: an ABSOLUTE `file_path` (a relative one is
//! refused with qwen's verbatim message), and an OVERWRITE contract - an
//! existing file is overwritten, not refused. The result string distinguishes a
//! fresh create ("Successfully created and wrote to new file: ...") from an
//! overwrite ("Successfully overwrote file: ...").
//!
//! qwen's write-file.ts also carries internal infrastructure this port does not
//! reproduce (out of the model-facing contract): BOM / encoding / line-ending
//! detection, git commit attribution, file-history backup, prior-read
//! enforcement (the `checkPriorRead` TOCTOU guards, gated on
//! `getFileReadCacheDisabled`), and hooks. What DOES survive is the read-cache
//! recording: after a successful write the Run's [`FileReadCache`] is stamped
//! (qwen's `recordWrite`), so a follow-up read_file serves fresh content and a
//! subsequent full read gets the unchanged placeholder rather than treating the
//! tool's own write as a stale external change.
//!
//! [`FileReadCache`]: crate::tool::read_cache::FileReadCache

use crate::tool::path::{
    FileError, PathReject, file_error, resolve_absolute_in, unescape_and_trim,
};
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

pub struct WriteFile;

/// VERBATIM from qwen v0.16.0 `tools/write-file.ts` (the description passed to the
/// `WriteFileTool` constructor), including its exact indentation on the second
/// line.
const DESCRIPTION: &str = "Writes content to a specified file in the local filesystem.

      The user has the ability to modify `content`. If modified, this will be stated in the response.";

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: DESCRIPTION.into(),
            // Schema property set + each description string are VERBATIM from
            // qwen write-file.ts's `parametersJsonSchema`.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The absolute path to the file to write to (e.g., '/home/user/project/file.txt'). Relative paths are not supported."
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file."
                    }
                },
                "required": ["file_path", "content"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let raw_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                "invalid input: write_file requires a string \"file_path\"".to_string()
            })?;
        // qwen's `unescapePath(params.file_path.trim())` (write-file.ts): trim
        // surrounding whitespace and strip shell-escaping backslashes BEFORE the
        // absolute-path check and before the path is echoed back.
        let path = unescape_and_trim(raw_path);
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: write_file requires a string \"content\"".to_string())?;

        let abs = resolve(&path, ctx)?;
        write(&abs, content, ctx)
    }
}

/// Resolve a model-supplied ABSOLUTE `file_path` and confine it to the Project
/// Root (or the trusted memory subtree), rendering qwen's verbatim
/// absolute-path message for a relative path (write-file.ts
/// `validateToolParamValues`) and the shared confinement wording for an escape.
fn resolve(path: &str, ctx: &ToolCtx) -> Result<std::path::PathBuf, String> {
    resolve_absolute_in(path, &ctx.root, ctx.memory_root.as_deref()).map_err(
        |reject| match reject {
            // VERBATIM qwen write-file.ts `validateToolParamValues`.
            PathReject::Relative => format!("File path must be absolute: {path}"),
            // qwen has no write-file.ts message for a path outside the workspace (it
            // asks for confirmation via getDefaultPermission instead). Suspenders
            // confines every tool path to the Project Root, so an escape is a hard
            // refusal with the shared confinement wording.
            PathReject::Escapes => "path escapes project root".to_string(),
        },
    )
}

/// Write `content` to the (already confined, absolute) `abs`, creating parent
/// directories, then stamp the read cache. `abs` is used in the model-facing
/// result string, matching qwen's `${file_path}` (its already-resolved absolute
/// path).
fn write(abs: &std::path::Path, content: &str, ctx: &ToolCtx) -> Result<String, String> {
    if abs.is_dir() {
        // qwen's `validateToolParamValues` rejects a directory target; keep the
        // POSIX-narrative wording the other file tools use.
        return Err(file_error(
            "write",
            &abs.display().to_string(),
            FileError::Eisdir,
        ));
    }

    // qwen creates parent directories only on the new-file path; `create_dir_all`
    // on an existing file's (existing) parent is a harmless no-op, so a single
    // unconditional call is equivalent and simpler.
    if let Some(parent) = abs.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return Err(file_error(
            "write",
            &abs.display().to_string(),
            FileError::from_io(&err),
        ));
    }

    let existed = abs.exists();
    if let Err(err) = std::fs::write(abs, content) {
        return Err(file_error(
            "write",
            &abs.display().to_string(),
            FileError::from_io(&err),
        ));
    }

    // Stamp the read cache with the post-write fingerprint (qwen `recordWrite`):
    // the model authored the current bytes, so a follow-up read_file serves fresh
    // content rather than reading the tool's own write as a stale external change.
    record_write(ctx, abs);

    Ok(result_message(abs, existed))
}

/// qwen's `llmSuccessMessageParts` (write-file.ts): a fresh create vs. an
/// overwrite, VERBATIM. `abs` stands in for qwen's already-resolved absolute
/// `file_path`.
fn result_message(abs: &std::path::Path, existed: bool) -> String {
    let path = abs.display();
    if existed {
        format!("Successfully overwrote file: {path}.")
    } else {
        format!("Successfully created and wrote to new file: {path}.")
    }
}

/// Record the write into the Run's read cache (F6, qwen `recordWrite`). Stats the
/// file for the `(mtime, size)` fingerprint; a stat miss is dropped - the write
/// already succeeded, and the next read re-stats.
fn record_write(ctx: &ToolCtx, abs: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(abs) else {
        return;
    };
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // `cacheable = true`: write_file authored plain-text bytes, so a following
    // full read_file can serve qwen's `file_unchanged` placeholder (qwen's
    // `recordWrite` default). Only the notebook writer passes `false`.
    ctx.read_cache()
        .record_write(abs.to_path_buf(), mtime_ms, meta.len(), true);
}

#[cfg(test)]
#[path = "../../tests/unit/tools/write_file.rs"]
mod tests;
