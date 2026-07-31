//! Path confinement and file-error wording for Suspenders tools.
//!
//! Two std-only concerns live here, split out of the authoring contract:
//!
//! * **Path confinement.** [`with_path`] resolves a model-supplied path against
//!   the Session's Project Root - OR the trusted managed-auto-memory subtree the
//!   ctx carries (P5, ADR-0062) - via [`resolve_path_in`], and keeps it inside
//!   one of those two normalized subtrees; a path that climbs out of both is
//!   refused before the tool ever touches the filesystem. The memory allowance
//!   is narrow: a resolved subtree with the same `..`-collapse + trailing-SEP
//!   containment as the root check (so a `<memory_root>-evil` sibling is
//!   refused), NOT a general escape.
//! * **File-error wording.** [`file_error`] formats a failed file operation's
//!   POSIX reason ([`FileError`] / [`format_posix`], baud's
//!   `:file.format_error/1`), appending closest-match suggestions on ENOENT
//!   ([`suggest_files`] / `closest_matches` / [`jaro_distance`]).

use std::path::{Path, PathBuf};

use crate::tool::ToolCtx;

/// A POSIX-ish file-operation failure reason, so `file_error` can format it as
/// baud's `:file.format_error/1` does. Mirrors the reasons the tools surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileError {
    Enoent,
    Eacces,
    Eisdir,
    Enotdir,
    Eexist,
    Other(&'static str),
}

impl FileError {
    fn atom(self) -> &'static str {
        match self {
            FileError::Enoent => "enoent",
            FileError::Eacces => "eacces",
            FileError::Eisdir => "eisdir",
            FileError::Enotdir => "enotdir",
            FileError::Eexist => "eexist",
            FileError::Other(a) => a,
        }
    }

    fn description(self) -> &'static str {
        match self {
            FileError::Enoent => "no such file or directory",
            FileError::Eacces => "permission denied",
            FileError::Eisdir => "illegal operation on a directory",
            FileError::Enotdir => "not a directory",
            FileError::Eexist => "file already exists",
            FileError::Other(_) => "unknown error",
        }
    }

    /// Maps a `std::io::Error` to a `FileError`, so tools built on `std::fs`
    /// get the same narrative as baud's POSIX reasons.
    pub fn from_io(err: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        match err.kind() {
            ErrorKind::NotFound => FileError::Enoent,
            ErrorKind::PermissionDenied => FileError::Eacces,
            ErrorKind::AlreadyExists => FileError::Eexist,
            _ => FileError::Other("eio"),
        }
    }
}

/// Resolves a model-supplied path against the ctx's Project Root (or the
/// trusted memory subtree, P5/ADR-0062) and runs `fun` with the absolute path;
/// a path that escapes both returns the confinement error instead.
pub fn with_path<F>(path: &str, ctx: &ToolCtx, fun: F) -> Result<String, String>
where
    F: FnOnce(&Path) -> Result<String, String>,
{
    let abs = resolve_path_in(path, &ctx.root, ctx.memory_root.as_deref())?;
    fun(&abs)
}

/// The shared narrative for a failed file operation on a model-supplied path:
/// `"could not <verb> <path>: <posix reason>"`, with closest-match file
/// suggestions appended when the path did not exist (ENOENT).
pub fn file_error(verb: &str, path: &str, reason: FileError) -> String {
    let hint = if reason == FileError::Enoent {
        suggest_files(path)
    } else {
        String::new()
    };
    format!("could not {verb} {path}: {}{hint}", format_posix(reason))
}

/// Maximum number of closest-match file suggestions shown on ENOENT.
const SUGGEST_FILES_LIMIT: usize = 5;

/// Enriches an ENOENT error message with a suggestion of files in the same
/// directory that closely match the model's path (Jaro-ranked recovery).
pub fn suggest_files(path: &str) -> String {
    let p = Path::new(path);
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
    let dir = match dir {
        Some(d) => d,
        None => Path::new("."),
    };
    let basename = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        Err(_) => return String::new(),
    };

    let suggestions = closest_matches(&basename, &entries, SUGGEST_FILES_LIMIT);
    if suggestions.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nFiles in {}/:\n  {}",
            dir.display(),
            suggestions.join("\n  ")
        )
    }
}

