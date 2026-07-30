// ===========================================================================
// Tests - ported 1:1 from baud/test/baud/agent_test.exs (ADR-0017). baud's
// process primitives translate to their tokio analogs, preserving OBSERVABLE
// behavior: `assert_receive` → a broadcast recv with a timeout helper;
// `GenServer.call` → the request/reply Commands; `spawn` + `Process.monitor`
// for the dead-subscriber test → tokio's auto-cleaning broadcast (a dropped
// Receiver is pruned on the next send), noted where it adapts baud's monitor.
// The busy/steer/cancel handshakes use the FakeLlm `Barrier` entry: the test
// observes the Run parked mid-`complete`, then releases (or aborts) it.
// ===========================================================================
use super::*;
use crate::approvals::ApprovalMode;
use crate::content::{ContentBlock, Message, Role, Usage};
use crate::llm::model::Api;
use crate::llm::response::{Response, StopReason as RStop};
use crate::llm::{Delta, LlmRequest};
use crate::session::{Session, SessionConfig, SessionOpts};
use crate::test_support::{Entry, FakeLlm, InFlight, Release};
use serde_json::{Value, json};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::broadcast::error::RecvError;

// ---- harness ----------------------------------------------------------

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

// Starts an Agent over the given FakeLlm with a fixed test system prompt (so
// the ported conversation assertions don't depend on the Voice default).
fn start(session: Session, fake: FakeLlm) -> AgentHandle {
    AgentHandle::start(
        StartOpts::new(session, Arc::new(fake)).with_system_prompt("You are a test agent."),
    )
    .expect("agent starts")
}

// Starts an Agent over the DEFAULT (Voice) system prompt - the compaction
// test needs the real prompt's bulk in the token estimate so the small
// Runs cross the compaction target (baud's agent runs the default prompt +
// context files; the Rust test uses the Voice default alone).
fn start_voiced(session: Session, fake: FakeLlm) -> AgentHandle {
    AgentHandle::start(StartOpts::new(session, Arc::new(fake))).expect("agent starts")
}

fn text_result(text: &str, stop: RStop) -> Response {
    Response {
        content: vec![ContentBlock::text(text)],
        stop_reason: stop,
        usage: Usage::default(),
        error: None,
    }
}

fn text_end(text: &str) -> Response {
    text_result(text, RStop::EndTurn)
}

fn tool_use_result(id: &str, name: &str, input: Value) -> Response {
    Response {
        content: vec![ContentBlock::tool_use(id, name, input)],
        stop_reason: RStop::ToolUse,
        usage: Usage::default(),
        error: None,
    }
}

// baud's assert_receive: pull events off a broadcast Receiver until one
// matches the predicate or the deadline passes. Skips Lagged (never in
// these tests) and returns the matched event.
async fn recv_match(rx: &mut broadcast::Receiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) => {
                if pred(&ev) {
                    return ev;
                }
            }
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => panic!("event channel closed"),
            Err(_) => panic!("timed out waiting for an event"),
        }
    }
}

// Asserts NO matching event arrives within a short window (baud's
// refute_receive / refute_received).
async fn refute_match(rx: &mut broadcast::Receiver<Event>, pred: impl Fn(&Event) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) => {
                if pred(&ev) {
                    panic!("unexpectedly received a matching event: {ev:?}");
                }
            }
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) => return,
            Err(_) => return,
        }
    }
}

fn is_run_started(e: &Event) -> bool {
    matches!(e, Event::RunStarted(_))
}
fn is_run_finished(e: &Event) -> bool {
    matches!(e, Event::RunFinished { .. })
}

// Builds the canonical three-line test harness (dir + agent + rx) for the
// most common case: a fresh tmp dir, `session_in`, `start`, and subscribe.
// Returns the dir so the caller keeps it alive for the test's duration.
fn harness(entries: Vec<Entry>) -> (TempDir, AgentHandle, broadcast::Receiver<Event>) {
    let dir = TempDir::new().unwrap();
    let agent = start(session_in(&dir), FakeLlm::script(entries));
    let rx = agent.subscribe();
    (dir, agent, rx)
}

// Like `harness` but also returns the session (needed when tests inspect
// session facts such as `session_dir` or `model.provenance()`).
fn harness_with_session(
    entries: Vec<Entry>,
) -> (TempDir, Session, AgentHandle, broadcast::Receiver<Event>) {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let agent = start(session.clone(), FakeLlm::script(entries));
    let rx = agent.subscribe();
    (dir, session, agent, rx)
}

// Generic session harness: caller supplies the Session builder.
fn session_harness(
    session: Session,
    entries: Vec<Entry>,
) -> (AgentHandle, broadcast::Receiver<Event>) {
    let agent = start(session, FakeLlm::script(entries));
    let rx = agent.subscribe();
    (agent, rx)
}

// Extracts the approval_id from an ApprovalRequest event. Used wherever tests
// receive an ApprovalRequest and immediately need its id for approve/deny.
fn approval_id(ev: Event) -> String {
    match ev {
        Event::ApprovalRequest { approval_id, .. } => approval_id,
        _ => panic!("expected ApprovalRequest, got {ev:?}"),
    }
}

