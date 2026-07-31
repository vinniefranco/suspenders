//! Background shell registry (Phase 9, ADR-0063) - the process-owning sibling of
//! the background subagent registry ([`super::background`]).
//!
//! A background shell is a detached OS subprocess: `run_command` with
//! `is_background: true` hands the Agent the already-processed command + resolved
//! cwd, and the AGENT (the single owner of mutable Session state, ADR-0017) spawns
//! the detached child, owns its wait+stream+capture-file, and settles a parallel
//! registry keyed by the minted shell id. A settling child hands its outcome back
//! through the SAME mpsc every Run event travels (a
//! [`super::RunMsg::BackgroundShellDone`]), so the single owner serializes the
//! registry mutation with everything else. The settlement notification reuses the
//! background subagent envelope ([`super::background::task_notification`]) and the
//! same notifications queue.
//!
//! Turn survival (ADR-0023 + ADR-0063): a background shell OUTLIVES the launching
//! turn. The detached watcher task does NOT observe the turn abort signal and the
//! child is NOT `kill_on_drop` - a user pressing Ctrl+C on a turn must not kill a
//! dev server / watcher they intentionally backgrounded. Cancellation flows ONLY
//! through `task_stop` (which killpg's the process group) or the actor-loop-exit
//! abortAll (the Session is ending). The child leads its own process group
//! (`setpgid`, ADR-0023) so a stop signals the whole subtree, not just the direct
//! child, and no orphaned grandchildren leak.
//!
//! Fidelity fallback (documented): qwen's background path refuses `git commit`
//! because its foreground completion path writes AI-attribution `git notes` that
//! the background lifecycle cannot hook. Suspenders has NO such notes path today,
//! but the refusal (and its verbatim wording) is kept so the model-facing contract
//! matches qwen exactly - a `git commit` still belongs on the foreground path.

use std::path::PathBuf;

use crate::agent::{AgentState, Msg, RunMsg};
use crate::event::Event;
use crate::session::log::Entry as LogEntry;

use super::background::task_notification;

/// One background shell the Agent is tracking (Phase 9, ADR-0063). The `abort`
/// handle cancels the detached watcher task at actor-loop exit; `pgid` is the
/// child's process-group id a `task_stop` killpg's; `status` is where it is in its
/// lifecycle; `command` is carried for the `<summary>` (the model's own command
/// text); `output_path` is the capture file the model Reads to view the output.
pub struct BackgroundShell {
    /// The detached watcher task's abort handle: cancels it at actor-loop exit.
    /// A `task_stop` sets the status to [`ShellStatus::Cancelled`] first, so the
    /// later [`RunMsg::BackgroundShellDone`] (if the killpg loses the race) is
    /// dropped.
    pub abort: tokio::task::AbortHandle,
    /// The child's process-group id (its own pid, since it leads its group). A
    /// `task_stop` signals `-pgid` so the whole subtree dies, not just the direct
    /// child. `None` only if the pid could not be read at spawn.
    pub pgid: Option<i32>,
    /// Where the shell is in its lifecycle.
    pub status: ShellStatus,
    /// The processed command text, carried for the notification `<summary>`.
    pub command: String,
    /// The capture file the model Reads to view the shell's output.
    pub output_path: PathBuf,
}

/// A background shell's lifecycle (Phase 9, ADR-0063). `Running` is the live
/// state a `task_stop` or a settling child transitions out of; `Completed`/
/// `Failed` are the settled terminal states; `Cancelled` is the state a
/// `task_stop` sets synchronously (so a racing `BackgroundShellDone` is dropped
/// rather than double-notifying).
pub enum ShellStatus {
    /// The child process is in flight.
    Running,
    /// The child exited 0 (success).
    Completed,
    /// The child exited non-zero, was killed by a signal, or failed to spawn.
    Failed,
    /// A `task_stop` cancelled the child (the killpg was requested).
    Cancelled,
}

/// A settled background shell's outcome (Phase 9, ADR-0063): the terminal facts
/// the watcher task hands back so [`AgentState::background_shell_done`] can map to
/// completed/failed. `exit_code` is the process exit status (`None` if killed by a
/// signal or never spawned); `signalled` is whether a signal terminated it;
/// `spawn_error` carries the OS error text when the child could not start at all.
pub struct ShellOutcome {
    pub exit_code: Option<i32>,
    pub signalled: bool,
    pub spawn_error: Option<String>,
}

