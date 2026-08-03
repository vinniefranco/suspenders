//! The `awk` program safety classifier - a faithful port of the awk half of qwen
//! v0.21.4's `shell-safety-rules.ts`. A `print`/`printf` redirecting to a literal
//! file (`awk '{print > "f"}'`) WRITES; `system(...)`/`close(...)`/`getline` or an
//! ambiguous redirect is unknown; a bare `print` is read-only.
//! [`classify_awk_command_safety`] is the entry the command dispatch
//! ([`super::super::commands`]) calls.
//!
//! qwen's `AWK_STATIC_WRITE` uses a negative-lookahead regex the `regex` crate
//! cannot express; [`is_awk_static_write`] is a hand scan with identical semantics.

use super::Safety;
use regex::Regex;
use std::sync::LazyLock;

// shell-safety-rules.ts:23 - `system(...)`/`close(...)`/`getline` make awk unknown.
static AWK_UNKNOWN_OPERATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:system|close)\s*\(|getline\b").unwrap());
// shell-safety-rules.ts:24 - a `print`/`printf` anywhere in the script.
static AWK_PRINT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(?:print|printf)\b").unwrap());

// --- awk ----------------------------------------------------------------------

/// The scan result of splitting an awk program into statements
/// (shell-safety-rules.ts:212 `splitAwkStatements`).
struct AwkSplit {
    statements: Vec<String>,
    ambiguous_slash: bool,
    unsupported_at: bool,
}

/// The lexer context while scanning an awk program: whether the cursor is inside a
/// `"string"`, a `/regex/`, or has just seen a `\` escape. Consuming a char inside
/// one of these contexts is handled by [`AwkContext::consume_in_context`], keeping
/// the statement-splitting loop free of that quoting detail.
#[derive(Default)]
struct AwkContext {
    escaped: bool,
    in_string: bool,
    in_regex: bool,
}

impl AwkContext {
    /// If `ch` is consumed by the current string/regex/escape context, update the
    /// context and return `true` (the caller advances without treating `ch` as a
    /// separator). Returns `false` when the cursor is in plain code.
    fn consume_in_context(&mut self, ch: char) -> bool {
        if self.escaped {
            self.escaped = false;
            return true;
        }
        if (self.in_string || self.in_regex) && ch == '\\' {
            self.escaped = true;
            return true;
        }
        if self.in_string {
            self.in_string = ch != '"';
            return true;
        }
        if self.in_regex {
            self.in_regex = ch != '/';
            return true;
        }
        false
    }

    /// In plain code, a `"` opens a string and a `/` opens a regex when it is in
    /// operator position (start or after `previous_significant` in the set). Returns
    /// `true` if `ch` opened a context (the caller advances).
    fn maybe_open(&mut self, ch: char, previous_significant: char) -> bool {
        if ch == '"' {
            self.in_string = true;
            return true;
        }
        if ch == '/'
            && (previous_significant == '\0' || "({[=,:;!~?&|".contains(previous_significant))
        {
            self.in_regex = true;
            return true;
        }
        false
    }
}

/// shell-safety-rules.ts:212 `splitAwkStatements` - a character scanner that peels
/// awk statements apart on `;`/`{`/`}`/newline while tracking string/regex/comment
/// context (via [`AwkContext`]), and flags `@` (unsupported) and bare `/`
/// (ambiguous division).
fn split_awk_statements(script: &str) -> AwkSplit {
    let chars: Vec<char> = script.chars().collect();
    let mut statements: Vec<String> = Vec::new();
    let mut ambiguous_slash = false;
    let mut unsupported_at = false;
    let mut start = 0usize;
    let mut ctx = AwkContext::default();
    let mut previous_significant: char = '\0';

    let slice = |a: usize, b: usize| -> String { chars[a..b].iter().collect() };

    // When a `#` comment runs to end-of-input, the trailing text is a comment and
    // is NOT emitted as a statement (qwen returns early). This flag suppresses the
    // post-loop trailing push so both exits build `AwkSplit` at one site.
    let mut push_trailing = true;
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ctx.consume_in_context(ch) || ctx.maybe_open(ch, previous_significant) {
            i += 1;
            continue;
        }
        if ch == '/' {
            ambiguous_slash = true;
        }
        if ch == '@' {
            unsupported_at = true;
        }
        if ch == '#' {
            statements.push(slice(start, i));
            // `script.indexOf('\n', i + 1)`
            let newline = chars[i + 1..]
                .iter()
                .position(|c| *c == '\n')
                .map(|p| i + 1 + p);
            match newline {
                None => {
                    push_trailing = false;
                    break;
                }
                Some(nl) => {
                    start = nl + 1;
                    i = nl + 1;
                    previous_significant = '\n';
                    continue;
                }
            }
        }
        if matches!(ch, ';' | '{' | '}' | '\n') {
            statements.push(slice(start, i));
            start = i + 1;
            previous_significant = ch;
            i += 1;
            continue;
        }
        if !ch.is_whitespace() {
            previous_significant = ch;
        }
        i += 1;
    }
    if push_trailing {
        statements.push(slice(start, chars.len()));
    }
    AwkSplit {
        statements,
        ambiguous_slash,
        unsupported_at,
    }
}

