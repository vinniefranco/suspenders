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

use crate::glob_match;
use crate::tool::path::{FileError, file_error, resolve_path, with_path};
use crate::tool::{Tool, ToolCtx, ToolSpec};
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
const PATTERN_DESCRIPTION: &str = "The regular expression pattern to search for in file contents";

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
        let path =
            super::opt_str(input, "path").map_err(|_| "path must be a string".to_string())?;
        // glob is optional; a non-string glob is an error.
        let glob =
            super::opt_str(input, "glob").map_err(|_| "glob must be a string".to_string())?;
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
                let parent = resolved
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(resolved.clone());
                (vec![abs.to_path_buf()], parent)
            } else if abs.is_dir() {
                (walk_files(abs), resolved)
            } else {
                return Err(file_error(
                    "grep_search",
                    path.unwrap_or("."),
                    FileError::Enoent,
                ));
            };

            // qwen's verbatim location clause (ripGrep.ts:195-199): a given
            // `path` reads "in path \"P\""; the default reads "in the workspace
            // directory". Suspenders has a single project root, so the
            // multi-workspace clause never applies.
            let location = match path {
                Some(p) => format!("in path \"{p}\""),
                None => "in the workspace directory".to_string(),
            };

            let query = GrepQuery {
                regex: &regex,
                glob_regex: glob_regex.as_ref(),
                pattern,
                glob,
                location: &location,
                limit,
            };
            Ok(search(&files, &root, &ctx.root, &query))
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

/// The search specification for a grep call: the compiled regexes plus the
/// display strings qwen includes in every header/message.
struct GrepQuery<'a> {
    regex: &'a Regex,
    glob_regex: Option<&'a Regex>,
    pattern: &'a str,
    glob: Option<&'a str>,
    location: &'a str,
    limit: Option<usize>,
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
fn search(
    files: &[std::path::PathBuf],
    rel_root: &Path,
    project_root: &Path,
    query: &GrepQuery<'_>,
) -> String {
    let mut all_lines: Vec<String> = Vec::new();
    for file in files {
        if qwenignore::is_ignored(project_root, file) {
            continue;
        }
        let relative = relative_to(file, rel_root);
        if let Some(gr) = query.glob_regex
            && !gr.is_match(&relative)
        {
            continue;
        }
        all_lines.extend(matches_in(file, query.regex, &relative));
    }

    // The location + filter clauses that qwen appends to both the header and the
    // zero-match message (ripGrep.ts:201-203, 289).
    let filter = match query.glob {
        Some(g) => format!(" (filter: \"{g}\")"),
        None => String::new(),
    };
    let pattern = query.pattern;
    let location = query.location;

    if all_lines.is_empty() {
        // VERBATIM qwen ripGrep.ts:207 (llmContent).
        return format!("No matches found for pattern \"{pattern}\" {location}{filter}.");
    }

    let total = all_lines.len();
    let match_term = if total == 1 { "match" } else { "matches" };

    // qwen's effective line limit: min(truncateToolOutputLines, limit ?? inf)
    // (ripGrep.ts:292-295).
    let line_limit = query
        .limit
        .map(|l| l.min(DEFAULT_LINE_LIMIT))
        .unwrap_or(DEFAULT_LINE_LIMIT);
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
        .map(|(i, line)| {
            format!(
                "{relative}:{}:{}",
                i + 1,
                line.strip_suffix('\r').unwrap_or(line)
            )
        })
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
#[path = "../../tests/unit/tools/grep.rs"]
mod tests;