/// Mints a background shell id (Phase 9, ADR-0063): `bg_{n}`. The `n` is the
/// Agent's monotonic per-Session counter, so ids never collide within a Session.
/// Kept a pure fn so the id shape is testable.
pub fn mint_shell_id(n: u64) -> String {
    format!("bg_{n}")
}

/// The capture-file path a background shell streams its output to (Phase 9,
/// ADR-0063): `<session_dir>/background-shells/<id>.output`. The SINGLE source of
/// this shape, so the path `run_command`'s "output file:" line reports can never
/// drift from the file the watcher actually writes.
pub fn output_path(session_dir: &str, id: &str) -> PathBuf {
    std::path::Path::new(session_dir)
        .join("background-shells")
        .join(format!("{id}.output"))
}

/// The background-shell lifecycle handlers the Agent's actor loop delegates to
/// (Phase 9, ADR-0063). They live here beside the registry vocabulary they drive
/// over (a launch, a settlement, a stop, and the actor-loop-exit abortAll), so the
/// actor file keeps the `RunMsg` dispatch and this module owns the process +
/// registry mechanics. Each takes `&mut self` (the Agent's single-owner state), so
/// the map never leaves the owning task (ADR-0017).
impl AgentState {
    /// A background-shell launch (Phase 9, ADR-0063): mint the id, build the
    /// capture file under `<session_dir>/background-shells/<id>.output`, spawn the
    /// DETACHED child (its own process group), stream stdout+stderr into the
    /// capture file (ANSI-stripped), register the entry Running, and return the id.
    /// The watcher task posts a [`RunMsg::BackgroundShellDone`] back over the SAME
    /// mpsc every Run event travels when the child exits, so the single owner
    /// serializes the settlement. On a spawn failure the entry is never registered
    /// and the Err string is returned so the tool folds it into its own error.
    pub(super) fn spawn_background_shell(&mut self, command: String, cwd: String) -> String {
        self.background_shell_counter += 1;
        let id = mint_shell_id(self.background_shell_counter);

        // The capture file path (shared shape, so the reported path can never drift
        // from the real one). Its parent is the per-Session background-shells dir.
        let output_path = output_path(&self.session.session_dir, &id);
        if let Some(output_dir) = output_path.parent()
            && let Err(err) = std::fs::create_dir_all(output_dir)
        {
            // Undo the counter bump so the id space is not skipped on a pure
            // filesystem failure, and return the Err for the tool to fold.
            self.background_shell_counter -= 1;
            return format!("could not create background-shells directory: {err}");
        }

        let tx = self.self_tx.clone();
        let done_id = id.clone();
        let spawn = spawn_detached_shell(&command, &cwd, output_path.clone(), tx, done_id);
        let (abort, pgid) = match spawn {
            Ok(pair) => pair,
            Err(err) => {
                self.background_shell_counter -= 1;
                return format!("could not run background command: {err}");
            }
        };

        self.background_shells.insert(
            id.clone(),
            BackgroundShell {
                abort,
                pgid,
                status: ShellStatus::Running,
                command,
                output_path,
            },
        );

        id
    }

    /// A background shell settled (Phase 9, ADR-0063): if the entry is still
    /// Running, record its terminal status, queue the `<task-notification>` for
    /// the next Run to drain, log it as a durable user-role entry, and broadcast
    /// the finished Event. If the entry is already Cancelled (a `task_stop`
    /// cancelled it), drop the outcome - the `was cancelled` notification was
    /// queued synchronously at stop.
    pub(super) fn background_shell_done(&mut self, id: String, outcome: ShellOutcome) {
        let Some(entry) = self.background_shells.get_mut(&id) else {
            return; // Unknown/pruned id: nothing to settle.
        };
        if !matches!(entry.status, ShellStatus::Running) {
            return; // Already Cancelled/settled: drop the racing outcome.
        }

        // A clean exit 0 completes; anything else (non-zero exit, a signal, a
        // spawn error) fails. The completed result names the capture file so the
        // model can Read it; the failed result carries the exit/signal/spawn-error
        // detail. A cancelled shell never reaches here (dropped above), so its
        // empty-result path lives only in `stop_background_shell`.
        let output_path = entry.output_path.display().to_string();
        let command = entry.command.clone();
        let (status_word, result_text) = if outcome.spawn_error.is_none()
            && !outcome.signalled
            && outcome.exit_code == Some(0)
        {
            entry.status = ShellStatus::Completed;
            (
                "completed",
                format!("Background shell exited 0. Output captured at: {output_path}"),
            )
        } else {
            entry.status = ShellStatus::Failed;
            let detail = if let Some(err) = &outcome.spawn_error {
                format!("spawn error: {err}")
            } else if outcome.signalled {
                "terminated by signal".to_string()
            } else {
                format!("exited with code {}", outcome.exit_code.unwrap_or(-1))
            };
            (
                "failed",
                format!("Background shell {detail}. Output captured at: {output_path}"),
            )
        };

        let notification = task_notification(&id, status_word, &command, &result_text);
        self.notifications.push(notification.clone());
        super::log_entry(self, LogEntry::UserText(notification.clone()));
        super::broadcast(self, Event::background_notification(notification));
        super::broadcast(self, Event::background_task_finished(id, status_word));
    }

