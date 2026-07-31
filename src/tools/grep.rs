//! `grep(pattern, path?, glob?, limit?)`: regex content search, a faithful port
//! of qwen v0.16.0 `tools/ripGrep.ts` (`RipGrepTool`, the default GREP tool -
//! `useRipgrep` defaults true). Returns matching lines as flat
//! `path:line:content` entries under a verbatim `Found N match(es) ...:\n---\n`
//! header, grouped only by qwen's join (one line per match, in walk order).
//!
//! The engine stays IN-PROCESS: qwen shells out to the `rg` binary with
//! `--ignore-case --regexp`, but suspenders keeps the Rust `regex` crate + the
//! shared [`crate::walk::walk_files`] walker. The Rust regex syntax is close
//! enough to ripgrep's that the model-facing "built on ripgrep" description
//! holds. Search is always CASE-INSENSITIVE (qwen passes `--ignore-case`).
//!
//! Matching respects `.gitignore` (via the walker, qwen's default
//! `respectGitIgnore`) and `.qwenignore` (via [`crate::walk::qwenignore`],
//! qwen's default `respectQwenIgnore`), the latter anchored to the PROJECT ROOT
//! (`ctx.root`), like glob (ripGrep.ts:404-435). Well-known vendored/build
//! directories are skipped and symlinks are never followed; binary files are
//! skipped.
//!
//! An invalid regex is REJECTED at validation with qwen's verbatim
//! `"Invalid regular expression pattern: ..."` message (ripGrep.ts:544-549) -
//! there is no literal-text fallback. The `glob` param filters walked files by
//! a shell glob (qwen's `rg --glob`); `limit` caps the number of matching lines
//! (qwen's `min(truncateToolOutputLines, limit)`), with a verbatim truncation
//! trailer.
//!
//! CONFINEMENT (a deliberate divergence from qwen). qwen resolves `path` with
//! `allowExternalPaths: true` and gates an out-of-workspace path behind a
//! runtime ask-permission (`getDefaultPermission` returns `'ask'`,
//! ripGrep.ts:146-159) rather than refusing it. Suspenders has no
//! ask-permission seam, so it deliberately TIGHTENS to hard-refuse a `path`
//! that climbs out of the Project Root (`with_path`, the sibling read-only
//! tools' confinement) before the walk. This is the faithful-as-possible analog
//! and it matches the project-wide confinement decision. Paths are resolved
//! relative to the project root (qwen's relative-to-target-dir handling), not as
//! required-absolute.

use crate::tool::path::{FileError, file_error, resolve_path, with_path};
use crate::tool::{Tool, ToolCtx, ToolSpec};
use crate::glob_match;
use crate::walk::{qwenignore, walk_files};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use std::path::Path;

pub struct Grep;

/// The number of matching lines beyond which the output is truncated (qwen's
/// default `truncateToolOutputLines`, config.ts:427). qwen's effective line
/// limit is `min(truncateToolOutputLines, params.limit ?? Infinity)`
/// (ripGrep.ts:292-295); with no `limit` this 1000-line cap is what binds.
const DEFAULT_LINE_LIMIT: usize = 1000;

const BINARY_PROBE_BYTES: usize = 8_192;

/// The verbatim tool description (qwen ripGrep.ts:497).
const DESCRIPTION: &str = "A powerful search tool built on ripgrep\n\n  Usage:\n  - ALWAYS use Grep for search tasks. NEVER invoke `grep` or `rg` as a Bash command. The Grep tool has been optimized for correct permissions and access.\n  - Supports full regex syntax (e.g., \"log.*Error\", \"function\\s+\\w+\")\n  - Filter files with glob parameter (e.g., \"*.js\", \"**/*.tsx\")\n  - Use Agent tool for open-ended searches requiring multiple rounds\n  - Pattern syntax: Uses ripgrep (not grep) - special regex characters need escaping (use `interface\\{\\}` to find `interface{}` in Go code)\n";

/// The verbatim `pattern` property description (qwen ripGrep.ts:503-504).
const PATTERN_DESCRIPTION: &str =
    "The regular expression pattern to search for in file contents";

/// The verbatim `glob` property description (qwen ripGrep.ts:508-509).
const GLOB_DESCRIPTION: &str =
    "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\") - maps to rg --glob";

/// The verbatim `path` property description (qwen ripGrep.ts:513-514).
const PATH_DESCRIPTION: &str =
    "File or directory to search in (rg PATH). Defaults to current working directory.";

