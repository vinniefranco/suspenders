//! `list_files(path, ignore?, file_filtering_options?)`: a faithful port of qwen
//! v0.16.0 `tools/ls.ts` (`LSTool`, ToolNames.LS = `list_directory`). Lists the
//! direct children of ONE directory (a flat `readdir`, not a recursive walk),
//! under a verbatim `Listed N item(s) in {path}:\n---\n` header, directories
//! prefixed with `[DIR] `, sorted directories-first then alphabetically, capped
//! at [`MAX_ENTRY_COUNT`] entries with qwen's verbatim truncation trailer.
//!
//! PATH is REQUIRED and ABSOLUTE (qwen `validateToolParams`: "Path must be
//! absolute"). It is resolved through the shared absolute-required path seam
//! ([`resolve_absolute_in`]) and confined to the Project Root (or the trusted
//! managed-memory subtree). A relative path is refused with qwen's verbatim
//! absolute-path message; an absolute path that escapes the root is refused with
//! the project-wide confinement message.
//!
//! FILTERING mirrors qwen's `filterFilesWithReport` (ls.ts:206-217) over the
//! direct children: each child is dropped if it matches `.gitignore`
//! (via [`crate::walk::gitignore`], qwen's default `respectGitIgnore` = true) or
//! `.qwenignore` (via [`crate::walk::qwenignore`], qwen's default
//! `respectQwenIgnore` = true), both anchored to the Project Root. The
//! `file_filtering_options` object toggles each; the dropped counts are appended
//! as qwen's `(N git-ignored, M qwen-ignored)` suffix. The `ignore` param drops
//! any child whose BASENAME matches a provided glob (ls.ts `shouldIgnore`),
//! translated through the shared [`crate::glob_match::compile`].
//!
//! CONFINEMENT is a deliberate tightening of qwen: qwen's ls lists external
//! directories behind an ask-permission (`getDefaultPermission` returns `'ask'`,
//! ls.ts:132-147); suspenders has no ask-permission seam, so an out-of-root
//! absolute path is hard-refused, matching the project-wide confinement
//! decision (like glob/grep).

use crate::glob_match;
use crate::tool::path::{PathReject, resolve_absolute_in, unescape_and_trim};
use crate::tool::{Tool, ToolCtx, ToolSpec};
use crate::walk::{gitignore, qwenignore};
use regex::Regex;
use serde_json::{Value, json};
use std::path::Path;

pub struct ListFiles;

/// The maximum number of entries listed (qwen `MAX_ENTRY_COUNT`, ls.ts:29).
/// qwen's effective limit is `min(MAX_ENTRY_COUNT, getTruncateToolOutputLines())`
/// (ls.ts:251-254); its default `truncateToolOutputLines` is 1000, so the
/// 100-entry cap is what binds, which is what we apply here.
const MAX_ENTRY_COUNT: usize = 100;

/// The verbatim tool description (qwen ls.ts:315).
const DESCRIPTION: &str = "Lists the names of files and subdirectories directly within a specified directory path. Can optionally ignore entries matching provided glob patterns.";

/// The verbatim `path` property description (qwen ls.ts:320-321).
const PATH_DESCRIPTION: &str =
    "The absolute path to the directory to list (must be absolute, not relative)";

/// The verbatim `ignore` property description (qwen ls.ts:325).
const IGNORE_DESCRIPTION: &str = "List of glob patterns to ignore";

/// The verbatim `file_filtering_options` property description (qwen ls.ts:332-333).
const FILE_FILTERING_OPTIONS_DESCRIPTION: &str =
    "Optional: Whether to respect ignore patterns from .gitignore or .qwenignore";

/// The verbatim `respect_git_ignore` property description (qwen ls.ts:337-338).
const RESPECT_GIT_IGNORE_DESCRIPTION: &str = "Optional: Whether to respect .gitignore patterns when listing files. Only available in git repositories. Defaults to true.";

/// The verbatim `respect_qwen_ignore` property description (qwen ls.ts:342-343).
const RESPECT_QWEN_IGNORE_DESCRIPTION: &str =
    "Optional: Whether to respect .qwenignore patterns when listing files. Defaults to true.";

/// A surviving child of the listed directory: its basename and whether it is a
/// directory (for the `[DIR] ` prefix and the directories-first sort). The
/// ignore checks run before an entry is built, so the absolute path is not
/// carried past that point.
struct Entry {
    name: String,
    is_dir: bool,
}