    /// A background-shell stop request (Phase 9, ADR-0063): killpg the process
    /// group, set the entry Cancelled, queue the `was cancelled` notification
    /// SYNCHRONOUSLY (with NO `<result>` tag - the shell carried no result to
    /// relay), and return the VERBATIM `task_stop` wording. Returns `None` when no
    /// shell owns the id, so the dual-registry handler can fall through to the
    /// verbatim not-found synthesis (NO string-sniffing). The two hit legs -
    /// found+running (stop confirmation) and found+not-running (the not-running
    /// error) - are VERBATIM qwen.
    pub(super) fn stop_background_shell(&mut self, id: String) -> Option<String> {
        let entry = self.background_shells.get_mut(&id)?;
        if !matches!(entry.status, ShellStatus::Running) {
            let status = shell_status_word(&entry.status);
            return Some(format!(
                "Error: Background agent \"{id}\" is not running (status: {status})."
            ));
        }

        // Signal the whole process group so the child + its grandchildren die, not
        // just the direct child (ADR-0023). Then abort the watcher task and mark
        // the entry Cancelled so the racing `BackgroundShellDone` (if the killpg
        // loses the race to the child's own exit) is dropped.
        killpg(entry.pgid);
        entry.abort.abort();
        entry.status = ShellStatus::Cancelled;
        let command = entry.command.clone();

        // Queue the terminal `was cancelled` notification synchronously with an
        // EMPTY result (no `<result>` tag) - the killpg means no
        // `BackgroundShellDone` will arrive to carry an outcome.
        let notification = task_notification(&id, "cancelled", &command, "");
        self.notifications.push(notification.clone());
        super::log_entry(self, LogEntry::UserText(notification.clone()));
        super::broadcast(self, Event::background_notification(notification));
        super::broadcast(self, Event::background_task_finished(id.clone(), "cancelled"));

        Some(format!(
            "Cancellation requested for background agent \"{id}\". A final \
             task-notification carrying the agent's last result will follow.\n\
             Description: {command}"
        ))
    }

    /// killpg + abort every tracked background shell at actor-loop exit (Phase 9,
    /// ADR-0063): the Session is ending, so a still-running detached child must not
    /// outlive it. Unlike a `task_stop` this is unconditional teardown - signal the
    /// group, abort the watcher, and drop the entries with the state.
    pub(super) fn abort_all_background_shells(&mut self) {
        for entry in self.background_shells.values() {
            killpg(entry.pgid);
            entry.abort.abort();
        }
        self.background_shells.clear();
    }
}

/// The lifecycle word qwen's `not-running` error shows (status: {status}).
fn shell_status_word(status: &ShellStatus) -> &'static str {
    match status {
        ShellStatus::Running => "running",
        ShellStatus::Completed => "completed",
        ShellStatus::Failed => "failed",
        ShellStatus::Cancelled => "cancelled",
    }
}

