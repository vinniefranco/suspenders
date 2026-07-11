//! `read_file(path, start_line = 1)`: returns file contents from `start_line`
//! on. Size is not this tool's concern: `Tools::run` shapes every Tool Result
//! to the Result Cap, and read_file's shaping cut names the `start_line` that
//! continues a truncated read — `start_line` is windowing (WHICH part), the
//! cap stays the size authority (HOW MUCH).

use crate::tool::{file_error, with_path, FileError, Tool, ToolCtx, ToolSpec};
use serde_json::{json, Value};

pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read the contents of a text file. Always read a file before you edit it. \
                Long output is truncated with a note naming the start_line that continues \
                the read - pass it to page through a large file. \
                If you are unsure the file exists, use list_files or grep first."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the project root, e.g. \"src/main.rs\"."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "1-based line to start reading from (default 1). A truncated \
                            read's note names the value that continues it."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: read_file requires a string \"path\"".to_string())?;
        let start = start_line(input)?;

        with_path(path, ctx, |abs| read_from(abs, path, start))
    }
}

fn read_from(abs: &std::path::Path, path: &str, start: i64) -> Result<String, String> {
    match std::fs::read_to_string(abs) {
        Ok(content) => slice_from(&content, start, path),
        Err(err) => Err(file_error("read", path, FileError::from_io(&err))),
    }
}

// The model may supply start_line; default 1, and reject non-positive/non-int.
fn start_line(input: &Value) -> Result<i64, String> {
    match input.get("start_line") {
        None | Some(Value::Null) => Ok(1),
        Some(Value::Number(n)) if n.is_i64() && n.as_i64().unwrap() >= 1 => Ok(n.as_i64().unwrap()),
        Some(other) => Err(format!(
            "invalid input: start_line must be a positive integer, got {}",
            inspect(other)
        )),
    }
}

fn slice_from(content: &str, start: i64, path: &str) -> Result<String, String> {
    if start == 1 {
        return Ok(content.to_string());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    // A trailing newline splits into a final empty string that is not a line.
    let count = if content.ends_with('\n') {
        lines.len() - 1
    } else {
        lines.len()
    } as i64;

    if start > count {
        Err(format!(
            "start_line {start} is past the end of {path} ({count} lines)"
        ))
    } else {
        Ok(lines[(start - 1) as usize..].join("\n"))
    }
}

// Elixir `inspect/1` for the values start_line can carry: a quoted string, or
// a JSON-ish rendering for anything else.
fn inspect(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
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
        ReadFile.run(&input, ctx).await
    }

    #[test]
    fn spec_requires_path() {
        let spec = ReadFile.spec();
        assert_eq!(spec.name, "read_file");
        assert_eq!(spec.input_schema["required"], json!(["path"]));
    }

    #[tokio::test]
    async fn reads_a_file_relative_to_the_project_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "hi there\n").unwrap();

        assert_eq!(
            run(json!({"path": "hello.txt"}), &ctx(tmp.path())).await,
            Ok("hi there\n".into())
        );
    }

    #[tokio::test]
    async fn reads_a_nested_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub/dir")).unwrap();
        std::fs::write(tmp.path().join("sub/dir/a.txt"), "nested").unwrap();

        assert_eq!(
            run(json!({"path": "sub/dir/a.txt"}), &ctx(tmp.path())).await,
            Ok("nested".into())
        );
    }

    #[tokio::test]
    async fn returns_large_files_whole() {
        let tmp = TempDir::new().unwrap();
        let content = "a".repeat(50_123);
        std::fs::write(tmp.path().join("big.txt"), &content).unwrap();

        assert_eq!(
            run(json!({"path": "big.txt"}), &ctx(tmp.path())).await,
            Ok(content)
        );
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"path": "nope.txt"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("nope.txt"));
        assert!(err.contains("enoent"));
    }

    #[tokio::test]
    async fn reading_a_directory_is_an_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("somedir")).unwrap();
        let err = run(json!({"path": "somedir"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("somedir"));
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(json!({"path": "../../etc/passwd"}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
        assert_eq!(
            run(json!({"path": "/etc/passwd"}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn missing_or_non_string_path_is_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(
            crate::tools::execute("read_file", &json!({}), &c)
                .await
                .is_error
        );
        assert!(
            crate::tools::execute("read_file", &json!({"path": 42}), &c)
                .await
                .is_error
        );
    }

    // ---- start_line windowing ----

    #[tokio::test]
    async fn returns_the_file_from_start_line_on() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo\nthree\nfour\n").unwrap();

        assert_eq!(
            run(json!({"path": "lines.txt", "start_line": 3}), &ctx(tmp.path())).await,
            Ok("three\nfour\n".into())
        );
    }

    #[tokio::test]
    async fn start_line_1_returns_the_whole_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo\n").unwrap();

        assert_eq!(
            run(json!({"path": "lines.txt", "start_line": 1}), &ctx(tmp.path())).await,
            Ok("one\ntwo\n".into())
        );
    }

    #[tokio::test]
    async fn the_last_line_is_reachable_with_or_without_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("nl.txt"), "one\ntwo\n").unwrap();
        std::fs::write(tmp.path().join("no_nl.txt"), "one\ntwo").unwrap();

        assert_eq!(
            run(json!({"path": "nl.txt", "start_line": 2}), &ctx(tmp.path())).await,
            Ok("two\n".into())
        );
        assert_eq!(
            run(json!({"path": "no_nl.txt", "start_line": 2}), &ctx(tmp.path())).await,
            Ok("two".into())
        );
    }

    #[tokio::test]
    async fn a_start_line_past_the_end_is_an_error_naming_the_line_count() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lines.txt"), "one\ntwo\n").unwrap();

        let err = run(json!({"path": "lines.txt", "start_line": 3}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("past the end"));
        assert!(err.contains("2 lines"));
    }

    #[tokio::test]
    async fn a_non_integer_or_non_positive_start_line_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        let err = run(json!({"path": "x.txt", "start_line": 0}), &c)
            .await
            .unwrap_err();
        assert!(err.contains("start_line"));
        assert!(run(json!({"path": "x.txt", "start_line": "3"}), &c)
            .await
            .is_err());
    }
}
