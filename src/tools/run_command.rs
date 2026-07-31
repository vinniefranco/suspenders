//! `run_command`: executes a shell command (`bash -o pipefail -c <command>`) in a
//! subprocess, foreground OR background (Phase 9, ADR-0063). Faithful to qwen
//! v0.16.0's `ShellTool` (`tools/shell.ts`): the tool `name` stays `run_command`
//! (the `run_shell_command` rename is the separate Phase 10 sweep), but the schema,
//! description, validation messages, and the background machinery are qwen's.
//!
//! FOREGROUND (the original path): stdout and stderr are merged and returned with
//! an exit-code tail, e.g. "[exit code: 0]". Approval-gated at the batch gate.
//! ADR-0023: the child leads its OWN process group (`pre_exec` + `setpgid(0, 0)`);
//! on timeout the whole group is killpg'd, and `kill_on_drop(true)` is a
//! Cancellation backstop. The effective timeout is `params.timeout` when given,
//! else the Session's `command_timeout_ms`.
//!
//! BACKGROUND (`is_background: true`, Phase 9): the tool computes the processed
//! command + cwd and hands them to the Agent (which owns the detached process
//! lifecycle, ADR-0017) through the
//! [`BackgroundShellSpawner`](crate::tool::caps::BackgroundShellSpawner)
//! capability. The Agent spawns the detached child, streams its output to a
//! capture file the model Reads, and settles a parallel registry; the tool returns
//! a "Background shell started." block IMMEDIATELY so the turn is not blocked. A
//! background shell OUTLIVES the turn; cancellation only via `task_stop`.
//!
//! Fidelity fallbacks (accepted, documented): the started block OMITS the pid line
//! and the `/tasks` inspect sentence (no such UI); ids are `bg_{n}`; the git-commit
//! refusal wording is kept VERBATIM though suspenders lacks qwen's `git notes`
//! attribution path (a `git commit` still belongs on the foreground path); the
//! sleep-interception message drops qwen's Monitor sentence (no Monitor tool).

use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};
use std::process::Stdio;

pub struct RunCommand;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

