---
name: stuck
description: Diagnose a frozen, stuck, or slow suspenders session on this machine. Scans for suspenders processes, high CPU or memory usage, hung child subprocesses, and the session log. Use /stuck or /stuck <PID> to focus on a specific process.
argument-hint: '[PID or symptom]'
allowedTools:
  - run_shell_command
  - read_file
---

# /stuck - diagnose a frozen or slow suspenders session

The user thinks another suspenders session on this machine is frozen, stuck, or very slow. Investigate and present a diagnostic report. suspenders is a single self-contained Rust binary named `suspenders`, so the process name (the `comm` column in `ps`) is `suspenders` directly, not a runtime interpreter.

## What to look for

Scan for other suspenders processes, and exclude the current one (exclude the PID you see running this prompt). Identify suspenders sessions by looking for a process whose command is the `suspenders` binary. Match on the `comm` column being exactly `suspenders`, or the `command` column containing a path segment ending in `/suspenders`. Avoid a loose `suspenders` substring match: it can false positive on an unrelated tool that merely passes a suspenders path as an argument.

Signs of a stuck session:

- **High CPU (>=90%) sustained**: likely an infinite loop. Sample twice, 1 to 2 seconds apart, to confirm it is not a transient spike.
- **Process state `D` (uninterruptible sleep)**: often an I/O hang. The `state` column in `ps` output; the first character matters (ignore modifiers like `+`, `s`, `<`).
- **Process state `T` (stopped)**: the user probably hit Ctrl+Z by accident.
- **Process state `Z` (zombie)**: the parent is not reaping.
- **Very high RSS (>=4GB)**: a possible memory leak making the session sluggish.
- **State `S` with low CPU**: the most common hang signature is a stalled HTTPS request to the model API. Not a process level red flag on its own, but combined with the user reporting "stuck", treat it as a strong signal to run the network check in step 3.
- **Stuck child process**: a hung `git`, shell, or other subprocess spawned by a Tool Call can freeze the parent. suspenders runs shell commands and background shells through `run_shell_command`, so a wedged child is a common cause. Check `pgrep -P <pid>` (then `ps -p` for state, see step 3) for each session.

## Argument validation

If the user gave an argument, treat it as a PID **only if it consists entirely of digits 0-9**. Anything else (letters, whitespace, punctuation) fails the check, in which case treat it as a free text symptom description (guidance for the report only, never substituted into a shell command). The strict digit only whitelist is safer than enumerating shell metacharacters.

## Where suspenders keeps its state

Two locations are useful for diagnosis. Resolve them with the shell before using them:

- **Session logs**: suspenders appends a linear JSONL session log per session (ADR-0010) under `${XDG_DATA_HOME:-$HOME/.local/share}/suspenders/sessions/`, one `*.jsonl` file per session. The most recently modified file is usually the live or last active session. The tail of that file shows what the session was doing before it hung.
- **Config**: the user config is `${XDG_CONFIG_HOME:-$HOME/.config}/suspenders/config.json`, and a project may carry `<project>/.suspenders/config.json`. You rarely need to read these to diagnose a hang, but they confirm where the session was launched from.

```
SESSIONS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/suspenders/sessions"
```

## Investigation steps

**Fast path for targeted diagnosis**: if a digit only PID argument was given, skip the step 1 enumeration. Validate that the PID is a live current user suspenders process before dumping any details:

```
kill -0 <pid> 2>/dev/null || { echo "PID <pid> is dead, or owned by another user"; exit 0; }
ps -p <pid> -o comm=,command= -ww 2>/dev/null | grep -qE '(^suspenders( |$)|/suspenders( |$))' || { echo "PID <pid> is yours but is not a suspenders process, refusing to dump details"; exit 0; }
```

If either guard prints, stop the diagnostic and surface the message verbatim. Otherwise gather stats, then jump to step 3:

```
ps -p <pid> -o pid=,pcpu=,rss=,etime=,state=,comm=,command= -ww
```

Note: the `command=` column may include credentials passed as CLI arguments (for example an API key flag). Redact any such value to `***` before quoting it in the report.

Otherwise (no argument, or a symptom only argument), run the general path below:

1. **List suspenders processes via `ps`** (macOS and Linux). This enriches each session with CPU, RSS, state, and uptime:

   ```
   ps -xo pid=,pcpu=,rss=,etime=,state=,comm=,command= -u "$(id -u)" -ww | grep -E '(^suspenders( |$)|/suspenders( |$)| suspenders( |$))' | grep -v grep
   ```

   `-u "$(id -u)"` restricts the scan to the current user, so on a shared host this avoids exposing another user's process paths and arguments into the chat. `-ww` disables column truncation so a long command line is not cut off. Exclude the PID running this prompt.

   Note: `ps` reports `rss` in **kilobytes** on both macOS and Linux. To report in MB, divide by 1024; to report in GB, divide by 1048576. The 4GB threshold is `4194304` KB. Compare the raw `rss` value against that, or compare the GB value against 4. Do not divide once and then compare against 4; that would flag every process above 4MB as "very high RSS".

   Note: a full command line may contain credentials passed as CLI arguments. Redact any such value to `***` before quoting it in the report.

