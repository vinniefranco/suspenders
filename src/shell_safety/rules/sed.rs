//! The `sed` script safety classifier - a faithful port of the sed half of qwen
//! v0.21.4's `shell-safety-rules.ts`. `sed -i`, `sed 's/a/b/w file'`, and a
//! `w`/`W` command all WRITE; the rest is read-only or, on any parse uncertainty,
//! unknown. [`classify_sed_command_safety`] is the entry the command dispatch
//! ([`super::super::commands`]) calls; the sed SCRIPT scanner
//! ([`classify_sed_script_safety`]) is the reusable inner sed-script scanner.
//!
//! Ported line-for-line from shell-safety-rules.ts; the JS regexes are recreated
//! with the `regex` crate (no backtracking/lookaround needed).

use super::Safety;
use regex::Regex;
use std::sync::LazyLock;

// shell-safety-rules.ts:10 - the leading sed ADDRESS (line number / `$` / `/re/`),
// matched at the START of a single sed command. Ported verbatim.
static SED_ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:(?:\d+|\$)(?:\s*,\s*(?:\d+|\$))?|/(?:\\[\s\S]|[^/\\])*/)?\s*").unwrap()
});
// shell-safety-rules.ts:14 - a single safe sed command letter.
static SAFE_SED_COMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[dDgGhHlnNpPqQxz=]$").unwrap());
// shell-safety-rules.ts:15 - substitution flags that keep it read-only.
static SAFE_SUBSTITUTION_FLAGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9gIpM]*$").unwrap());
// shell-safety-rules.ts:16 - sed options that keep it read-only.
static SAFE_SED_OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:-[nElrsuz]|--(?:quiet|silent|line-length(?:=.*)?))$").unwrap()
});

// shell-safety-rules.ts:18 - value-bearing sed options (their argument is consumed).
const SED_VALUE_OPTIONS: &[&str] = &["-f", "--file", "-e", "--expression", "-l", "--line-length"];

// --- generic helper regexes used at call sites below ---
static HELP_OR_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?i:--help|--version)$").unwrap());

