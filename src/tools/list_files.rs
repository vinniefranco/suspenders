//! `list_files(path)`: one entry per line, directories get a trailing `/`.
//! Sorted, directories first. Empty dir → `"[empty directory]"` (an
//! empty-string result confuses small models).

use crate::tool::{FileError, Tool, ToolCtx, ToolSpec, file_error, with_path};
use serde_json::{Value, json};

pub struct ListFiles;

#[async_trait::async_trait]
impl Tool for ListFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_files".into(),
            description:
                "List the entries of a directory, one per line. Directories end with a trailing / \
                and are listed first; everything is sorted. Omit path to list the project root. \
                Use this to explore the project before reading or editing files."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path relative to the project root. Defaults to \".\" (the project root)."
                    }
                },
                "required": []
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // path is optional (default "."); a non-string path is an error.
        let path = match input.get("path") {
            None | Some(Value::Null) => ".",
            Some(Value::String(s)) => s.as_str(),
            Some(_) => return Err("path must be a string".to_string()),
        };

        with_path(path, ctx, |abs| match std::fs::read_dir(abs) {
            Ok(rd) => {
                let entries: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                if entries.is_empty() {
                    Ok("[empty directory]".to_string())
                } else {
                    Ok(format(&entries, abs))
                }
            }
            Err(err) => Err(file_error("list", path, FileError::from_io(&err))),
        })
    }
}

fn format(entries: &[String], abs: &std::path::Path) -> String {
    let (mut dirs, mut files): (Vec<&String>, Vec<&String>) =
        entries.iter().partition(|e| abs.join(e).is_dir());
    dirs.sort();
    files.sort();

    let mut out: Vec<String> = dirs.iter().map(|d| format!("{d}/")).collect();
    out.extend(files.iter().map(|f| (*f).clone()));
    out.join("\n")
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
        ListFiles.run(&input, ctx).await
    }

    #[test]
    fn spec_makes_path_optional() {
        let spec = ListFiles.spec();
        assert_eq!(spec.name, "list_files");
        assert_eq!(spec.input_schema["required"], json!([]));
    }

    #[tokio::test]
    async fn lists_directories_first_with_trailing_slash_each_group_sorted() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("b_dir")).unwrap();
        std::fs::create_dir_all(tmp.path().join("a_dir")).unwrap();
        std::fs::write(tmp.path().join("z.txt"), "").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();

        assert_eq!(
            run(json!({"path": "."}), &ctx(tmp.path())).await,
            Ok("a_dir/\nb_dir/\na.txt\nz.txt".into())
        );
    }

    #[tokio::test]
    async fn path_defaults_to_the_project_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("only.txt"), "").unwrap();

        assert_eq!(
            run(json!({}), &ctx(tmp.path())).await,
            Ok("only.txt".into())
        );
    }

    #[tokio::test]
    async fn lists_a_subdirectory() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub/inner")).unwrap();
        std::fs::write(tmp.path().join("sub/file.txt"), "").unwrap();

        assert_eq!(
            run(json!({"path": "sub"}), &ctx(tmp.path())).await,
            Ok("inner/\nfile.txt".into())
        );
    }

    #[tokio::test]
    async fn empty_directory_is_reported_explicitly() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();

        assert_eq!(
            run(json!({"path": "empty"}), &ctx(tmp.path())).await,
            Ok("[empty directory]".into())
        );
    }

    #[tokio::test]
    async fn missing_directory_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"path": "nope"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("enoent"));
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(json!({"path": "../.."}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
        assert_eq!(
            run(json!({"path": "/etc"}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn non_string_path_is_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(run(json!({"path": 42}), &ctx(tmp.path())).await.is_err());
    }
}