// ---- subscribe + submit happy path -----------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn relays_deltas_in_order_updates_the_conversation_returns_to_idle() {
    let dir = TempDir::new().unwrap();
    let fake = FakeLlm::script(vec![Entry::response(
        vec![
            Delta::Thinking("let me think".into()),
            Delta::Text("Hel".into()),
            Delta::Text("lo".into()),
        ],
        text_end("Hello"),
    )]);
    let session = session_in(&dir);
    let provenance = session.model.provenance();
    let agent = start(session, fake);
    let mut rx = agent.subscribe();

    assert_eq!(agent.status().await, Status::Idle);
    agent.submit("hi there").await.unwrap();

    let started = recv_match(&mut rx, is_run_started).await;
    assert!(matches!(started, Event::RunStarted(_)));

    recv_match(&mut rx, |e| matches!(e, Event::MessageStart { pass: 1 })).await;
    recv_match(&mut rx, |e| {
        matches!(e, Event::MessageUpdate { delta: Delta::Thinking(t), .. } if t == "let me think")
    })
    .await;
    recv_match(
        &mut rx,
        |e| matches!(e, Event::MessageUpdate { delta: Delta::Text(t), .. } if t == "Hel"),
    )
    .await;
    let last_update = recv_match(
        &mut rx,
        |e| matches!(e, Event::MessageUpdate { delta: Delta::Text(t), .. } if t == "lo"),
    )
    .await;
    if let Event::MessageUpdate { content, .. } = last_update {
        assert_eq!(content.last(), Some(&ContentBlock::text("Hello")));
    }

    recv_match(&mut rx, |e| {
        matches!(
            e,
            Event::MessageEnd {
                stop_reason: RStop::EndTurn,
                ..
            }
        )
    })
    .await;

    let finished = recv_match(&mut rx, is_run_finished).await;
    if let Event::RunFinished {
        stop_reason,
        token_estimate,
        context_budget,
    } = finished
    {
        assert_eq!(stop_reason, RStop::EndTurn);
        assert!(context_budget > 0);
        let _ = token_estimate; // >= 0 always for u64
    }

    assert_eq!(agent.status().await, Status::Idle);

    let conv = agent.conversation().await;
    assert_eq!(
        conv.messages,
        vec![
            Message::user(vec![ContentBlock::text("hi there")]),
            // The reply enters stamped with the Run's captured Model.
            Message::assistant_from(vec![ContentBlock::text("Hello")], provenance),
        ]
    );
}

// ---- session cost metering (ADR-0037 Stage F) --------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_priced_response_broadcasts_the_cumulative_session_cost() {
    let dir = TempDir::new().unwrap();
    // 1M input at $10/M + 100K output at $50/M = $15.
    let priced = Response {
        usage: Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(100_000),
            ..Usage::default()
        },
        ..text_end("Hello")
    };
    let fake = FakeLlm::script(vec![Entry::just(priced)]);
    let mut session = session_in(&dir);
    session.model.pricing = Some(crate::llm::cost::Pricing {
        input: 10.0,
        output: 50.0,
        cache_read: None,
        cache_write: None,
    });
    let agent = start(session, fake);
    let mut rx = agent.subscribe();

    agent.submit("hi").await.unwrap();

    let cost = recv_match(&mut rx, |e| matches!(e, Event::SessionCost { .. })).await;
    assert_eq!(cost, Event::SessionCost { total: 15.0 });
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpriced_model_broadcasts_no_session_cost() {
    // Usage rides the Response, but the test session's Model carries no
    // pricing - a local-only Session must see no cost events at all.
    let unpriced = Response {
        usage: Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(100_000),
            ..Usage::default()
        },
        ..text_end("Hello")
    };
    let (_dir, agent, mut rx) = harness(vec![Entry::just(unpriced)]);

    agent.submit("hi").await.unwrap();

    // Watch the whole Run: a cost event anywhere in it fails (a metered
    // zero is silence, not a $0.00), so the first match must be the finish.
    let ev = recv_match(&mut rx, |e| {
        matches!(e, Event::SessionCost { .. }) || is_run_finished(e)
    })
    .await;
    assert!(
        is_run_finished(&ev),
        "unpriced model emitted a cost event: {ev:?}"
    );
}

// ---- busy rejection ---------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn submit_while_running_is_busy_idle_again_after_the_run() {
    let (barrier, mut inflight) = Entry::barrier();
    let (_dir, agent, mut rx) = harness(vec![barrier]);

    agent.submit("first").await.unwrap();

    // The Run is parked mid-complete.
    let InFlight { release, .. } = inflight.recv().await.expect("in-flight signal");
    assert_eq!(agent.status().await, Status::Running);
    assert_eq!(agent.submit("second").await, Err(Busy));

    release
        .send(Release {
            deltas: vec![],
            response: text_end("done"),
        })
        .ok();

    recv_match(&mut rx, is_run_finished).await;
    assert_eq!(agent.status().await, Status::Idle);
}

// ---- approval flow ----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn denied_run_command_is_never_executed_and_yields_the_denial_tool_result() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("deny_marker");
    let script = vec![
        Entry::just(tool_use_result(
            "tu_run",
            "run_command",
            json!({ "command": format!("touch {}", marker.display()) }),
        )),
        Entry::just(text_end("understood")),
    ];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("touch that file").await.unwrap();

    let req = recv_match(&mut rx, |e| matches!(e, Event::ApprovalRequest { .. })).await;
    if let Event::ApprovalRequest { command, .. } = &req {
        assert!(command.contains("touch"));
    }
    let id = approval_id(req);

    agent.approve(id.clone(), Decision::Deny).await;

    recv_match(&mut rx, |e| {
        matches!(e, Event::ApprovalResolved { approval_id, approved: false } if *approval_id == id)
    })
    .await;
    recv_match(&mut rx, |e| {
        matches!(e, Event::ToolResult { id, content, is_error: true, .. }
            if id == "tu_run" && content == "[command denied by user]")
    })
    .await;
    recv_match(&mut rx, is_run_finished).await;

    assert!(!marker.exists(), "the command never ran");

    let conv = agent.conversation().await;
    assert!(conv.messages.iter().any(|m| {
        m.role == Role::User
            && m.content.iter().any(|b| {
                matches!(b,
                ContentBlock::ToolResult { tool_use_id, is_error: true, content }
                    if tool_use_id == "tu_run" && content == "[command denied by user]")
            })
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn approved_run_command_executes_and_returns_its_output() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "tu_run",
            "run_command",
            json!({ "command": "echo hi" }),
        )),
        Entry::just(text_end("it said hi")),
    ]);

    agent.submit("say hi").await.unwrap();

    let id = approval_id(
        recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
        )
        .await,
    );

    agent.approve(id.clone(), Decision::Approve).await;
    recv_match(&mut rx, |e| {
        matches!(e, Event::ApprovalResolved { approval_id, approved: true } if *approval_id == id)
    })
    .await;
    let result = recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "tu_run"),
    )
    .await;
    if let Event::ToolResult { content, .. } = result {
        assert!(content.contains("hi"));
    }
    recv_match(&mut rx, is_run_finished).await;
}