/// Minimum Jaro distance for a file to appear in the closest-match suggestions.
const JARO_SUGGEST_THRESHOLD: f64 = 0.4;

fn closest_matches(needle: &str, haystack: &[String], limit: usize) -> Vec<String> {
    if haystack.is_empty() {
        return Vec::new();
    }
    // Sort by Jaro distance descending (stable), take limit, keep only above
    // the threshold.
    let mut scored: Vec<(f64, &String)> = haystack
        .iter()
        .map(|name| (jaro_distance(needle, name), name))
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(limit)
        .filter(|(d, _)| *d > JARO_SUGGEST_THRESHOLD)
        .map(|(_, name)| name.clone())
        .collect()
}

/// The three-component denominator in the Jaro formula.
const JARO_COMPONENT_COUNT: f64 = 3.0;
/// Transpositions are counted in halves in the Jaro formula.
const JARO_TRANSPOSITION_DIVISOR: f64 = 2.0;
/// Jaro distance for two identical (or both empty) strings.
const JARO_PERFECT_MATCH: f64 = 1.0;
/// Jaro distance when one string is empty or there are no matching characters.
const JARO_NO_MATCH: f64 = 0.0;

/// Jaro distance (0.0..=1.0), matching Elixir's `String.jaro_distance/2`.
pub fn jaro_distance(s1: &str, s2: &str) -> f64 {
    let a: Vec<char> = s1.chars().collect();
    let b: Vec<char> = s2.chars().collect();
    let (len1, len2) = (a.len(), b.len());

    if len1 == 0 && len2 == 0 {
        return JARO_PERFECT_MATCH;
    }
    if len1 == 0 || len2 == 0 {
        return JARO_NO_MATCH;
    }

    let match_distance = (len1.max(len2) / 2).saturating_sub(1);

    let mut a_matches = vec![false; len1];
    let mut b_matches = vec![false; len2];
    let mut matches = 0usize;

    for i in 0..len1 {
        let start = i.saturating_sub(match_distance);
        let end = (i + match_distance + 1).min(len2);
        for j in start..end {
            if !b_matches[j] && a[i] == b[j] {
                a_matches[i] = true;
                b_matches[j] = true;
                matches += 1;
                break;
            }
        }
    }

    if matches == 0 {
        return JARO_NO_MATCH;
    }

    // Count transpositions.
    let mut transpositions = 0usize;
    let mut k = 0usize;
    for i in 0..len1 {
        if a_matches[i] {
            while !b_matches[k] {
                k += 1;
            }
            if a[i] != b[k] {
                transpositions += 1;
            }
            k += 1;
        }
    }
    let t = transpositions as f64 / JARO_TRANSPOSITION_DIVISOR;
    let m = matches as f64;

    (m / len1 as f64 + m / len2 as f64 + (m - t) / m) / JARO_COMPONENT_COUNT
}

/// Resolves a model-supplied path against the Project Root and refuses paths
/// that escape it. A cheap guard, not a sandbox. The Project-Root-only form;
/// [`resolve_path_in`] adds the trusted-memory allowance.
pub fn resolve_path(path: &str, root: &Path) -> Result<PathBuf, String> {
    resolve_path_in(path, root, None)
}

/// Resolves a model-supplied path against the Project Root OR, when `memory`
/// is `Some`, the trusted managed-auto-memory subtree (P5, ADR-0062). A path
/// that lands in EITHER normalized subtree is accepted; one that escapes both
/// is refused.
///
/// SECURITY-CRITICAL: both boundaries use the SAME normalized-containment
/// discipline (lexical `..`-collapse, then `== boundary` OR
/// `starts_with(boundary <> SEP)`). The trailing separator is what refuses a
/// `<memory_root>-evil` sibling: it shares the string prefix but not the
/// path-component boundary, exactly the `isAutoMemPath` shape. The memory root
/// is normalized here too, so a `..` inside the configured root cannot widen
/// the allowance.
///
/// DEFENSE-IN-DEPTH: a memory boundary that normalizes to the filesystem root
/// (or fewer than [`MIN_MEMORY_BOUNDARY_COMPONENTS`] path components) is IGNORED,
/// treated as no memory allowance. `default_memory_root` always yields a
/// `<base>/projects/<slug>/memory` shape (>= 3 components), so this is
/// unreachable through the documented interface; but the memory boundary is now
/// security-load-bearing, and a `/`-rooted boundary would otherwise widen
/// confinement to the ENTIRE filesystem. Cheaper to guard than to trust every
/// future caller.
pub fn resolve_path_in(path: &str, root: &Path, memory: Option<&Path>) -> Result<PathBuf, String> {
    let root = expand(root);
    let expanded = expand_against(path, &root);

    if contained(&expanded, &root) {
        return Ok(expanded);
    }
    if let Some(mem) = memory {
        let mem = expand(mem);
        if is_usable_memory_boundary(&mem) && contained(&expanded, &mem) {
            return Ok(expanded);
        }
    }
    Err("path escapes project root".to_string())
}