/// The verbatim `limit` property description (qwen ripGrep.ts:518-519).
const LIMIT_DESCRIPTION: &str =
    "Limit output to first N lines/entries. Optional - shows all matches if not specified.";

#[async_trait::async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep_search".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": PATTERN_DESCRIPTION
                    },
                    "glob": {
                        "type": "string",
                        "description": GLOB_DESCRIPTION
                    },
                    "path": {
                        "type": "string",
                        "description": PATH_DESCRIPTION
                    },
                    "limit": {
                        "type": "number",
                        "description": LIMIT_DESCRIPTION
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
        // path is optional (qwen: defaults to the search directory); a
        // non-string path is an error.
        let path = match input.get("path") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return Err("path must be a string".to_string()),
        };
        // glob is optional; a non-string glob is an error.
        let glob = match input.get("glob") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return Err("glob must be a string".to_string()),
        };
        // limit is optional; a non-integer limit is an error. qwen caps the
        // number of matching lines shown.
        let limit = match input.get("limit") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => return Err("limit must be a number".to_string()),
            },
        };

        // CASE-INSENSITIVE always (qwen `--ignore-case`). An invalid regex is
        // rejected with qwen's verbatim message, no literal fallback.
        let regex = compile(pattern)?;
        // A glob filter, translated to a case-insensitive path regex by the
        // shared helper (the same translation glob uses), or none.
        let glob_regex = match glob {
            Some(g) => Some(glob_match::compile(g)?),
            None => None,
        };

        with_path(path.unwrap_or("."), ctx, |abs| {
            // The search dir root, expanded the way with_path expands it so the
            // reported relative paths strip cleanly. For a single-file target,
            // `rg PATH` reports the file as passed; stripping against its PARENT
            // yields the filename (stripping against the file itself would give
            // an empty path).
            let resolved =
                resolve_path(path.unwrap_or("."), &ctx.root).unwrap_or_else(|_| abs.to_path_buf());

            let (files, root) = if abs.is_file() {
                let parent = resolved.parent().map(Path::to_path_buf).unwrap_or(resolved.clone());
                (vec![abs.to_path_buf()], parent)
            } else if abs.is_dir() {
                (walk_files(abs), resolved)
            } else {
                return Err(file_error("grep_search", path.unwrap_or("."), FileError::Enoent));
            };

            // qwen's verbatim location clause (ripGrep.ts:195-199): a given
            // `path` reads "in path \"P\""; the default reads "in the workspace
            // directory". Suspenders has a single project root, so the
            // multi-workspace clause never applies.
            let location = match path {
                Some(p) => format!("in path \"{p}\""),
                None => "in the workspace directory".to_string(),
            };

            Ok(search(
                &files,
                &regex,
                glob_regex.as_ref(),
                &root,
                &ctx.root,
                pattern,
                glob,
                &location,
                limit,
            ))
        })
    }
}

// Compile the pattern as a CASE-INSENSITIVE regex (qwen `--ignore-case`). An
// invalid regex is rejected with qwen's verbatim message (ripGrep.ts:548),
// there is no literal-text fallback.
fn compile(pattern: &str) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("Invalid regular expression pattern: {pattern}. Error: {e}"))
}