#[async_trait::async_trait]
impl Tool for ListFiles {
    // Read-only (qwen ls.ts:316 `Kind.Search` for list_directory): allowed in
    // plan mode. qwen kinds `list_directory` as Search, not Read.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Search
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_directory".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "description": PATH_DESCRIPTION,
                        "type": "string"
                    },
                    "ignore": {
                        "description": IGNORE_DESCRIPTION,
                        "items": {
                            "type": "string"
                        },
                        "type": "array"
                    },
                    "file_filtering_options": {
                        "description": FILE_FILTERING_OPTIONS_DESCRIPTION,
                        "type": "object",
                        "properties": {
                            "respect_git_ignore": {
                                "description": RESPECT_GIT_IGNORE_DESCRIPTION,
                                "type": "boolean"
                            },
                            "respect_qwen_ignore": {
                                "description": RESPECT_QWEN_IGNORE_DESCRIPTION,
                                "type": "boolean"
                            }
                        }
                    }
                },
                "required": ["path"],
                "type": "object"
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // path is REQUIRED and must be a string (qwen ls.ts:360-369).
        let raw_path = match input.get("path") {
            Some(Value::String(s)) => s.as_str(),
            _ => return Err("path must be a string".to_string()),
        };
        // qwen's `unescapePath(params.path.trim())` (ls.ts:363), applied BEFORE
        // the absolute check and BEFORE the header echoes the path: trim, then
        // strip shell-escaping backslashes. The trimmed/unescaped form is what is
        // validated and displayed.
        let path = unescape_and_trim(raw_path);
        // ignore is optional; a non-array (or non-string element) is an error.
        let ignore = match input.get("ignore") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut patterns = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(s) => patterns.push(s.to_string()),
                        None => return Err("ignore must be an array of strings".to_string()),
                    }
                }
                patterns
            }
            Some(_) => return Err("ignore must be an array of strings".to_string()),
        };
        let filtering = FilteringOptions::from_input(input)?;

        // Compile the ignore globs to basename matchers (qwen `shouldIgnore`),
        // reusing the shared glob->regex translation rather than a new engine.
        // CASE-SENSITIVE: qwen's `shouldIgnore` regex carries no `i` flag
        // (ls.ts:104-108), unlike glob/grep's nocase match, so `*.LOG` does not
        // drop `run.log`.
        let ignore_matchers: Vec<Regex> = ignore
            .iter()
            .map(|p| glob_match::compile_case_sensitive(p))
            .collect::<Result<_, _>>()?;

        // qwen REQUIRES an absolute path and confines it to the workspace. A
        // relative path is refused with qwen's verbatim message; an escaping
        // absolute path with the project-wide confinement message.
        let abs = resolve_absolute_in(&path, &ctx.root, ctx.memory_root.as_deref()).map_err(
            |reject| match reject {
                // VERBATIM qwen ls.ts:366.
                PathReject::Relative => format!("Path must be absolute: {path}"),
                PathReject::Escapes => "path escapes project root".to_string(),
            },
        )?;

        list(&abs, &path, &ctx.root, &ignore_matchers, &filtering)
    }
}

/// The resolved `file_filtering_options` for one listing: whether to honor
/// `.gitignore` and `.qwenignore`. Both default to `true`
/// (qwen `DEFAULT_FILE_FILTERING_OPTIONS`, constants.ts).
struct FilteringOptions {
    respect_git_ignore: bool,
    respect_qwen_ignore: bool,
}

impl FilteringOptions {
    // Read the nested `file_filtering_options` object, defaulting each flag to
    // true (qwen's `?? DEFAULT_FILE_FILTERING_OPTIONS.*`). A non-object options
    // value, or a non-boolean flag, is an error.
    fn from_input(input: &Value) -> Result<Self, String> {
        let opts = match input.get("file_filtering_options") {
            None | Some(Value::Null) => {
                return Ok(FilteringOptions {
                    respect_git_ignore: true,
                    respect_qwen_ignore: true,
                });
            }
            Some(Value::Object(map)) => map,
            Some(_) => return Err("file_filtering_options must be an object".to_string()),
        };
        Ok(FilteringOptions {
            respect_git_ignore: read_bool(opts.get("respect_git_ignore"), "respect_git_ignore")?,
            respect_qwen_ignore: read_bool(opts.get("respect_qwen_ignore"), "respect_qwen_ignore")?,
        })
    }
}