/// The tool description, VERBATIM from qwen `getShellToolDescription()` rendered
/// for a bash shell on unix (the `${executionWrapper}` is `bash -c <command>`, the
/// `${ToolNames.X}` interpolations are the real tool-names.ts values, and the
/// unix `processGroupNote` is present). The `run_shell_command` references inside
/// are qwen's own text (the wire tool `name` is still `run_command`; Phase 10
/// renames it).
const DESCRIPTION: &str = "\
Executes a given shell command (as `bash -c <command>`) in a subprocess with optional timeout, ensuring proper handling and security measures.\n\
\n\
IMPORTANT: This tool is for terminal operations like git, npm, docker, etc. DO NOT use it for file operations (reading, writing, editing, searching, finding files) - use the specialized tools for this instead.\n\
\n\
**Usage notes**:\n\
- The command argument is required.\n\
- You can specify an optional timeout in milliseconds (up to 600000ms / 10 minutes). If not specified, commands will timeout after 120000ms (2 minutes).\n\
- It is very helpful if you write a clear, concise description of what this command does in 5-10 words.\n\
\n\
- Avoid using run_shell_command with the `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands, unless explicitly instructed or when these commands are truly necessary for the task. Instead, always prefer using the dedicated tools for these commands:\n\
  - File search: Use glob (NOT find or ls)\n\
  - Content search: Use grep_search (NOT grep or rg)\n\
  - Read files: Use read_file (NOT cat/head/tail)\n\
  - Edit files: Use edit (NOT sed/awk)\n\
  - Write files: Use write_file (NOT echo >/cat <<EOF)\n\
  - Communication: Output text directly (NOT echo/printf)\n\
- **Shell argument quoting and special characters**: The active shell is Bash. When passing arguments that contain special characters (parentheses `()`, backticks ````, dollar signs `$`, backslashes `\\`, semicolons `;`, pipes `|`, angle brackets `<>`, ampersands `&`, exclamation marks `!`, etc.), you MUST ensure they are properly quoted to prevent Bash from misinterpreting them as shell syntax:\n\
  - **Single quotes** `'...'` pass everything literally, but cannot contain a literal single quote.\n\
  - **ANSI-C quoting** `$'...'` supports escape sequences (e.g. `\\n` for newline, `\\'` for single quote) and is the safest approach for multi-line strings or strings with single quotes.\n\
  - **Heredoc** is the most robust approach for large, multi-line text with mixed quotes:\n\
    ```bash\n\
    gh pr create --title \"My Title\" --body \"$(cat <<'HEREDOC'\n\
    Multi-line body with (parentheses), `backticks`, and 'single-quotes'.\n\
    HEREDOC\n\
    )\"\n\
    ```\n\
  - NEVER use unescaped single quotes inside single-quoted strings (e.g. `'it\\'s'` is wrong; use `$'it\\'s'` or `\"it's\"` instead).\n\
  - If unsure, prefer double-quoting arguments and escape inner double-quotes as `\\\"`.\n\
- When issuing multiple commands:\n\
  - If the commands are independent and can run in parallel, make multiple run_shell_command tool calls in a single message. For example, if you need to run \"git status\" and \"git diff\", send a single message with two run_shell_command tool calls in parallel.\n\
  - If the commands depend on each other and must run sequentially, use a single run_shell_command call with '&&' to chain them together (e.g., `git add . && git commit -m \"message\" && git push`). For instance, if one operation must complete before another starts (like mkdir before cp, Write before run_shell_command for git operations, or git add before git commit), run these operations sequentially instead.\n\
  - Use ';' only when you need to run commands sequentially but don't care if earlier commands fail.\n\
  - DO NOT use newlines to separate commands (newlines are ok in quoted strings).\n\
- Try to maintain your current working directory throughout the session by using absolute paths and avoiding usage of `cd`. You may use `cd` if the User explicitly requests it.\n\
  <good-example>\n\
  pytest /foo/bar/tests\n\
  </good-example>\n\
  <bad-example>\n\
  cd /foo/bar && pytest tests\n\
  </bad-example>\n\
\n\
**Background vs Foreground Execution:**\n\
- You should decide whether commands should run in background or foreground based on their nature:\n\
- Use background execution (is_background: true) for:\n\
  - Long-running development servers: `npm run start`, `npm run dev`, `yarn dev`, `bun run start`\n\
  - Build watchers: `npm run watch`, `webpack --watch`\n\
  - Database servers: `mongod`, `mysql`, `redis-server`\n\
  - Web servers: `python -m http.server`, `php -S localhost:8000`\n\
  - Any command expected to run indefinitely until manually stopped\n\
\n\
  - Command is executed as a subprocess that leads its own process group. Command process group can be terminated as `kill -- -PGID` or signaled as `kill -s SIGNAL -- -PGID`.\n\
- Use foreground execution (is_background: false) for:\n\
  - One-time commands: `ls`, `cat`, `grep`\n\
  - Build commands: `npm run build`, `make`\n\
  - Installation commands: `npm install`, `pip install`\n\
  - Git operations: `git commit`, `git push`\n\
  - Test runs: `npm test`, `pytest`\n\
";

