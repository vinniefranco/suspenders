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

/// VERBATIM from qwen v0.21.4 `tools/write-file.ts` (the description passed to the
/// `WriteFileTool` constructor), including its exact indentation on the second
/// paragraph.
const DESCRIPTION: &str = "Writes content to a specified file in the local filesystem. A request to create or generate a file does not establish that the target path is new. Unless the target's absence or current text contents have already been established in this session, you MUST use the read_file tool first; if the file does not exist, then create it. With prior-read enforcement enabled, blind overwrites are rejected. The file_path argument MUST be an absolute path. Always construct it by combining the project root with the file's relative path (e.g. project root '/path/to/project/' + relative 'foo/bar.txt' = '/path/to/project/foo/bar.txt'). If the user provides a relative path, resolve it against the project root first.

The user has the ability to modify `content`. If modified, this will be stated in the response.";

#[async_trait::async_trait]
impl Tool for WriteFile {
    // Mutator (qwen write-file.ts:781 `Kind.Edit`): BLOCKED in plan mode.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Edit
    }

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
        // The text projection: the model-facing message, dropping the display
        // Artifact. `run_rich` (the Registry's dispatch path) keeps the diff.
        write_file(input, ctx).map(|outcome| outcome.message)
    }

    async fn run_rich(
        &self,
        input: &Value,
        ctx: &ToolCtx,
    ) -> Result<crate::tool::ToolOutput, String> {
        // write_file knows the before-content (an overwrite) and the written
        // content, so it computes its own diff (ADR-0007's diff behavior,
        // relocated here) and attaches the `diff` display Artifact, which the
        // Transcript store swaps for a first-class Diff item.
        let outcome = write_file(input, ctx)?;
        let output = crate::tool::ToolOutput::text(outcome.message);
        Ok(match outcome.diff {
            Some(diff) => output.with_artifact(crate::tools::file_diff::DIFF, diff),
            None => output,
        })
    }
}

/// One write's outcome: the model-facing message and the optional `diff` display
/// Artifact (a serialized [`crate::tools::file_diff::DiffArtifact`]). `run`
/// projects the message; `run_rich` also attaches the diff.
struct WriteOutcome {
    message: String,
    diff: Option<Value>,
}

/// Decodes the input, resolves the path, and writes the file, returning the
/// message and the diff Artifact. Shared by `run` and `run_rich`.
fn write_file(input: &Value, ctx: &ToolCtx) -> Result<WriteOutcome, String> {
    let raw_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "invalid input: write_file requires a string \"file_path\"".to_string())?;
    // qwen's `unescapePath(params.file_path.trim())` (write-file.ts): trim
    // surrounding whitespace and strip shell-escaping backslashes BEFORE the
    // absolute-path check and before the path is echoed back.
    let path = unescape_and_trim(raw_path);
    let content = input
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "invalid input: write_file requires a string \"content\"".to_string())?;

    let abs = resolve(&path, ctx)?;
    write(&abs, raw_path, content, ctx)
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
fn write(
    abs: &std::path::Path,
    diff_path: &str,
    content: &str,
    ctx: &ToolCtx,
) -> Result<WriteOutcome, String> {
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

    // Snapshot the existing content BEFORE the overwrite so the diff renders
    // old->new; a fresh create reads nothing and renders an all-added diff.
    let existed = abs.exists();
    let before = if existed {
        std::fs::read_to_string(abs).ok()
    } else {
        None
    };
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

    // The diff Artifact: an overwrite diffs the snapshot->new content, a fresh
    // create is one all-added created-file diff. `diff_path` is the RAW model
    // `file_path` input, echoed verbatim in the diff title.
    let diff = crate::tools::file_diff::artifact(before.as_deref(), content, diff_path);
    Ok(WriteOutcome {
        message: result_message(abs, existed),
        diff,
    })
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
#[path = "../../tests/tools/write_file.rs"]
mod tests;
