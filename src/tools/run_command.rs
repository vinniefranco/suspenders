//! `run_command(command)`: runs `bash -o pipefail -c command` in the Project Root with
//! stdout and stderr merged, reporting the exit code. Approval-gated: the
//! Approval seam ([`crate::approvals::gate_text`]) shows the user the command
//! before it runs (web_fetch's URL is gated the same way - ADR-0024).
//!
//! ADR-0023: the child runs in its OWN process group (`process_group(0)`);
//! on timeout the whole group is signalled (`killpg`), and `kill_on_drop(true)`
//! is a Cancellation backstop. Killing only the direct child would leak
//! orphaned grandchildren. Unix-only by design - a future reader must not
//! "simplify" this to `child.kill()`.

use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};
use std::process::Stdio;

pub struct RunCommand;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

const DESCRIPTION: &str = "\
Executes a given shell command (as `bash -o pipefail -c <command>`) in a subprocess in the project \
root, ensuring proper handling and security measures. stdout and stderr are merged and returned \
together, followed by an exit-code tail, e.g. \"[exit code: 0]\". Every command must be approved by \
the user before it runs, so keep commands simple and purposeful.\n\
\n\
IMPORTANT: This tool is for terminal operations like git, cargo, docker, etc. DO NOT use it for \
file operations (reading, writing, editing, searching, finding files) - use the specialized tools \
for this instead.\n\
\n\
**Usage notes**:\n\
- The command argument is required.\n\
- Commands run with `-o pipefail`: a piped pipeline reports the FIRST failing stage, not the last \
stage's success. A red test suite piped anywhere still surfaces as a failure.\n\
- Output is truncated automatically when it is long, keeping the start and the end (the exit code \
and last errors live at the end). Do NOT pipe long output through `head`, `tail`, or `wc` - a pipe \
hides the real exit code and defeats the truncation that already keeps the important parts. Prefer \
a runner's own quiet flags (e.g. `cargo nextest run --status-level fail`, `cargo build -q`) so \
passing runs stay short.\n\
- A long-running command is killed when it exceeds the configured timeout (the whole process group \
is signalled, not just the direct child). The default is 120000ms (2 minutes). Do not launch \
servers or watchers that run indefinitely; they will time out.\n\
- It is very helpful if you write a clear, concise description of what this command does in 5-10 \
words.\n\
\n\
- Avoid using run_command with the `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo` \
commands, unless explicitly instructed or when these commands are truly necessary for the task. \
Instead, always prefer using the dedicated tools for these operations:\n\
  - File search: Use glob (NOT find or ls)\n\
  - Content search: Use grep (NOT grep or rg)\n\
  - Read files: Use read_file (NOT cat/head/tail)\n\
  - Edit files: Use edit_file (NOT sed/awk)\n\
  - Write files: Use write_file (NOT echo >/cat <<EOF)\n\
  - Communication: Output text directly (NOT echo/printf)\n\
- **Shell argument quoting and special characters**: The active shell is Bash. When passing \
arguments that contain special characters (parentheses `()`, backticks `` ` ``, dollar signs `$`, \
backslashes `\\`, semicolons `;`, pipes `|`, angle brackets `<>`, ampersands `&`, exclamation marks \
`!`, etc.), you MUST ensure they are properly quoted to prevent Bash from misinterpreting them as \
shell syntax:\n\
  - **Single quotes** `'...'` pass everything literally, but cannot contain a literal single quote.\n\
  - **ANSI-C quoting** `$'...'` supports escape sequences (e.g. `\\n` for newline, `\\'` for single \
quote) and is the safest approach for multi-line strings or strings with single quotes.\n\
  - **Heredoc** is the most robust approach for large, multi-line text with mixed quotes:\n\
    ```bash\n\
    gh pr create --title \"My Title\" --body \"$(cat <<'HEREDOC'\n\
    Multi-line body with (parentheses), backticks, and 'single-quotes'.\n\
    HEREDOC\n\
    )\"\n\
    ```\n\
  - NEVER use unescaped single quotes inside single-quoted strings. Use `$'it\\'s'` or `\"it's\"` \
instead.\n\
  - If unsure, prefer double-quoting arguments and escape inner double-quotes as `\\\"`.\n\
- When issuing multiple commands:\n\
  - If the commands are independent and can run in parallel, make multiple run_command tool calls \
in a single message.\n\
  - If the commands depend on each other and must run sequentially, use a single run_command call \
with `&&` to chain them together (e.g., `git add . && git commit -m \"message\"`). For instance, if \
one operation must complete before another starts (like mkdir before cp, or git add before git \
commit), run these operations sequentially instead.\n\
  - Use `;` only when you need to run commands sequentially but don't care if earlier commands \
fail.\n\
  - DO NOT use newlines to separate commands (newlines are ok in quoted strings).\n\
- The command always runs in the project root. Prefer absolute or root-relative paths and avoid \
`cd`; run tools against their target path directly.\n\
  <good-example>\n\
  cargo nextest run -p mycrate\n\
  </good-example>\n\
  <bad-example>\n\
  cd crates/mycrate && cargo nextest run\n\
  </bad-example>";

