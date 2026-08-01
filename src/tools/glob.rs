//! `glob(pattern, path?)`: fast file-name pattern matching, a faithful port of
//! qwen v0.16.0 `tools/glob.ts`. Finds files whose path matches a shell glob
//! (`*`, `?`, `**`, `[set]`) and returns ABSOLUTE paths sorted by modification
//! time (newest first), under a verbatim header.
//!
//! Matching is CASE-INSENSITIVE (qwen's `nocase: true`). The sort is qwen's
//! `sortFileEntries`: files modified within the last 24h (the recency window)
//! come first, newest to oldest; everything older follows, sorted
//! alphabetically by absolute path. The list is capped at [`MAX_FILE_COUNT`]
//! with qwen's verbatim truncation trailer.
//!
//! Walks through the shared [`crate::walk::walk_files`], so it respects
//! `.gitignore`, skips the well-known vendored/build directories the other
//! read-only tools skip, and never follows symlinks. On top of that it filters
//! `.qwenignore` matches through the shared [`crate::walk::qwenignore`] matcher
//! (qwen's `respectQwenIgnore` default, on), anchored to the PROJECT ROOT (not
//! the search dir) so a root-anchored pattern means the same thing under any
//! `path` (glob.ts:155-159).
//!
//! CONFINEMENT (a deliberate divergence from qwen). qwen glob resolves `path`
//! with `allowExternalPaths: true` and gates an out-of-workspace path behind a
//! runtime ask-permission (`getDefaultPermission` returns `'ask'`,
//! glob.ts:112-128,193-197) rather than refusing it. Suspenders has no
//! ask-permission seam, so it deliberately TIGHTENS to hard-refuse a `path`
//! that climbs out of the Project Root (`with_path`, the sibling read-only
//! tools' confinement) before the walk. This is the faithful-as-possible analog
//! and it matches the project-wide confinement decision.
//!
//! The glob is translated to a regex by the shared [`crate::glob_match`] leaf
//! (the single source of truth grep's `glob` filter also calls) so `**` can
//! cross directory boundaries, `*`/`?` cannot, and a slash-free pattern matches
//! its basename at any depth. A pattern that does not translate to a valid regex
//! is a tool error, not a panic - bounced back so the model can fix it rather
//! than crashing the Run.

use crate::glob_match;
use crate::tool::path::{FileError, file_error, resolve_path, with_path};
use crate::tool::{Tool, ToolCtx, ToolSpec};
use crate::walk::{qwenignore, walk_files};
use regex::Regex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

// The vendored/build skip-list now lives with the shared walk (its single
// source of truth), re-exported here so env_context's `use` of it, and any
// reader that thinks of it as glob's prune, still resolve.
pub use crate::walk::SKIP_DIRS;

/// The maximum number of files glob lists (qwen `MAX_FILE_COUNT`, glob.ts:32).
/// qwen's effective limit is `min(MAX_FILE_COUNT, getTruncateToolOutputLines())`
/// (glob.ts:250); its default `truncateToolOutputLines` is 1000, so the 100-file
/// cap is what binds, which is what we apply here.
const MAX_FILE_COUNT: usize = 100;

/// The recency window for the sort (qwen `oneDayInMs`, glob.ts:239): a file
/// modified within this window of "now" is a "recent" file, sorted before older
/// files and newest-first among the recent set.
const RECENCY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// The verbatim tool description (qwen glob.ts:320).
const DESCRIPTION: &str = "Fast file pattern matching tool that works with any codebase size\n\
- Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\"\n\
- Returns matching file paths sorted by modification time\n\
- Use this tool when you need to find files by name patterns\n\
- When you are doing an open ended search that may require multiple rounds of globbing and grepping, use the Agent tool instead\n\
- You have the capability to call multiple tools in a single response. It is always better to speculatively perform multiple searches as a batch that are potentially useful.";

/// The verbatim `path` property description (qwen glob.ts:330).
const PATH_DESCRIPTION: &str = "The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter \"undefined\" or \"null\" - simply omit it for the default behavior. Must be a valid directory path if provided.";

pub struct Glob;

