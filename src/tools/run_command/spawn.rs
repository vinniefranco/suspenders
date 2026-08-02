//! Spawn and execution: the foreground [`spawn_and_wait`] (a process-group-leading
//! child, ADR-0023) and the background [`run_background`] handoff (Phase 9,
//! ADR-0063), plus the exit-code [`report`] tail both paths and the exit-badge
//! extension key on.

use std::process::Stdio;

use crate::tool::ToolCtx;

use super::command_shape::{
    has_top_level_git_commit, strip_shell_wrapper, strip_trailing_background_amp,
};

#[cfg(unix)]
pub(super) async fn spawn_and_wait(
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
    // SAFETY: `pre_exec` requires an async-signal-safe closure because it runs in
    // the child after `fork` and before `exec`, where only async-signal-safe calls
    // are permitted (no allocation, no re-entrant libc). The sole call here is
    // `setpgid(0, 0)`, which POSIX lists as async-signal-safe; it neither allocates
    // nor touches shared parent state, so the required invariant holds. There is no
    // safe stdlib API for setting the child's process group, so the `unsafe` is
    // irreducible.
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
pub(super) async fn spawn_and_wait(
    _command: &str,
    _cwd: &std::path::Path,
    _timeout_ms: u64,
) -> Result<String, String> {
    Err("run_shell_command is only supported on unix".into())
}

/// The BACKGROUND branch (Phase 9, ADR-0063): refuse a top-level `git commit`,
/// strip ONE bare trailing `&`, and hand the processed command + cwd to the Agent
/// through the [`BackgroundShellSpawner`](crate::tool::caps::BackgroundShellSpawner)
/// capability. Returns the "Background shell started." block (OMITTING the pid line
/// and `/tasks` inspect sentence - no such UI here). A capability `Err` (a degraded
/// host) folds into the tool error.
pub(super) async fn run_background(
    command: &str,
    cwd: &std::path::Path,
    ctx: &ToolCtx,
) -> Result<String, String> {
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
    let output_path =
        crate::agent::background_shell::output_path(&ctx.session_dir.to_string_lossy(), &id);
    Ok(format!(
        "Background shell started.\n\
         id: {id}\n\
         output file: {path}\n\
         Read the output file directly to view the captured output.",
        id = id,
        path = output_path.display(),
    ))
}

/// Attach the `[exit code: N]` tail to a command's merged output. The single source
/// for the tail the run_command extension's `present` keys on (its inverse is
/// [`super::parse_exit_code`]).
pub(super) fn report(output: &str, exit_code: i32) -> String {
    if output.is_empty() {
        format!("[exit code: {exit_code}]")
    } else if output.ends_with('\n') {
        format!("{output}[exit code: {exit_code}]")
    } else {
        format!("{output}\n[exit code: {exit_code}]")
    }
}
