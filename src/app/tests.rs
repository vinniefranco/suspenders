// ===========================================================================
// Tests for the bootstrap's extracted seams: the pure headless event
// formatter (`event_lines`), the pure bare-`--resume` decision
// (`resolve_resume`), and the headless drive loop (`drive`) over an Agent
// spawned on the FakeLlm double (the same pattern as agent/tests.rs). The
// terminal-touching edges - println!, pick_session - stay untested by design;
// everything they decide is here.
// ===========================================================================
use super::*;
use crate::content::{ContentBlock, Usage};
use crate::event::WaveStats;
use crate::llm::response::{Response, StopReason};
use crate::session::SessionConfig;
use crate::test_support::{Entry, FakeLlm};
use serde_json::json;
use std::collections::HashMap;
use tempfile::TempDir;

// ---- event_lines: one assertion per event kind -------------------------

#[test]
fn message_start_prints_the_pass_header_with_elapsed_seconds() {
    // Elapsed rides as a number and is formatted to one decimal here, so the
    // formatter never touches a clock.
    let lines = event_lines(&Event::MessageStart { pass: 3 }, 1.26);
    assert_eq!(lines, vec!["\n-- pass 3 (t=1.3s) model call".to_string()]);
}

#[test]
fn message_update_prints_nothing() {
    let event = Event::MessageUpdate {
        delta: crate::llm::Delta::Text("hi".into()),
        content: vec![ContentBlock::text("hi")],
    };
    assert!(event_lines(&event, 0.0).is_empty());
}

#[test]
fn message_end_lists_tool_names_and_the_joined_text() {
    let event = Event::MessageEnd {
        content: vec![
            ContentBlock::text("first"),
            ContentBlock::tool_use("t1", "read_file", json!({})),
            ContentBlock::text("second"),
        ],
        stop_reason: StopReason::ToolUse,
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec![
            "   message_end (t=0.0s) tools=[\"read_file\"]".to_string(),
            "   text: first | second".to_string(),
        ]
    );
}

#[test]
fn message_end_without_text_prints_only_the_tools_line() {
    let event = Event::MessageEnd {
        content: vec![ContentBlock::tool_use("t1", "list_files", json!({}))],
        stop_reason: StopReason::ToolUse,
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec!["   message_end (t=0.0s) tools=[\"list_files\"]".to_string()]
    );
}

#[test]
fn tool_call_prints_the_name_and_the_input() {
    let event = Event::ToolCall {
        id: "t1".into(),
        name: "read_file".into(),
        input: json!({"path": "a.txt"}),
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec!["   -> read_file {\"path\":\"a.txt\"} (t=0.0s)".to_string()]
    );
}

#[test]
fn tool_result_flags_ok_and_err_with_the_byte_count() {
    let result = |is_error| Event::ToolResult {
        id: "t1".into(),
        name: "read_file".into(),
        content: "hello".into(),
        is_error,
        artifacts: HashMap::new(),
    };
    assert_eq!(
        event_lines(&result(false), 0.0),
        vec!["   <- ok 5B (t=0.0s): hello".to_string()]
    );
    assert_eq!(
        event_lines(&result(true), 0.0),
        vec!["   <- ERR 5B (t=0.0s): hello".to_string()]
    );
}

#[test]
fn context_pressure_prints_the_budget_numbers_and_the_dead_mass_percent() {
    let event = Event::ContextPressure {
        token_estimate: 1000,
        context_budget: 2000,
        max_tokens_reserve: 300,
        dead_mass: 0.25,
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec![
            "   ## pressure token_estimate=1000 context_budget=2000 max_tokens_reserve=300 (dead_mass=25%) (t=0.0s)"
                .to_string()
        ]
    );
}

#[test]
fn eviction_wave_prints_the_per_kind_counts() {
    let event = Event::EvictionWave {
        stats: WaveStats {
            results_elided: 1,
            cmd_superseded: 2,
            read_superseded: 3,
            edits_husked: 4,
            anchors_elided: 5,
            dead_mass: 0.5,
        },
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec![
            "   ## EVICTION wave: results=1 cmd_superseded=2 read_superseded=3 edit_husked=4 anchors=5 (dead_mass=50%) (t=0.0s)"
                .to_string()
        ]
    );
}

#[test]
fn compaction_progress_prints_the_status() {
    let event = Event::CompactionProgress {
        status: "start".into(),
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec!["\n   ## COMPACTION start (t=0.0s)".to_string()]
    );
}

#[test]
fn approval_request_prints_the_command_being_auto_approved() {
    // The formatter only says what happens; the approve() call itself lives
    // at the handle_event edge.
    let event = Event::ApprovalRequest {
        approval_id: "a1".into(),
        command: "ls -la".into(),
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec!["   ?? approval for: ls -la -- auto-approving".to_string()]
    );
}

#[test]
fn run_finished_prints_the_stop_reason_and_the_estimates() {
    let event = Event::RunFinished {
        stop_reason: StopReason::EndTurn,
        token_estimate: 123,
        context_budget: 456,
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec![
            "\n== turn_finished (t=0.0s): stop_reason=end_turn token_estimate=123 context_budget=456"
                .to_string()
        ]
    );
}

