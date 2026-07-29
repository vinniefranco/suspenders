//! `glob(pattern, path?)`: find files whose path matches a shell glob (`*`,
//! `?`, `**`, `[set]`), returning matching paths relative to the search root,
//! one per line, sorted. Searches recursively under `path` (default the project root).
//! Skips the same well-known vendored/build directories `grep`/`list_files`
//! skip, and never follows symlinks - the same confinement the sibling
//! read-only tools use.
//!
//! The glob is translated to a regex (reusing the `regex` dep grep already
//! pulls) so `**` can cross directory boundaries and `*`/`?` cannot. A pattern
//! that does not translate to a valid regex is a tool error, not a panic -
//! bounced back so the model can fix it rather than crashing the Run. Zero
//! matches is a clean explicit result, not an error (an empty-string result
//! confuses small models).

use crate::tool::path::{FileError, file_error, resolve_path, with_path};
use crate::tool::{Tool, ToolCtx, ToolSpec};
use regex::Regex;
use serde_json::{Value, json};
use std::path::Path;

pub struct Glob;

// The same vendored/build directories grep and list_files never descend into.
// Public so the opening environment tree (env_context) prunes the SAME set the
// read-only tools do, and the tree the model sees matches what glob would walk.
pub const SKIP_DIRS: &[&str] = &[
    ".claude",
    ".git",
    "_build",
    "deps",
    "node_modules",
    ".direnv",
    ".nix-hex",
    ".nix-mix",
    ".elixir_ls",
];

#[async_trait::async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description:
                "Fast file-name pattern matching tool that works with any codebase size.\n\
                \n\
                - Finds files by name using a glob pattern and returns the matching paths, one per \
                line, sorted lexicographically (a stable, depth-first sorted walk).\n\
                - Supports glob patterns like \"**/*.rs\", \"src/**/mod.rs\", or \"Cargo.toml\": \
                `**` matches across directory boundaries, `*` matches within a single path segment, \
                `?` matches a single character, and `[set]` is a character class.\n\
                - Use this tool when you need to find files by name or extension but do not know \
                the directory - it is faster than listing directories by hand.\n\
                - Scope: searches recursively under `path` (default the project root); returned \
                paths are relative to that search root. Skips .git, _build, deps, node_modules and \
                other vendored/build directories, and does not follow symlinks.\n\
                - Returns \"[no matches]\" when nothing matches (not an error). An invalid pattern \
                is reported back so you can fix it.\n\
                - You can call multiple tools in a single response. It is often better to \
                speculatively perform several searches as a batch that are potentially useful.\n\
                - To search file CONTENTS rather than names, use grep instead."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "The glob pattern to match paths against, e.g. \"**/*.rs\" \
                            or \"src/**/mod.rs\". `**` matches across directories, `*` within a \
                            segment, `?` a single character, `[set]` a character class."
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search under, relative to the project root. \
                            Omit to search the whole project root (defaults to \".\"). Do not pass \
                            an absolute path."
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
            .ok_or_else(|| "invalid input: glob requires a string \"pattern\"".to_string())?;
        // path is optional (default "."); a non-string path is an error.
        let path = match input.get("path") {
            None | Some(Value::Null) => ".",
            Some(Value::String(s)) => s.as_str(),
            Some(_) => return Err("path must be a string".to_string()),
        };

        let regex = compile(pattern)?;

        with_path(path, ctx, |abs| {
            if !abs.is_dir() {
                return Err(file_error("glob", path, FileError::Enotdir));
            }
            // The root paths are reported relative to - the same expansion
            // with_path applied, so strip_prefix cleaves cleanly.
            let root = resolve_path(path, &ctx.root).unwrap_or_else(|_| abs.to_path_buf());
            Ok(search(abs, &root, &regex))
        })
    }
}

// Translate the glob to an anchored regex and compile it, reusing the `regex`
// dep. A glob that produces an invalid regex is bounced back as a tool error.
fn compile(pattern: &str) -> Result<Regex, String> {
    let regex_src = glob_to_regex(pattern);
    Regex::new(&regex_src)
        .map_err(|_| format!("invalid glob pattern: {pattern:?}"))
}