2. **Cross reference with the session logs**: for each live suspenders PID, the most recently modified `*.jsonl` under `SESSIONS_DIR` is usually the session it is running. List them newest first:

   ```
   ls -t "$SESSIONS_DIR"/*.jsonl 2>/dev/null | head -n 5
   ```

   If nothing lists, the session may predate any log or the sessions directory may be elsewhere; fall through and rely on `ps` alone.

3. **For anything suspicious**, gather more context. If the process state alone explains the problem (`T` = accidentally stopped, `Z` = parent not reaping), skip directly to the report; child, log, and stack inspection add nothing. Otherwise:
   - Child processes (with state, so a hung `git` or shell shows up): `CHILDREN=$(pgrep -P <pid> | tr '\n' ',' | sed 's/,$//'); [ -n "$CHILDREN" ] && ps -p "$CHILDREN" -o pid=,ppid=,pcpu=,state=,etime=,command= -ww`. A single `ps` call (avoids forking one per child) and `-ww` so a long child command line is not truncated.
   - If high CPU: sample again after 1 to 2 seconds to confirm it is sustained.
   - **Network hang**: if CPU is low and the state is `S` despite the user reporting "stuck", the most likely cause is a stalled HTTPS request to the model API. Linux: `ss -tnp 2>/dev/null | grep "pid=<pid>,"`. Note that `ss -tnp`'s `-p` needs root or `CAP_NET_ADMIN`; without it the PID column shows `-` and the grep returns empty, so if you see no matches but `ss -t 2>/dev/null` shows ESTABLISHED sockets, fall back to `lsof -nP -i -p <pid>` rather than reporting "no connections". macOS: `lsof -nP -i -p <pid> 2>/dev/null | head -20` (the `-nP` flags skip reverse DNS and port lookups, which can themselves hang); if `lsof` feels slow, prefix with `timeout 10` (or `gtimeout 10` on macOS with Homebrew coreutils). A long lived `ESTABLISHED` connection to a model host with no recent traffic is the smoking gun.
   - **Session log tail**: use `read_file` (or `tail -n 200`) on the most recently modified `*.jsonl` in `SESSIONS_DIR` to see the last events the session recorded before hanging. The session log can be large, so bound the read with `tail -n 200 <path>`. A session log may contain prompts and file contents; paste only lines relevant to the hang, and never quote secrets or API keys you happen to see.

4. **Consider a stack dump** for a truly frozen process (advanced, optional):
   - Linux: `cat /proc/<pid>/stack` for the kernel stack (read only, no `ptrace` permission needed). Avoid `strace -p` for this purpose: it needs `CAP_SYS_PTRACE` (often denied under `kernel.yama.ptrace_scope=1`), and `strace -c` blocks until the target exits, so it would hang on the very kind of stuck process you are diagnosing.
   - macOS: `sample <pid> 3` gives a 3 second native stack sample. If `sample` itself seems to hang, wrap it: `timeout 15 sample <pid> 3` (or `gtimeout 15 ...` on Homebrew coreutils). Stack frames may include function arguments containing API keys or tokens held in memory; redact any such value to `***` before including the dump in the report.
   - This is big, so only grab it if the process is clearly hung and you want to know _why_.

## Report

Present a structured diagnostic report directly to the user with these sections:

**For each stuck or slow session found:**

- PID, CPU%, RSS (in MB), process state, uptime, full command line
- Child processes and their states
- Your diagnosis of what is likely wrong
- Relevant session log tail if you captured it
- Stack dump output if you captured it
- A suggested next step for the user to decide (for example "the user may consider `kill <pid>` if the session is unresponsive", "likely waiting on I/O, check disk", "accidentally stopped, the user can resume with `kill -CONT <pid>`"). Do not execute these actions yourself; present them as options for the user.

**If every session looks healthy**, tell the user directly, no diagnostic dump needed. Mention how many sessions you checked and that none showed signs of being stuck.

**If no sessions are found at all** (zero matching `ps` rows), say so explicitly: which `SESSIONS_DIR` you searched and that `ps` returned no suspenders processes for the current user. Suggest the session may have already exited.

## Notes

- Do not kill or signal any process; this is diagnostic only.
- If the user gave an argument (a specific PID or symptom), focus there first.
