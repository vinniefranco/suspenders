//! Blocking-`sleep` detection: the foreground path refuses a standalone or leading
//! `sleep N` (N >= 2s) at top level so a deliberate delay does not park the turn
//! (qwen `detectBlockedSleepPattern`). Sleeps inside pipelines, subshells, or
//! backgrounded commands are allowed.

/// The smallest top-level `sleep` (in seconds) the foreground path refuses: a
/// deliberate blocking delay at or above this parks the turn, so it must be
/// backgrounded or kept shorter.
const MIN_BLOCKED_SLEEP_SECS: f64 = 2.0;

/// Human-duration units, one step apart, for converting a `sleep` token to
/// seconds (`ms`→`s`→`m`→`h`→`d`). Named so the ladder reads as domain units, not
/// bare literals.
const MILLIS_PER_SEC: f64 = 1000.0;
const SECS_PER_MIN: f64 = 60.0;
const MINS_PER_HOUR: f64 = 60.0;
const HOURS_PER_DAY: f64 = 24.0;

/// A candidate top-level `sleep` command, scanned char-by-char. Holds the
/// comment-trimmed command chars so the duration scan and the trailing-separator
/// scan share one cursor position rather than passing string slices around.
struct SleepScan {
    /// The command past `sleep`, comment-trimmed, as chars.
    after: Vec<char>,
    /// Cursor into `after`; advances through leading whitespace, the duration
    /// token, then whatever separator follows.
    idx: usize,
}

impl SleepScan {
    /// Build a scan positioned just past the `sleep` keyword, or `None` when the
    /// command is not a leading `sleep` (a top-level trailing comment is trimmed
    /// first so `sleep 5 # nap` still matches).
    fn opening(command: &str) -> Option<Self> {
        let trimmed = trim_trailing_shell_comment(command);
        let rest = trimmed.trim().strip_prefix("sleep")?.to_string();
        let after: Vec<char> = rest.chars().collect();
        // The char after `sleep` must be whitespace (`sleepy` is not `sleep`).
        if after.first().is_none_or(|c| !c.is_whitespace()) {
            return None;
        }
        Some(Self { after, idx: 0 })
    }

    /// Detect the blocked pattern: consume the duration, require it be >= the
    /// threshold, then require a recognised sequential separator (or end of
    /// command). Returns the interpolated `{pattern}` for the blocked message.
    fn blocked_pattern(mut self) -> Option<String> {
        let duration_token = self.take_duration_token();
        if parse_sleep_duration_to_seconds(&duration_token)? < MIN_BLOCKED_SLEEP_SECS {
            return None;
        }
        let rest_after = self.take_sequential_separator()?;
        let rest_after = rest_after.trim();
        Some(if rest_after.is_empty() {
            format!("standalone sleep {duration_token}")
        } else {
            format!("sleep {duration_token} followed by: {rest_after}")
        })
    }

    /// Skip leading whitespace, then take the duration token (up to the next
    /// whitespace or a `;`/`&`/`|`/newline separator), leaving the cursor on it.
    fn take_duration_token(&mut self) -> String {
        while self.idx < self.after.len() && self.after[self.idx].is_whitespace() {
            self.idx += 1;
        }
        let start = self.idx;
        while self.idx < self.after.len()
            && !self.after[self.idx].is_whitespace()
            && !matches!(self.after[self.idx], ';' | '&' | '|' | '\n')
        {
            self.idx += 1;
        }
        self.after[start..self.idx].iter().collect()
    }

    /// The command after the duration must be end-of-input or a sequential
    /// separator (`&&`/`||`/`;`/newline); return the rest after it. `None` means
    /// the sleep is NOT a standalone / leading blocking sleep (e.g. it is inside a
    /// pipeline or backgrounded) - qwen `getSleepSequentialSeparator`.
    fn take_sequential_separator(&self) -> Option<String> {
        let mut i = self.idx;
        while i < self.after.len() && self.after[i] != '\n' && self.after[i].is_whitespace() {
            i += 1;
        }
        let rest: String = self.after[i..].iter().collect();
        if rest.is_empty() {
            return Some(String::new());
        }
        if let Some(stripped) = rest.strip_prefix("&&").or_else(|| rest.strip_prefix("||")) {
            return Some(stripped.to_string());
        }
        if rest.starts_with(';') || rest.starts_with('\n') {
            return Some(rest[1..].to_string());
        }
        None
    }
}

/// Detect a standalone or leading `sleep N` (N >= 2s) at top level (qwen
/// `detectBlockedSleepPattern`): `sleep 5`, `sleep 2.5`, `sleep 2s`,
/// `sleep 5 && check`, `sleep 5; check` - but NOT sleep inside pipelines,
/// subshells, or backgrounded commands. Returns the interpolated `{pattern}` for
/// the blocked message, or `None`.
pub(in crate::tools::run_command) fn detect_blocked_sleep_pattern(command: &str) -> Option<String> {
    SleepScan::opening(command)?.blocked_pattern()
}

/// Parse a sleep duration token to seconds (qwen `parseSleepDurationToSeconds`):
/// a number with an optional `ms`/`s`/`m`/`h`/`d` unit (default `s`).
fn parse_sleep_duration_to_seconds(token: &str) -> Option<f64> {
    if token.is_empty() {
        return None;
    }
    let mut idx = 0;
    let chars: Vec<char> = token.chars().collect();
    let mut seen_digit = false;
    let mut seen_dot = false;
    while idx < chars.len() {
        let c = chars[idx];
        if c.is_ascii_digit() {
            seen_digit = true;
            idx += 1;
        } else if c == '.' && !seen_dot {
            seen_dot = true;
            idx += 1;
        } else {
            break;
        }
    }
    if !seen_digit {
        return None;
    }
    let value: f64 = token[..idx].parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    let unit = token[idx..].to_ascii_lowercase();
    match unit.as_str() {
        "ms" => Some(value / MILLIS_PER_SEC),
        "" | "s" => Some(value),
        "m" => Some(value * SECS_PER_MIN),
        "h" => Some(value * SECS_PER_MIN * MINS_PER_HOUR),
        "d" => Some(value * SECS_PER_MIN * MINS_PER_HOUR * HOURS_PER_DAY),
        _ => None,
    }
}

/// Trim a top-level trailing shell comment (`# ...`), respecting quote/backtick
/// regions (a narrowed port of qwen `trimTrailingShellComment`): a `#` that begins
/// a word outside quotes starts a comment.
fn trim_trailing_shell_comment(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut escape = false;
    for (i, &ch) in chars.iter().enumerate() {
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
            // A `#` begins a comment only at start-of-word (start of input or
            // preceded by whitespace), outside quotes/backticks.
            '#' if !in_double && !in_backtick && (i == 0 || chars[i - 1].is_whitespace()) => {
                return chars[..i].iter().collect();
            }
            _ => {}
        }
    }
    command.to_string()
}
