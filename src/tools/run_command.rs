//! `run_command(command)`: runs `sh -c command` in the Project Root with
//! stdout and stderr merged, reporting the exit code. The only tool that
//! requires the user's Approval (see [`crate::tools::requires_approval`]).
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

#[async_trait::async_trait]
impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description:
                "Run a shell command (sh -c) in the project root and return stdout and stderr \
                merged, followed by the exit code. Use this to compile, run tests, or inspect \
                the project after making changes. Long-running commands are killed when they \
                exceed the configured timeout. The user must approve each command before it runs."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to run, e.g. \"mix test\"."
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
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx {
            root: root.to_path_buf(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
            scout: None,
        }
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
    async fn times_out_per_the_ctx_command_timeout_ms() {
        let tmp = TempDir::new().unwrap();
        let mut c = ctx(tmp.path());
        c.command_timeout_ms = 100;

        assert_eq!(
            run(json!({"command": "sleep 5"}), &c).await,
            Err("[command timed out after 100ms]".into())
        );
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
