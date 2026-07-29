//! Context Files - project and global files that supplement or replace the
//! default system prompt.
//!
//! Conventions (in priority order, within each category):
//!
//!   * `.suspenders/SYSTEM.md` in the Project Root - **replaces** the default system
//!     prompt entirely. When absent, [`crate::voice::system_prompt`] is used.
//!   * `.suspenders/APPEND_SYSTEM.md` in the Project Root - appended verbatim to the
//!     system prompt (whether default or replaced).
//!   * `.suspenders/AGENTS.md` / `.suspenders/CLAUDE.md` - project-specific instructions.
//!     All matching files in every ancestor directory of the Project Root are
//!     loaded (root first, then parents walking up to the filesystem root),
//!     each placed under a descriptive header. Appended after any
//!     SYSTEM.md/APPEND_SYSTEM.md content.
//!   * `~/.config/suspenders/AGENTS.md` / `~/.config/suspenders/CLAUDE.md` - global context
//!     files in the user's XDG config directory, loaded last.
//!
//! Context loading is fail-open - it must never prevent the Session from
//! starting. A missing file is the normal case and stays silent; a file that
//! EXISTS but could not be used (permission denied, I/O error, invalid UTF-8)
//! is still skipped, but the skip is returned in [`ContextFiles::skipped`] so
//! the frontend can surface the degradation instead of leaving the user to
//! wonder why their SYSTEM.md had no effect.

use std::path::{Component, Path, PathBuf};

use crate::voice;

/// Where a loaded context source came from, for user-facing display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    System,
    Append,
    Context,
    Global,
}

/// Why a present context file was skipped. Only failure modes a file can
/// exhibit while existing: absence is not a reason, it is silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    PermissionDenied,
    /// The bytes are not valid UTF-8 (`read_to_string`'s `InvalidData`).
    InvalidUtf8,
    /// Any other I/O failure; carries the OS message.
    Io(String),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::PermissionDenied => write!(f, "permission denied"),
            SkipReason::InvalidUtf8 => write!(f, "invalid UTF-8"),
            SkipReason::Io(msg) => write!(f, "{msg}"),
        }
    }
}

/// A context file that exists but could not be used. [`load`] returns these
/// beside the loaded content; the fail-open policy is unchanged (the file is
/// still skipped), only the silence is lifted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
}

impl SkippedFile {
    /// The launch info line the frontend shows for this skip, phrased plainly.
    pub fn info_line(&self) -> String {
        format!(
            "context file {} exists but could not be read ({}); continuing without it",
            self.path, self.reason
        )
    }
}

/// How reading one context file went, before the fail-open policy collapses
/// it. [`read_outcome`] is the classifying seam; [`try_read`] and the loaders
/// compose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Present with non-empty content.
    Loaded(String),
    /// No file at this path, or an empty one. The normal case (most projects
    /// have no context files; an empty one is deliberately blank), silent.
    Absent,
    /// Present but unusable - the one case worth surfacing.
    Failed(SkipReason),
}

/// Result of loading context files for a Project Root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFiles {
    /// The assembled system prompt string, with all context files merged in.
    /// Never empty (falls back to the Voice default).
    pub system_prompt: String,
    /// A list of `(type, path)` pairs describing what was loaded.
    pub sources: Vec<(SourceType, String)>,
    /// Files that exist but could not be used, with the reason each was
    /// skipped. Empty for the normal project (no context files at all).
    pub skipped: Vec<SkippedFile>,
}

/// Loads context files from the Project Root and its ancestors, plus the global
/// config directory. Always succeeds: a missing file is silently normal, and a
/// present-but-unusable file is skipped and reported in `skipped`.
/// `system_prompt` falls back to the Voice default when no SYSTEM.md and no
/// context files exist.
pub fn load(root: &str) -> ContextFiles {
    let mut acc = Acc {
        system_prompt: voice::system_prompt().to_string(),
        sources: Vec::new(),
        skipped: Vec::new(),
    };

    load_system_md(root, &mut acc);
    load_append_md(root, &mut acc);
    load_project_context_files(root, &mut acc);
    load_global_context_files(&mut acc);
    append_environment_context(root, &mut acc);

    ContextFiles {
        system_prompt: acc.system_prompt,
        sources: acc.sources,
        skipped: acc.skipped,
    }
}

