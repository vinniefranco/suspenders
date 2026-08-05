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
//!
//! This module is the Tool facade; the mechanics live in cohesive submodules:
//! [`command_shape`] (parse the command text), [`validate`] (pure param checks),
//! and [`spawn`] (foreground execution + the background handoff + the exit tail).

mod command_shape;
mod condense;
mod spawn;
mod validate;

use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use self::spawn::{run_background, spawn_and_wait};
use self::validate::validate_params;

/// The Artifact keys the run_command exit badge reserves, declared in one place:
/// a producer (this tool) and consumer (the Transcript store) that disagree fail
/// to *compile*, and a rename touches this module alone.
pub mod keys {
    /// `artifacts`: the exit code recovered from the result's `[exit code: N]`
    /// tail, read back by the Transcript store to build the `✓ exit 0` / `✗ exit
    /// N` badge.
    pub const EXIT_CODE: &str = "exit_code";

    /// `artifacts`: set when the result is the timeout report, so the badge reads
    /// `✗ timed out` (there is no exit code on a timeout).
    pub const TIMED_OUT: &str = "timed_out";
}

// Re-exported so the `#[path]` unit tests (which use `super::*`) can reach the
// spawn/command-shape helpers they exercise directly.
#[cfg(test)]
use self::command_shape::{has_top_level_git_commit, strip_trailing_background_amp};
#[cfg(test)]
use self::spawn::report;

pub struct RunCommand;

/// The wire name of this tool, shared with the Transcript store's exit-badge
/// swap so the two never drift.
pub const SHELL: &str = "run_shell_command";

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
  - To stop a background command started by this tool, use `task_stop` when a task id is available. Do not use broad process-name kills such as `kill $(pgrep node)`, `pkill node`, or `killall node`; use a specific PID or process group id where supported.\n\
- Use foreground execution (is_background: false) for:\n\
  - One-time commands: `ls`, `cat`, `grep`\n\
  - Build commands: `npm run build`, `make`\n\
  - Installation commands: `npm install`, `pip install`\n\
  - Git operations: `git commit`, `git push`\n\
  - Test runs: `npm test`, `pytest`\n\
";

#[async_trait::async_trait]
impl Tool for RunCommand {
    // Mutator (qwen shell.ts:5026 `Kind.Execute` for run_shell_command): BLOCKED
    // in plan mode this phase. Phase 4's plan-mode shell classifier (ADR-0067)
    // will let a read-only command through in `classify`; the Kind stays
    // `Execute` regardless (the classifier special-cases it, not this Kind).
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Execute
    }

    // A cut keeps the start AND the end: the exit code and last errors live at
    // the end of a command's output.
    fn cut_policy(&self) -> crate::tool::CutPolicy {
        crate::tool::CutPolicy::HeadTail
    }

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
        // The text projection: condense the model-facing output (BEFORE Shaping
        // caps it) on both the Ok and the completed-but-failed arm - a failing
        // run carries compile-progress noise too. `run_rich` (the Registry's
        // dispatch path) additionally attaches the exit-code badge Artifact.
        execute_shell(input, ctx)
            .await
            .map(|out| condense::condense(&out))
            .map_err(|err| condense::condense(&err))
    }

    async fn run_rich(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, String> {
        // A completed-but-failed command (nonzero exit / timeout) is not a tool
        // failure: `execute_shell` returns Err, but we route it through
        // `Ok(ToolOutput.error(true))` so the exit-code Artifact rides alongside
        // `is_error`. Condensing runs first (so the badge parses the SAME
        // condensed content the model sees), then the badge Artifact is attached.
        let (raw, is_error) = match execute_shell(input, ctx).await {
            Ok(out) => (out, false),
            Err(err) => (err, true),
        };
        let content = condense::condense(&raw);
        let output = ToolOutput::text(&content).error(is_error);
        Ok(attach_badge(output, &content))
    }
}

/// Runs the shell command and returns its RAW report (or a completed-but-failed
/// report as `Err`, or a genuine tool error as `Err`), BEFORE condensing. Shared
/// by `run` (text projection) and `run_rich` (badge Artifact). A nonzero-exit
/// foreground command lands as `Err(report_with_tail)`; the caller decides how
/// to surface it.
async fn execute_shell(input: &Value, ctx: &ToolCtx) -> Result<String, String> {
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

/// Attaches the exit-code / timeout badge Artifact to a run_command output, read
/// from the (condensed) result content the model sees. A timeout wins over an
/// exit code (a timed-out command has no meaningful code); content with neither
/// tail (a validation error, a background start block) attaches nothing, so the
/// Transcript store keeps the plain summary. The `[exit code: N]` tail is the
/// single-sourced fact [`report`] owns; parsing it here reads a SEMANTIC fact
/// carried from execution, not a fragile UI-side re-parse.
fn attach_badge(output: ToolOutput, content: &str) -> ToolOutput {
    if parse_timed_out(content) {
        return output.with_artifact(keys::TIMED_OUT, true);
    }
    match parse_exit_code(content) {
        Some(code) => output.with_artifact(keys::EXIT_CODE, code as i64),
        None => output,
    }
}

/// Recovers the exit code [`report`] owns from a run_command result, or `None`
/// when the tail is absent (a timeout, or content produced elsewhere). The
/// inverse of `report`: the `[exit code: N]` tail is the single-sourced contract
/// between here and [`attach_badge`], so the badge is a semantic fact, not a
/// fragile fold-time parse. Searched from the END so command output that happens
/// to contain the phrase cannot spoof it.
pub fn parse_exit_code(content: &str) -> Option<i32> {
    let last = content.lines().last()?.trim_end();
    let inner = last.strip_prefix("[exit code: ")?.strip_suffix(']')?;
    inner.parse().ok()
}

/// Whether a run_command result is the timeout report `spawn_and_wait`
/// emits (`[command timed out after Nms]`), matched semantically rather than by
/// substring so ordinary output cannot masquerade as a timeout.
pub fn parse_timed_out(content: &str) -> bool {
    let line = content.trim_end();
    line.starts_with("[command timed out after") && line.ends_with("ms]")
}

#[cfg(test)]
#[path = "../../tests/tools/run_command.rs"]
mod tests;