#[test]
fn run_error_prints_the_reason() {
    let event = Event::RunError {
        reason: "boom".into(),
    };
    assert_eq!(
        event_lines(&event, 0.0),
        vec!["\n== TURN ERROR (t=0.0s): boom".to_string()]
    );
}

#[test]
fn run_cancelled_prints_the_marker() {
    assert_eq!(
        event_lines(&Event::RunCancelled, 0.0),
        vec!["\n== turn_cancelled (t=0.0s)".to_string()]
    );
}

#[test]
fn an_unhandled_event_prints_its_truncated_debug_form() {
    // The catch-all keeps a new event visible in the stream (never silent)
    // without the formatter having to know it.
    let event = Event::RunStarted("t1".into());
    assert_eq!(
        event_lines(&event, 0.0),
        vec!["   .. RunStarted(\"t1\") (t=0.0s)".to_string()]
    );
}

// ---- resolve_resume: the bare --resume picker decision ------------------

#[test]
fn a_concrete_resume_value_passes_through_untouched() {
    // No picker ran, so the outcome is None.
    assert_eq!(
        resolve_resume(Some("latest".into()), None),
        ResumeAction::Start(Some("latest".into()))
    );
}

#[test]
fn no_resume_argument_starts_fresh() {
    assert_eq!(resolve_resume(None, None), ResumeAction::Start(None));
}

#[test]
fn bare_resume_with_no_sessions_to_pick_from_is_silently_a_fresh_start() {
    assert_eq!(
        resolve_resume(Some(PICK.into()), None),
        ResumeAction::Start(None)
    );
}

#[test]
fn picking_a_session_resumes_from_its_path() {
    assert_eq!(
        resolve_resume(
            Some(PICK.into()),
            Some(PickerOutcome::Resume("/logs/s1.jsonl".into()))
        ),
        ResumeAction::Start(Some("/logs/s1.jsonl".into()))
    );
}

#[test]
fn declining_the_picker_starts_a_fresh_session() {
    assert_eq!(
        resolve_resume(Some(PICK.into()), Some(PickerOutcome::FreshSession)),
        ResumeAction::Start(None)
    );
}

#[test]
fn quitting_the_picker_leaves_without_starting_the_agent() {
    assert_eq!(
        resolve_resume(Some(PICK.into()), Some(PickerOutcome::Quit)),
        ResumeAction::Quit
    );
}

// ---- drive: the headless per-prompt loop over a FakeLlm Agent -----------

fn session_in(dir: &TempDir) -> Session {
    let root = dir.path().to_string_lossy().into_owned();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .expect("session builds")
}

fn start(session: Session, fake: FakeLlm) -> AgentHandle {
    AgentHandle::start(
        StartOpts::new(session, Arc::new(fake)).with_system_prompt("You are a test agent."),
    )
    .expect("agent starts")
}

fn text_end(text: &str) -> Response {
    Response {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
        error: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scripted_run_settles_and_the_drive_loop_terminates() {
    let dir = TempDir::new().unwrap();
    let fake = FakeLlm::script(vec![Entry::just(text_end("done"))]);
    let agent = start(session_in(&dir), fake);

    let mut lines: Vec<String> = Vec::new();
    drive(&agent, "r", vec!["hello".into()], &mut |l| lines.push(l))
        .await
        .expect("drive settles");

    // The submit banner, the settlement line, and the estimate trio all
    // flowed through the seam - and the loop returned (no hang on settle).
    assert_eq!(lines[0], "\n== submit (root=r): hello");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("\n== turn_finished") && l.contains("stop_reason=end_turn"))
    );
    assert!(lines.iter().any(|l| l.starts_with("   token_estimate=")));
    assert!(lines.iter().any(|l| l.starts_with("   messages=")));
    assert!(lines.iter().any(|l| l.starts_with("   plan=")));
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_prompts_drive_the_default_evaluate_run() {
    let dir = TempDir::new().unwrap();
    let fake = FakeLlm::script(vec![Entry::just(text_end("done"))]);
    let agent = start(session_in(&dir), fake);

    let mut lines: Vec<String> = Vec::new();
    drive(&agent, "r", vec![], &mut |l| lines.push(l))
        .await
        .expect("drive settles");

    // drive.exs's default: no prompts means one "evaluate this project" Run.
    assert_eq!(lines[0], "\n== submit (root=r): evaluate this project");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_submit_prints_the_skip_line_and_continues() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut in_flight) = Entry::barrier();
    let agent = start(session_in(&dir), FakeLlm::script(vec![barrier]));

    // Park a Run mid-complete so the Agent is genuinely busy; the InFlight
    // must stay alive through the drive call or the parked Run errors out
    // (and the Agent goes Idle) before drive submits.
    agent.submit("park here").await.unwrap();
    let _parked = in_flight.recv().await.expect("turn parks in complete");

    let mut lines: Vec<String> = Vec::new();
    drive(&agent, "r", vec!["second".into()], &mut |l| lines.push(l))
        .await
        .expect("drive continues past the busy submit");

    assert_eq!(
        lines,
        vec![
            "\n== submit (root=r): second".to_string(),
            "!! agent busy; skipping".to_string(),
        ]
    );
}
