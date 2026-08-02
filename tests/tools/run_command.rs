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
    assert!(
        spec.description
            .contains("**Background vs Foreground Execution:**")
    );
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

// ---- exit-code badge Artifact (relocated from the run_command extension) ----

async fn run_rich(input: Value, ctx: &ToolCtx) -> crate::tool::ToolOutput {
    RunCommand
        .run_rich(&input, ctx)
        .await
        .expect("run_shell_command always returns Ok(ToolOutput) for a completed run")
}

#[cfg(unix)]
#[tokio::test]
async fn run_rich_attaches_the_exit_code_artifact_on_success() {
    let tmp = TempDir::new().unwrap();
    let output = run_rich(json!({"command": "true"}), &ctx(tmp.path())).await;

    assert!(!output.is_error);
    assert_eq!(output.artifacts.get(keys::EXIT_CODE), Some(&json!(0)));
    assert!(!output.artifacts.contains_key(keys::TIMED_OUT));
}

#[cfg(unix)]
#[tokio::test]
async fn run_rich_reports_a_failed_command_as_ok_with_is_error_and_the_exit_code() {
    // A nonzero-exit command is NOT a tool failure: `run_rich` returns
    // `Ok(ToolOutput { is_error: true, .. })` so the exit-code Artifact rides
    // alongside `is_error` (Option A). The model still sees is_error: true.
    let tmp = TempDir::new().unwrap();
    let output = run_rich(json!({"command": "echo boom; exit 3"}), &ctx(tmp.path())).await;

    assert!(output.is_error);
    assert!(crate::content::result_blocks_text(&output.blocks).contains("boom"));
    assert_eq!(output.artifacts.get(keys::EXIT_CODE), Some(&json!(3)));
}

#[cfg(unix)]
#[tokio::test]
async fn run_rich_marks_a_timeout_and_attaches_no_exit_code() {
    let tmp = TempDir::new().unwrap();
    let mut c = ctx(tmp.path());
    c.command_timeout_ms = 100;

    // A busy loop (not `sleep`, which the sleep guard blocks) exercises the
    // timeout path so the badge reads `✗ timed out`.
    let output = run_rich(json!({"command": "while true; do :; done"}), &c).await;

    assert!(output.is_error);
    assert_eq!(output.artifacts.get(keys::TIMED_OUT), Some(&json!(true)));
    assert!(!output.artifacts.contains_key(keys::EXIT_CODE));
}

// ---- noise-run condensing (relocated from the condense extension) ----

#[cfg(unix)]
#[tokio::test]
async fn condenses_a_long_run_of_compile_progress_in_the_tool_output() {
    // The tool applies condensing to its own model-facing output: a run of >= 5
    // same-class noise lines collapses to its first line plus an exact-count
    // marker. Printed via a single echo so the merged stdout carries the run.
    let tmp = TempDir::new().unwrap();
    let script = "echo '   Compiling a v0.1.0'; \
                  echo '   Compiling b v0.1.0'; \
                  echo '   Compiling c v0.1.0'; \
                  echo '   Compiling d v0.1.0'; \
                  echo '   Compiling e v0.1.0'; \
                  echo done";
    let out = run(json!({"command": script}), &ctx(tmp.path()))
        .await
        .unwrap();

    assert!(out.contains("   Compiling a v0.1.0"));
    assert!(out.contains("[condense: 4 more compile-progress lines omitted]"));
    // The non-noise lines and the exit tail survive verbatim.
    assert!(out.contains("done"));
    assert!(out.ends_with("[exit code: 0]"));
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
    assert_eq!(
        strip_trailing_background_amp("npm run dev &"),
        "npm run dev"
    );
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