// Walk the collected files, gathering `path:line:content` matches (in walk
// order, one per matching line), filtered by the `.qwenignore` matcher and the
// optional `glob`, capped at qwen's line limit, and rendered under qwen's
// verbatim header (or the zero-match line).
//
// `rel_root` is the search dir: reported paths and the glob are matched against
// each file's path relative to it (`rg` reports paths relative to its PATH arg,
// and `--glob` matches those relative paths). `.qwenignore` is anchored to the
// PROJECT ROOT (`project_root`), NOT the search dir, so a root-anchored pattern
// means the same thing regardless of `path` (ripGrep.ts:404-435, mirroring
// glob.rs).
#[allow(clippy::too_many_arguments)]
fn search(
    files: &[std::path::PathBuf],
    regex: &Regex,
    glob_regex: Option<&Regex>,
    rel_root: &Path,
    project_root: &Path,
    pattern: &str,
    glob: Option<&str>,
    location: &str,
    limit: Option<usize>,
) -> String {
    let mut all_lines: Vec<String> = Vec::new();
    for file in files {
        if qwenignore::is_ignored(project_root, file) {
            continue;
        }
        let relative = relative_to(file, rel_root);
        if let Some(gr) = glob_regex
            && !gr.is_match(&relative)
        {
            continue;
        }
        all_lines.extend(matches_in(file, regex, &relative));
    }

    // The location + filter clauses that qwen appends to both the header and the
    // zero-match message (ripGrep.ts:201-203, 289).
    let filter = match glob {
        Some(g) => format!(" (filter: \"{g}\")"),
        None => String::new(),
    };

    if all_lines.is_empty() {
        // VERBATIM qwen ripGrep.ts:207 (llmContent).
        return format!("No matches found for pattern \"{pattern}\" {location}{filter}.");
    }

    let total = all_lines.len();
    let match_term = if total == 1 { "match" } else { "matches" };

    // qwen's effective line limit: min(truncateToolOutputLines, limit ?? inf)
    // (ripGrep.ts:292-295).
    let line_limit = limit.map(|l| l.min(DEFAULT_LINE_LIMIT)).unwrap_or(DEFAULT_LINE_LIMIT);
    let truncated = total > line_limit;
    let shown = if truncated {
        &all_lines[..line_limit]
    } else {
        &all_lines[..]
    };

    // VERBATIM qwen header (ripGrep.ts:289).
    let mut message = format!(
        "Found {total} {match_term} for pattern \"{pattern}\" {location}{filter}:\n---\n{}",
        shown.join("\n")
    );

    if truncated {
        // VERBATIM qwen truncation trailer (ripGrep.ts:349): omitted = total
        // minus the number of lines actually included.
        let omitted = total - shown.len();
        let line_term = if omitted == 1 { "line" } else { "lines" };
        message.push_str(&format!("\n---\n[{omitted} {line_term} truncated] ..."));
    }

    message
}

// Read a file and collect `path:line:content` for each line the regex matches.
// A binary file (or an unreadable one) yields no matches. The trailing newline
// is already stripped by the line split; content is NOT otherwise trimmed
// (qwen only strips a trailing `\r?\n`, ripGrep.ts:232).
fn matches_in(file: &Path, regex: &Regex, relative: &str) -> Vec<String> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    if binary_content(&bytes) {
        return Vec::new();
    }
    let content = String::from_utf8_lossy(&bytes);
    // Strip ONE trailing `\n` so a file ending in a newline does not split into
    // a phantom trailing empty segment: `"a\nb\n".split('\n')` yields
    // `["a", "b", ""]`, and an empty-matching regex (`^`, `.*`, `x*`) would
    // otherwise count that empty tail as a match, inflating the total past what
    // ripgrep reports. An empty file yields ZERO lines (not one empty line).
    let body = content.strip_suffix('\n').unwrap_or(&content);
    if body.is_empty() {
        return Vec::new();
    }
    body.split('\n')
        .enumerate()
        .filter(|(_, line)| regex.is_match(line))
        .map(|(i, line)| format!("{relative}:{}:{}", i + 1, line.strip_suffix('\r').unwrap_or(line)))
        .collect()
}