// ---- ask_user_question (ADR-0057) -------------------------------------

// A Run whose model calls `ask_user_question`: assert a QuestionRequest is
// broadcast, answer it, and assert the tool result is the VERBATIM formatted
// string. Mirrors the approval integration test (request event -> resolve ->
// tool result), but through the question round-trip.
#[tokio::test(flavor = "multi_thread")]
async fn ask_user_question_opens_the_modal_and_formats_the_answer() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "tu_ask",
            "ask_user_question",
            json!({
                "questions": [{
                    "question": "Which library should we use?",
                    "header": "Library",
                    "options": [
                        { "label": "serde", "description": "the standard" },
                        { "label": "miniserde", "description": "smaller" }
                    ]
                }]
            }),
        )),
        Entry::just(text_end("using serde then")),
    ]);

    agent.submit("pick a library").await.unwrap();

    // The tool opened the modal: a QuestionRequest carrying the question id and
    // the shaped questions.
    let req = recv_match(&mut rx, |e| matches!(e, Event::QuestionRequest { .. })).await;
    let (id, questions) = match req {
        Event::QuestionRequest {
            question_id,
            questions,
        } => (question_id, questions),
        other => panic!("expected a QuestionRequest, got {other:?}"),
    };
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].header, "Library");

    // Answer it as the UI would (row 0 = "serde").
    agent
        .answer_question(id.clone(), Ok(vec![(0, "serde".to_string())]))
        .await;

    // The Agent emits QuestionResolved once the tool reads the reply.
    recv_match(&mut rx, |e| {
        matches!(e, Event::QuestionResolved { question_id } if *question_id == id)
    })
    .await;

    // The tool result is the VERBATIM formatted answer string.
    let result = recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "tu_ask"),
    )
    .await;
    if let Event::ToolResult { content, .. } = result {
        assert_eq!(
            content,
            "User has provided the following answers:\n\n**Library**: serde"
        );
    }
    recv_match(&mut rx, is_run_finished).await;
}

// The decline path: answering with the VERBATIM decline string yields it as the
// tool result content (an ordinary, non-error result - qwen returns it as both
// llmContent and display).
#[tokio::test(flavor = "multi_thread")]
async fn declining_a_question_yields_the_verbatim_decline_string() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "tu_ask",
            "ask_user_question",
            json!({
                "questions": [{
                    "question": "Which library should we use?",
                    "header": "Library",
                    "options": [
                        { "label": "serde", "description": "the standard" },
                        { "label": "miniserde", "description": "smaller" }
                    ]
                }]
            }),
        )),
        Entry::just(text_end("okay, skipping")),
    ]);

    agent.submit("pick a library").await.unwrap();

    let id = match recv_match(&mut rx, |e| matches!(e, Event::QuestionRequest { .. })).await {
        Event::QuestionRequest { question_id, .. } => question_id,
        other => panic!("expected a QuestionRequest, got {other:?}"),
    };

    agent
        .answer_question(
            id.clone(),
            Err("User declined to answer the questions.".to_string()),
        )
        .await;

    let result = recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, .. } if id == "tu_ask"),
    )
    .await;
    if let Event::ToolResult { content, .. } = result {
        assert_eq!(content, "User declined to answer the questions.");
    }
    recv_match(&mut rx, is_run_finished).await;
}

// ---- standing approval ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn approve_always_records_the_command_the_identical_command_is_auto_approved() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "r1",
            "run_command",
            json!({ "command": "echo hi" }),
        )),
        Entry::just(tool_use_result("ls", "list_files", json!({ "path": "." }))),
        Entry::just(tool_use_result(
            "r2",
            "run_command",
            json!({ "command": "echo hi" }),
        )),
        Entry::just(text_end("done")),
    ]);

    agent.submit("run it twice").await.unwrap();

    let id = approval_id(
        recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
        )
        .await,
    );
    agent.approve(id.clone(), Decision::ApproveAlways).await;
    recv_match(&mut rx, |e| {
        matches!(e, Event::ApprovalResolved { approval_id, approved: true } if *approval_id == id)
    })
    .await;
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "r1"),
    )
    .await;

    // The identical second command: no modal, an approval_auto, still runs.
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalAuto { command } if command == "echo hi"),
    )
    .await;
    let r2 = recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "r2"),
    )
    .await;
    if let Event::ToolResult { content, .. } = r2 {
        assert!(content.contains("hi"));
    }
    recv_match(&mut rx, is_run_finished).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_approval_never_widens_beyond_the_identical_string() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "r1",
            "run_command",
            json!({ "command": "echo hi" }),
        )),
        Entry::just(tool_use_result(
            "r2",
            "run_command",
            json!({ "command": "echo  hi" }),
        )),
        Entry::just(text_end("done")),
    ]);

    agent.submit("run variants").await.unwrap();

    let id1 = approval_id(
        recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
        )
        .await,
    );
    agent.approve(id1, Decision::ApproveAlways).await;
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "r1"),
    )
    .await;

    // Two spaces is a different command: the modal comes back.
    let id2 = approval_id(
        recv_match(
            &mut rx,
            |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo  hi"),
        )
        .await,
    );
    agent.approve(id2, Decision::Deny).await;
    recv_match(&mut rx, |e| {
        matches!(e, Event::ToolResult { id, is_error: true, content, .. }
            if id == "r2" && content == "[command denied by user]")
    })
    .await;
    recv_match(&mut rx, is_run_finished).await;
}

