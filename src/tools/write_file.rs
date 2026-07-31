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

use crate::tool::path::{FileError, PathReject, file_error, resolve_absolute_in, unescape_and_trim};
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
    resolve_absolute_in(path, &ctx.root, ctx.memory_root.as_deref()).map_err(|reject| match reject {
        // VERBATIM qwen write-file.ts `validateToolParamValues`.
        PathReject::Relative => format!("File path must be absolute: {path}"),
        // qwen has no write-file.ts message for a path outside the workspace (it
        // asks for confirmation via getDefaultPermission instead). Suspenders
        // confines every tool path to the Project Root, so an escape is a hard
        // refusal with the shared confinement wording.
        PathReject::Escapes => "path escapes project root".to_string(),
    })
}

/// Write `content` to the (already confined, absolute) `abs`, creating parent
/// directories, then stamp the read cache. `abs` is used in the model-facing
/// result string, matching qwen's `${file_path}` (its already-resolved absolute
/// path).
fn write(abs: &std::path::Path, content: &str, ctx: &ToolCtx) -> Result<String, String> {
    if abs.is_dir() {
        // qwen's `validateToolParamValues` rejects a directory target; keep the
        // POSIX-narrative wording the other file tools use.
        return Err(file_error("write", &abs.display().to_string(), FileError::Eisdir));
    }

    // qwen creates parent directories only on the new-file path; `create_dir_all`
    // on an existing file's (existing) parent is a harmless no-op, so a single
    // unconditional call is equivalent and simpler.
    if let Some(parent) = abs.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return Err(file_error("write", &abs.display().to_string(), FileError::from_io(&err)));
    }

    let existed = abs.exists();
    if let Err(err) = std::fs::write(abs, content) {
        return Err(file_error("write", &abs.display().to_string(), FileError::from_io(&err)));
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
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx::for_test(root.to_path_buf(), 10_000)
    }

    // An absolute path to `rel` inside `root`, as a JSON string, since the tool
    // now requires an absolute file_path (qwen contract).
    fn abs(root: &std::path::Path, rel: &str) -> String {
        root.join(rel).to_string_lossy().into_owned()
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        WriteFile.run(&input, ctx).await
    }

    #[test]
    fn spec_requires_file_path_and_content() {
        let spec = WriteFile.spec();
        assert_eq!(spec.name, "write_file");
        assert_eq!(
            spec.input_schema["required"],
            json!(["file_path", "content"])
        );
        // The old relative `path` param is gone.
        assert!(spec.input_schema["properties"]["path"].is_null());
        assert!(spec.input_schema["properties"]["file_path"].is_object());
    }

    #[test]
    fn description_is_the_verbatim_qwen_string() {
        let spec = WriteFile.spec();
        let desc = spec.description;
        assert_eq!(
            desc,
            "Writes content to a specified file in the local filesystem.\n\n      The user has the ability to modify `content`. If modified, this will be stated in the response."
        );
        // No suspenders-only additions.
        assert!(!desc.contains("Usage:"));
        assert!(!desc.contains("relative to the project root"));
        assert!(!desc.contains("only creates new files"));
    }

    #[tokio::test]
    async fn creates_a_new_file_with_the_verbatim_created_message() {
        let tmp = TempDir::new().unwrap();
        let target = abs(tmp.path(), "new.txt");
        let msg = run(
            json!({"file_path": &target, "content": "hello"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert_eq!(
            msg,
            format!("Successfully created and wrote to new file: {target}.")
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn a_padded_file_path_is_trimmed_before_validation() {
        // qwen unescapePath(file_path.trim()) (write-file.ts): surrounding
        // whitespace is stripped, so a padded absolute path writes fine (and is
        // echoed trimmed) rather than being rejected as relative.
        let tmp = TempDir::new().unwrap();
        let target = abs(tmp.path(), "only.txt");
        let padded = format!("  {target}  ");
        let msg = run(
            json!({"file_path": padded, "content": "body"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert_eq!(
            msg,
            format!("Successfully created and wrote to new file: {target}.")
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("only.txt")).unwrap(),
            "body"
        );
    }

    #[tokio::test]
    async fn creates_missing_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let target = abs(tmp.path(), "deeply/nested/dir/file.txt");
        let msg = run(json!({"file_path": &target, "content": "x"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(msg.starts_with("Successfully created and wrote to new file:"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("deeply/nested/dir/file.txt")).unwrap(),
            "x"
        );
    }

    #[tokio::test]
    async fn overwrites_an_existing_file_with_the_verbatim_overwrite_message() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "old").unwrap();
        let target = abs(tmp.path(), "a.txt");

        let msg = run(json!({"file_path": &target, "content": "new"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(msg, format!("Successfully overwrote file: {target}."));
        // The overwrite actually replaced the bytes.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn empty_content_is_allowed() {
        let tmp = TempDir::new().unwrap();
        let target = abs(tmp.path(), "empty.txt");
        assert!(
            run(json!({"file_path": &target, "content": ""}), &ctx(tmp.path()))
                .await
                .is_ok()
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("empty.txt")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn writing_over_a_directory_is_an_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("somedir")).unwrap();
        let target = abs(tmp.path(), "somedir");

        let err = run(json!({"file_path": &target, "content": "x"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("directory"));
    }

    #[tokio::test]
    async fn a_relative_path_is_the_verbatim_absolute_required_message() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"file_path": "new.txt", "content": "x"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert_eq!(err, "File path must be absolute: new.txt");
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        // An absolute path outside the root escapes.
        assert_eq!(
            run(
                json!({"file_path": "/tmp/absolute_escape.txt", "content": "x"}),
                &ctx(tmp.path())
            )
            .await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn writes_into_the_trusted_memory_root_outside_the_project_root() {
        // P5, ADR-0062: a memory-dir write is reachable through the shared path
        // seam, even though the memory root sits outside the Project Root.
        let proj = TempDir::new().unwrap();
        let mem = TempDir::new().unwrap();
        let mut ctx = ctx(proj.path());
        ctx.memory_root = Some(mem.path().to_path_buf());

        let target = mem.path().join("MEMORY.md");
        let msg = run(
            json!({"file_path": target.to_str().unwrap(), "content": "- [X](x.md) - hook"}),
            &ctx,
        )
        .await
        .unwrap();
        assert!(msg.starts_with("Successfully created and wrote to new file:"));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "- [X](x.md) - hook"
        );
    }

    #[tokio::test]
    async fn a_write_outside_both_the_project_and_memory_roots_is_refused() {
        let proj = TempDir::new().unwrap();
        let mem = TempDir::new().unwrap();
        let mut ctx = ctx(proj.path());
        ctx.memory_root = Some(mem.path().to_path_buf());

        // A sibling of the memory root that merely shares its string prefix.
        let evil = format!("{}-evil/x.md", mem.path().to_str().unwrap());
        assert_eq!(
            run(json!({"file_path": evil, "content": "x"}), &ctx).await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn a_successful_write_records_into_the_read_cache() {
        // qwen `recordWrite`: after a write, the path reads Fresh (the tool's own
        // write is not a stale external change) and is a full read the model has
        // "seen", so read_file's unchanged fast-path is eligible.
        use crate::tool::read_cache::ReadState;
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        let target = abs(tmp.path(), "cached.txt");
        run(json!({"file_path": &target, "content": "body"}), &c)
            .await
            .unwrap();

        let path = std::path::Path::new(&target);
        let meta = std::fs::metadata(path).unwrap();
        let mtime_ms = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert_eq!(
            c.read_cache().check(path, mtime_ms, meta.len()),
            ReadState::Fresh
        );
        assert!(c.read_cache().entry(path).unwrap().last_read_was_full);
    }

    #[tokio::test]
    async fn missing_or_non_string_arguments_are_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        let target = abs(tmp.path(), "a.txt");
        assert!(
            crate::tools::execute("write_file", &json!({"file_path": target}), &c)
                .await
                .is_error
        );
        assert!(
            crate::tools::execute(
                "write_file",
                &json!({"file_path": abs(tmp.path(), "a.txt"), "content": 42}),
                &c
            )
            .await
            .is_error
        );
    }
}