/// The minimum normal-component count a memory boundary must have to widen
/// confinement. `<base>/projects/<slug>/memory` clears this comfortably; a bare
/// `/` or `""` does not, so it is treated as no allowance rather than opening
/// the whole filesystem.
const MIN_MEMORY_BOUNDARY_COMPONENTS: usize = 2;

// Whether a (already-normalized) memory boundary is safe to widen confinement
// to: it must NOT be the filesystem root and must carry at least
// `MIN_MEMORY_BOUNDARY_COMPONENTS` normal path segments. A `/` or `""` boundary
// fails this and is ignored (no memory allowance).
fn is_usable_memory_boundary(mem: &Path) -> bool {
    use std::path::Component;
    let normal_components = mem
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    normal_components >= MIN_MEMORY_BOUNDARY_COMPONENTS
}

// Normalized containment: `abs` IS `boundary` or sits strictly inside it. The
// `starts_with(boundary <> SEP)` half is what refuses a sibling that merely
// shares the string prefix (`<boundary>-evil`).
fn contained(abs: &Path, boundary: &Path) -> bool {
    abs == boundary || abs.starts_with(join_sep(boundary))
}

// Path.expand semantics: absolute normalization without touching the
// filesystem. `.` and `..` components are resolved lexically.
fn expand(p: &Path) -> PathBuf {
    normalize(p)
}

fn expand_against(path: &str, root: &Path) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&root.join(p))
    }
}

// Lexical normalization: collapse `.` and `..` without hitting the FS, so a
// model path that climbs out of the root is detected regardless of what exists.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(comp),
            },
            other => out.push(other),
        }
    }
    out.iter().collect()
}

// Root with a trailing separator, for the `starts_with(root <> "/")` check.
fn join_sep(root: &Path) -> PathBuf {
    let mut s = root.as_os_str().to_os_string();
    s.push(std::path::MAIN_SEPARATOR.to_string());
    PathBuf::from(s)
}

