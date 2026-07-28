//! The authoring contract Suspenders tools share.
//!
//! A tool exposes a spec (Anthropic tool format; `input_schema` is a JSON
//! Schema map) and a run function that gets the decoded input plus the
//! [`ToolCtx`] carrying the Session's Project Root, the Result Cap (applied by
//! the Tools dispatch, not by the tools), and the command timeout. The ctx is
//! built by the Session; nothing in a tool reads the cwd or config.
//!
//! ## The authoring contract
//!
//! * **Input arrives validated.** [`validate`] runs against the tool's schema
//!   before every call: required fields present, string-typed fields are
//!   strings, unknown fields rejected.
//! * **Model-supplied paths go through [`with_path`]**, which confines them to
//!   the Project Root.
//! * **Failed file operations are worded by [`file_error`]**, which formats the
//!   POSIX reason and appends closest-match suggestions on ENOENT.
//! * **Errors return, never raise.**
//! * **Size is not a tool concern** - `tools::shaping` cuts every result.

use crate::scout::ScoutFn;
use std::path::{Path, PathBuf};

/// A tool's spec in Anthropic tool format: a name, a description, and a JSON
/// Schema `input_schema` (an open edge, so it stays a `serde_json::Value`).
/// Mirrors baud's `Baud.Tool.spec/0` shape. Serializes to exactly its wire
/// shape, so the Conversation's tool-spec overhead estimate counts what a
/// request carries without reaching into an adapter.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// The authoring contract a Suspenders tool implements (baud's `Baud.Tool`
/// behaviour). `spec` is the Anthropic tool format; `run` gets the decoded
/// input (an open edge - a `serde_json::Value`) plus the [`ToolCtx`].
///
/// `run` is async so the object-safe registry can hold `Box<dyn Tool>` (via
/// `async-trait`); most tools implement a sync heuristic core and make `run` a
/// thin wrapper. Errors return (`Err`), never raise - `Tools::execute` maps an
/// `Err` to an `is_error` Tool Result.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    async fn run(&self, input: &serde_json::Value, ctx: &ToolCtx) -> Result<String, String>;
}

/// The ctx every Tool Call executes with: the Session's Project Root, the
/// Result Cap, the command timeout, and the optional `scout` capture (read
/// only by explore, which dispatches a Scout).
#[derive(Clone)]
pub struct ToolCtx {
    pub root: PathBuf,
    pub result_cap: usize,
    pub command_timeout_ms: u64,
    /// The `scout` capture: an effect wired to the Session that dispatches a
    /// Scout. `None` on the extension-free path (tests, direct callers) - explore
    /// then returns a graceful "no Scout wired" error.
    pub scout: Option<ScoutFn>,
}

impl std::fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCtx")
            .field("root", &self.root)
            .field("result_cap", &self.result_cap)
            .field("command_timeout_ms", &self.command_timeout_ms)
            .field("scout", &self.scout.as_ref().map(|_| "<scout>"))
            .finish()
    }
}

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

/// Validates the model-supplied input against a tool's JSON Schema. Returns
/// `Ok(())` or `Err(reason)` with a precise description (unknown fields,
/// missing required fields, wrong types).
pub fn validate(
    input_schema: &serde_json::Value,
    input: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let required: Vec<String> = input_schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let empty = serde_json::Map::new();
    let properties = input_schema
        .get("properties")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);

    let known: Vec<&String> = properties.keys().collect();

    // 1. Unknown fields first.
    let unknown: Vec<&String> = input.keys().filter(|k| !known.contains(k)).collect();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown field(s): {}. Valid fields: {}",
            quote_join(unknown.iter().map(|s| s.as_str())),
            quote_join(known.iter().map(|s| s.as_str())),
        ));
    }

    // 2. Missing required fields.
    let missing: Vec<&String> = required
        .iter()
        .filter(|r| !input.contains_key(*r))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required field(s): {}. Required: {}",
            quote_join(missing.iter().map(|s| s.as_str())),
            quote_join(required.iter().map(|s| s.as_str())),
        ));
    }

    // 3. Type mismatches (string fields that aren't strings).
    let type_errors: Vec<String> = properties
        .iter()
        .filter(|(name, prop)| {
            input.contains_key(*name)
                && prop.get("type").and_then(|t| t.as_str()) == Some("string")
                && !input.get(*name).map(|v| v.is_string()).unwrap_or(false)
        })
        .filter_map(|(name, _)| {
            // `contains_key` above guarantees `get` returns `Some`; use
            // `filter_map` to propagate that without `unwrap`.
            let val = input.get(name)?;
            Some(format!(
                "field {:?} should be a string, got: {} ({})",
                name,
                inspect(val),
                type_name(val)
            ))
        })
        .collect();

    if type_errors.is_empty() {
        Ok(())
    } else {
        Err(type_errors.join("; "))
    }
}

fn quote_join<'a, I: Iterator<Item = &'a str>>(items: I) -> String {
    items
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn type_name(value: &serde_json::Value) -> &'static str {
    use serde_json::Value;
    match value {
        Value::String(_) => "string",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "float",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "map",
        Value::Array(_) => "list",
        Value::Null => "atom",
    }
}

fn inspect(value: &serde_json::Value) -> String {
    value.to_string()
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

    // ---- validate/2 + type_name ----

    use serde_json::json;

    #[test]
    fn type_name_calls_a_string_a_string() {
        assert_eq!(type_name(&json!("hi")), "string");
    }

    #[test]
    fn type_name_calls_signed_and_unsigned_integers_integer() {
        // Both sides of the `is_i64() || is_u64()` guard: a negative fits only
        // i64, u64::MAX fits only u64 - each alone must still read "integer".
        assert_eq!(type_name(&json!(-3)), "integer");
        let unsigned = serde_json::Value::Number(serde_json::Number::from(u64::MAX));
        assert_eq!(type_name(&unsigned), "integer");
    }

    #[test]
    fn type_name_calls_a_fractional_number_a_float() {
        assert_eq!(type_name(&json!(1.5)), "float");
    }

    #[test]
    fn type_name_calls_a_bool_a_boolean() {
        assert_eq!(type_name(&json!(true)), "boolean");
    }

    #[test]
    fn type_name_calls_an_object_a_map() {
        // Elixir vocabulary on purpose (baud heritage): an object is a "map".
        assert_eq!(type_name(&json!({"a": 1})), "map");
    }

    #[test]
    fn type_name_calls_an_array_a_list() {
        assert_eq!(type_name(&json!([1, 2])), "list");
    }

    #[test]
    fn type_name_calls_null_an_atom() {
        // baud reported `nil` - an atom - so the wording survives the port.
        assert_eq!(type_name(&json!(null)), "atom");
    }

    #[test]
    fn validate_reports_the_offending_value_with_its_type_name() {
        // The malformed-input message the model sees: the value inspected,
        // then its type name in parentheses.
        let schema = json!({
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        });
        let mut input = serde_json::Map::new();
        input.insert("path".to_string(), json!(42));
        assert_eq!(
            validate(&schema, &input),
            Err("field \"path\" should be a string, got: 42 (integer)".to_string())
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