// Glob -> regex over the whole relative path (matched with `/` separators):
//   **      any characters including `/` (crosses directories)
//   *       any characters except `/` (stays within a segment)
//   ?       any single character except `/`
//   [set]   a character class, passed through to the regex verbatim
// Everything else is a literal (regex-escaped). Anchored at both ends. An
// unclosed `[` passes through and makes the regex fail to compile, which the
// caller reports as an invalid pattern rather than a panic.
fn glob_to_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    i += 2;
                    if chars.get(i) == Some(&'/') {
                        // `**/` matches any number of leading directories,
                        // INCLUDING none, so `**/x` also matches a top-level
                        // `x`.
                        out.push_str("(?:.*/)?");
                        i += 1;
                    } else {
                        // A bare `**` (or `**foo`) matches anything, crossing
                        // directory boundaries.
                        out.push_str(".*");
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '[' => {
                // A character class, verbatim up to and including the `]`. An
                // unclosed class is passed through as a lone `[`, which the
                // regex engine rejects - the invalid-pattern error path.
                out.push('[');
                i += 1;
                while i < chars.len() && chars[i] != ']' {
                    out.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    out.push(']');
                    i += 1;
                }
            }
            c => {
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out.push('$');
    out
}

// Match every walked file's relative path against the regex, sorted (the walk
// yields sorted paths). Zero matches is a clean explicit result.
fn search(root: &Path, rel_root: &Path, regex: &Regex) -> String {
    let matches: Vec<String> = collect_files(root)
        .iter()
        .map(|file| relative_to(file, rel_root))
        .filter(|rel| regex.is_match(rel))
        .collect();

    if matches.is_empty() {
        "[no matches]".to_string()
    } else {
        matches.join("\n")
    }
}

// Depth-first, sorted for stable output. Skip-dirs are pruned by basename.
// The same shape grep's `collect_files` uses.
fn collect_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut entries: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => return Vec::new(),
    };
    entries.sort();

    let mut out = Vec::new();
    for entry in entries {
        out.extend(collect_entry(dir, &entry));
    }
    out
}

// lstat, not stat: symlinks are skipped entirely, the same reason grep gives -
// following them could walk out of the root or loop forever.
fn collect_entry(dir: &Path, entry: &str) -> Vec<std::path::PathBuf> {
    let full = dir.join(entry);
    match std::fs::symlink_metadata(&full) {
        Ok(meta) if meta.file_type().is_dir() => {
            if SKIP_DIRS.contains(&entry) {
                Vec::new()
            } else {
                collect_files(&full)
            }
        }
        Ok(meta) if meta.file_type().is_file() => vec![full],
        _ => Vec::new(),
    }
}

