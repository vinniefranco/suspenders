//! `grep(pattern, path?)`: regex search under a path (default `.`), returning
//! `relative/path:line: text` lines. Walks through the shared
//! [`crate::walk::walk_files`], so it respects `.gitignore`, skips well-known
//! vendored/build directories, and never follows symlinks; also skips binary
//! files; capped at 100 matches.
//!
//! A pattern that fails to compile as a regex is retried as literal text, with
//! a note prefixed to the result - the same forgive-the-caller philosophy as
//! edit_file's two-pass matching. Small models routinely emit `foo(bar`
//! meaning the literal characters; bouncing that back costs a full model
//! response on a slow local server.

use crate::tool::path::{FileError, file_error, resolve_path, with_path};
use crate::tool::{Tool, ToolCtx, ToolSpec};
use crate::walk::walk_files;
use regex::Regex;
use serde_json::{Value, json};
use std::path::Path;

pub struct Grep;

const MAX_MATCHES: usize = 100;
const BINARY_PROBE_BYTES: usize = 8_192;
const LITERAL_NOTE: &str = "[pattern is not valid regex - searched for it as literal text]\n";

#[async_trait::async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: format!(
                "A powerful content-search tool that scans file contents with a regular expression \
                and returns matching lines as `path:line: text`.\n\
                \n\
                Usage:\n\
                - ALWAYS use this grep tool for searching file contents. Do NOT invoke `grep` or \
                `rg` through run_command - this tool is set up with the correct project confinement \
                (it stays inside the project root, skips vendored/build directories, and never \
                follows symlinks).\n\
                - Supports full regular-expression syntax, e.g. \"log.*Error\", \"fn\\\\s+\\\\w+\", \
                \"TODO|FIXME\". Special regex metacharacters need escaping to match them literally \
                (use \"interface\\\\{{\\\\}}\" to find `interface{{}}` in Go code).\n\
                - Forgiving fallback: a pattern that fails to compile as a regex is retried as \
                literal text (with a note prefixed to the result) rather than erroring, so \
                \"foo(bar\" finds the literal characters. Zero matches from a VALID regex is a real \
                answer and never falls back.\n\
                - Scope: searches recursively under `path` (default the project root); `path` may \
                also be a single file. Respects .gitignore, skips .git, _build, deps, node_modules \
                and other vendored/build directories, and skips binary files.\n\
                - Output is capped at {MAX_MATCHES} matches; a trailer notes when the cap was \
                reached. \"[no matches]\" is returned when nothing matches.\n\
                - Use this to find where a symbol or string is defined or used before reading \
                files. To find files by NAME rather than by content, use glob instead."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The regular expression to search for in file contents, \
                            matched against each line, e.g. \"fn run\" or \"impl\\\\s+Tool\". A \
                            pattern that is not valid regex is searched as literal text instead of \
                            erroring."
                    },
                    "path": {
                        "type": "string",
                        "description": "The directory or file to search in, relative to the \
                            project root (e.g. \"src/tools\"). Defaults to \".\" (the project \
                            root). Do not pass an absolute path."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: grep requires a string \"pattern\"".to_string())?;
        let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let (regex, note) = compile(pattern);

        with_path(path, ctx, |abs| {
            // Expanded the same way with_path expands it, so relative_to
            // strips cleanly.
            let root = resolve_path(".", &ctx.root).unwrap_or_else(|_| ctx.root.clone());

            let result = if abs.is_file() {
                search(&[abs.to_path_buf()], &regex, &root)
            } else if abs.is_dir() {
                search(&walk_files(abs), &regex, &root)
            } else {
                Err(file_error("grep", path, FileError::Enoent))
            };

            result.map(|out| format!("{note}{out}"))
        })
    }
}

// Two-pass, like edit_file: (1) compile as regex; (2) only when that fails,
// escape and search the pattern as literal text, telling the model via the
// note. Zero matches from a VALID regex is a real answer and never falls back.
fn compile(pattern: &str) -> (Regex, String) {
    match Regex::new(pattern) {
        Ok(re) => (re, String::new()),
        Err(_) => (
            Regex::new(&regex::escape(pattern)).expect("escaped pattern always compiles"),
            LITERAL_NOTE.to_string(),
        ),
    }
}

