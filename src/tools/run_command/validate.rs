//! Pure parameter validation (qwen `validateToolParamValues`, shell.ts:4204-4272),
//! VERBATIM messages. Runs BEFORE any spawn, foreground or background.

use serde_json::Value;

use super::MAX_TIMEOUT_MS;
use super::command_shape::{
    detect_blocked_sleep_pattern, has_top_level_trailing_background_operator, strip_shell_wrapper,
};

/// A `timeout` value must be a whole (integer) count of milliseconds; a fraction
/// is rejected. `x.fract() == NO_FRACTION` is the "integer" test.
const NO_FRACTION: f64 = 0.0;

/// The exclusive lower bound for a `timeout`: it must be strictly greater than
/// this many milliseconds.
const MIN_TIMEOUT_MS: f64 = 0.0;

/// Pure parameter validation (qwen `validateToolParamValues`), VERBATIM messages.
/// Returns `Err(message)` on the first failing check, else `Ok`. Adapted to a
/// single project root (qwen's multi-workspace check narrows to the one root) and
/// dropping the user-skills-dir check (no such directory here); the
/// sleep-interception message drops qwen's Monitor sentence (no Monitor tool).
pub(super) fn validate_params(
    command: &str,
    is_background: bool,
    timeout: Option<&Value>,
    directory: Option<&str>,
    root: &std::path::Path,
) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("Command cannot be empty.".into());
    }
    let stripped = strip_shell_wrapper(command);
    if is_background && has_top_level_trailing_background_operator(&stripped) {
        return Err("Background shell commands must not end with a bare \"&\". Remove the trailing \"&\" and rely on is_background: true instead.".into());
    }
    if let Some(timeout) = timeout {
        validate_timeout(timeout)?;
    }
    if let Some(directory) = directory {
        validate_directory(directory, root)?;
    }
    // Sleep interception (foreground only): block `sleep >= 2s` at top level so a
    // deliberate blocking delay does not park the turn. Strip shell wrappers first
    // so `bash -c 'sleep 5'` cannot route around the check. The message drops
    // qwen's Monitor sentence (Suspenders has no Monitor tool).
    let sleep_pattern = if is_background {
        None
    } else {
        detect_blocked_sleep_pattern(&stripped)
    };
    if let Some(pattern) = sleep_pattern {
        return Err(format!(
            "Blocked: {pattern}. Run blocking commands in the background with is_background: true. If you genuinely need a delay (rate limiting, deliberate pacing), keep it under 2 seconds."
        ));
    }
    Ok(())
}

/// Validate a present `timeout` value: `undefined` is Ok upstream, but anything
/// present must be a positive integer count of milliseconds no greater than
/// [`MAX_TIMEOUT_MS`]. VERBATIM qwen messages.
fn validate_timeout(timeout: &Value) -> Result<(), String> {
    match timeout.as_f64() {
        Some(n) if n.fract() != NO_FRACTION => {
            Err("Timeout must be an integer number of milliseconds.".into())
        }
        None => Err("Timeout must be an integer number of milliseconds.".into()),
        Some(n) if n <= MIN_TIMEOUT_MS => Err("Timeout must be a positive number.".into()),
        Some(n) if n > MAX_TIMEOUT_MS as f64 => {
            Err("Timeout cannot exceed 600000ms (10 minutes).".into())
        }
        _ => Ok(()),
    }
}

/// Validate a present `directory`: it must be an absolute path within (or equal
/// to) the project root. Narrowed from qwen's multi-workspace membership to the
/// single project root.
fn validate_directory(directory: &str, root: &std::path::Path) -> Result<(), String> {
    if !std::path::Path::new(directory).is_absolute() {
        return Err("Directory must be an absolute path.".into());
    }
    let resolved = std::path::Path::new(directory);
    if !resolved.starts_with(root) {
        return Err(format!(
            "Directory '{directory}' is not within the project root."
        ));
    }
    Ok(())
}
