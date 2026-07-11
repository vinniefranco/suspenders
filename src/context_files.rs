//! Context Files — project and global files that supplement or replace the
//! default system prompt.
//!
//! Conventions (in priority order, within each category):
//!
//!   * `.suspenders/SYSTEM.md` in the Project Root — **replaces** the default system
//!     prompt entirely. When absent, [`crate::voice::system_prompt`] is used.
//!   * `.suspenders/APPEND_SYSTEM.md` in the Project Root — appended verbatim to the
//!     system prompt (whether default or replaced).
//!   * `.suspenders/AGENTS.md` / `.suspenders/CLAUDE.md` — project-specific instructions.
//!     All matching files in every ancestor directory of the Project Root are
//!     loaded (root first, then parents walking up to the filesystem root),
//!     each placed under a descriptive header. Appended after any
//!     SYSTEM.md/APPEND_SYSTEM.md content.
//!   * `~/.config/suspenders/AGENTS.md` / `~/.config/suspenders/CLAUDE.md` — global context
//!     files in the user's XDG config directory, loaded last.
//!
//! Missing or unreadable files are silently skipped — context loading must
//! never prevent the Session from starting.

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

/// Result of loading context files for a Project Root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFiles {
    /// The assembled system prompt string, with all context files merged in.
    /// Never empty (falls back to the Voice default).
    pub system_prompt: String,
    /// A list of `(type, path)` pairs describing what was loaded.
    pub sources: Vec<(SourceType, String)>,
}

/// Loads context files from the Project Root and its ancestors, plus the global
/// config directory. Always succeeds: missing/unreadable files are silently
/// skipped. `system_prompt` falls back to the Voice default when no SYSTEM.md
/// and no context files exist.
pub fn load(root: &str) -> ContextFiles {
    let mut sources: Vec<(SourceType, String)> = Vec::new();
    let mut system_prompt = voice::system_prompt().to_string();

    load_system_md(root, &mut system_prompt, &mut sources);
    load_append_md(root, &mut system_prompt, &mut sources);
    load_project_context_files(root, &mut system_prompt, &mut sources);
    load_global_context_files(&mut system_prompt, &mut sources);

    ContextFiles {
        system_prompt,
        sources,
    }
}

// -- SYSTEM.md (replaces default) --------------------------------------------

fn load_system_md(root: &str, prompt: &mut String, sources: &mut Vec<(SourceType, String)>) {
    let path = join(root, ".suspenders/SYSTEM.md");
    if let Some(content) = try_read(&path) {
        *prompt = content.trim().to_string();
        // baud prepends {type, path}; the sources list is display-only and its
        // order is not asserted, but we keep baud's newest-first ordering.
        sources.insert(0, (SourceType::System, path));
    }
}

// -- APPEND_SYSTEM.md (appends to whatever the system prompt is) -------------

fn load_append_md(root: &str, prompt: &mut String, sources: &mut Vec<(SourceType, String)>) {
    let path = join(root, ".suspenders/APPEND_SYSTEM.md");
    if let Some(content) = try_read(&path) {
        prompt.push_str("\n\n");
        prompt.push_str(content.trim());
        sources.insert(0, (SourceType::Append, path));
    }
}

// -- AGENTS.md / CLAUDE.md in project and ancestors --------------------------

fn load_project_context_files(
    root: &str,
    prompt: &mut String,
    sources: &mut Vec<(SourceType, String)>,
) {
    for dir in ancestor_dirs(root) {
        load_context_file(&dir, ".suspenders/AGENTS.md", SourceType::Context, prompt, sources);
        load_context_file(&dir, ".suspenders/CLAUDE.md", SourceType::Context, prompt, sources);
    }
}

// -- Global AGENTS.md / CLAUDE.md --------------------------------------------

fn load_global_context_files(prompt: &mut String, sources: &mut Vec<(SourceType, String)>) {
    let config_dir = global_config_dir();
    load_context_file(&config_dir, "AGENTS.md", SourceType::Global, prompt, sources);
    load_context_file(&config_dir, "CLAUDE.md", SourceType::Global, prompt, sources);
}

// -- Helpers -----------------------------------------------------------------

fn load_context_file(
    dir: &str,
    filename: &str,
    ty: SourceType,
    prompt: &mut String,
    sources: &mut Vec<(SourceType, String)>,
) {
    let path = join(dir, filename);
    if let Some(content) = try_read(&path) {
        let header = context_header(ty, &path);
        prompt.push_str(&format!("\n\n{header}\n{}", content.trim()));
        sources.insert(0, (ty, path));
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

/// Reads a file, returning `Some(content)` or `None` on any failure (including
/// an empty file). Never panics.
pub fn try_read(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) if !content.is_empty() => Some(content),
        _ => None,
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

        assert_eq!(result.system_prompt, voice::system_prompt());
        assert_eq!(result.sources, Vec::new());
    }

    // ---- SYSTEM.md ----

    #[test]
    fn replaces_the_default_system_prompt() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "You are a custom agent.");

        let result = load(&root(&tmp));
        assert_eq!(result.system_prompt, "You are a custom agent.");
        assert!(result
            .sources
            .iter()
            .any(|(t, _)| *t == SourceType::System));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "\n  Be brief.  \n");

        let result = load(&root(&tmp));
        assert_eq!(result.system_prompt, "Be brief.");
    }

    #[test]
    fn empty_system_md_is_ignored_falls_back_to_default() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "");

        let result = load(&root(&tmp));
        assert_eq!(result.system_prompt, voice::system_prompt());
        assert_eq!(result.sources, Vec::new());
    }

    // ---- APPEND_SYSTEM.md ----

    #[test]
    fn appends_to_the_default_system_prompt() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/APPEND_SYSTEM.md", "Always use strict typing.");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.contains(voice::system_prompt()));
        assert!(result.system_prompt.contains("Always use strict typing."));
        assert!(result
            .sources
            .iter()
            .any(|(t, _)| *t == SourceType::Append));
    }

    #[test]
    fn appends_after_system_md_when_both_exist() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/SYSTEM.md", "Custom prompt.");
        write(&tmp, ".suspenders/APPEND_SYSTEM.md", "Extra instructions.");

        let result = load(&root(&tmp));
        assert_eq!(result.system_prompt, "Custom prompt.\n\nExtra instructions.");
    }

    // ---- AGENTS.md / CLAUDE.md ----

    #[test]
    fn loads_baud_agents_md_from_the_project_root() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/AGENTS.md", "Project conventions.");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.contains("Project conventions."));
        assert!(result.system_prompt.contains("[Context from"));
        assert!(result.sources.iter().any(|(t, p)| *t == SourceType::Context
            && p.ends_with(".suspenders/AGENTS.md")));
    }

    #[test]
    fn loads_baud_claude_md_from_the_project_root() {
        let tmp = temp_dir();
        write(&tmp, ".suspenders/CLAUDE.md", "Claude conventions.");

        let result = load(&root(&tmp));
        assert!(result.system_prompt.contains("Claude conventions."));
        assert!(result.sources.iter().any(|(t, p)| *t == SourceType::Context
            && p.ends_with(".suspenders/CLAUDE.md")));
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
        write(&tmp, "parent/child/.suspenders/AGENTS.md", "Child-specific rules.");
        write(&tmp, "parent/.suspenders/AGENTS.md", "Parent-wide rules.");

        let result = load(&child);

        let child_start = result.system_prompt.split("Child-specific rules.").next().unwrap();
        let parent_start = result.system_prompt.split("Parent-wide rules.").next().unwrap();
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
        assert_eq!(result.system_prompt, voice::system_prompt());
    }
}