#[async_trait::async_trait]
impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Exact bash command to execute as `bash -o pipefail -c \
                            <command>` in the project root, e.g. \"cargo nextest run\" or \"git \
                            status\". Each command is shown to the user for approval before it \
                            runs."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let command = match input.get("command") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => {
                return Err(
                    "invalid input: run_command requires a non-empty string \"command\"".into(),
                );
            }
        };

        let timeout = if ctx.command_timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            ctx.command_timeout_ms
        };

        spawn_and_wait(&command, &ctx.root, timeout).await
    }
}

#[cfg(unix)]
async fn spawn_and_wait(
    command: &str,
    root: &std::path::Path,
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
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group so a timeout can killpg the whole subtree (ADR-0023).
    // Safety: process_group only sets the pgid on fork; no async-signal issues.
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
    _root: &std::path::Path,
    _timeout_ms: u64,
) -> Result<String, String> {
    Err("run_command is only supported on unix".into())
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx::for_test(root.to_path_buf(), 10_000)
    }

    async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
        RunCommand.run(&input, ctx).await
    }

    #[test]
    fn spec_requires_command() {
        let spec = RunCommand.spec();
        assert_eq!(spec.name, "run_command");
        assert_eq!(spec.input_schema["required"], json!(["command"]));
    }

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

    #[tokio::test]
    async fn merges_stderr_into_the_output() {
        let tmp = TempDir::new().unwrap();
        let out = run(json!({"command": "echo oops 1>&2"}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert!(out.contains("oops"));
        assert!(out.contains("[exit code: 0]"));
    }

    #[tokio::test]
    async fn a_command_with_no_output_still_reports_its_exit_code() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            run(json!({"command": "true"}), &ctx(tmp.path())).await,
            Ok("[exit code: 0]".into())
        );
    }

    #[tokio::test]
    async fn nonzero_exit_code_is_an_error_with_output_and_code() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"command": "echo boom; exit 3"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("boom"));
        assert!(err.contains("[exit code: 3]"));
    }

    #[tokio::test]
    async fn pipefail_reports_the_producers_failure_not_the_consumers_success() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"command": "false | cat"}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert!(err.contains("[exit code: 1]"));
    }

    #[tokio::test]
    async fn times_out_per_the_ctx_command_timeout_ms() {
        let tmp = TempDir::new().unwrap();
        let mut c = ctx(tmp.path());
        c.command_timeout_ms = 100;

        assert_eq!(
            run(json!({"command": "sleep 5"}), &c).await,
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
        // A spoof line in the MIDDLE is not the tail, so it is ignored.
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
        assert!(run(json!({"command": ""}), &c).await.is_err());
        assert!(run(json!({"command": 42}), &c).await.is_err());
    }
}
