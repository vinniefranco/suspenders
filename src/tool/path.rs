//! Path confinement and file-error wording for Suspenders tools.
//!
//! Two std-only concerns live here, split out of the authoring contract:
//!
//! * **Path confinement.** [`with_path`] resolves a model-supplied path against
//!   the Session's Project Root (via [`resolve_path`]) and keeps it inside that
//!   root; a path that climbs out is refused before the tool ever touches the
//!   filesystem.
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

/// Resolves a model-supplied path against the ctx's Project Root and runs
/// `fun` with the absolute path; a path that escapes the root returns the
/// confinement error instead.
pub fn with_path<F>(path: &str, ctx: &ToolCtx, fun: F) -> Result<String, String>
where
    F: FnOnce(&Path) -> Result<String, String>,
{
    let abs = resolve_path(path, &ctx.root)?;
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
/// that escape it. A cheap guard, not a sandbox.
pub fn resolve_path(path: &str, root: &Path) -> Result<PathBuf, String> {
    let root = expand(root);
    let expanded = expand_against(path, &root);

    if expanded == root || expanded.starts_with(join_sep(&root)) {
        Ok(expanded)
    } else {
        Err("path escapes project root".to_string())
    }
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
            ToolCtx {
                root: self.path.clone(),
                result_cap: 4000,
                command_timeout_ms: 120_000,
                scout: None,
            }
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