#[async_trait::async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "description": "The glob pattern to match files against",
                        "type": "string"
                    },
                    "path": {
                        "description": PATH_DESCRIPTION,
                        "type": "string"
                    }
                },
                "required": ["pattern"],
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "invalid input: glob requires a string \"pattern\"".to_string())?;
        // path is optional (qwen: omit to search the workspace directory); a
        // non-string path is an error.
        let path = match input.get("path") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return Err("path must be a string".to_string()),
        };

        let regex = glob_match::compile(pattern)?;

        // The directory to search, and qwen's verbatim location clause. Without
        // a path we search the Project Root ("the workspace directory"); with
        // one we search that (confined) directory ("within {abs}").
        with_path(path.unwrap_or("."), ctx, |abs| {
            if !abs.is_dir() {
                return Err(file_error("glob", path.unwrap_or("."), FileError::Enotdir));
            }
            let location = match path {
                Some(_) => format!("within {}", abs.display()),
                None => "in the workspace directory".to_string(),
            };
            let root =
                resolve_path(path.unwrap_or("."), &ctx.root).unwrap_or_else(|_| abs.to_path_buf());
            // The pattern matches relative to `root` (the search dir); ignores
            // anchor to `ctx.root` (the project root, glob.ts:159).
            Ok(search(abs, &root, &ctx.root, &regex, pattern, &location))
        })
    }
}

// Walk the search directory, keep the files whose relative path matches the
// regex and are not `.qwenignore`'d, sort by qwen's recency algorithm, cap at
// MAX_FILE_COUNT, and render qwen's verbatim header (or the zero-match line).
//
// The glob PATTERN is matched against each file's path relative to the search
// dir (`rel_root`), so `src/*.rs` under `path: "src"` is written the same way
// whether or not a subdir was given. But `.qwenignore` is anchored to the
// PROJECT ROOT (`project_root`), NOT the search dir, so a root-anchored pattern
// like `/build/` means the same thing regardless of `path` - this mirrors
// qwen's explicit choice to evaluate ignores against the project root even when
// searchDir != projectRoot (glob.ts:155-159).
fn search(
    dir: &Path,
    rel_root: &Path,
    project_root: &Path,
    regex: &Regex,
    pattern: &str,
    location: &str,
) -> String {
    let mut matched: Vec<Entry> = walk_files(dir)
        .into_iter()
        .filter(|file| regex.is_match(&relative_to(file, rel_root)))
        // qwen's `respectQwenIgnore` default is on: filter `.qwenignore` hits,
        // anchored to the PROJECT ROOT (glob.ts:159), not the search dir.
        .filter(|file| !qwenignore::is_ignored(project_root, file))
        .map(Entry::stat)
        .collect();

    if matched.is_empty() {
        // VERBATIM qwen glob.ts:233 (llmContent).
        return format!("No files found matching pattern \"{pattern}\" {location}");
    }

    sort_file_entries(&mut matched, SystemTime::now(), RECENCY_WINDOW);

    let total = matched.len();
    let truncated = total > MAX_FILE_COUNT;
    let shown = if truncated {
        &matched[..MAX_FILE_COUNT]
    } else {
        &matched[..]
    };

    let paths = shown
        .iter()
        .map(|e| e.path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>()
        .join("\n");

    // VERBATIM qwen glob.ts:266-267.
    let mut message = format!("Found {total} file(s) matching \"{pattern}\" {location}");
    message.push_str(&format!(
        ", sorted by modification time (newest first):\n---\n{paths}"
    ));

    if truncated {
        // VERBATIM qwen glob.ts:271-273.
        let omitted = total - MAX_FILE_COUNT;
        let file_term = if omitted == 1 { "file" } else { "files" };
        message.push_str(&format!("\n---\n[{omitted} {file_term} truncated] ..."));
    }

    message
}

// A matched file paired with its modification time, so the sort does not re-stat.
// A file whose mtime cannot be read is treated as the epoch (qwen's `?? 0`), so
// it sorts as an "old" file.
struct Entry {
    path: PathBuf,
    mtime: SystemTime,
}

impl Entry {
    fn stat(path: PathBuf) -> Entry {
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        Entry { path, mtime }
    }
}

// qwen's `sortFileEntries` (glob.ts:45-68): files modified within
// `recency_window` of `now` are "recent" and sort first, newest to oldest;
// everything older sorts after them, alphabetically by absolute path.
fn sort_file_entries(entries: &mut [Entry], now: SystemTime, recency_window: Duration) {
    let is_recent = |t: SystemTime| {
        now.duration_since(t)
            .map(|d| d < recency_window)
            .unwrap_or(true)
    };
    entries.sort_by(|a, b| {
        match (is_recent(a.mtime), is_recent(b.mtime)) {
            // Both recent: newest first (qwen `mtimeB - mtimeA`).
            (true, true) => b.mtime.cmp(&a.mtime),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            // Both old: alphabetical by absolute path. `Path::cmp` is a byte
            // ordering, not qwen's `localeCompare`: it agrees for ASCII
            // filenames and may differ only for non-ASCII ones.
            (false, false) => a.path.cmp(&b.path),
        }
    });
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
#[path = "../../tests/tools/glob.rs"]
mod tests;