// ---- approval mode cycle (ADR-0050) -----------------------------------

// CycleApprovalMode folds the pure Approvals mode and broadcasts the new mode
// so the Screen mirror updates. One press from the Default start lands on
// AutoEdit (the qwen APPROVAL_MODES order: plan → default → auto-edit → …).
#[tokio::test(flavor = "multi_thread")]
async fn cycle_approval_mode_folds_and_broadcasts_the_new_mode() {
    let (_dir, agent, mut rx) = harness(vec![Entry::just(text_end("hi"))]);

    agent.cycle_approval_mode().await;
    recv_match(&mut rx, |e| {
        matches!(
            e,
            Event::ApprovalModeChanged {
                mode: ApprovalMode::AutoEdit
            }
        )
    })
    .await;

    // A second press advances to Auto (auto-edit → auto).
    agent.cycle_approval_mode().await;
    recv_match(&mut rx, |e| {
        matches!(
            e,
            Event::ApprovalModeChanged {
                mode: ApprovalMode::Auto
            }
        )
    })
    .await;
}

// In Yolo mode every gated run_command auto-approves with NO ApprovalRequest and
// NO modal - it runs straight through. Cycling Default → AutoEdit → Auto → Yolo
// is three presses.
#[tokio::test(flavor = "multi_thread")]
async fn yolo_mode_auto_runs_a_gated_command_without_a_modal() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "y1",
            "run_command",
            json!({ "command": "echo yolo" }),
        )),
        Entry::just(text_end("done")),
    ]);

    // Default → AutoEdit → Auto → Yolo.
    for _ in 0..3 {
        agent.cycle_approval_mode().await;
    }
    recv_match(&mut rx, |e| {
        matches!(
            e,
            Event::ApprovalModeChanged {
                mode: ApprovalMode::Yolo
            }
        )
    })
    .await;

    agent.submit("run it").await.unwrap();

    // The gated command runs with no ApprovalRequest: the ToolResult arrives
    // directly. (A raced ApprovalRequest would fail this by never matching.)
    let result = recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "y1"),
    )
    .await;
    if let Event::ToolResult { content, .. } = result {
        assert!(content.contains("yolo"));
    }
    recv_match(&mut rx, is_run_finished).await;
}

// ---- steering ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn steer_while_idle_is_idle() {
    let (_dir, agent, _rx) = harness(vec![]);
    assert_eq!(agent.steer("too early").await, Err(Idle));
}

