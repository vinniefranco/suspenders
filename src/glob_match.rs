//! Shared shell-glob to regex translation, the single source of truth for the
//! two read-only search tools that filter walked file paths by a glob: `glob`
//! (its match pattern) and `grep` (its `glob` filter, qwen's `rg --glob`).
//! Living as a crate leaf keeps the translation - and its ripgrep/gitignore
//! depth semantics - in one place, so the two tools cannot drift.
//!
//! [`compile`] translates the glob to a CASE-INSENSITIVE anchored path regex
//! (qwen glob's `nocase: true`; grep's `--ignore-case`) and compiles it,
//! reusing the `regex` dep both tools already pull. A glob that produces an
//! invalid regex is bounced back as a tool error, not a panic.
//!
//! DEPTH SEMANTICS (ripgrep/gitignore `--glob`): a pattern with NO `/` matches
//! its basename at ANY depth, so `*.rs` finds `src/nested/a.rs`, not only a
//! top-level `a.rs`. A pattern that DOES contain a `/` is anchored to the search
//! root, so `src/*.rs` stays within `src/`. This is what `rg --glob` and the
//! gitignore rule ("a slash-free pattern matches at any level") do.

use regex::{Regex, RegexBuilder};

/// Translate a shell glob to a CASE-INSENSITIVE anchored path regex and compile
/// it. Returns a tool error message (not a panic) for a glob that does not
/// translate to a valid regex (e.g. an unclosed `[`). The case-insensitive
/// entry point for glob (`nocase: true`) and grep (`--ignore-case`); ls's
/// `ignore` param takes the case-SENSITIVE [`compile_case_sensitive`] instead.
pub(crate) fn compile(pattern: &str) -> Result<Regex, String> {
    compile_with(pattern, true)
}

/// Translate a shell glob to a CASE-SENSITIVE anchored path regex and compile
/// it. The `list_directory` `ignore` param uses this: qwen's `shouldIgnore`
/// (ls.ts:104-108) builds `new RegExp("^" + pat + "$")` with NO `i` flag, so
/// `ignore: ["*.LOG"]` does NOT drop `run.log`. Same translation as [`compile`],
/// only the case-fold differs.
pub(crate) fn compile_case_sensitive(pattern: &str) -> Result<Regex, String> {
    compile_with(pattern, false)
}

// Shared body: translate the glob and compile it with the given case-fold. The
// two public entry points differ ONLY in `case_insensitive`, so the glob->regex
// translation cannot drift between them.
fn compile_with(pattern: &str, case_insensitive: bool) -> Result<Regex, String> {
    let regex_src = glob_to_regex(pattern);
    RegexBuilder::new(&regex_src)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|_| format!("invalid glob pattern: {pattern:?}"))
}

// Glob -> regex over the whole relative path (matched with `/` separators):
//   **      any characters including `/` (crosses directories)
//   *       any characters except `/` (stays within a segment)
//   ?       any single character except `/`
//   [set]   a character class, passed through to the regex verbatim
// Everything else is a literal (regex-escaped). Anchored at both ends. A
// slash-free pattern is treated as if `**/`-prefixed, so it matches the
// basename at ANY depth (ripgrep/gitignore semantics); a pattern that already
// contains a `/` is anchored to the search root as written. An unclosed `[`
// passes through and makes the regex fail to compile, which `compile` reports
// as an invalid pattern rather than a panic.
fn glob_to_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    // A slash-free glob matches its basename at any depth: prefix the same
    // `(?:.*/)?` that a leading `**/` produces, so `*.rs` also matches
    // `src/nested/a.rs`.
    if !pattern.contains('/') {
        out.push_str("(?:.*/)?");
    }
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

#[cfg(test)]
#[path = "../tests/glob_match.rs"]
mod tests;