/// The in-progress load the section loaders thread through: the prompt being
/// assembled, the loaded sources, and the present-but-unusable skips.
struct Acc {
    system_prompt: String,
    sources: Vec<(SourceType, String)>,
    skipped: Vec<SkippedFile>,
}

// -- SYSTEM.md (replaces default) --------------------------------------------

fn load_system_md(root: &str, acc: &mut Acc) {
    let path = join(root, ".suspenders/SYSTEM.md");
    if let Some(content) = read_or_skip(&path, &mut acc.skipped) {
        acc.system_prompt = content.trim().to_string();
        // baud prepends {type, path}; the sources list is display-only and its
        // order is not asserted, but we keep baud's newest-first ordering.
        acc.sources.insert(0, (SourceType::System, path));
    }
}

// -- APPEND_SYSTEM.md (appends to whatever the system prompt is) -------------

fn load_append_md(root: &str, acc: &mut Acc) {
    let path = join(root, ".suspenders/APPEND_SYSTEM.md");
    if let Some(content) = read_or_skip(&path, &mut acc.skipped) {
        acc.system_prompt.push_str("\n\n");
        acc.system_prompt.push_str(content.trim());
        acc.sources.insert(0, (SourceType::Append, path));
    }
}

// -- AGENTS.md / CLAUDE.md in project and ancestors --------------------------

fn load_project_context_files(root: &str, acc: &mut Acc) {
    for dir in ancestor_dirs(root) {
        load_context_file(&dir, ".suspenders/AGENTS.md", SourceType::Context, acc);
        load_context_file(&dir, ".suspenders/CLAUDE.md", SourceType::Context, acc);
    }
}

// -- Global AGENTS.md / CLAUDE.md --------------------------------------------

fn load_global_context_files(acc: &mut Acc) {
    let config_dir = global_config_dir();
    load_context_file(&config_dir, "AGENTS.md", SourceType::Global, acc);
    load_context_file(&config_dir, "CLAUDE.md", SourceType::Global, acc);
}

// -- Environment grounding (appended after the resolved prompt) --------------

// The opening environment block (date, OS, cwd, folder tree, git snapshot)
// rides in AFTER SYSTEM.md/APPEND_SYSTEM.md and every context file, so whatever
// prompt resolves, the Run still starts grounded in the live project state.
// Built from live facts, never a context source, so it is not listed in
// `sources` and cannot be a `skipped` file - it degrades in place instead.
fn append_environment_context(root: &str, acc: &mut Acc) {
    let block = crate::env_context::environment_context(Path::new(&expand(root)));
    acc.system_prompt.push_str("\n\n");
    acc.system_prompt.push_str(&block);
}

// -- Helpers -----------------------------------------------------------------

fn load_context_file(dir: &str, filename: &str, ty: SourceType, acc: &mut Acc) {
    let path = join(dir, filename);
    if let Some(content) = read_or_skip(&path, &mut acc.skipped) {
        let header = context_header(ty, &path);
        acc.system_prompt
            .push_str(&format!("\n\n{header}\n{}", content.trim()));
        acc.sources.insert(0, (ty, path));
    }
}

/// The loaders' fail-open read: `Loaded` yields the content, `Absent` stays
/// silent, and `Failed` is recorded before yielding nothing - the collapse to
/// `Option` happens here, but the reason survives in `skipped`.
fn read_or_skip(path: &str, skipped: &mut Vec<SkippedFile>) -> Option<String> {
    match read_outcome(path) {
        ReadOutcome::Loaded(content) => Some(content),
        ReadOutcome::Absent => None,
        ReadOutcome::Failed(reason) => {
            skipped.push(SkippedFile {
                path: path.to_string(),
                reason,
            });
            None
        }
    }
}

fn context_header(ty: SourceType, path: &str) -> String {
    match ty {
        SourceType::Global => format!("[Global context from {path}]"),
        _ => format!("[Context from {path}]"),
    }
}

/// The list of ancestor directories from `root` up to the filesystem root,
/// inclusive. Root first, then parents.
pub fn ancestor_dirs(root: &str) -> Vec<String> {
    let expanded = expand(root);
    let mut acc: Vec<String> = Vec::new();
    let mut cur: Option<&Path> = Some(expanded.as_path());
    while let Some(dir) = cur {
        acc.push(dir.to_string_lossy().into_owned());
        cur = dir.parent();
    }
    acc
}