#[async_trait::async_trait]
impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_shell_command".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Exact bash command to execute as `bash -c <command>`"
                    },
                    "is_background": {
                        "type": "boolean",
                        // VERBATIM qwen shell.ts is_background description.
                        "description": "Optional: Whether to run the command in background. If not specified, defaults to false (foreground execution). Explicitly set to true for long-running processes like development servers, watchers, or daemons that should continue running without blocking further commands."
                    },
                    "timeout": {
                        "type": "number",
                        // VERBATIM qwen shell.ts timeout description.
                        "description": "Optional timeout in milliseconds (max 600000)"
                    },
                    "description": {
                        "type": "string",
                        // VERBATIM qwen shell.ts description description.
                        "description": "Brief description of the command for the user. Be specific and concise. Ideally a single sentence. Can be up to 3 sentences for clarity. No line breaks."
                    },
                    "directory": {
                        "type": "string",
                        // VERBATIM qwen shell.ts directory description.
                        "description": "(OPTIONAL) The absolute path of the directory to run the command in. If not provided, the project root directory is used. Must be a directory within the workspace and must already exist."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let command = match input.get("command") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    "invalid input: run_shell_command requires a non-empty string \"command\"".into(),
                );
            }
        };
        let is_background = input
            .get("is_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let timeout_param = input.get("timeout");
        let directory = input.get("directory").and_then(Value::as_str);

        // Pure parameter validation (qwen `validateToolParamValues`), VERBATIM
        // messages. Runs BEFORE any spawn, foreground or background.
        validate_params(&command, is_background, timeout_param, directory, &ctx.root)?;

        // The resolved cwd: the validated absolute-and-within-root `directory`, else
        // the project root (qwen `params.directory || getTargetDir()`).
        let cwd = directory
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.root.clone());

        if is_background {
            return run_background(&command, &cwd, ctx).await;
        }

        // Foreground: the effective timeout is `params.timeout` when given, else the
        // Session's `command_timeout_ms` (overrides the old always-ctx behavior).
        let timeout = timeout_param
            .and_then(Value::as_u64)
            .filter(|t| *t > 0)
            .unwrap_or(if ctx.command_timeout_ms == 0 {
                DEFAULT_TIMEOUT_MS
            } else {
                ctx.command_timeout_ms
            });

        spawn_and_wait(&command, &cwd, timeout).await
    }
}