fn search(files: &[std::path::PathBuf], regex: &Regex, root: &Path) -> Result<String, String> {
    let mut matches: Vec<String> = Vec::new();
    let mut capped = false;

    for file in files {
        matches.extend(matches_in(file, regex, root));
        if matches.len() > MAX_MATCHES {
            matches.truncate(MAX_MATCHES);
            capped = true;
            break;
        }
    }

    if matches.is_empty() {
        Ok("[no matches]".to_string())
    } else if capped {
        Ok(format!(
            "{}\n[capped at {MAX_MATCHES} matches]",
            matches.join("\n")
        ))
    } else {
        Ok(matches.join("\n"))
    }
}

fn matches_in(file: &Path, regex: &Regex, root: &Path) -> Vec<String> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    if binary_content(&bytes) {
        return Vec::new();
    }
    let content = String::from_utf8_lossy(&bytes);
    matching_lines(&content, file, regex, root)
}

fn matching_lines(content: &str, file: &Path, regex: &Regex, root: &Path) -> Vec<String> {
    let relative = relative_to(file, root);
    content
        .split('\n')
        .enumerate()
        .filter(|(_, line)| regex.is_match(line))
        .map(|(i, line)| format!("{relative}:{}: {line}", i + 1))
        .collect()
}

// Elixir's Path.relative_to/2: strip the root prefix, else return the path.
fn relative_to(file: &Path, root: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => file.to_string_lossy().into_owned(),
    }
}