/// shell-safety-rules.ts:26 `scanDelimitedSection` - walk from `start` to the
/// unescaped `delimiter`, returning the index PAST it, or -1 (here `None`).
fn scan_delimited_section(script: &[u8], start: usize, delimiter: u8) -> Option<usize> {
    let mut escaped = false;
    let mut i = start;
    while i < script.len() {
        let ch = script[i];
        if escaped {
            escaped = false;
        } else if ch == b'\\' {
            escaped = true;
        } else if ch == delimiter {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// shell-safety-rules.ts:45 `classifySingleSedCommandSafety`.
fn classify_single_sed_command_safety(script: &str) -> Safety {
    // `/(?:^|[^\\])[ewr]\s/` - a bare e/w/r command with a following space is a
    // GNU-vs-POSIX compatibility hazard -> unknown.
    let compatibility_unknown = compat_unknown(script);
    let bytes = script.as_bytes();
    let command_offset = SED_ADDRESS.find(script).map(|m| m.end()).unwrap_or(0);
    if command_offset == bytes.len() {
        return Safety::ReadOnly;
    }
    let command = bytes[command_offset];
    if command == b'w' || command == b'W' {
        return if script[command_offset + 1..].trim().is_empty() {
            Safety::Unknown
        } else {
            Safety::Write
        };
    }
    if matches!(command, b'e' | b'E' | b'r' | b'R') {
        return Safety::Unknown;
    }
    if command == b's' {
        let delimiter = match bytes.get(command_offset + 1).copied() {
            None => return Safety::Unknown,
            Some(d) if d == b'\\' || (d as char).is_whitespace() => return Safety::Unknown,
            Some(d) => d,
        };
        let Some(replacement_start) = scan_delimited_section(bytes, command_offset + 2, delimiter)
        else {
            return Safety::Unknown;
        };
        let Some(flags_start) = scan_delimited_section(bytes, replacement_start, delimiter) else {
            return Safety::Unknown;
        };
        let flags = script[flags_start..].trim();
        if flags.contains([';', '\n', '{', '}']) {
            return Safety::Unknown;
        }
        if let Some(write_flag) = flags.find('w') {
            return if flags[write_flag + 1..].trim().is_empty() {
                Safety::Unknown
            } else {
                Safety::Write
            };
        }
        if flags.contains(['e', 'E', 'r', 'R', 'w', 'W']) {
            return Safety::Unknown;
        }
        if !SAFE_SUBSTITUTION_FLAGS.is_match(flags) {
            return Safety::Unknown;
        }
        return if compatibility_unknown {
            Safety::Unknown
        } else {
            Safety::ReadOnly
        };
    }
    if script[command_offset + 1..].contains([';', '\n', '{', '}']) {
        return Safety::Unknown;
    }
    if !SAFE_SED_COMMAND.is_match(&script[command_offset..=command_offset]) {
        return Safety::Unknown;
    }
    if compatibility_unknown {
        Safety::Unknown
    } else {
        Safety::ReadOnly
    }
}

/// `/(?:^|[^\\])[ewr]\s/` - is there an UNESCAPED e/w/r immediately followed by a
/// space? (The JS regex; Rust regex has no lookbehind, so it is scanned by hand.)
fn compat_unknown(text: &str) -> bool {
    let b = text.as_bytes();
    for i in 0..b.len() {
        if matches!(b[i], b'e' | b'w' | b'r') {
            let prev_ok = i == 0 || b[i - 1] != b'\\';
            let next_space = b.get(i + 1).map(|c| (*c as char).is_whitespace()) == Some(true);
            if prev_ok && next_space {
                return true;
            }
        }
    }
    false
}

/// shell-safety-rules.ts:87 `nextSedSeparator`.
fn next_sed_separator(script: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < script.len() {
        if script[i] == b';' || script[i] == b'\n' {
            return i;
        }
        i += 1;
    }
    script.len()
}

/// shell-safety-rules.ts:94 `classifySedScriptSafety` - a whole sed program (may
/// contain several `;`/newline-separated commands).
fn classify_sed_script_safety(script: &str) -> Safety {
    let bytes = script.as_bytes();
    let mut result = Safety::ReadOnly;
    let mut start = 0usize;
    while start < bytes.len() {
        // SED_ADDRESS_AT: the sticky (`y`) form of SED_ADDRESS, matched AT `start`.
        // Rust regex has no sticky flag; anchor to the slice and add `start`.
        let Some(m) = SED_ADDRESS.find(&script[start..]) else {
            return Safety::Unknown;
        };
        let command_offset = start + m.end();
        if command_offset == bytes.len() {
            return result;
        }
        let command = bytes[command_offset];

        if command == b'w' || command == b'W' {
            let writer = classify_single_sed_command_safety(&script[start..]);
            return if writer == Safety::Write {
                Safety::Write
            } else {
                Safety::Unknown
            };
        }
        if matches!(command, b'e' | b'E' | b'r' | b'R') {
            return Safety::Unknown;
        }
        if command != b's' && !SAFE_SED_COMMAND.is_match(&script[command_offset..=command_offset]) {
            return Safety::Unknown;
        }

        let separator = if command == b's' {
            let delimiter = match bytes.get(command_offset + 1).copied() {
                None => return Safety::Unknown,
                Some(d) if d == b'\\' || (d as char).is_whitespace() => return Safety::Unknown,
                Some(d) => d,
            };
            let Some(replacement_start) =
                scan_delimited_section(bytes, command_offset + 2, delimiter)
            else {
                return Safety::Unknown;
            };
            let Some(flags_start) = scan_delimited_section(bytes, replacement_start, delimiter)
            else {
                return Safety::Unknown;
            };
            next_sed_separator(bytes, flags_start)
        } else {
            next_sed_separator(bytes, command_offset + 1)
        };

        let current = classify_single_sed_command_safety(&script[start..separator]);
        if current == Safety::Write {
            return Safety::Write;
        }
        if current == Safety::Unknown {
            result = Safety::Unknown;
        }
        if separator == bytes.len() {
            return result;
        }
        start = separator + 1;
    }
    result
}

/// A `--help`/`--version` in sed's pre-`--` options that is not a value-option's
/// value (shell-safety-rules.ts:148) -> unknown.
fn sed_has_help(args: &[String]) -> bool {
    let terminator = args.iter().position(|a| a == "--");
    let options = &args[..terminator.unwrap_or(args.len())];
    options.iter().enumerate().any(|(index, arg)| {
        HELP_OR_VERSION.is_match(arg)
            && !(index > 0 && SED_VALUE_OPTIONS.contains(&options[index - 1].as_str()))
    })
}

/// Whether sed writes in place (`-i`/`-I`/`--in-place`), skipping value-option
/// values and `-e<script>` bundles (shell-safety-rules.ts:157).
fn sed_writes_in_place(args: &[String]) -> bool {
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            break;
        }
        if SED_VALUE_OPTIONS.contains(&arg.as_str()) {
            i += 1;
        } else if regex_e_script(arg) {
            // `/^-[nErsuz]*e.+/` - `-e` with an inline script, skip.
        } else if in_place(arg) {
            return true;
        }
        i += 1;
    }
    false
}

/// The sed SCRIPTS (positional or `-e`/`--expression`) with their arg indices, or
/// `Err(Unknown)` for a malformed option (shell-safety-rules.ts:165). Split out so
/// [`classify_sed_command_safety`] stays a three-phase pipeline.
/// The outcome of scanning one sed argument at index `i` (see [`sed_scan_arg`]):
/// what to record and how far to advance.
enum SedStep {
    /// Nothing recorded (a plain option); advance one.
    Skip,
    /// A malformed option: the whole command is [`Safety::Unknown`].
    Malformed,
    /// The `--` terminator: optionally record a following script, then STOP.
    Terminate { script: Option<(String, usize)> },
    /// Record `(script, index)` and advance by `consumed` tokens (1 for a
    /// positional / `-e<script>`, 2 for `-e VALUE`).
    Script {
        script: String,
        index: usize,
        consumed: usize,
    },
}

/// Classify one sed argument at index `i` into a [`SedStep`] (the per-token body of
/// [`collect_sed_scripts`], shell-safety-rules.ts:167). `have_script` says whether a
/// script has already been collected (a bare positional is a script only if none
/// has been). `length_value_missing` is the `-l`/`--line-length` value-consume:
/// when the flag has no following value the command is malformed.
fn sed_scan_arg(args: &[String], i: usize, have_script: bool) -> SedStep {
    let arg = &args[i];
    if arg == "--" {
        return SedStep::Terminate {
            script: (!have_script)
                .then(|| args.get(i + 1))
                .flatten()
                .map(|s| (s.clone(), i + 1)),
        };
    }
    // `-l`/`--line-length` consumes its value; a missing value is malformed. The
    // flag itself then falls through to the checks below (accepted by SAFE_SED_OPTION).
    let value_index = if line_length_flag(arg) { i + 1 } else { i };
    if line_length_flag(arg) && args.get(i + 1).is_none() {
        return SedStep::Malformed;
    }
    // Each arm yields the SCRIPT it found at `(text, index)` (or a `SedStep` for the
    // non-script outcomes), and the single `SedStep::Script` is built once at the
    // end - so the reader sees the one field that differs (the script index).
    let found: (String, usize) = if file_flag(arg) {
        return SedStep::Malformed;
    } else if arg == "-e" || arg == "--expression" {
        match args.get(value_index + 1) {
            Some(s) if !s.starts_with('-') => (s.clone(), value_index + 1),
            _ => return SedStep::Malformed,
        }
    } else if let Some(script) = inline_expression(arg) {
        if script.starts_with('-') {
            return SedStep::Malformed;
        }
        (script.to_string(), i)
    } else if long_non_line_length(arg) || (arg.starts_with('-') && !SAFE_SED_OPTION.is_match(arg))
    {
        // qwen's two `else if`s (a long option that is not --line-length; an
        // unrecognized short option) both -> unknown.
        return SedStep::Malformed;
    } else if !arg.starts_with('-') && !have_script {
        (arg.clone(), i)
    } else {
        return SedStep::Skip;
    };
    let (script, index) = found;
    SedStep::Script {
        script,
        consumed: index + 1 - i,
        index,
    }
}

/// The sed SCRIPTS (positional or `-e`/`--expression`) with their arg indices, or
/// `Err(Unknown)` for a malformed option (shell-safety-rules.ts:165). A thin driver
/// over [`sed_scan_arg`].
fn collect_sed_scripts(args: &[String]) -> Result<(Vec<String>, Vec<usize>), Safety> {
    let mut scripts: Vec<String> = Vec::new();
    let mut indices: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        match sed_scan_arg(args, i, !scripts.is_empty()) {
            SedStep::Malformed => return Err(Safety::Unknown),
            SedStep::Terminate { script } => {
                match script {
                    Some((s, _)) if s.starts_with('-') => return Err(Safety::Unknown),
                    Some((s, idx)) => {
                        scripts.push(s);
                        indices.push(idx);
                    }
                    // `--` with nothing after, when no script yet, is malformed.
                    None if scripts.is_empty() && args.get(i + 1).is_none() => {
                        return Err(Safety::Unknown);
                    }
                    None => {}
                }
                break;
            }
            SedStep::Script {
                script,
                index,
                consumed,
            } => {
                scripts.push(script);
                indices.push(index);
                i += consumed;
            }
            SedStep::Skip => i += 1,
        }
    }
    Ok((scripts, indices))
}