/// Pure parameter validation (qwen `validateToolParamValues`, shell.ts:4204-4272),
/// VERBATIM messages. Returns `Err(message)` on the first failing check, else `Ok`.
/// Adapted to a single project root (qwen's multi-workspace check narrows to the
/// one root) and dropping the user-skills-dir check (no such directory here); the
/// sleep-interception message drops qwen's Monitor sentence (no Monitor tool).
fn validate_params(
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
        // `undefined` is Ok; anything present must be a positive integer <= 600000.
        match timeout.as_f64() {
            Some(n) if n.fract() != 0.0 => {
                return Err("Timeout must be an integer number of milliseconds.".into());
            }
            None => {
                return Err("Timeout must be an integer number of milliseconds.".into());
            }
            Some(n) if n <= 0.0 => {
                return Err("Timeout must be a positive number.".into());
            }
            Some(n) if n > MAX_TIMEOUT_MS as f64 => {
                return Err("Timeout cannot exceed 600000ms (10 minutes).".into());
            }
            _ => {}
        }
    }
    if let Some(directory) = directory {
        if !std::path::Path::new(directory).is_absolute() {
            return Err("Directory must be an absolute path.".into());
        }
        // Narrowed from qwen's multi-workspace membership to the single project
        // root: the directory must be within (or equal to) the root.
        let resolved = std::path::Path::new(directory);
        if !resolved.starts_with(root) {
            return Err(format!(
                "Directory '{directory}' is not within the project root."
            ));
        }
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

/// The BACKGROUND branch (Phase 9, ADR-0063): refuse a top-level `git commit`,
/// strip ONE bare trailing `&`, and hand the processed command + cwd to the Agent
/// through the [`BackgroundShellSpawner`](crate::tool::caps::BackgroundShellSpawner)
/// capability. Returns the "Background shell started." block (OMITTING the pid line
/// and `/tasks` inspect sentence - no such UI here). A capability `Err` (a degraded
/// host) folds into the tool error.
async fn run_background(command: &str, cwd: &std::path::Path, ctx: &ToolCtx) -> Result<String, String> {
    let stripped = strip_shell_wrapper(command);
    // Refuse a top-level `git commit` in background mode. VERBATIM qwen wording
    // (shell.ts:2714-2717). Suspenders lacks qwen's `git notes` attribution path,
    // but the refusal is kept so the model-facing contract matches: a `git commit`
    // belongs on the foreground completion path.
    if has_top_level_git_commit(&stripped) {
        return Err("Refusing to run `git commit` in background mode: AI-attribution notes are written by the foreground completion path. Re-run the commit with is_background=false (or split it out of the compound command).".into());
    }

    // Strip a single bare trailing `&` (bash's background-detach operator) before
    // spawn: the managed path IS the backgrounding mechanism, so the trailing `&`
    // is redundant and would detach the wrapper early. Deliberately precise: not
    // `&&` (logical AND), not `\&` (escaped literal `&`). Operate on the trimmed
    // ORIGINAL so leading env assignments / wrappers survive to execution.
    let trimmed_original = command.trim();
    let processed = strip_trailing_background_amp(trimmed_original);
    let cwd_str = cwd.to_string_lossy().into_owned();

    // Hand off to the Agent (the process-lifecycle owner). An Err folds into the
    // tool error (the degraded host wording).
    let id = ctx
        .caps
        .bg_shells
        .spawn_background(processed.to_string(), cwd_str)
        .await?;

    // The "Background shell started." block, VERBATIM qwen leading lines but
    // OMITTING the pid line and the `/tasks`/dialog inspect sentence (no such UI).
    // The capture-file path comes from the SHARED helper the Agent's watcher also
    // uses, so the reported path can never drift from the file that gets written.
    let output_path = crate::agent::background_shell::output_path(
        &ctx.session_dir.to_string_lossy(),
        &id,
    );
    Ok(format!(
        "Background shell started.\n\
         id: {id}\n\
         output file: {path}\n\
         Read the output file directly to view the captured output.",
        id = id,
        path = output_path.display(),
    ))
}

#[cfg(unix)]
async fn spawn_and_wait(
    command: &str,
    cwd: &std::path::Path,
    timeout_ms: u64,
) -> Result<String, String> {
    // bash with pipefail: a piped command must report the producer's failure,
    // not the consumer's success - the Verify and failure Governors key on the
    // exit code, and `cargo test | head` must not launder a red suite into
    // is_error=false.
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-o")
        .arg("pipefail")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group so a timeout can killpg the whole subtree (ADR-0023).
    // Safety: the `pre_exec` closure runs post-fork/pre-exec in the child; the sole
    // call is `setpgid`, which is async-signal-safe, so there is no re-entrancy
    // hazard.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                .map_err(std::io::Error::from)?;
            Ok(())
        });
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => return Err(format!("could not run command: {err}")),
    };

    let pid = child.id().map(|p| p as i32);

    let wait = child.wait_with_output();
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), wait).await {
        Ok(Ok(output)) => {
            let mut merged = output.stdout;
            merged.extend_from_slice(&output.stderr);
            let text = String::from_utf8_lossy(&merged).into_owned();
            let code = output.status.code().unwrap_or(-1);
            if code == 0 {
                Ok(report(&text, code))
            } else {
                Err(report(&text, code))
            }
        }
        Ok(Err(err)) => Err(format!("command runner exited: {err}")),
        Err(_elapsed) => {
            // Timeout: signal the whole process group, not just the child.
            if let Some(pid) = pid {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            Err(format!("[command timed out after {timeout_ms}ms]"))
        }
    }
}

#[cfg(not(unix))]
async fn spawn_and_wait(
    _command: &str,
    _cwd: &std::path::Path,
    _timeout_ms: u64,
) -> Result<String, String> {
    Err("run_shell_command is only supported on unix".into())
}

fn report(output: &str, exit_code: i32) -> String {
    if output.is_empty() {
        format!("[exit code: {exit_code}]")
    } else if output.ends_with('\n') {
        format!("{output}[exit code: {exit_code}]")
    } else {
        format!("{output}\n[exit code: {exit_code}]")
    }
}

/// Recovers the exit code [`report`] owns from a run_command result, or `None`
/// when the tail is absent (a timeout, or content produced elsewhere). The
/// inverse of [`report`]: the `[exit code: N]` tail is the single-sourced
/// contract between here and the run_command extension's `present`, so the badge is
/// a semantic fact, not a fragile fold-time parse. Searched from the END so
/// command output that happens to contain the phrase cannot spoof it.
pub fn parse_exit_code(content: &str) -> Option<i32> {
    let last = content.lines().last()?.trim_end();
    let inner = last.strip_prefix("[exit code: ")?.strip_suffix(']')?;
    inner.parse().ok()
}