// A file is binary if its first 8KB contain a null byte.
fn binary_content(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(BINARY_PROBE_BYTES)];
    probe.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(root: &Path) -> ToolCtx {
        ToolCtx::for_test(root.to_path_buf(), 10_000)
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        Grep.run(&input, ctx).await
    }

    #[test]
    fn spec_requires_pattern_only() {
        let spec = Grep.spec();
        assert_eq!(spec.name, "grep");
        assert_eq!(spec.input_schema["required"], json!(["pattern"]));
    }

    #[tokio::test]
    async fn returns_relative_path_line_text_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
        std::fs::write(
            tmp.path().join("lib/a.ex"),
            "defmodule A do\n  # needle here\nend\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("other.txt"), "nothing to see\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("lib/a.ex:2:   # needle here".into())
        );
    }

    #[tokio::test]
    async fn supports_regex_patterns() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "foo1\nfoo2\nbar3\n").unwrap();

        assert_eq!(
            run(json!({"pattern": r"^foo\d$"}), &ctx(tmp.path())).await,
            Ok("a.txt:1: foo1\na.txt:2: foo2".into())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("real.txt"), "needle inside\n").unwrap();

        // Cycle: a symlink back to the Project Root itself.
        symlink(".", tmp.path().join("loop")).unwrap();

        // Escape: a symlink to a directory outside the Project Root.
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle outside\n").unwrap();
        symlink(outside.path(), tmp.path().join("escape")).unwrap();

        // Symlinked file is skipped too.
        symlink(
            outside.path().join("secret.txt"),
            tmp.path().join("linked.txt"),
        )
        .unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("real.txt:1: needle inside".into())
        );
    }

    #[tokio::test]
    async fn searches_only_under_the_given_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        std::fs::write(tmp.path().join("lib/a.ex"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("other/b.ex"), "needle\n").unwrap();

        assert_eq!(
            run(
                json!({"pattern": "needle", "path": "lib"}),
                &ctx(tmp.path())
            )
            .await,
            Ok("lib/a.ex:1: needle".into())
        );
    }

    #[tokio::test]
    async fn path_may_be_a_single_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("single.txt"), "one needle\n").unwrap();

        assert_eq!(
            run(
                json!({"pattern": "needle", "path": "single.txt"}),
                &ctx(tmp.path())
            )
            .await,
            Ok("single.txt:1: one needle".into())
        );
    }

    #[tokio::test]
    async fn skips_git_build_deps_and_friends() {
        let tmp = TempDir::new().unwrap();
        for dir in [
            ".git",
            "_build",
            "deps",
            "node_modules",
            ".direnv",
            ".nix-hex",
            ".nix-mix",
            ".elixir_ls",
        ] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
            std::fs::write(tmp.path().join(dir).join("match.txt"), "needle\n").unwrap();
        }
        std::fs::write(tmp.path().join("visible.txt"), "needle\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("visible.txt:1: needle".into())
        );
    }

    #[tokio::test]
    async fn respects_a_gitignore() {
        let tmp = TempDir::new().unwrap();
        // The unification proof: grep no longer returns a match inside a
        // .gitignore'd file, where the old SKIP_DIRS-only walk would have.
        std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(tmp.path().join("ignored.txt"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("kept.txt"), "needle\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("kept.txt:1: needle".into())
        );
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let tmp = TempDir::new().unwrap();
        let mut bin = vec![0u8, 1, 2];
        bin.extend_from_slice(b"needle\n");
        std::fs::write(tmp.path().join("binary.dat"), bin).unwrap();
        std::fs::write(tmp.path().join("text.txt"), "needle\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("text.txt:1: needle".into())
        );
    }

    #[tokio::test]
    async fn caps_output_at_100_matches_with_a_trailer() {
        let tmp = TempDir::new().unwrap();
        let lines = (1..=150)
            .map(|n| format!("needle {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("many.txt"), format!("{lines}\n")).unwrap();

        let out = run(json!({"pattern": "needle"}), &ctx(tmp.path()))
            .await
            .unwrap();
        let out_lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(out_lines.len(), 101);
        assert_eq!(*out_lines.last().unwrap(), "[capped at 100 matches]");
        assert_eq!(out_lines[0], "many.txt:1: needle 1");
        assert_eq!(out_lines[99], "many.txt:100: needle 100");
    }

    #[tokio::test]
    async fn exactly_100_matches_is_not_capped() {
        let tmp = TempDir::new().unwrap();
        let lines = (1..=100)
            .map(|n| format!("needle {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("hundred.txt"), format!("{lines}\n")).unwrap();

        let out = run(json!({"pattern": "needle"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(!out.contains("capped"));
        assert_eq!(out.split('\n').count(), 100);
    }

    #[tokio::test]
    async fn no_matches_is_a_successful_explicit_result() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "nothing\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("[no matches]".into())
        );
    }

    #[tokio::test]
    async fn a_pattern_that_fails_to_compile_is_searched_as_literal_text() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.ex"), "def foo(bar) do\n  :ok\nend\n").unwrap();

        let out = run(json!({"pattern": "foo(bar"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(
            out.starts_with("[pattern is not valid regex - searched for it as literal text]\n")
        );
        assert!(out.contains("code.ex:1: def foo(bar) do"));
    }

    #[tokio::test]
    async fn the_literal_fallback_with_zero_matches_keeps_note_and_marker() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "nothing\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "missing(thing"}), &ctx(tmp.path())).await,
            Ok(
                "[pattern is not valid regex - searched for it as literal text]\n[no matches]"
                    .into()
            )
        );
    }

    #[tokio::test]
    async fn zero_matches_from_a_valid_regex_never_falls_back() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x+y\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "x+y"}), &ctx(tmp.path())).await,
            Ok("[no matches]".into())
        );
    }

    #[tokio::test]
    async fn missing_search_path_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let err = run(
            json!({"pattern": "x", "path": "no_such_dir"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("enoent"));
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(
                json!({"pattern": "root", "path": "../.."}),
                &ctx(tmp.path())
            )
            .await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn missing_or_non_string_pattern_is_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(crate::tools::execute("grep", &json!({}), &c).await.is_error);
        assert!(
            crate::tools::execute("grep", &json!({"pattern": 42}), &c)
                .await
                .is_error
        );
    }
}