// Strip the search-root prefix so paths report relative to the search dir
// (qwen's `rg PATH`-relative paths), always with `/` separators.
fn relative_to(file: &Path, root: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => file.to_string_lossy().replace('\\', "/"),
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
    fn spec_requires_pattern_only_and_carries_the_verbatim_strings() {
        let spec = Grep.spec();
        assert_eq!(spec.name, "grep_search");
        assert_eq!(spec.input_schema["required"], json!(["pattern"]));
        assert_eq!(
            spec.input_schema["properties"]["pattern"]["description"],
            json!(PATTERN_DESCRIPTION)
        );
        assert_eq!(
            spec.input_schema["properties"]["glob"]["description"],
            json!(GLOB_DESCRIPTION)
        );
        assert_eq!(
            spec.input_schema["properties"]["path"]["description"],
            json!(PATH_DESCRIPTION)
        );
        assert_eq!(
            spec.input_schema["properties"]["limit"]["description"],
            json!(LIMIT_DESCRIPTION)
        );
        assert!(spec.description.starts_with("A powerful search tool built on ripgrep"));
        assert!(spec.description.contains("Use Agent tool for open-ended searches"));
    }

    #[tokio::test]
    async fn returns_matches_under_the_verbatim_header() {
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
            Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\nlib/a.ex:2:  # needle here".into())
        );
    }

    #[tokio::test]
    async fn plural_match_term_and_multiple_lines() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "foo1\nfoo2\nbar3\n").unwrap();

        assert_eq!(
            run(json!({"pattern": r"^foo\d$"}), &ctx(tmp.path())).await,
            Ok("Found 2 matches for pattern \"^foo\\d$\" in the workspace directory:\n---\na.txt:1:foo1\na.txt:2:foo2".into())
        );
    }

    #[tokio::test]
    async fn search_is_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "Needle here\nNEEDLE too\n").unwrap();

        let out = run(json!({"pattern": "needle"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(out.contains("a.txt:1:Needle here"));
        assert!(out.contains("a.txt:2:NEEDLE too"));
        assert!(out.starts_with("Found 2 matches"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("real.txt"), "needle inside\n").unwrap();
        symlink(".", tmp.path().join("loop")).unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle outside\n").unwrap();
        symlink(outside.path(), tmp.path().join("escape")).unwrap();
        symlink(
            outside.path().join("secret.txt"),
            tmp.path().join("linked.txt"),
        )
        .unwrap();

        let out = run(json!({"pattern": "needle"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(out.contains("real.txt:1:needle inside"));
        assert!(!out.contains("outside"));
    }

    #[tokio::test]
    async fn searches_only_under_the_given_path_and_reports_it() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        std::fs::write(tmp.path().join("lib/a.ex"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("other/b.ex"), "needle\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle", "path": "lib"}), &ctx(tmp.path())).await,
            Ok("Found 1 match for pattern \"needle\" in path \"lib\":\n---\na.ex:1:needle".into())
        );
    }

    #[tokio::test]
    async fn path_may_be_a_single_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("single.txt"), "one needle\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle", "path": "single.txt"}), &ctx(tmp.path())).await,
            Ok("Found 1 match for pattern \"needle\" in path \"single.txt\":\n---\nsingle.txt:1:one needle".into())
        );
    }

    #[tokio::test]
    async fn glob_filters_the_walked_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "needle\n").unwrap();

        let out = run(json!({"pattern": "needle", "glob": "*.rs"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(
            out,
            "Found 1 match for pattern \"needle\" in the workspace directory (filter: \"*.rs\"):\n---\na.rs:1:needle"
        );
    }

    #[tokio::test]
    async fn glob_can_cross_directories_with_double_star() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/net")).unwrap();
        std::fs::write(tmp.path().join("src/net/client.rs"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("top.txt"), "needle\n").unwrap();

        let out = run(json!({"pattern": "needle", "glob": "**/*.rs"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(out.contains("src/net/client.rs:1:needle"));
        assert!(!out.contains("top.txt"));
    }

    #[tokio::test]
    async fn a_slash_free_glob_filter_matches_at_any_depth() {
        // ripgrep --glob semantics: a slash-free `*.rs` filters files at ANY
        // depth, so a deeply nested match survives the filter.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/nested")).unwrap();
        std::fs::write(tmp.path().join("src/nested/a.rs"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "needle\n").unwrap();

        let out = run(json!({"pattern": "needle", "glob": "*.rs"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(out.contains("src/nested/a.rs:1:needle"));
        assert!(!out.contains("b.txt"));
    }

    #[tokio::test]
    async fn an_empty_matching_regex_does_not_count_a_phantom_trailing_line() {
        // A file ending in "\n" must NOT report a phantom match on the empty
        // segment `split('\n')` produces after the final newline: `^` over
        // "a\nb\n" is 2 matches (the two real lines), not 3.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "a\nb\n").unwrap();

        let out = run(json!({"pattern": "^"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(
            out.starts_with("Found 2 matches for pattern \"^\" in the workspace directory:\n---\n"),
            "got: {out}"
        );
        assert!(out.contains("f.txt:1:a"));
        assert!(out.contains("f.txt:2:b"));
        assert!(!out.contains("f.txt:3:"));
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
            Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\nvisible.txt:1:needle".into())
        );
    }

    #[tokio::test]
    async fn respects_a_gitignore() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(tmp.path().join("ignored.txt"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("kept.txt"), "needle\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\nkept.txt:1:needle".into())
        );
    }

    #[tokio::test]
    async fn respects_a_qwenignore() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".qwenignore"), "secret.txt\n").unwrap();
        std::fs::write(tmp.path().join("secret.txt"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("kept.txt"), "needle\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\nkept.txt:1:needle".into())
        );
    }

    #[tokio::test]
    async fn a_qwenignore_anchors_to_the_project_root_not_the_search_dir() {
        // A ROOT-ANCHORED `.qwenignore` pattern (`/build/`) ignores only the
        // top-level `build/`, not a nested `sub/build/`. Searching under
        // `path: "sub"`, the ignore anchors to the PROJECT ROOT (ripGrep.ts
        // loads .qwenignore from the workspace dirs): if it anchored to the
        // search dir instead, `/build/` would over-match `sub/build/`.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".qwenignore"), "/build/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("build")).unwrap();
        std::fs::create_dir_all(tmp.path().join("sub/build")).unwrap();
        std::fs::write(tmp.path().join("build/top.txt"), "needle\n").unwrap();
        std::fs::write(tmp.path().join("sub/build/nested.txt"), "needle\n").unwrap();

        let out = run(json!({"pattern": "needle", "path": "sub"}), &ctx(tmp.path()))
            .await
            .unwrap();
        // Root-anchored: the nested build survives (it is not the top-level one).
        assert!(out.contains("build/nested.txt:1:needle"));

        // Cross-check: over the project root, the top-level build IS ignored.
        let root_out = run(json!({"pattern": "needle"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(!root_out.contains("build/top.txt"));
        assert!(root_out.contains("sub/build/nested.txt:1:needle"));
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
            Ok("Found 1 match for pattern \"needle\" in the workspace directory:\n---\ntext.txt:1:needle".into())
        );
    }

    #[tokio::test]
    async fn limit_caps_matches_with_the_verbatim_trailer() {
        let tmp = TempDir::new().unwrap();
        let lines = (1..=10)
            .map(|n| format!("needle {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("many.txt"), format!("{lines}\n")).unwrap();

        let out = run(json!({"pattern": "needle", "limit": 4}), &ctx(tmp.path()))
            .await
            .unwrap();
        // Header shows the REAL total (10); the body is capped at 4 lines; the
        // trailer names the 6 omitted lines.
        assert!(out.starts_with("Found 10 matches for pattern \"needle\" in the workspace directory:\n---\n"));
        assert!(out.ends_with("\n---\n[6 lines truncated] ..."));
        let body = out
            .split("\n---\n")
            .nth(1)
            .unwrap();
        assert_eq!(body.split('\n').count(), 4);
        assert!(body.starts_with("many.txt:1:needle 1"));
    }

    #[tokio::test]
    async fn a_single_truncated_line_says_line_singular() {
        let tmp = TempDir::new().unwrap();
        let lines = (1..=3)
            .map(|n| format!("needle {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path().join("f.txt"), format!("{lines}\n")).unwrap();

        let out = run(json!({"pattern": "needle", "limit": 2}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(out.ends_with("\n---\n[1 line truncated] ..."));
    }

    #[tokio::test]
    async fn no_matches_is_the_verbatim_message() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "nothing\n").unwrap();

        assert_eq!(
            run(json!({"pattern": "needle"}), &ctx(tmp.path())).await,
            Ok("No matches found for pattern \"needle\" in the workspace directory.".into())
        );
    }

    #[tokio::test]
    async fn no_matches_message_includes_path_and_filter_clauses() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("lib")).unwrap();
        std::fs::write(tmp.path().join("lib/a.rs"), "nothing\n").unwrap();

        assert_eq!(
            run(
                json!({"pattern": "needle", "path": "lib", "glob": "*.rs"}),
                &ctx(tmp.path())
            )
            .await,
            Ok("No matches found for pattern \"needle\" in path \"lib\" (filter: \"*.rs\").".into())
        );
    }

    #[tokio::test]
    async fn an_invalid_regex_is_rejected_with_the_verbatim_message() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("code.ex"), "def foo(bar) do\n").unwrap();

        let err = run(json!({"pattern": "foo(bar"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(
            err.starts_with("Invalid regular expression pattern: foo(bar. Error:"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn zero_matches_from_a_valid_regex_is_a_real_answer() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x+y\n").unwrap();

        // `x+y` is a valid regex (one-or-more x, then y) and does not match the
        // literal `x+y`; qwen returns the no-matches message, never a fallback.
        assert_eq!(
            run(json!({"pattern": "x+y"}), &ctx(tmp.path())).await,
            Ok("No matches found for pattern \"x+y\" in the workspace directory.".into())
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
            run(json!({"pattern": "root", "path": "../.."}), &ctx(tmp.path())).await,
            Err("path escapes project root".into())
        );
    }

    #[tokio::test]
    async fn missing_or_non_string_pattern_is_a_structured_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(crate::tools::execute("grep_search", &json!({}), &c).await.is_error);
        assert!(
            crate::tools::execute("grep_search", &json!({"pattern": 42}), &c)
                .await
                .is_error
        );
    }
}