#[tokio::test(flavor = "multi_thread")]
async fn steer_mid_run_is_drained_after_the_tool_batch_and_delivered_unadorned() {
    let (barrier, mut inflight) = Entry::barrier();
    let (second_tx, mut second_rx) = mpsc::unbounded_channel::<LlmRequest>();
    let (_dir, agent, mut rx) = harness(vec![
        barrier,
        Entry::dynamic(vec![], move |req: &LlmRequest, _model: &Model| {
            let _ = second_tx.send(req.clone());
            text_end("done")
        }),
    ]);

    agent.submit("look around").await.unwrap();

    // First call is parked; steer, then release into a tool_use.
    let InFlight { release, .. } = inflight.recv().await.expect("first call parked");
    agent.steer("also check the README").await.unwrap();
    recv_match(
        &mut rx,
        |e| matches!(e, Event::SteeringQueued { text } if text == "also check the README"),
    )
    .await;

    release
        .send(Release {
            deltas: vec![],
            response: tool_use_result("t1", "list_files", json!({ "path": "." })),
        })
        .ok();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::SteeringDelivered { text } if text == "also check the README"),
    )
    .await;

    // Unadorned, riding the SAME user message as the tool results.
    let request = second_rx.recv().await.expect("second request");
    let last = request.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(matches!(
        &last.content[0],
        ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"
    ));
    assert_eq!(last.content[1], ContentBlock::text("also check the README"));

    recv_match(&mut rx, is_run_finished).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rollover_steering_the_run_never_drained_auto_submits_the_next_run() {
    let (barrier, mut inflight) = Entry::barrier();
    let (roll_tx, mut roll_rx) = mpsc::unbounded_channel::<LlmRequest>();
    let (_dir, agent, mut rx) = harness(vec![
        barrier,
        Entry::dynamic(vec![], move |req: &LlmRequest, _model: &Model| {
            let _ = roll_tx.send(req.clone());
            text_end("second done")
        }),
    ]);

    agent.submit("first thing").await.unwrap();

    let InFlight { release, .. } = inflight.recv().await.expect("first call parked");
    // No tool batch ever runs, so this steering misses its Run.
    agent.steer("and then this").await.unwrap();
    release
        .send(Release {
            deltas: vec![],
            response: text_end("first done"),
        })
        .ok();

    recv_match(&mut rx, is_run_finished).await;
    recv_match(&mut rx, is_run_started).await;

    let request = roll_rx.recv().await.expect("rollover request");
    let last = request.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content[0], ContentBlock::text("and then this"));

    recv_match(&mut rx, is_run_finished).await;
    assert_eq!(agent.status().await, Status::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_discards_queued_steering_no_rollover() {
    let (barrier, mut inflight) = Entry::barrier();
    let (_dir, agent, mut rx) = harness(vec![barrier]);

    agent.submit("slow work").await.unwrap();
    recv_match(&mut rx, is_run_started).await;

    // The Run parks in complete forever (we never release it).
    let _inflight = inflight.recv().await.expect("parked");
    agent.steer("never mind this").await.unwrap();
    agent.cancel().await;

    recv_match(&mut rx, |e| matches!(e, Event::RunCancelled)).await;
    refute_match(&mut rx, is_run_started).await;
    assert_eq!(agent.status().await, Status::Idle);

    // The discarded text never entered the Conversation.
    let conv = agent.conversation().await;
    assert!(!conv.messages.iter().any(|m| m.content.iter().any(|b| {
        matches!(b, ContentBlock::Text { text } if text.contains("never mind this"))
    })));
}

// ---- cancellation -----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn cancel_mid_run_emits_run_cancelled_and_records_the_cancellation() {
    let (barrier, mut inflight) = Entry::barrier();
    let (_dir, agent, mut rx) = harness(vec![barrier]);

    agent.submit("do something slow").await.unwrap();
    let _inflight = inflight.recv().await.expect("parked in llm");
    agent.cancel().await;

    recv_match(&mut rx, |e| matches!(e, Event::RunCancelled)).await;
    assert_eq!(agent.status().await, Status::Idle);

    let conv = agent.conversation().await;
    let n = conv.messages.len();
    assert_eq!(
        conv.messages[n - 2],
        Message::user(vec![ContentBlock::text("do something slow")])
    );
    assert_eq!(
        conv.messages[n - 1],
        Message::assistant(vec![ContentBlock::text(voice::run_cancelled_marker())])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_when_idle_is_a_no_op() {
    let (_dir, agent, _rx) = harness(vec![]);
    agent.cancel().await;
    assert_eq!(agent.status().await, Status::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_after_a_tool_ran_keeps_the_partial_run() {
    let (barrier, mut inflight) = Entry::barrier();
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
        barrier,
    ]);

    agent.submit("explore then hang").await.unwrap();

    // The tool ran; only then cancel (its result is on disk/in the conv).
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "t1"),
    )
    .await;
    let _inflight = inflight.recv().await.expect("second call parked");
    agent.cancel().await;

    recv_match(&mut rx, |e| matches!(e, Event::RunCancelled)).await;

    let conv = agent.conversation().await;
    let tail: Vec<_> = conv.messages.iter().rev().take(3).rev().cloned().collect();
    assert!(matches!(&tail[0],
        Message { role: Role::Assistant, content, .. } if matches!(&content[0], ContentBlock::ToolUse { id, .. } if id == "t1")));
    assert!(matches!(&tail[1],
        Message { role: Role::User, content, .. } if matches!(&content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")));
    assert_eq!(
        tail[2],
        Message::assistant(vec![ContentBlock::text(voice::run_cancelled_marker())])
    );
}

// ---- run error -------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn llm_error_emits_run_error_keeps_user_message_and_closes_with_failure_marker() {
    let (_dir, session, agent, mut rx) = harness_with_session(vec![Entry::error("boom")]);
    let provenance = session.model.provenance();

    agent.submit("hello?").await.unwrap();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::RunError { reason } if reason == "boom"),
    )
    .await;
    assert_eq!(agent.status().await, Status::Idle);

    let conv = agent.conversation().await;
    let n = conv.messages.len();
    assert_eq!(
        conv.messages[n - 2],
        Message::user(vec![ContentBlock::text("hello?")])
    );
    // The failed close keeps the response remnant's Provenance (the marker
    // rides what the model produced, here nothing).
    assert_eq!(
        conv.messages[n - 1],
        Message::assistant_from(
            vec![ContentBlock::text(voice::run_failed_marker())],
            provenance
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_llm_error_after_a_tool_ran_keeps_the_partial_run_under_the_failure_marker() {
    let (_dir, session, agent, mut rx) = harness_with_session(vec![
        Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
        Entry::error("boom"),
    ]);
    let provenance = session.model.provenance();

    agent.submit("explore then die").await.unwrap();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::RunError { reason } if reason == "boom"),
    )
    .await;

    let conv = agent.conversation().await;
    let tail: Vec<_> = conv.messages.iter().rev().take(3).rev().cloned().collect();
    assert!(matches!(&tail[0],
        Message { role: Role::Assistant, content, .. } if matches!(&content[0], ContentBlock::ToolUse { id, .. } if id == "t1")));
    assert!(matches!(&tail[1],
        Message { role: Role::User, content, .. } if matches!(&content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")));
    assert_eq!(
        tail[2],
        Message::assistant_from(
            vec![ContentBlock::text(voice::run_failed_marker())],
            provenance
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_failing_with_an_llm_error_logs_a_settled_entry_carrying_the_error_reason() {
    // The error reason must reach the settled log entry verbatim.
    let (_dir, session, agent, mut rx) =
        harness_with_session(vec![Entry::error("{:llm_error, \"connection refused\"}")]);
    let session_dir = session.session_dir.clone();

    agent.submit("evaluate this project").await.unwrap();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::RunError { reason } if reason.contains("connection refused")),
    )
    .await;
    assert_eq!(agent.status().await, Status::Idle);

    let path = log::latest(&session_dir).expect("a log file");
    let content = std::fs::read_to_string(&path).unwrap();
    let settled: Vec<Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["e"] == "settled")
        .collect();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0]["outcome"], "failed");
    assert_eq!(settled[0]["stop_reason"], "error");
    assert!(
        settled[0]["reason"]
            .as_str()
            .unwrap()
            .contains("connection refused")
    );
}

// ---- session log + resume --------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_settled_session_resumes_into_a_new_agent_conversation_rebuilt() {
    let (_dir, session, first, mut rx) = harness_with_session(vec![
        Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
        Entry::just(text_end("Nothing here.")),
    ]);
    let session_dir = session.session_dir.clone();

    first.submit("look around").await.unwrap();
    recv_match(&mut rx, is_run_finished).await;

    let live = first.conversation().await;
    drop(first);

    let path = log::latest(&session_dir).expect("a log file");
    let resumed = AgentHandle::start(
        StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
            .with_system_prompt("You are a test agent.")
            .with_resume(Resume::Path(path.clone())),
    )
    .expect("resumes");

    assert_eq!(resumed.conversation().await.messages, live.messages);
    let info = resumed.resume_info().await.expect("resume info");
    assert_eq!(info.path, path);
    assert_eq!(info.drift, vec![]);
}

// ---- a multi-Pass Run in the Session Log ------------------------------

// A Session with run_limit 4 so the exploration script runs several Passes
// and writes a multi-message Conversation to the log for the resume check.
fn rider_session(dir: &TempDir) -> Session {
    let root = dir.path().to_string_lossy().into_owned();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            run_limit: Some(4),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .expect("session builds")
}

// Three exploration Passes then a conclusion, crossing a Run boundary so the
// carried Conversation resumes byte-for-byte.
fn exploring_script() -> Vec<Entry> {
    vec![
        Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
        Entry::just(tool_use_result("t2", "list_files", json!({ "path": "." }))),
        Entry::just(tool_use_result("t3", "list_files", json!({ "path": "." }))),
        Entry::just(text_end("done")),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_that_carried_riders_resumes_byte_for_byte() {
    let dir = TempDir::new().unwrap();
    let session = rider_session(&dir);
    let session_dir = session.session_dir.clone();
    let (agent, mut rx) = session_harness(session.clone(), exploring_script());

    agent.submit("look around").await.unwrap();
    recv_match(&mut rx, is_run_finished).await;

    let live = agent.conversation().await;
    drop(agent);

    let path = log::latest(&session_dir).expect("a log file");
    let resumed = AgentHandle::start(
        StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
            .with_system_prompt("You are a test agent.")
            .with_resume(Resume::Path(path)),
    )
    .expect("resumes");

    // The reconstructed Conversation carries the same bytes the model
    // read live.
    assert_eq!(resumed.conversation().await.messages, live.messages);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plan_survives_a_run_boundary_and_is_restored_on_resume() {
    let (_dir, session, first, mut rx) = harness_with_session(vec![
        Entry::just(tool_use_result(
            "p1",
            "todo_write",
            json!({ "todos": [{ "content": "do Y", "status": "in_progress" }] }),
        )),
        Entry::just(text_end("planned")),
    ]);
    let session_dir = session.session_dir.clone();

    first.submit("do Y").await.unwrap();
    recv_match(&mut rx, is_run_finished).await;

    assert_eq!(first.plan().await.as_deref(), Some("◐ do Y"));
    drop(first);

    let path = log::latest(&session_dir).expect("a log file");
    let resumed = AgentHandle::start(
        StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
            .with_system_prompt("You are a test agent.")
            .with_resume(Resume::Path(path)),
    )
    .expect("resumes");

    assert_eq!(resumed.plan().await.as_deref(), Some("◐ do Y"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proactive_compaction_is_written_to_the_session_log_and_round_trips_through_resume() {
    let dir = TempDir::new().unwrap();
    let model = Model::new("local", "test-model", Api::AnthropicMessages, 64_000, 200);
    let session = Session::build(
        SessionOpts {
            root: Some(dir.path().to_string_lossy().into_owned()),
            session_dir: Some(dir.path().join("sessions").to_string_lossy().into_owned()),
            model: Some(model),
            // Tuned so THREE small Runs cross the Compaction Target and
            // two do not: the tool-spec overhead rides the estimate, so
            // this number tracks the registry (web_fetch, ADR-0024, moved
            // it from 4000; run_command's pipefail description moved it
            // from 4200; the no-invented-line-numbers Voice rule moved it
            // from 4230; the grow-in-verified-steps Voice rule moved it
            // from 4320; the run-commands-whole Voice rule moved it from
            // 4480; the quiet-flags Voice rule and run_command description
            // moved it from 4640; the qwen-structured system prompt rewrite
            // (Core Mandates + Workflow + Tone + worked Examples) roughly
            // doubled the prompt and moved it from 4800; the Scout removal
            // (Group E: dropped the explore tool spec and reworded the
            // Understand step to inline grep/list_files/read_file) shrank both
            // the prompt and the tool-spec overhead and moved it from 5900;
            // Stage 4a (added the glob tool spec and rewrote every tool
            // description in qwen-code's concrete style) grew the tool-spec
            // overhead and moved it from 5750; Stage 4b (replaced the flat
            // one-string `plan` tool with the nested `todos` array `todo_write`
            // spec and reworded the Plan Workflow step) grew the tool-spec
            // overhead again and moved it from 6480; the faithful qwen-code
            // tool-description port (Stage 4c: each of the 8 tool descriptions
            // rewritten to carry qwen-code's full depth - todo_write alone
            // ~9.3k chars, run_command ~4.2k, edit_file ~1.8k - grew the
            // serialized tool-spec overhead from ~7.9k to ~25.3k chars, about
            // +5.0k tokens on every request's estimate) moved it from 6900; the
            // faithful qwen-code system-prompt port (Core Mandates, Task
            // Management, the Understand->Verify workflow, Operational
            // Guidelines, Executing-with-care, Git-as-source-of-truth, worked
            // Examples, Final Reminder - grew the system prompt from ~5k to
            // ~22.6k chars, about +5.0k tokens on every request's estimate)
            // moved it from 13786).
            //
            // Retune mechanism: `Compaction::proactive` fires when
            // `token_estimate > compaction_target`, with `compaction_target =
            // budget - reserve(200) - trunc(0.3 * budget) ~= 0.7*budget - 200`.
            // The estimate is `ceil((overhead + system_prompt + messages)/3.5)`
            // and is INDEPENDENT of `budget`, so `budget` is the free knob that
            // slides the target. Measured estimates here are run2 ~14283 tokens
            // and run3 ~14642; budget 20950 puts the target at 14465, which sits
            // between them so exactly the third Run crosses.
            context_budget: Some(20_950),
            compaction_slack: Some(0.3),
            compaction_keep: Some(0.1),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .expect("session builds");
    let session_dir = session.session_dir.clone();

    // Adaptation of baud's mid-test `Baud.FakeLLM.script(...)` re-scripting:
    // the Rust FakeLlm is per-instance with a fixed queue (ADR-0020), so all
    // entries ride ONE script up front - three small Runs to build history
    // past the compaction target, then the proactive summarization call
    // (popped FIRST on the next submit) and that Run's own reply.
    let reply = "word ".repeat(250);
    let entries = vec![
        Entry::just(text_end(&format!("{reply} 1"))),
        Entry::just(text_end(&format!("{reply} 2"))),
        Entry::just(text_end(&format!("{reply} 3"))),
        Entry::just(text_end("[Compaction narrative] work summarized")),
        Entry::just(text_end("continuing")),
    ];
    let agent = start_voiced(session.clone(), FakeLlm::script(entries));
    let mut rx = agent.subscribe();
    for n in 1..=3 {
        agent.submit(format!("step {n}")).await.unwrap();
        recv_match(&mut rx, is_run_finished).await;
    }
    // The next submit trips proactive compaction before its own reply.
    agent.submit("keep going").await.unwrap();
    recv_match(&mut rx, is_run_finished).await;

    // The proactive Compaction replaced old messages: the head is a summary.
    let live = agent.conversation().await;
    let head_text: String = live.messages[0]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(head_text.contains("Compaction narrative"));
    drop(agent);

    let path = log::latest(&session_dir).expect("a log file");
    let content = std::fs::read_to_string(&path).unwrap();
    let compacted: Vec<Value> = content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["e"] == "compacted")
        .collect();
    assert_eq!(compacted.len(), 1);
    assert!(
        compacted[0]["summary"]
            .as_str()
            .unwrap()
            .contains("Compaction narrative")
    );
    assert_eq!(compacted[0]["original_task"], "step 1");

    // Resume folds to the COMPACTED view, not the raw pre-compaction msgs.
    let resumed = AgentHandle::start(
        StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
            .with_system_prompt("You are a test agent.")
            .with_resume(Resume::Path(path)),
    )
    .expect("resumes");
    let resumed_msgs = resumed.conversation().await.messages;
    let resumed_head: String = resumed_msgs[0]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(resumed_head.contains("Compaction narrative"));
    assert!(resumed_head.contains("step 1"));
    assert!(!resumed_msgs.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "step 1"))
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_from_a_different_project_root_fails_init_loudly() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("other")).unwrap();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    let session = Session::build(
        SessionOpts {
            root: Some(dir.path().to_string_lossy().into_owned()),
            session_dir: Some(session_dir.clone()),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .unwrap();
    let mut log = Log::open(&session).unwrap();
    log.append(LogEntry::UserText("hi".into()));
    let path = log.path.clone();
    drop(log);

    let other = Session::build(
        SessionOpts {
            root: Some(dir.path().join("other").to_string_lossy().into_owned()),
            session_dir: Some(session_dir),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .unwrap();

    let result = AgentHandle::start(
        StartOpts::new(other, Arc::new(FakeLlm::script(vec![])))
            .with_system_prompt("You are a test agent.")
            .with_resume(Resume::Path(path)),
    );
    assert!(matches!(result, Err(StartError::ResumeRootMismatch(_))));
}

// ---- subscriber pruning ----------------------------------------------

// Adaptation of baud's "a dead subscriber is pruned and does not break later
// runs": in tokio a dropped broadcast Receiver auto-cleans on the next
// send, so there is no monitor/DOWN to model. We DROP a Receiver (the tokio
// analog of the subscriber process dying), then run a full Run and assert a
// live subscriber still gets every event and the Agent stays healthy.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_subscriber_is_pruned_and_does_not_break_later_runs() {
    let (_dir, agent, _initial_rx) = harness(vec![Entry::response(
        vec![Delta::Text("ok".into())],
        text_end("ok"),
    )]);

    // A subscriber that immediately goes away.
    let dead = agent.subscribe();
    drop(dead);

    let mut rx = agent.subscribe();
    agent.submit("still alive?").await.unwrap();
    recv_match(&mut rx, is_run_finished).await;
    assert_eq!(agent.status().await, Status::Idle);
}

// ---- streaming responsiveness ----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn tool_use_during_streaming_steer_then_unblock_no_crash() {
    let (barrier, mut inflight) = Entry::barrier();
    let (_dir, agent, mut rx) = harness(vec![barrier, Entry::just(text_end("done"))]);

    agent.submit("test streaming").await.unwrap();

    // The first model call parks in-flight (mid-Run). The first Pass's
    // MessageStart has already gone out; steer NOW - the Run is running but
    // has not reached its drain point - then release into a tool_use.
    // `steer().await` round-trips through the Agent, so the text is queued
    // before the tool batch runs and the drain delivers it (this removes
    // baud's implicit scheduler race while preserving the observable
    // behavior: steering issued mid-Run, delivered after the tool batch, no
    // crash).
    let InFlight { release, .. } = inflight.recv().await.expect("blocked in llm");
    assert_eq!(agent.status().await, Status::Running);
    recv_match(&mut rx, |e| matches!(e, Event::MessageStart { pass: 1 })).await;
    agent.steer("more data").await.unwrap();
    recv_match(&mut rx, |e| matches!(e, Event::SteeringQueued { .. })).await;

    release
        .send(Release {
            deltas: vec![
                Delta::Text("Thinking".into()),
                Delta::Text(" carefully".into()),
            ],
            response: tool_use_result("t1", "list_files", json!({ "path": "." })),
        })
        .ok();

    // The parked call's deltas flush now (streaming), then the tool batch
    // runs and the drain delivers the queued Steering.
    recv_match(&mut rx, |e| matches!(e, Event::MessageUpdate { .. })).await;
    recv_match(&mut rx, |e| matches!(e, Event::SteeringDelivered { .. })).await;
    recv_match(&mut rx, is_run_finished).await;
    assert_eq!(agent.status().await, Status::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_during_streaming_does_not_crash() {
    let (barrier, mut inflight) = Entry::barrier();
    let (_dir, agent, mut rx) = harness(vec![barrier]);

    agent.submit("cancel me").await.unwrap();

    let _inflight = inflight.recv().await.expect("blocked in llm");
    agent.cancel().await;
    // The barrier drops its release when the test ends; the parked call is
    // aborted at the await.

    recv_match(&mut rx, |e| matches!(e, Event::RunCancelled)).await;
    assert_eq!(agent.status().await, Status::Idle);
}

// ---- Active Model / set_model (ADR-0033) --------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn set_model_changes_what_active_model_returns() {
    let (_dir, agent, _rx) = harness(vec![]);

    // Seeded from the Session's launch-resolved Model (the scoped config id).
    assert_eq!(
        agent.active_model().await,
        SessionConfig::test_defaults().model
    );

    agent.set_model("local/picked-model".into()).await.unwrap();
    assert_eq!(agent.active_model().await, "local/picked-model");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_rejects_an_unknown_provider_and_keeps_the_active_model() {
    // Resolution against the Session's fixed Provider set guards the swap
    // (ADR-0037): an unknown provider is an Err and nothing changes.
    let (_dir, agent, _rx) = harness(vec![]);
    let before = agent.active_model().await;

    let err = agent.set_model("nowhere/model".into()).await.unwrap_err();
    assert!(err.contains("nowhere"), "error was: {err}");
    assert_eq!(agent.active_model().await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_spawned_after_set_model_uses_the_new_model() {
    // The next Run captures the Agent's mutable Model, so the boundary call
    // carries the new one - not the Session's launch-time one (ADR-0033).
    let (model_tx, mut model_rx) = mpsc::unbounded_channel::<Model>();
    let (_dir, agent, mut rx) = harness(vec![Entry::dynamic(
        vec![],
        move |_req: &LlmRequest, model: &Model| {
            let _ = model_tx.send(model.clone());
            text_end("done")
        },
    )]);

    agent.set_model("local/picked-model".into()).await.unwrap();
    agent.submit("go").await.unwrap();

    let captured = model_rx.recv().await.expect("model");
    assert_eq!(captured.scoped_id(), "local/picked-model");
    assert_eq!(captured.id, "picked-model");

    recv_match(&mut rx, is_run_finished).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_models_discovers_each_custom_provider_live() {
    // The Agent owns the Llm + the Session's Provider set, so `list_models`
    // walks it through `llm::offerings` (ADR-0037): the test config's one
    // custom Provider (`local`) discovers its models from the scripted
    // boundary. Built-ins ride the ambient credential environment, so the
    // assertion pins only the custom listing (the hermetic Ok/Err matrix
    // lives with `offerings` itself).
    let dir = TempDir::new().unwrap();
    let fake =
        FakeLlm::script(vec![]).with_models(vec![Ok(vec!["a/model".into(), "b/model".into()])]);
    let agent = start(session_in(&dir), fake);

    let listings = agent.list_models().await.unwrap();
    let local = listings
        .iter()
        .find(|l| l.provider == "local")
        .expect("the custom Provider lists");
    assert_eq!(
        local.models,
        vec!["a/model".to_string(), "b/model".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_model_rejects_an_unresolvable_pick_and_keeps_the_active_model() {
    // A `/model` swap to an id whose Provider is not in the Session's set is
    // rejected with the reason, and nothing changes - the rejected swap is
    // inert (ADR-0033 amendment).
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    let session = Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            context_budget: Some(2_000),
            model: Some(Model::new(
                "local",
                "small",
                Api::AnthropicMessages,
                64_000,
                500,
            )),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .unwrap();
    let agent = start(session, FakeLlm::script(vec![]));
    let before = agent.active_model().await;

    let err = agent
        .set_model("nonesuch/way-too-big".into())
        .await
        .unwrap_err();
    assert!(err.contains("nonesuch"), "error was: {err}");
    assert_eq!(agent.active_model().await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_budget_follows_the_captured_model_across_a_swap() {
    // The budget derives from the Model each Run captures (ADR-0037): the
    // first Run runs at the launch Model's window, and after a `/model` swap
    // to a narrower Provider the NEXT Run runs at the picked window - visible
    // on RunFinished, which carries the settling Conversation's budget.
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    let mut config = SessionConfig::test_defaults();
    config.max_tokens = 500;
    config.providers.insert(
        "tiny".to_string(),
        crate::session::ProviderConfig {
            base_url: "http://localhost:0/v1".into(),
            api: Api::AnthropicMessages,
            // A narrower window than the 64k launch window, but still wide
            // enough to hold the tool-spec overhead (~25.3k chars ~= 7.2k
            // tokens after the faithful qwen-code description port) plus the
            // reserve and a small Conversation - otherwise the post-swap Run
            // can never fit its request and this test would hang. The value
            // only has to be distinct from the launch window for the assertion.
            context_window: Some(16_000),
            token: None,
        },
    );
    let session = Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            ..Default::default()
        },
        &config,
    )
    .unwrap();
    let agent = start(
        session,
        FakeLlm::script(vec![
            Entry::just(text_end("one")),
            Entry::just(text_end("two")),
        ]),
    );
    let mut rx = agent.subscribe();

    let finished_budget = |e: &Event| match e {
        Event::RunFinished { context_budget, .. } => Some(*context_budget),
        _ => None,
    };

    agent.submit("first").await.unwrap();
    let ev = recv_match(&mut rx, is_run_finished).await;
    assert_eq!(finished_budget(&ev), Some(64_000), "the launch window");

    agent.set_model("tiny/m".into()).await.unwrap();
    agent.submit("second").await.unwrap();
    let ev = recv_match(&mut rx, is_run_finished).await;
    assert_eq!(finished_budget(&ev), Some(16_000), "the captured window");
}