/// shell-safety-rules.ts:145 `classifySedCommandSafety` - classify a full `sed`
/// argument list as three phases: a help query -> unknown; an in-place write ->
/// write; else classify each collected script and apply the trailing-arg
/// compatibility check (shell-safety-rules.ts:200).
pub fn classify_sed_command_safety(args: &[String]) -> Safety {
    if sed_has_help(args) {
        return Safety::Unknown;
    }
    if sed_writes_in_place(args) {
        return Safety::Write;
    }
    let (scripts, script_arguments) = match collect_sed_scripts(args) {
        Ok(collected) => collected,
        Err(safety) => return safety,
    };

    let mut result = Safety::ReadOnly;
    for script in &scripts {
        match classify_sed_script_safety(script) {
            Safety::Write => return Safety::Write,
            Safety::Unknown => result = Safety::Unknown,
            Safety::ReadOnly => {}
        }
    }
    let remaining: Vec<&str> = args
        .iter()
        .enumerate()
        .filter(|(idx, _)| !script_arguments.contains(idx))
        .map(|(_, a)| a.as_str())
        .collect();
    if compat_unknown(&remaining.join(" ")) {
        Safety::Unknown
    } else {
        result
    }
}

// small sed-flag predicates recreating the JS inline regexes ---------------------
fn regex_e_script(arg: &str) -> bool {
    // `/^-[nErsuz]*e.+/`
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-[nErsuz]*e.+").unwrap());
    R.is_match(arg)
}
fn in_place(arg: &str) -> bool {
    // `/^(?:-[nErsuz]*[iI]|--in-place(?:=|$))/`
    static R: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:-[nErsuz]*[iI]|--in-place(?:=|$))").unwrap());
    R.is_match(arg)
}
fn line_length_flag(arg: &str) -> bool {
    // `/^(?:-l|--line-length)$/`
    arg == "-l" || arg == "--line-length"
}
fn file_flag(arg: &str) -> bool {
    // `/^(?:-f|--file(?:=|$))/`
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:-f|--file(?:=|$))").unwrap());
    R.is_match(arg)
}
fn inline_expression(arg: &str) -> Option<&str> {
    // `/^(?:-e.+|--expression=)/` -> slice at 2 (`-e`) or 13 (`--expression=`).
    if arg.starts_with("-e") && arg.len() > 2 {
        Some(&arg[2..])
    } else if let Some(rest) = arg.strip_prefix("--expression=") {
        Some(rest)
    } else {
        None
    }
}
fn long_non_line_length(arg: &str) -> bool {
    // `/^--(?!line-length(?:=|$))/` - a long option that is NOT --line-length.
    if let Some(rest) = arg.strip_prefix("--") {
        !(rest == "line-length" || rest.starts_with("line-length="))
    } else {
        false
    }
}
