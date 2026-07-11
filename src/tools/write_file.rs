//! `write_file(path, content)`: creates a new file, creating parent
//! directories as needed. Refuses to overwrite an existing file - changing an
//! existing file is edit_file's job. Closing the whole-file-rewrite channel
//! stops a small model from destroying a file by rewriting it (each
//! reproduction shorter than the last) instead of making a targeted edit.

use crate::tool::{file_error, with_path, FileError, Tool, ToolCtx, ToolSpec};
use serde_json::{json, Value};

pub struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create a new file with the given content. Fails if the file already exists; \
                change an existing file with edit_file. Parent directories are created \
                automatically."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the project root."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full contents to write to the file."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: write_file requires a string \"path\"".to_string())?;
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: write_file requires a string \"content\"".to_string())?;

        with_path(path, ctx, |abs| {
            if abs.is_dir() {
                Err(format!("{path} is a directory"))
            } else if abs.exists() {
                Err(format!(
                    "{path} already exists; write_file only creates new files. \
                     Use edit_file to change it."
                ))
            } else {
                write(abs, path, content)
            }
        })
    }
}

fn write(abs: &std::path::Path, path: &str, content: &str) -> Result<String, String> {
    if let Some(parent) = abs.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return Err(file_error("write", path, FileError::from_io(&err)));
    }
    match std::fs::write(abs, content) {
        Ok(()) => Ok(format!("created {path} ({} bytes)", content.len())),
        Err(err) => Err(file_error("write", path, FileError::from_io(&err))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx {
            root: root.to_path_buf(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
            scout: None,
        }
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        WriteFile.run(&input, ctx).await
    }

    #[test]
    fn spec_requires_path_and_content() {
        let spec = WriteFile.spec();
        assert_eq!(spec.name, "write_file");
        assert_eq!(spec.input_schema["required"], json!(["path", "content"]));
    }

    #[tokio::test]
    async fn creates_a_new_file_and_reports_it_as_created() {
        let tmp = TempDir::new().unwrap();
        let msg = run(json!({"path": "new.txt", "content": "hello"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(msg.contains("created new.txt"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn creates_missing_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let msg = run(
            json!({"path": "deeply/nested/dir/file.txt", "content": "x"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap();
        assert!(msg.contains("created"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("deeply/nested/dir/file.txt")).unwrap(),
            "x"
        );
    }

    #[tokio::test]
    async fn overwriting_an_existing_file_is_refused() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "old").unwrap();

        let err = run(json!({"path": "a.txt", "content": "new"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("a.txt"));
        assert!(err.contains("edit_file"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "old"
        );
    }

    #[tokio::test]
    async fn empty_content_is_allowed() {
        let tmp = TempDir::new().unwrap();
        assert!(run(json!({"path": "empty.txt", "content": ""}), &ctx(tmp.path()))
            .await
            .is_ok());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("empty.txt")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn writing_over_a_directory_is_an_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("somedir")).unwrap();

        let err = run(json!({"path": "somedir", "content": "x"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("directory"));
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(json!({"path": "../escape.txt", "content": "x"}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
        assert_eq!(
            run(
                json!({"path": "/tmp/absolute_escape.txt", "content": "x"}),
                &ctx(tmp.path())
            )
            .await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn missing_or_non_string_arguments_are_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(
            crate::tools::execute("write_file", &json!({"path": "a.txt"}), &c)
                .await
                .is_error
        );
        assert!(
            crate::tools::execute("write_file", &json!({"path": "a.txt", "content": 42}), &c)
                .await
                .is_error
        );
    }
}
