//! Shell-operator and wrapper parsing: trailing background `&`, top-level segment
//! splitting, `git commit` detection, env assignments, and leading shell-wrapper
//! stripping. All quote/backtick aware so a pattern inside quotes is not mistaken
//! for a shell operator.

/// Strip a single bare trailing `&` (qwen `stripTrailingBackgroundAmp`): the bash
/// background operator, redundant when the managed path is the backgrounding
/// mechanism. Deliberately precise - NOT `&&` (logical AND) and NOT `\&` (escaped
/// literal `&`).
pub(in crate::tools::run_command) fn strip_trailing_background_amp(command: &str) -> String {
    let trimmed = command.trim_end();
    if !trimmed.ends_with('&') || trimmed.ends_with("&&") || trimmed.ends_with("\\&") {
        return command.to_string();
    }
    trimmed[..trimmed.len() - 1].trim_end().to_string()
}

/// Whether the command ends in a top-level bare background `&` (qwen
/// `hasTopLevelTrailingBackgroundOperator`), scanning quote/backtick state so a `&`
/// inside quotes or after `&&`/`|`/`\` is NOT a bare trailing operator.
pub(in crate::tools::run_command) fn has_top_level_trailing_background_operator(
    command: &str,
) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let trimmed_len = {
        let mut n = chars.len();
        while n > 0 && chars[n - 1].is_whitespace() {
            n -= 1;
        }
        n
    };
    if trimmed_len == 0 || chars[trimmed_len - 1] != '&' {
        return false;
    }
    let amp_index = trimmed_len - 1;
    // The previous non-whitespace char: `&`/`|`/`\` before the `&` means it is not
    // a bare trailing operator (`&&`, `|&`, `\&`).
    let mut prev = None;
    for i in (0..amp_index).rev() {
        if !chars[i].is_whitespace() {
            prev = Some(chars[i]);
            break;
        }
    }
    if matches!(prev, Some('&') | Some('|') | Some('\\')) {
        return false;
    }
    // An odd run of backslashes immediately before the `&` escapes it.
    let mut backslashes = 0;
    let mut i = amp_index;
    while i > 0 && chars[i - 1] == '\\' {
        backslashes += 1;
        i -= 1;
    }
    if backslashes % 2 == 1 {
        return false;
    }
    // The `&` must be outside any quote/backtick region.
    !in_quoted_region(&chars, amp_index)
}

/// Whether the char at `target` sits inside a single-quote, double-quote, or
/// backtick region (a simplified port of qwen's quote-state scan): a bare `&`
/// inside quotes is not a shell operator.
fn in_quoted_region(chars: &[char], target: usize) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut escape = false;
    for (i, &ch) in chars.iter().enumerate() {
        if i > target {
            break;
        }
        if i == target {
            return in_single || in_double || in_backtick;
        }
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_double || in_backtick => escape = true,
            '\'' if !in_double && !in_backtick => in_single = true,
            '"' if !in_backtick => in_double = !in_double,
            '`' if !in_double => in_backtick = !in_backtick,
            _ => {}
        }
    }
    false
}

/// Whether any top-level command segment is a `git commit` (a narrowed port of
/// qwen's `gitCommitContext(...).hasCommit`): split on top-level `&&`/`||`/`;`/`|`,
/// tokenise each segment, and check for `git` ... `commit` (past global flags).
/// Used to refuse `git commit` in background mode.
pub(in crate::tools::run_command) fn has_top_level_git_commit(command: &str) -> bool {
    for segment in split_top_level_segments(command) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let mut i = 0;
        // Skip leading env assignments (`FOO=bar git commit`).
        while i < tokens.len() && is_env_assignment(tokens[i]) {
            i += 1;
        }
        if i >= tokens.len() || tokens[i] != "git" {
            continue;
        }
        i += 1;
        // Skip git global flags (and the value of `-C <dir>` / `--git-dir <dir>`).
        while i < tokens.len() {
            let t = tokens[i];
            if t == "-C" || t == "--git-dir" || t == "--work-tree" {
                i += 2;
                continue;
            }
            if t.starts_with('-') {
                i += 1;
                continue;
            }
            break;
        }
        if i < tokens.len() && tokens[i] == "commit" {
            return true;
        }
    }
    false
}

/// Split a command into top-level segments on `&&`, `||`, `;`, `|` and `&`,
/// respecting quote/backtick regions. Used by [`has_top_level_git_commit`].
fn split_top_level_segments(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut i = 0;
    while i < chars.len() {
        if in_quoted_region(&chars, i) {
            current.push(chars[i]);
            i += 1;
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if two == "&&" || two == "||" {
            segments.push(std::mem::take(&mut current));
            i += 2;
            continue;
        }
        if matches!(chars[i], ';' | '|' | '&') {
            segments.push(std::mem::take(&mut current));
            i += 1;
            continue;
        }
        current.push(chars[i]);
        i += 1;
    }
    segments.push(current);
    segments
}

/// Whether a token is a `NAME=value` env assignment (leading assignments precede a
/// wrapper / a git invocation).
fn is_env_assignment(token: &str) -> bool {
    if let Some(eq) = token.find('=') {
        let name = &token[..eq];
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    } else {
        false
    }
}

/// Strip a leading shell wrapper (qwen `stripShellWrapper`, narrowed): skip leading
/// env assignments, then a known wrapper (`bash`/`sh`/`zsh`/`dash`, optionally
/// path-prefixed) plus its flags up to the `-c` marker, and return the
/// symmetric-quote-stripped inner command. Returns the trimmed original when there
/// is no recognised wrapper. Hardens the trailing-`&` and sleep checks so
/// `bash -c 'sleep 5'` cannot hide the foreground pattern.
pub(in crate::tools::run_command) fn strip_shell_wrapper(command: &str) -> String {
    let trimmed = command.trim();
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() && is_env_assignment(tokens[i]) {
        i += 1;
    }
    if i >= tokens.len() || !is_known_wrapper(tokens[i]) {
        return trimmed.to_string();
    }
    i += 1;
    // Consume wrapper flags until the `-c` command marker.
    while i < tokens.len() {
        let t = tokens[i];
        if t == "-c" {
            i += 1;
            if i >= tokens.len() {
                return trimmed.to_string();
            }
            // The inner command is the remaining tokens rejoined, symmetric-quote
            // stripped (bash re-parses the single argument).
            let inner = tokens[i..].join(" ");
            let unquoted = strip_symmetric_quotes(&inner);
            return if unquoted.is_empty() {
                trimmed.to_string()
            } else {
                unquoted
            };
        }
        // `-o pipefail`: a flag that consumes an operand.
        if t == "-o" {
            i += 2;
            continue;
        }
        if t.starts_with('-') {
            i += 1;
            continue;
        }
        // A non-flag token that is not `-c`: not a wrapper we understand.
        return trimmed.to_string();
    }
    trimmed.to_string()
}

/// Whether `token` names a known shell wrapper (`bash`/`sh`/`zsh`/`dash`),
/// optionally path-prefixed (`/bin/bash`).
fn is_known_wrapper(token: &str) -> bool {
    let base = token.rsplit('/').next().unwrap_or(token);
    matches!(base, "bash" | "sh" | "zsh" | "dash")
}

/// Strip a symmetric surrounding quote pair (`'...'` or `"..."`) from `s`.
fn strip_symmetric_quotes(s: &str) -> String {
    let bytes = s.as_bytes();
    if s.len() >= 2
        && ((bytes[0] == b'\'' && bytes[s.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[s.len() - 1] == b'"'))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}