/// Classifies one read: loaded, absent (missing or empty), or failed with the
/// reason. Never panics. The policy split lives here: only `Failed` marks a
/// file that exists but could not be used.
pub fn read_outcome(path: &str) -> ReadOutcome {
    use std::io::ErrorKind;
    match std::fs::read_to_string(path) {
        Ok(content) if !content.is_empty() => ReadOutcome::Loaded(content),
        // An empty file is treated as absent: deliberately blank, not degraded.
        Ok(_) => ReadOutcome::Absent,
        Err(e) => match e.kind() {
            ErrorKind::NotFound => ReadOutcome::Absent,
            ErrorKind::PermissionDenied => ReadOutcome::Failed(SkipReason::PermissionDenied),
            // read_to_string reports non-UTF-8 bytes as InvalidData.
            ErrorKind::InvalidData => ReadOutcome::Failed(SkipReason::InvalidUtf8),
            _ => ReadOutcome::Failed(SkipReason::Io(e.to_string())),
        },
    }
}

/// Reads a file, returning `Some(content)` or `None` on any failure (including
/// an empty file). The plain fail-open wrapper over [`read_outcome`], for
/// callers with no skip to report. Never panics.
// qual:test_helper
pub fn try_read(path: &str) -> Option<String> {
    match read_outcome(path) {
        ReadOutcome::Loaded(content) => Some(content),
        ReadOutcome::Absent | ReadOutcome::Failed(_) => None,
    }
}

// baud joins with `Path.join`; a leading `/` on the second component would be
// treated as absolute by std's join, so we always join relative segments here.
fn join(base: &str, rel: &str) -> String {
    Path::new(base).join(rel).to_string_lossy().into_owned()
}

// Mirror Elixir's `Path.expand/1`: make relative paths absolute against the
// current working directory and normalize `.`/`..`.
fn expand(path: &str) -> PathBuf {
    let p = Path::new(path);
    let base = if p.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };
    let mut out = base;
    for comp in p.components() {
        match comp {
            Component::RootDir => {
                out = PathBuf::from("/");
            }
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(seg) => out.push(seg),
            Component::Prefix(_) => {}
        }
    }
    out
}