/// Whether a run_command result is the timeout report [`spawn_and_wait`]
/// emits (`[command timed out after Nms]`), matched semantically rather than by
/// substring so ordinary output cannot masquerade as a timeout.
pub fn parse_timed_out(content: &str) -> bool {
    let line = content.trim_end();
    line.starts_with("[command timed out after") && line.ends_with("ms]")
}

// ===========================================================================
// Command-shape helpers (ported from qwen `shell.ts` / `shell-utils.ts`). Used by
// validation and the background branch to reason about the command text before it
// runs (trailing `&`, top-level `git commit`, a blocking `sleep`), with leading
// shell wrappers stripped so `bash -c '...'` cannot hide the pattern.
// ===========================================================================

/// Strip a single bare trailing `&` (qwen `stripTrailingBackgroundAmp`): the bash
/// background operator, redundant when the managed path is the backgrounding
/// mechanism. Deliberately precise - NOT `&&` (logical AND) and NOT `\&` (escaped
/// literal `&`).
fn strip_trailing_background_amp(command: &str) -> String {
    let trimmed = command.trim_end();
    if !trimmed.ends_with('&') || trimmed.ends_with("&&") || trimmed.ends_with("\\&") {
        return command.to_string();
    }
    trimmed[..trimmed.len() - 1].trim_end().to_string()
}