// Strip the search-root prefix so patterns match against a clean relative path
// (grep's `relative_to`), always with `/` separators.
fn relative_to(file: &Path, root: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => file.to_string_lossy().replace('\\', "/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(root: &Path) -> ToolCtx {
        ToolCtx {
            root: root.to_path_buf(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
        }
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        Glob.run(&input, ctx).await
    }

    #[test]
    fn spec_requires_pattern_only() {
        let spec = Glob.spec();
        assert_eq!(spec.name, "glob");
        assert_eq!(spec.input_schema["required"], json!(["pattern"]));
    }

    #[tokio::test]
    async fn matches_a_recursive_extension_pattern_sorted() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/net")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        std::fs::write(tmp.path().join("src/net/client.rs"), "").unwrap();
        std::fs::write(tmp.path().join("README.md"), "").unwrap();

        assert_eq!(
            run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path())).await,
            Ok("src/main.rs\nsrc/net/client.rs".into())
        );
    }

    #[tokio::test]
    async fn double_star_matches_files_at_the_root_too() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("top.rs"), "").unwrap();
        std::fs::write(tmp.path().join("sub/deep.rs"), "").unwrap();

        assert_eq!(
            run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path())).await,
            Ok("sub/deep.rs\ntop.rs".into())
        );
    }

    #[tokio::test]
    async fn single_star_stays_within_a_path_segment() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), "").unwrap();
        std::fs::write(tmp.path().join("src/b.rs"), "").unwrap();
        std::fs::create_dir_all(tmp.path().join("src/inner")).unwrap();
        std::fs::write(tmp.path().join("src/inner/c.rs"), "").unwrap();

        // `src/*.rs` matches the two direct children, not the nested one.
        assert_eq!(
            run(json!({"pattern": "src/*.rs"}), &ctx(tmp.path())).await,
            Ok("src/a.rs\nsrc/b.rs".into())
        );
    }

    #[tokio::test]
    async fn question_mark_matches_exactly_one_character() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a1.txt"), "").unwrap();
        std::fs::write(tmp.path().join("a2.txt"), "").unwrap();
        std::fs::write(tmp.path().join("a10.txt"), "").unwrap();

        assert_eq!(
            run(json!({"pattern": "a?.txt"}), &ctx(tmp.path())).await,
            Ok("a1.txt\na2.txt".into())
        );
    }

    #[tokio::test]
    async fn a_character_class_matches_the_enumerated_characters() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();
        std::fs::write(tmp.path().join("c.txt"), "").unwrap();

        assert_eq!(
            run(json!({"pattern": "[ab].txt"}), &ctx(tmp.path())).await,
            Ok("a.txt\nb.txt".into())
        );
    }

    #[tokio::test]
    async fn matches_an_exact_literal_filename() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), "").unwrap();

        assert_eq!(
            run(json!({"pattern": "Cargo.toml"}), &ctx(tmp.path())).await,
            Ok("Cargo.toml".into())
        );
    }

    #[tokio::test]
    async fn searches_only_under_the_given_path_and_reports_relative_to_it() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        std::fs::write(tmp.path().join("lib/a.ex"), "").unwrap();
        std::fs::write(tmp.path().join("other/b.ex"), "").unwrap();

        assert_eq!(
            run(json!({"pattern": "*.ex", "path": "lib"}), &ctx(tmp.path())).await,
            Ok("a.ex".into())
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
            std::fs::write(tmp.path().join(dir).join("hidden.rs"), "").unwrap();
        }
        std::fs::write(tmp.path().join("visible.rs"), "").unwrap();

        assert_eq!(
            run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path())).await,
            Ok("visible.rs".into())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("real.rs"), "").unwrap();

        // Cycle: a symlink back to the Project Root itself.
        symlink(".", tmp.path().join("loop")).unwrap();

        // Escape: a symlink to a directory outside the Project Root.
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "").unwrap();
        symlink(outside.path(), tmp.path().join("escape")).unwrap();

        assert_eq!(
            run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path())).await,
            Ok("real.rs".into())
        );
    }

    #[tokio::test]
    async fn no_matches_is_a_successful_explicit_result() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();

        assert_eq!(
            run(json!({"pattern": "**/*.rs"}), &ctx(tmp.path())).await,
            Ok("[no matches]".into())
        );
    }

    #[tokio::test]
    async fn an_invalid_glob_pattern_is_a_tool_error_not_a_panic() {
        let tmp = TempDir::new().unwrap();
        // A lone `[` opens a character class the glob translation passes
        // through to the regex, which fails to compile.
        let err = run(json!({"pattern": "["}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("invalid glob pattern"));
    }

    #[tokio::test]
    async fn a_missing_search_path_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let err = run(
            json!({"pattern": "*.rs", "path": "no_such_dir"}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("glob"));
    }

    #[tokio::test]
    async fn paths_escaping_the_project_root_are_refused() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(json!({"pattern": "*.rs", "path": "../.."}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
        assert_eq!(
            run(json!({"pattern": "*.rs", "path": "/etc"}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn missing_or_non_string_pattern_is_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(crate::tools::execute("glob", &json!({}), &c).await.is_error);
        assert!(
            crate::tools::execute("glob", &json!({"pattern": 42}), &c)
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn non_string_path_is_an_error() {
        let tmp = TempDir::new().unwrap();
        assert!(
            run(json!({"pattern": "*.rs", "path": 42}), &ctx(tmp.path()))
                .await
                .is_err()
        );
    }
}