/// The body of a `print`/`printf` statement (the text after the keyword) when the
/// statement is a bare print form (keyword at a word boundary, NOT a `print(...)`
/// function call), else `None`. Mirrors the `\bprint(f)?\b(?!\s*\()` head of qwen's
/// `AWK_STATIC_WRITE` regex (shell-safety-rules.ts:22).
fn awk_print_body(statement: &str) -> Option<&str> {
    let s = statement.trim_start();
    let rest = s
        .strip_prefix("printf")
        .or_else(|| s.strip_prefix("print"))?;
    // `\b` after print/printf: next char must not be a word char.
    if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    // `(?!\s*\()` - not a function-call form `print(...)`.
    if rest.trim_start().starts_with('(') {
        return None;
    }
    Some(rest)
}

/// Scan the args part `(?:"…"|[^">|])*` and the redirect `>>?`, returning the byte
/// index in `body` just past the redirect operator, or `None` if the args do not
/// terminate in a `>`/`>>` (shell-safety-rules.ts:22 middle). A string literal is
/// consumed whole (so a `>` inside it is not a redirect).
fn awk_scan_to_redirect(body: &str) -> Option<usize> {
    let b = body.as_bytes();
    let mut i = 0usize;
    loop {
        if b.get(i) == Some(&b'"') {
            i = awk_skip_string(b, i)?;
        } else if matches!(b.get(i), Some(&c) if c != b'"' && c != b'>' && c != b'|') {
            i += 1;
        } else {
            break;
        }
    }
    if b.get(i) != Some(&b'>') {
        return None;
    }
    i += 1;
    if b.get(i) == Some(&b'>') {
        i += 1;
    }
    Some(i)
}

/// Skip a double-quoted string starting at `start` (a `"`), returning the index
/// just past the closing quote, or `None` if unterminated. Backslash escapes the
/// next byte.
fn awk_skip_string(b: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < b.len() {
        if b[j] == b'\\' && j + 1 < b.len() {
            j += 2;
        } else if b[j] == b'"' {
            return Some(j + 1);
        } else {
            j += 1;
        }
    }
    None
}

/// Whether `tail` is exactly a quoted filename then trailing whitespace to end
/// (`\s*"[^"]*"\s*$`, shell-safety-rules.ts:22 tail).
fn awk_tail_is_quoted_filename(tail: &str) -> bool {
    let tail = tail.trim_start();
    let Some(rest) = tail.strip_prefix('"') else {
        return false;
    };
    match rest.split_once('"') {
        Some((_name, after)) => after.trim().is_empty(),
        None => false,
    }
}

/// shell-safety-rules.ts:22 `AWK_STATIC_WRITE` - a `print`/`printf` (not a function
/// call) redirecting to a `"literal"` file. Rust regex lacks the lookahead the JS
/// pattern uses, so this is three hand scans with identical semantics.
fn is_awk_static_write(statement: &str) -> bool {
    let Some(body) = awk_print_body(statement) else {
        return false;
    };
    let Some(after_redirect) = awk_scan_to_redirect(body) else {
        return false;
    };
    awk_tail_is_quoted_filename(&body[after_redirect..])
}

/// shell-safety-rules.ts:278 `classifyAwkScriptSafety`.
fn classify_awk_script_safety(script: &str) -> Safety {
    let split = split_awk_statements(script);
    if !split.ambiguous_slash && split.statements.iter().any(|s| is_awk_static_write(s)) {
        return Safety::Write;
    }
    if split.unsupported_at || AWK_UNKNOWN_OPERATION.is_match(script) {
        return Safety::Unknown;
    }
    if AWK_PRINT.is_match(script) && script.contains(['>', '|']) {
        Safety::Unknown
    } else {
        Safety::ReadOnly
    }
}

/// shell-safety-rules.ts:292 `classifyAwkCommandSafety`.
pub fn classify_awk_command_safety(args: &[String]) -> Safety {
    let mut program_index: isize = -1;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            program_index = (i + 1) as isize;
            break;
        }
        if arg == "-F" || arg == "-v" {
            if args.get(i + 1).is_none() {
                return Safety::Unknown;
            }
            i += 1;
            i += 1;
            continue;
        }
        // `/^-[Fv].+/`
        if arg.len() > 2 && (arg.starts_with("-F") || arg.starts_with("-v")) {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Safety::Unknown;
        }
        program_index = i as isize;
        break;
    }
    if program_index < 0 {
        return Safety::ReadOnly;
    }
    let Some(program) = args.get(program_index as usize) else {
        return Safety::Unknown;
    };
    let result = classify_awk_script_safety(program);
    if result != Safety::ReadOnly {
        return result;
    }
    if classify_awk_script_safety(&args.join(" ")) == Safety::ReadOnly {
        Safety::ReadOnly
    } else {
        Safety::Unknown
    }
}