// One `file_filtering_options` flag: absent/null defaults to true, a bool is
// taken as-is, anything else is an error.
fn read_bool(value: Option<&Value>, field: &str) -> Result<bool, String> {
    match value {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{field} must be a boolean")),
    }
}

// List the direct children of `abs`, applying the ignore filters and rendering
// qwen's output. `display_path` is the model-supplied path echoed in the header
// (qwen echoes `this.params.path` verbatim); `root` anchors the git/qwen ignore
// checks.
fn list(
    abs: &Path,
    display_path: &str,
    root: &Path,
    ignore_matchers: &[Regex],
    filtering: &FilteringOptions,
) -> Result<String, String> {
    let meta = match std::fs::metadata(abs) {
        // VERBATIM qwen ls.ts:177 (llmContent, leading "Error: " and all).
        Err(_) => {
            return Err(format!(
                "Error: Directory not found or inaccessible: {display_path}"
            ));
        }
        Ok(m) => m,
    };
    if !meta.is_dir() {
        // VERBATIM qwen ls.ts:184 (llmContent).
        return Err(format!("Error: Path is not a directory: {display_path}"));
    }

    let read = match std::fs::read_dir(abs) {
        Err(e) => return Err(format!("Error listing directory: {e}")),
        Ok(rd) => rd,
    };
    let names: Vec<String> = read
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();

    if names.is_empty() {
        // VERBATIM qwen ls.ts:194.
        return Ok(format!("Directory {display_path} is empty."));
    }

    // Filter the direct children through .gitignore / .qwenignore (qwen's
    // filterFilesWithReport), then the ignore globs (qwen's shouldIgnore), then
    // stat the survivors into entries.
    let mut git_ignored = 0usize;
    let mut qwen_ignored = 0usize;
    let mut entries: Vec<Entry> = Vec::new();

    for name in names {
        let full = abs.join(&name);
        let is_dir = full.is_dir();

        if filtering.respect_git_ignore && gitignore::is_ignored(root, &full, is_dir) {
            git_ignored += 1;
            continue;
        }
        if filtering.respect_qwen_ignore && qwenignore::is_ignored(root, &full) {
            qwen_ignored += 1;
            continue;
        }
        if ignore_matchers.iter().any(|m| m.is_match(&name)) {
            continue;
        }
        entries.push(Entry { name, is_dir });
    }

    Ok(render(entries, display_path, git_ignored, qwen_ignored))
}

// Sort (directories first, then alphabetical by name), cap at MAX_ENTRY_COUNT,
// and render qwen's header + `[DIR] ` lines + truncation trailer + ignored-count
// suffix (ls.ts:243-280).
fn render(
    mut entries: Vec<Entry>,
    display_path: &str,
    git_ignored: usize,
    qwen_ignored: usize,
) -> String {
    // Directories first, then alphabetical by name (qwen ls.ts:244-248).
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });

    let total = entries.len();
    let truncated = total > MAX_ENTRY_COUNT;
    let shown = if truncated {
        &entries[..MAX_ENTRY_COUNT]
    } else {
        &entries[..]
    };

    let body = shown
        .iter()
        .map(|e| {
            if e.is_dir {
                format!("[DIR] {}", e.name)
            } else {
                e.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    // VERBATIM qwen header (ls.ts:263).
    let mut message = format!("Listed {total} item(s) in {display_path}:\n---\n{body}");

    if truncated {
        // VERBATIM qwen truncation trailer (ls.ts:266-268).
        let omitted = total - MAX_ENTRY_COUNT;
        let term = if omitted == 1 { "item" } else { "items" };
        message.push_str(&format!("\n---\n[{omitted} {term} truncated] ..."));
    }

    // The ignored-count suffix (ls.ts:271-280): only the non-zero counts, joined.
    let mut ignored_messages: Vec<String> = Vec::new();
    if git_ignored > 0 {
        ignored_messages.push(format!("{git_ignored} git-ignored"));
    }
    if qwen_ignored > 0 {
        ignored_messages.push(format!("{qwen_ignored} qwen-ignored"));
    }
    if !ignored_messages.is_empty() {
        message.push_str(&format!("\n\n({})", ignored_messages.join(", ")));
    }

    message
}

#[cfg(test)]
#[path = "../../tests/tools/list_files.rs"]
mod tests;