/// Formats a POSIX error as `"enoent (no such file or directory)"`.
pub fn format_posix(reason: FileError) -> String {
    format!("{} ({})", reason.atom(), reason.description())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TmpDir {
        path: PathBuf,
    }

    impl TmpDir {
        fn new() -> Self {
            let mut path = std::env::temp_dir();
            let unique = format!(
                "suspenders_tool_test_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            path.push(unique);
            fs::create_dir_all(&path).unwrap();
            TmpDir { path }
        }

        fn ctx(&self) -> ToolCtx {
            ToolCtx::for_test(self.path.clone(), 4000)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    // ---- with_path/3 ----

    #[test]
    fn with_path_resolves_relative_path_against_root() {
        let tmp = TmpDir::new();
        let ctx = tmp.ctx();
        let expected = tmp.path.join("sub/file.txt");

        let result = with_path("sub/file.txt", &ctx, |abs| {
            assert_eq!(abs, expected.as_path());
            Ok("resolved".to_string())
        });

        assert_eq!(result, Ok("resolved".to_string()));
    }

    #[test]
    fn with_path_refuses_escaping_path_without_calling_fun() {
        let tmp = TmpDir::new();
        let ctx = tmp.ctx();

        let result = with_path("../../etc/passwd", &ctx, |_abs| {
            panic!("must not run");
        });

        assert_eq!(result, Err("path escapes project root".to_string()));
    }

    #[test]
    fn with_path_non_string_is_not_applicable_but_resolve_rejects_escape() {
        // The Elixir "non-string path" case is enforced by resolve_path's
        // guard clause; in Rust the type system enforces `&str`, so the
        // remaining behavioral guarantee is the escape refusal above. A
        // resolve of a plainly-escaping path stands in for the structured error.
        let tmp = TmpDir::new();
        assert_eq!(
            resolve_path("/etc/passwd", &tmp.path),
            Err("path escapes project root".to_string())
        );
    }

    // ---- resolve_path_in: the trusted-memory allowance (P5, ADR-0062) ----

    #[test]
    fn without_a_memory_root_out_of_root_is_refused_unchanged() {
        // The Project-Root-only behavior is preserved when there is no memory.
        let root = Path::new("/proj");
        assert_eq!(
            resolve_path_in("/etc/passwd", root, None),
            Err("path escapes project root".to_string())
        );
    }

    #[test]
    fn a_path_inside_the_memory_root_is_accepted() {
        let root = Path::new("/proj");
        let mem = Path::new("/data/projects/slug/memory");
        // An absolute path landing inside the trusted memory subtree resolves,
        // even though it is far outside the Project Root.
        assert_eq!(
            resolve_path_in("/data/projects/slug/memory/MEMORY.md", root, Some(mem)),
            Ok(PathBuf::from("/data/projects/slug/memory/MEMORY.md"))
        );
        // The memory root itself is inside.
        assert_eq!(
            resolve_path_in("/data/projects/slug/memory", root, Some(mem)),
            Ok(PathBuf::from("/data/projects/slug/memory"))
        );
    }

    #[test]
    fn a_sibling_sharing_the_memory_string_prefix_is_still_refused() {
        // SECURITY: `<memory_root>-evil` shares the string prefix but not the
        // path-component boundary - the trailing-separator check refuses it.
        let root = Path::new("/proj");
        let mem = Path::new("/data/projects/slug/memory");
        assert_eq!(
            resolve_path_in("/data/projects/slug/memory-evil/x.md", root, Some(mem)),
            Err("path escapes project root".to_string())
        );
    }

    #[test]
    fn a_filesystem_root_memory_boundary_does_not_widen_confinement() {
        // DEFENSE-IN-DEPTH: a memory boundary that normalized to `/` (or too
        // few components) is IGNORED - it must not open the whole filesystem.
        // `/etc/passwd` is outside the Project Root and would be "contained" by a
        // `/` boundary, so the guard is the only thing refusing it here.
        let root = Path::new("/proj");
        let fs_root = Path::new("/");
        assert_eq!(
            resolve_path_in("/etc/passwd", root, Some(fs_root)),
            Err("path escapes project root".to_string())
        );
        // A one-component boundary is below the minimum too.
        let shallow = Path::new("/data");
        assert_eq!(
            resolve_path_in("/etc/passwd", root, Some(shallow)),
            Err("path escapes project root".to_string())
        );
    }

    #[test]
    fn a_dotdot_escape_out_of_the_memory_root_is_refused() {
        let root = Path::new("/proj");
        let mem = Path::new("/data/projects/slug/memory");
        // Climbing out of the memory subtree lands nowhere trusted.
        assert_eq!(
            resolve_path_in("/data/projects/slug/memory/../secret", root, Some(mem)),
            Err("path escapes project root".to_string())
        );
    }

    // ---- file_error/3 ----

    #[test]
    fn file_error_formats_the_posix_reason() {
        assert_eq!(
            file_error("write", "a.txt", FileError::Eacces),
            "could not write a.txt: eacces (permission denied)"
        );
    }

    #[test]
    fn file_error_appends_closest_match_suggestions_on_enoent() {
        let tmp = TmpDir::new();
        fs::write(tmp.path.join("config.exs"), "").unwrap();
        let missing = tmp.path.join("confg.exs");
        let missing = missing.to_string_lossy().into_owned();

        let message = file_error("read", &missing, FileError::Enoent);
        assert!(message.contains(&format!("could not read {missing}: enoent")));
        assert!(message.contains("config.exs"));
    }
}