fn global_config_dir() -> String {
    let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.config")
    });
    join(&base, "suspenders")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_dir() -> TempDir {
        tempfile::Builder::new()
            .prefix("baud_context_files_")
            .tempdir()
            .unwrap()
    }

    fn path(dir: &TempDir, rel: &str) -> String {
        dir.path().join(rel).to_string_lossy().into_owned()
    }

    fn write(dir: &TempDir, rel: &str, content: &str) {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn root(dir: &TempDir) -> String {
        dir.path().to_string_lossy().into_owned()
    }

    // ---- load/1 with no context files ----

    #[test]
    fn returns_the_voice_default_when_no_files_exist() {
        let tmp = temp_dir();
        let result = load(&root(&tmp));

        // The resolved prompt is the Voice default, then the appended
        // environment grounding; the base leads, and no context source loaded.
        assert!(result.system_prompt.starts_with(voice::system_prompt()));
        assert!(result.system_prompt.contains("# Environment"));
        assert_eq!(result.sources, Vec::new());
    }

    // ---- SYSTEM.md ----

    #[test]
    fn replaces_the_default_system_prompt() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "You are a custom agent.");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.starts_with("You are a custom agent."));
        assert!(result.sources.iter().any(|(t, _)| *t == SourceType::System));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "\n  Be brief.  \n");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.starts_with("Be brief."));
    }

    #[test]
    fn empty_system_md_is_ignored_falls_back_to_default() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.starts_with(voice::system_prompt()));
        assert_eq!(result.sources, Vec::new());
    }

    // ---- APPEND_SYSTEM.md ----

    #[test]
    fn appends_to_the_default_system_prompt() {
        let tmp = temp_dir();
        write(
            &tmp,
            ".suspenders/APPEND_SYSTEM.md",
            "Always use strict typing.",
        );

        let result = load(&root(&tmp));
        assert!(result.system_prompt.contains(voice::system_prompt()));
        assert!(result.system_prompt.contains("Always use strict typing."));
        assert!(result.sources.iter().any(|(t, _)| *t == SourceType::Append));
    }

    #[test]
    fn appends_after_system_md_when_both_exist() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "Custom prompt.");
        write(&tmp, ".suspenders/APPEND_SYSTEM.md", "Extra instructions.");

        let result = load(&root(&tmp));
        assert!(
            result
                .system_prompt
                .starts_with("Custom prompt.\n\nExtra instructions.")
        );
    }

    // ---- AGENTS.md / CLAUDE.md ----

    #[test]
    fn loads_baud_agents_md_from_the_project_root() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/AGENTS.md", "Project conventions.");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.contains("Project conventions."));
        assert!(result.system_prompt.contains("[Context from"));
        assert!(
            result
                .sources
                .iter()
                .any(|(t, p)| *t == SourceType::Context && p.ends_with(".suspenders/AGENTS.md"))
        );
    }

    #[test]
    fn loads_baud_claude_md_from_the_project_root() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/CLAUDE.md", "Claude conventions.");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.contains("Claude conventions."));
        assert!(
            result
                .sources
                .iter()
                .any(|(t, p)| *t == SourceType::Context && p.ends_with(".suspenders/CLAUDE.md"))
        );
    }

    #[test]
    fn loads_both_agents_md_and_claude_md() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/AGENTS.md", "Project conventions.");
        write(&tmp, ".suspenders/CLAUDE.md", "Claude conventions.");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.contains("Project conventions."));
        assert!(result.system_prompt.contains("Claude conventions."));
        assert_eq!(result.sources.len(), 2);
    }

    #[test]
    fn loads_from_ancestor_directories() {
        let tmp = temp_dir();
        let child = path(&tmp, "parent/child");
        std::fs::create_dir_all(&child).unwrap();
        write(&tmp, "parent/.suspenders/AGENTS.md", "Parent conventions.");

        let result = load(&child);
        assert!(result.system_prompt.contains("Parent conventions."));
    }

    #[test]
    fn loads_from_root_and_ancestors_root_first() {
        let tmp = temp_dir();
        let child = path(&tmp, "parent/child");
        write(
            &tmp,
            "parent/child/.suspenders/AGENTS.md",
            "Child-specific rules.",
        );
        write(&tmp, "parent/.suspenders/AGENTS.md", "Parent-wide rules.");

        let result = load(&child);

        let child_start = result
            .system_prompt
            .split("Child-specific rules.")
            .next()
            .unwrap();
        let parent_start = result
            .system_prompt
            .split("Parent-wide rules.")
            .next()
            .unwrap();
        assert!(
            child_start.len() < parent_start.len(),
            "root dir content should appear before ancestor content"
        );
    }

    // ---- ancestor_dirs/1 ----

    #[test]
    fn includes_root_and_all_ancestors_up_to_slash() {
        let dirs = ancestor_dirs("/home/user/project");
        assert!(dirs.contains(&"/home/user/project".to_string()));
        assert!(dirs.contains(&"/home/user".to_string()));
        assert!(dirs.contains(&"/home".to_string()));
        assert!(dirs.contains(&"/".to_string()));
    }

    #[test]
    fn single_component() {
        let dirs = ancestor_dirs("/");
        assert_eq!(dirs, vec!["/".to_string()]);
    }

    #[test]
    fn handles_relative_paths_by_expanding() {
        let dirs = ancestor_dirs("relative");
        let expanded = expand("relative").to_string_lossy().into_owned();
        assert!(dirs.contains(&expanded));
    }

    // ---- try_read/1 ----

    #[test]
    fn returns_ok_content_for_a_readable_file() {
        let tmp = temp_dir();
        let p = path(&tmp, "test.txt");
        std::fs::write(&p, "hello").unwrap();
        assert_eq!(try_read(&p), Some("hello".to_string()));
    }

    #[test]
    fn returns_error_for_a_missing_file() {
        assert_eq!(try_read("/nonexistent/file.md"), None);
    }

    #[test]
    fn returns_error_for_an_empty_file() {
        let tmp = temp_dir();
        let p = path(&tmp, "empty.txt");
        std::fs::write(&p, "").unwrap();
        assert_eq!(try_read(&p), None);
    }

    #[test]
    fn try_read_collapses_a_failed_read_to_none() {
        let tmp = temp_dir();
        let p = path(&tmp, "binary.md");
        std::fs::write(&p, [0xFF, 0xFE, 0x00]).unwrap();
        assert_eq!(try_read(&p), None);
    }

    // ---- read_outcome/1 ----

    #[test]
    fn read_outcome_loads_a_readable_file() {
        let tmp = temp_dir();
        let p = path(&tmp, "test.txt");
        std::fs::write(&p, "hello").unwrap();
        assert_eq!(read_outcome(&p), ReadOutcome::Loaded("hello".to_string()));
    }

    #[test]
    fn read_outcome_classifies_a_missing_file_as_absent() {
        assert_eq!(read_outcome("/nonexistent/file.md"), ReadOutcome::Absent);
    }

    #[test]
    fn read_outcome_classifies_an_empty_file_as_absent() {
        let tmp = temp_dir();
        let p = path(&tmp, "empty.txt");
        std::fs::write(&p, "").unwrap();
        assert_eq!(read_outcome(&p), ReadOutcome::Absent);
    }

    #[test]
    fn read_outcome_classifies_invalid_utf8_as_failed() {
        let tmp = temp_dir();
        let p = path(&tmp, "binary.md");
        std::fs::write(&p, [0xFF, 0xFE, 0x00]).unwrap();
        assert_eq!(
            read_outcome(&p),
            ReadOutcome::Failed(SkipReason::InvalidUtf8)
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_outcome_classifies_permission_denied_as_failed() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = temp_dir();
        let p = path(&tmp, "locked.md");
        std::fs::write(&p, "secret").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();

        // Root bypasses mode bits, making the case unenforceable; skip then.
        if std::fs::read_to_string(&p).is_ok() {
            return;
        }
        assert_eq!(
            read_outcome(&p),
            ReadOutcome::Failed(SkipReason::PermissionDenied)
        );
    }

    // ---- skips reported beside the loads ----

    #[test]
    fn load_reports_a_present_but_unusable_file_beside_the_loads() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/AGENTS.md", "Project conventions.");
        let system = path(&tmp, ".suspenders/SYSTEM.md");
        std::fs::write(&system, [0xFF, 0xFE, 0x00]).unwrap();

        let result = load(&root(&tmp));

        // The unusable SYSTEM.md is skipped exactly as before (fail-open, the
        // default prompt stands), and the healthy file still loads.
        assert!(result.system_prompt.contains(voice::system_prompt()));
        assert!(result.system_prompt.contains("Project conventions."));
        assert_eq!(
            result.skipped,
            vec![SkippedFile {
                path: system,
                reason: SkipReason::InvalidUtf8,
            }]
        );
    }

    #[test]
    fn load_reports_no_skips_for_a_project_with_no_context_files() {
        let tmp = temp_dir();
        let result = load(&root(&tmp));
        assert_eq!(result.skipped, Vec::new());
    }

    #[test]
    fn skip_info_line_names_the_path_and_the_reason() {
        let skip = SkippedFile {
            path: ".suspenders/SYSTEM.md".to_string(),
            reason: SkipReason::PermissionDenied,
        };
        assert_eq!(
            skip.info_line(),
            "context file .suspenders/SYSTEM.md exists but could not be read \
             (permission denied); continuing without it"
        );
    }

    // ---- system prompt assembly order ----

    #[test]
    fn system_md_append_system_md_then_context_files() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "Custom prompt.");
        write(&tmp, ".suspenders/APPEND_SYSTEM.md", "Appendix.");
        write(&tmp, ".suspenders/AGENTS.md", "Context.");

        let result = load(&root(&tmp));

        let first_line = result.system_prompt.split('\n').next().unwrap();
        assert_eq!(first_line, "Custom prompt.");
        assert!(result.system_prompt.contains("Appendix."));
        assert!(result.system_prompt.contains("[Context from"));
    }

    #[test]
    fn default_prompt_when_no_files_exist() {
        let tmp = temp_dir();
        let result = load(&root(&tmp));
        assert!(result.system_prompt.starts_with(voice::system_prompt()));
    }

    // ---- environment grounding is appended last ----

    #[test]
    fn appends_the_environment_block_after_the_resolved_prompt() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "Custom prompt.");
        write(&tmp, ".suspenders/AGENTS.md", "Project conventions.");

        let result = load(&root(&tmp));

        // The base and context files lead; the grounding block trails them,
        // carrying the cwd and OS so the Run starts grounded.
        let env_at = result.system_prompt.find("# Environment").unwrap();
        let ctx_at = result.system_prompt.find("Project conventions.").unwrap();
        assert!(ctx_at < env_at, "context files precede the env block");
        assert!(result.system_prompt.contains("My operating system is:"));
        assert!(
            result
                .system_prompt
                .contains("I'm currently working in the directory:")
        );
    }
}