/// Signal the whole process group (`kill -- -PGID`) with SIGKILL. A `None` pgid
/// (the pid was unreadable at spawn) is a no-op. Unix-only.
#[cfg(unix)]
fn killpg(pgid: Option<i32>) {
    if let Some(pgid) = pgid {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pgid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[cfg(not(unix))]
fn killpg(_pgid: Option<i32>) {}

/// Append an ANSI-stripped chunk to the capture file, dropping the file on a write
/// failure (log+drop): a disk-full / fs-gone error truncates the capture but never
/// kills the watcher. The chunk is UTF-8-lossily decoded before stripping (a
/// background dev-server / build tool emits text, not binary).
#[cfg(unix)]
fn append_chunk(file: &mut Option<std::fs::File>, bytes: &[u8]) {
    use std::io::Write;
    if let Some(f) = file.as_mut() {
        let text = crate::text::strip_ansi(&String::from_utf8_lossy(bytes));
        if f.write_all(text.as_bytes()).is_err() {
            *file = None;
        }
    }
}

/// Spawns the detached background child and its watcher task (Phase 9, ADR-0023 +
/// ADR-0063). The child leads its OWN process group; its stdout+stderr stream
/// INCREMENTALLY (via `.take()` + `AsyncReadExt`, NOT `wait_with_output` which
/// buffers to exit) into the capture file, each chunk ANSI-stripped, with a
/// log+drop error handler on write failure. On child exit the watcher builds a
/// [`ShellOutcome`] and posts a [`RunMsg::BackgroundShellDone`] back. Deliberately
/// NO `kill_on_drop` and NO turn-abort observation - a background shell outlives
/// its launching turn. Returns the watcher's AbortHandle + the child's pgid.
#[cfg(unix)]
fn spawn_detached_shell(
    command: &str,
    cwd: &str,
    output_path: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<Msg>,
    done_id: String,
) -> Result<(tokio::task::AbortHandle, Option<i32>), std::io::Error> {
    use std::process::Stdio;

    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-o")
        .arg("pipefail")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group so a `task_stop`/abortAll can killpg the whole subtree
    // (ADR-0023). Safety: the `pre_exec` closure runs post-fork/pre-exec in the
    // child; the sole call is `setpgid`, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                .map_err(std::io::Error::from)?;
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let pgid = child.id().map(|p| p as i32);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // The detached watcher task: incrementally drain stdout+stderr into the capture
    // file, then await the child's exit and post the outcome. A plain
    // `tokio::spawn` (fire-and-forget) - the Agent keeps only the AbortHandle.
    let handle = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        // Drain both pipes CONCURRENTLY via `select!`: each iteration awaits a read
        // on whichever pipe is ready first (guarded by `if out.is_some()` /
        // `if err.is_some()` so an exhausted stream drops out of the select). This
        // must NOT read the two pipes sequentially: a child that writes >64KB to one
        // stream while the other stays open-and-idle would deadlock a sequential
        // loop (the idle-pipe read parks, the full-pipe write blocks the child).
        // qwen drains both `on('data')` streams simultaneously; `select!` is the
        // Rust analog. One writer, no lock - only one arm runs per iteration.
        //
        // Each ANSI-stripped chunk appends to the capture file, and a write failure
        // drops the file (log+drop) so a disk-full / fs-gone error never kills the
        // watcher or the CLI session - the process keeps running and still settles
        // via `wait`. There is no log facility here and this runs under a TUI
        // (stderr would corrupt the display), so the drop is silent.
        let mut file = std::fs::File::create(&output_path).ok();
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        let mut out = stdout;
        let mut err = stderr;
        loop {
            tokio::select! {
                res = async { out.as_mut().unwrap().read(&mut out_buf).await }, if out.is_some() => {
                    match res {
                        Ok(0) => out = None,
                        Ok(n) => append_chunk(&mut file, &out_buf[..n]),
                        Err(_) => out = None,
                    }
                }
                res = async { err.as_mut().unwrap().read(&mut err_buf).await }, if err.is_some() => {
                    match res {
                        Ok(0) => err = None,
                        Ok(n) => append_chunk(&mut file, &err_buf[..n]),
                        Err(_) => err = None,
                    }
                }
                else => break,
            }
        }

        // The streams are drained (both closed); the child has produced all its
        // output. Await the exit status to build the outcome.
        let outcome = match child.wait().await {
            Ok(status) => {
                use std::os::unix::process::ExitStatusExt;
                ShellOutcome {
                    exit_code: status.code(),
                    signalled: status.signal().is_some(),
                    spawn_error: None,
                }
            }
            Err(err) => ShellOutcome {
                exit_code: None,
                signalled: false,
                spawn_error: Some(err.to_string()),
            },
        };

        let _ = tx.send(Msg::Run(RunMsg::BackgroundShellDone {
            id: done_id,
            outcome,
        }));
    });

    Ok((handle.abort_handle(), pgid))
}

#[cfg(not(unix))]
fn spawn_detached_shell(
    _command: &str,
    _cwd: &str,
    _output_path: PathBuf,
    _tx: tokio::sync::mpsc::UnboundedSender<Msg>,
    _done_id: String,
) -> Result<(tokio::task::AbortHandle, Option<i32>), std::io::Error> {
    Err(std::io::Error::other(
        "background shells are only supported on unix",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_shell_id_is_bg_n() {
        assert_eq!(mint_shell_id(1), "bg_1");
        assert_eq!(mint_shell_id(7), "bg_7");
    }
}