/// Whether the command ends in a top-level bare background `&` (qwen
/// `hasTopLevelTrailingBackgroundOperator`), scanning quote/backtick state so a `&`
/// inside quotes or after `&&`/`|`/`\` is NOT a bare trailing operator.
fn has_top_level_trailing_background_operator(command: &str) -> bool {
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
fn has_top_level_git_commit(command: &str) -> bool {
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
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
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
fn strip_shell_wrapper(command: &str) -> String {
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

/// Detect a standalone or leading `sleep N` (N >= 2s) at top level (qwen
/// `detectBlockedSleepPattern`): `sleep 5`, `sleep 2.5`, `sleep 2s`,
/// `sleep 5 && check`, `sleep 5; check` - but NOT sleep inside pipelines,
/// subshells, or backgrounded commands. Returns the interpolated `{pattern}` for
/// the blocked message, or `None`.
fn detect_blocked_sleep_pattern(command: &str) -> Option<String> {
    let trimmed = trim_trailing_shell_comment(command);
    let trimmed = trimmed.trim();
    let rest = trimmed.strip_prefix("sleep")?;
    // The char after `sleep` must be whitespace (`sleepy` is not `sleep`).
    let after: Vec<char> = rest.chars().collect();
    if after.is_empty() || !after[0].is_whitespace() {
        return None;
    }
    let mut idx = 0;
    while idx < after.len() && after[idx].is_whitespace() {
        idx += 1;
    }
    let duration_start = idx;
    while idx < after.len()
        && !after[idx].is_whitespace()
        && !matches!(after[idx], ';' | '&' | '|' | '\n')
    {
        idx += 1;
    }
    let duration_token: String = after[duration_start..idx].iter().collect();
    let secs = parse_sleep_duration_to_seconds(&duration_token)?;
    if secs < 2.0 {
        return None;
    }
    let suffix: String = after[idx..].iter().collect();
    let rest_after = sleep_sequential_separator(&suffix)?;
    let rest_after = rest_after.trim();
    Some(if rest_after.is_empty() {
        format!("standalone sleep {duration_token}")
    } else {
        format!("sleep {duration_token} followed by: {rest_after}")
    })
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
        "ms" => Some(value / 1000.0),
        "" | "s" => Some(value),
        "m" => Some(value * 60.0),
        "h" => Some(value * 60.0 * 60.0),
        "d" => Some(value * 60.0 * 60.0 * 24.0),
        _ => None,
    }
}

/// Whether the suffix after a sleep duration is a recognised sequential separator
/// (`&&`/`||`/`;`/newline), returning the rest of the command after it (qwen
/// `getSleepSequentialSeparator`). `None` means the sleep is NOT a standalone /
/// leading blocking sleep (e.g. it is inside a pipeline or backgrounded).
fn sleep_sequential_separator(suffix: &str) -> Option<String> {
    let chars: Vec<char> = suffix.chars().collect();
    let mut idx = 0;
    while idx < chars.len() && chars[idx] != '\n' && chars[idx].is_whitespace() {
        idx += 1;
    }
    let rest: String = chars[idx..].iter().collect();
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
            '#' if !in_double
                && !in_backtick
                && (i == 0 || chars[i - 1].is_whitespace()) =>
            {
                return chars[..i].iter().collect();
            }
            _ => {}
        }
    }
    command.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx::for_test(root.to_path_buf(), 10_000)
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        RunCommand.run(&input, ctx).await
    }

    #[test]
    fn spec_requires_command_and_declares_the_new_params() {
        let spec = RunCommand.spec();
        assert_eq!(spec.name, "run_shell_command");
        assert_eq!(spec.input_schema["required"], json!(["command"]));
        let props = &spec.input_schema["properties"];
        assert!(props.get("is_background").is_some());
        assert!(props.get("timeout").is_some());
        assert!(props.get("directory").is_some());
        // The description carries the Background vs Foreground section verbatim.
        assert!(spec.description.contains("**Background vs Foreground Execution:**"));
        assert!(spec.description.contains("is_background: true"));
    }

    #[test]
    fn spec_accepts_an_optional_description_field() {
        let spec = RunCommand.spec();
        let input = json!({"command": "git log --oneline -30", "description": "list recent commits"})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(crate::tool::validate(&spec.input_schema, &input), Ok(()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_in_the_project_root_and_reports_stdout_plus_exit_code() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("marker.txt"), "").unwrap();

        let out = run(json!({"command": "ls"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(out.contains("marker.txt"));
        assert!(out.contains("[exit code: 0]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn merges_stderr_into_the_output() {
        let tmp = TempDir::new().unwrap();
        let out = run(json!({"command": "echo oops 1>&2"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(out.contains("oops"));
        assert!(out.contains("[exit code: 0]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_with_no_output_still_reports_its_exit_code() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(json!({"command": "true"}), &ctx(tmp.path())).await,
            Ok("[exit code: 0]".into())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_code_is_an_error_with_output_and_code() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"command": "echo boom; exit 3"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("boom"));
        assert!(err.contains("[exit code: 3]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipefail_reports_the_producers_failure_not_the_consumers_success() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"command": "false | cat"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("[exit code: 1]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn times_out_per_the_ctx_command_timeout_ms() {
        let tmp = TempDir::new().unwrap();
        let mut c = ctx(tmp.path());
        c.command_timeout_ms = 100;

        assert_eq!(
            // `sleep 5` in background would be blocked by the sleep guard, but the
            // FOREGROUND path allows it (only >= 2s standalone sleeps are blocked
            // BEFORE spawn) - wait, the guard blocks it. Use a busy loop instead so
            // the timeout path (not the sleep guard) is exercised.
            run(json!({"command": "while true; do :; done"}), &c).await,
            Err("[command timed out after 100ms]".into())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_timeout_param_overrides_the_ctx_timeout() {
        let tmp = TempDir::new().unwrap();
        let mut c = ctx(tmp.path());
        c.command_timeout_ms = 60_000; // the ctx default is generous
        // The param pins a tight 100ms, so the busy loop times out at 100ms.
        assert_eq!(
            run(
                json!({"command": "while true; do :; done", "timeout": 100}),
                &c
            )
            .await,
            Err("[command timed out after 100ms]".into())
        );
    }

    #[test]
    fn parse_exit_code_round_trips_report_including_empty_output() {
        for code in [0, 1, 3, 42, -1, 130] {
            assert_eq!(parse_exit_code(&report("some output", code)), Some(code));
            assert_eq!(parse_exit_code(&report("", code)), Some(code));
            assert_eq!(
                parse_exit_code(&report("trailing newline\n", code)),
                Some(code)
            );
        }
    }

    #[test]
    fn parse_exit_code_none_when_the_tail_is_absent() {
        assert_eq!(parse_exit_code("just output, no tail"), None);
        assert_eq!(parse_exit_code("[command timed out after 100ms]"), None);
        assert_eq!(parse_exit_code("[exit code: 9]\nreal tail"), None);
    }

    #[test]
    fn parse_timed_out_matches_only_the_timeout_report() {
        assert!(parse_timed_out("[command timed out after 100ms]"));
        assert!(parse_timed_out("[command timed out after 100ms]\n"));
        assert!(!parse_timed_out(&report("ok", 0)));
        assert!(!parse_timed_out("timed out somewhere in the middle"));
    }

    #[tokio::test]
    async fn missing_empty_or_non_string_command_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(run(json!({}), &c).await.is_err());
        // An empty command is the VERBATIM qwen message.
        assert_eq!(
            run(json!({"command": ""}), &c).await,
            Err("Command cannot be empty.".into())
        );
        assert!(run(json!({"command": 42}), &c).await.is_err());
    }

    // ---- validation table (pure) ----------------------------------------

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from("/project/root")
    }

    #[test]
    fn validate_empty_command() {
        assert_eq!(
            validate_params("   ", false, None, None, &root()),
            Err("Command cannot be empty.".into())
        );
    }

    #[test]
    fn validate_background_bare_trailing_amp() {
        assert_eq!(
            validate_params("npm run dev &", true, None, None, &root()),
            Err("Background shell commands must not end with a bare \"&\". Remove the trailing \"&\" and rely on is_background: true instead.".into())
        );
        // `&&` is fine (logical AND, not a bare trailing background operator).
        assert!(validate_params("a && b", true, None, None, &root()).is_ok());
        // Foreground with a trailing `&` is NOT this error (only background).
        assert!(validate_params("npm run dev &", false, None, None, &root()).is_ok());
    }

    #[test]
    fn validate_timeout_bounds() {
        assert_eq!(
            validate_params("ls", false, Some(&json!(1.5)), None, &root()),
            Err("Timeout must be an integer number of milliseconds.".into())
        );
        assert_eq!(
            validate_params("ls", false, Some(&json!(0)), None, &root()),
            Err("Timeout must be a positive number.".into())
        );
        assert_eq!(
            validate_params("ls", false, Some(&json!(600001)), None, &root()),
            Err("Timeout cannot exceed 600000ms (10 minutes).".into())
        );
        assert!(validate_params("ls", false, Some(&json!(600000)), None, &root()).is_ok());
    }

    #[test]
    fn validate_directory_absolute_and_within_root() {
        assert_eq!(
            validate_params("ls", false, None, Some("relative/dir"), &root()),
            Err("Directory must be an absolute path.".into())
        );
        assert_eq!(
            validate_params("ls", false, None, Some("/somewhere/else"), &root()),
            Err("Directory '/somewhere/else' is not within the project root.".into())
        );
        assert!(validate_params("ls", false, None, Some("/project/root/sub"), &root()).is_ok());
    }

    #[test]
    fn validate_sleep_interception_foreground_only() {
        // Foreground: a standalone >= 2s sleep is blocked (Monitor sentence dropped).
        assert_eq!(
            validate_params("sleep 5", false, None, None, &root()),
            Err("Blocked: standalone sleep 5. Run blocking commands in the background with is_background: true. If you genuinely need a delay (rate limiting, deliberate pacing), keep it under 2 seconds.".into())
        );
        // `sleep 5 && check` names the follow-on in the pattern.
        assert_eq!(
            validate_params("sleep 5 && echo done", false, None, None, &root()),
            Err("Blocked: sleep 5 followed by: echo done. Run blocking commands in the background with is_background: true. If you genuinely need a delay (rate limiting, deliberate pacing), keep it under 2 seconds.".into())
        );
        // A wrapper cannot hide the sleep (`bash -c 'sleep 5'` is still blocked).
        assert!(validate_params("bash -c 'sleep 5'", false, None, None, &root()).is_err());
        // < 2s is allowed.
        assert!(validate_params("sleep 1", false, None, None, &root()).is_ok());
        // BACKGROUND: the sleep guard does NOT fire (a background sleep is fine).
        assert!(validate_params("sleep 5", true, None, None, &root()).is_ok());
    }

    #[test]
    fn git_commit_detection_for_the_background_refusal() {
        assert!(has_top_level_git_commit("git commit -m x"));
        assert!(has_top_level_git_commit("git add . && git commit -m x"));
        assert!(has_top_level_git_commit("git -C /repo commit -m x"));
        assert!(has_top_level_git_commit("FOO=bar git commit -m x"));
        // Not a top-level commit.
        assert!(!has_top_level_git_commit("git status"));
        assert!(!has_top_level_git_commit("echo git commit"));
    }

    #[test]
    fn strip_trailing_amp_is_precise() {
        assert_eq!(strip_trailing_background_amp("npm run dev &"), "npm run dev");
        assert_eq!(strip_trailing_background_amp("a && b"), "a && b");
        assert_eq!(strip_trailing_background_amp("echo \\&"), "echo \\&");
        assert_eq!(strip_trailing_background_amp("plain"), "plain");
    }

    // ---- background branch via the capability seam -----------------------

    struct FakeBackgroundShellSpawner {
        id: String,
    }

    #[async_trait::async_trait]
    impl crate::tool::caps::BackgroundShellSpawner for FakeBackgroundShellSpawner {
        async fn spawn_background(&self, _command: String, _cwd: String) -> Result<String, String> {
            Ok(self.id.clone())
        }
        async fn stop_background(&self, id: String) -> Result<String, String> {
            Ok(format!("Error: No background task found with ID \"{id}\"."))
        }
    }

    fn ctx_with_bg(
        root: &std::path::Path,
        bg: Arc<dyn crate::tool::caps::BackgroundShellSpawner>,
    ) -> ToolCtx {
        ToolCtx {
            caps: crate::tool::caps::Capabilities::for_test_with_bg_shells(bg),
            ..ToolCtx::for_test(root.to_path_buf(), 10_000)
        }
    }

    #[tokio::test]
    async fn background_branch_returns_the_verbatim_started_block() {
        let tmp = TempDir::new().unwrap();
        let c = ctx_with_bg(
            tmp.path(),
            Arc::new(FakeBackgroundShellSpawner { id: "bg_7".into() }),
        );
        let out = run(json!({"command": "npm run dev", "is_background": true}), &c)
            .await
            .unwrap();
        assert!(out.starts_with("Background shell started.\nid: bg_7\noutput file: "));
        assert!(out.ends_with("Read the output file directly to view the captured output."));
        // No pid line and no /tasks inspect sentence (the fidelity fallbacks).
        assert!(!out.contains("pid:"));
        assert!(!out.contains("/tasks"));
    }

    #[tokio::test]
    async fn background_branch_refuses_git_commit_verbatim() {
        let tmp = TempDir::new().unwrap();
        let c = ctx_with_bg(
            tmp.path(),
            Arc::new(FakeBackgroundShellSpawner { id: "bg_1".into() }),
        );
        let err = run(
            json!({"command": "git commit -m wip", "is_background": true}),
            &c,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            "Refusing to run `git commit` in background mode: AI-attribution notes are written by the foreground completion path. Re-run the commit with is_background=false (or split it out of the compound command)."
        );
    }

    #[tokio::test]
    async fn background_branch_folds_a_degraded_spawn_err() {
        let tmp = TempDir::new().unwrap();
        // The default for_test ctx carries an UnavailableBackgroundShellSpawner.
        let out = run(
            json!({"command": "npm run dev", "is_background": true}),
            &ctx(tmp.path()),
        )
        .await;
        assert_eq!(
            out,
            Err("background shells are unavailable in this environment".into())
        );
    }
}
