// ===========================================================================
// Tests - ported 1:1 from baud/test/baud/agent_test.exs (ADR-0017). baud's
// process primitives translate to their tokio analogs, preserving OBSERVABLE
// behavior: `assert_receive` → a broadcast recv with a timeout helper;
// `GenServer.call` → the request/reply Commands; `spawn` + `Process.monitor`
// for the dead-subscriber test → tokio's auto-cleaning broadcast (a dropped
// Receiver is pruned on the next send), noted where it adapts baud's monitor.
// The busy/steer/cancel handshakes use the FakeLlm `Barrier` entry: the test
// observes the Turn parked mid-`complete`, then releases (or aborts) it.
// ===========================================================================
use super::*;
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
// Turns cross the compaction target (baud's agent runs the default prompt +
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

fn is_turn_started(e: &Event) -> bool {
    matches!(e, Event::TurnStarted(_))
}
fn is_turn_finished(e: &Event) -> bool {
    matches!(e, Event::TurnFinished { .. })
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

    let started = recv_match(&mut rx, is_turn_started).await;
    assert!(matches!(started, Event::TurnStarted(_)));

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

    let finished = recv_match(&mut rx, is_turn_finished).await;
    if let Event::TurnFinished {
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
            // The reply enters stamped with the Turn's captured Model.
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
    let dir = TempDir::new().unwrap();
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
    let agent = start(
        session_in(&dir),
        FakeLlm::script(vec![Entry::just(unpriced)]),
    );
    let mut rx = agent.subscribe();

    agent.submit("hi").await.unwrap();

    // Watch the whole Turn: a cost event anywhere in it fails (a metered
    // zero is silence, not a $0.00), so the first match must be the finish.
    let ev = recv_match(&mut rx, |e| {
        matches!(e, Event::SessionCost { .. }) || is_turn_finished(e)
    })
    .await;
    assert!(
        is_turn_finished(&ev),
        "unpriced model emitted a cost event: {ev:?}"
    );
}

// ---- busy rejection ---------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn submit_while_running_is_busy_idle_again_after_the_turn() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut inflight) = Entry::barrier();
    let fake = FakeLlm::script(vec![barrier]);
    let agent = start(session_in(&dir), fake);
    let mut rx = agent.subscribe();

    agent.submit("first").await.unwrap();

    // The Turn is parked mid-complete.
    let InFlight { release, .. } = inflight.recv().await.expect("in-flight signal");
    assert_eq!(agent.status().await, Status::Running);
    assert_eq!(agent.submit("second").await, Err(Busy));

    release
        .send(Release {
            deltas: vec![],
            response: text_end("done"),
        })
        .ok();

    recv_match(&mut rx, is_turn_finished).await;
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
    let id = match &req {
        Event::ApprovalRequest {
            approval_id,
            command,
        } => {
            assert!(command.contains("touch"));
            approval_id.clone()
        }
        _ => unreachable!(),
    };

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
    recv_match(&mut rx, is_turn_finished).await;

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
    let dir = TempDir::new().unwrap();
    let script = vec![
        Entry::just(tool_use_result(
            "tu_run",
            "run_command",
            json!({ "command": "echo hi" }),
        )),
        Entry::just(text_end("it said hi")),
    ];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("say hi").await.unwrap();

    let req = recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
    )
    .await;
    let id = match req {
        Event::ApprovalRequest { approval_id, .. } => approval_id,
        _ => unreachable!(),
    };

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
    recv_match(&mut rx, is_turn_finished).await;
}

// ---- standing approval ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn approve_always_records_the_command_the_identical_command_is_auto_approved() {
    let dir = TempDir::new().unwrap();
    let script = vec![
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
    ];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("run it twice").await.unwrap();

    let req = recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
    )
    .await;
    let id = match req {
        Event::ApprovalRequest { approval_id, .. } => approval_id,
        _ => unreachable!(),
    };
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
    recv_match(&mut rx, is_turn_finished).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_standing_approval_never_widens_beyond_the_identical_string() {
    let dir = TempDir::new().unwrap();
    let script = vec![
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
    ];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("run variants").await.unwrap();

    let req1 = recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo hi"),
    )
    .await;
    let id1 = match req1 {
        Event::ApprovalRequest { approval_id, .. } => approval_id,
        _ => unreachable!(),
    };
    agent.approve(id1, Decision::ApproveAlways).await;
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "r1"),
    )
    .await;

    // Two spaces is a different command: the modal comes back.
    let req2 = recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalRequest { command, .. } if command == "echo  hi"),
    )
    .await;
    let id2 = match req2 {
        Event::ApprovalRequest { approval_id, .. } => approval_id,
        _ => unreachable!(),
    };
    agent.approve(id2, Decision::Deny).await;
    recv_match(&mut rx, |e| {
        matches!(e, Event::ToolResult { id, is_error: true, content, .. }
            if id == "r2" && content == "[command denied by user]")
    })
    .await;
    recv_match(&mut rx, is_turn_finished).await;
}

// ---- steering ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn steer_while_idle_is_idle() {
    let dir = TempDir::new().unwrap();
    let agent = start(session_in(&dir), FakeLlm::script(vec![]));
    assert_eq!(agent.steer("too early").await, Err(Idle));
}

#[tokio::test(flavor = "multi_thread")]
async fn steer_mid_turn_is_drained_after_the_tool_batch_and_delivered_unadorned() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut inflight) = Entry::barrier();
    let (second_tx, mut second_rx) = mpsc::unbounded_channel::<LlmRequest>();
    let script = vec![
        barrier,
        Entry::dynamic(vec![], move |req: &LlmRequest, _model: &Model| {
            let _ = second_tx.send(req.clone());
            text_end("done")
        }),
    ];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

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

    recv_match(&mut rx, is_turn_finished).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rollover_steering_the_turn_never_drained_auto_submits_the_next_turn() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut inflight) = Entry::barrier();
    let (roll_tx, mut roll_rx) = mpsc::unbounded_channel::<LlmRequest>();
    let script = vec![
        barrier,
        Entry::dynamic(vec![], move |req: &LlmRequest, _model: &Model| {
            let _ = roll_tx.send(req.clone());
            text_end("second done")
        }),
    ];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("first thing").await.unwrap();

    let InFlight { release, .. } = inflight.recv().await.expect("first call parked");
    // No tool batch ever runs, so this steering misses its Turn.
    agent.steer("and then this").await.unwrap();
    release
        .send(Release {
            deltas: vec![],
            response: text_end("first done"),
        })
        .ok();

    recv_match(&mut rx, is_turn_finished).await;
    recv_match(&mut rx, is_turn_started).await;

    let request = roll_rx.recv().await.expect("rollover request");
    let last = request.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content[0], ContentBlock::text("and then this"));

    recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(agent.status().await, Status::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_discards_queued_steering_no_rollover() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut inflight) = Entry::barrier();
    let agent = start(session_in(&dir), FakeLlm::script(vec![barrier]));
    let mut rx = agent.subscribe();

    agent.submit("slow work").await.unwrap();
    recv_match(&mut rx, is_turn_started).await;

    // The Turn parks in complete forever (we never release it).
    let _inflight = inflight.recv().await.expect("parked");
    agent.steer("never mind this").await.unwrap();
    agent.cancel().await;

    recv_match(&mut rx, |e| matches!(e, Event::TurnCancelled)).await;
    refute_match(&mut rx, is_turn_started).await;
    assert_eq!(agent.status().await, Status::Idle);

    // The discarded text never entered the Conversation.
    let conv = agent.conversation().await;
    assert!(!conv.messages.iter().any(|m| m.content.iter().any(|b| {
        matches!(b, ContentBlock::Text { text } if text.contains("never mind this"))
    })));
}

// ---- cancellation -----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn cancel_mid_turn_emits_turn_cancelled_and_records_the_cancellation() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut inflight) = Entry::barrier();
    let agent = start(session_in(&dir), FakeLlm::script(vec![barrier]));
    let mut rx = agent.subscribe();

    agent.submit("do something slow").await.unwrap();
    let _inflight = inflight.recv().await.expect("parked in llm");
    agent.cancel().await;

    recv_match(&mut rx, |e| matches!(e, Event::TurnCancelled)).await;
    assert_eq!(agent.status().await, Status::Idle);

    let conv = agent.conversation().await;
    let n = conv.messages.len();
    assert_eq!(
        conv.messages[n - 2],
        Message::user(vec![ContentBlock::text("do something slow")])
    );
    assert_eq!(
        conv.messages[n - 1],
        Message::assistant(vec![ContentBlock::text(voice::turn_cancelled_marker())])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_when_idle_is_a_no_op() {
    let dir = TempDir::new().unwrap();
    let agent = start(session_in(&dir), FakeLlm::script(vec![]));
    agent.cancel().await;
    assert_eq!(agent.status().await, Status::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_after_a_tool_ran_keeps_the_partial_turn() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut inflight) = Entry::barrier();
    let script = vec![
        Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
        barrier,
    ];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("explore then hang").await.unwrap();

    // The tool ran; only then cancel (its result is on disk/in the conv).
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "t1"),
    )
    .await;
    let _inflight = inflight.recv().await.expect("second call parked");
    agent.cancel().await;

    recv_match(&mut rx, |e| matches!(e, Event::TurnCancelled)).await;

    let conv = agent.conversation().await;
    let tail: Vec<_> = conv.messages.iter().rev().take(3).rev().cloned().collect();
    assert!(matches!(&tail[0],
        Message { role: Role::Assistant, content, .. } if matches!(&content[0], ContentBlock::ToolUse { id, .. } if id == "t1")));
    assert!(matches!(&tail[1],
        Message { role: Role::User, content, .. } if matches!(&content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")));
    assert_eq!(
        tail[2],
        Message::assistant(vec![ContentBlock::text(voice::turn_cancelled_marker())])
    );
}

// ---- turn error -------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn llm_error_emits_turn_error_keeps_user_message_and_closes_with_failure_marker() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let provenance = session.model.provenance();
    let agent = start(session, FakeLlm::script(vec![Entry::error("boom")]));
    let mut rx = agent.subscribe();

    agent.submit("hello?").await.unwrap();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::TurnError { reason } if reason == "boom"),
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
            vec![ContentBlock::text(voice::turn_failed_marker())],
            provenance
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_llm_error_after_a_tool_ran_keeps_the_partial_turn_under_the_failure_marker() {
    let dir = TempDir::new().unwrap();
    let script = vec![
        Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
        Entry::error("boom"),
    ];
    let session = session_in(&dir);
    let provenance = session.model.provenance();
    let agent = start(session, FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("explore then die").await.unwrap();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::TurnError { reason } if reason == "boom"),
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
            vec![ContentBlock::text(voice::turn_failed_marker())],
            provenance
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_failing_with_an_llm_error_logs_a_settled_entry_carrying_the_error_reason() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let session_dir = session.session_dir.clone();
    // The error reason must reach the settled log entry verbatim.
    let agent = start(
        session,
        FakeLlm::script(vec![Entry::error("{:llm_error, \"connection refused\"}")]),
    );
    let mut rx = agent.subscribe();

    agent.submit("evaluate this project").await.unwrap();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::TurnError { reason } if reason.contains("connection refused")),
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
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let session_dir = session.session_dir.clone();
    let script = vec![
        Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
        Entry::just(text_end("Nothing here.")),
    ];
    let first = start(session.clone(), FakeLlm::script(script));
    let mut rx = first.subscribe();

    first.submit("look around").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;

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

// ---- Recovery Turns (Continuation + Handoff) ---------------------------

// A Session whose every Turn caps on Pass 2 - one working Pass, then a
// tool-insistent final Pass (refused at dispatch, ADR-0035) - so any
// unfinished work triggers the Endgame Governor's recovery judgment.
fn recovery_session(dir: &TempDir, shape: crate::session::RecoveryShape) -> Session {
    let root = dir.path().to_string_lossy().into_owned();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            turn_limit: Some(2),
            recovery_shape: Some(shape),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .expect("session builds")
}

fn write_tool(id: &str, path: &str) -> Response {
    tool_use_result(id, "write_file", json!({ "path": path, "content": "x" }))
}

// A tool-insistent reply on the final Pass: the call is refused at dispatch
// (ADR-0035; ADR-0015 withdrew the Tools), and the refusal carries the Turn
// to its cap. (turn/loop_.rs keeps its own copy - see its note.)
fn insistent_reply(id: &str) -> Response {
    tool_use_result(id, "list_files", json!({ "path": "." }))
}

fn is_recovery_turn(e: &Event) -> bool {
    matches!(e, Event::RecoveryTurn { .. })
}

// All user-role text blocks of a conversation, flattened.
fn user_texts(conv: &Conversation) -> Vec<String> {
    conv.messages
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capped_unfinished_turn_opens_a_continuation_recovery_turn() {
    let dir = TempDir::new().unwrap();
    let session = recovery_session(&dir, crate::session::RecoveryShape::Continuation);
    let session_dir = session.session_dir.clone();
    let script = vec![
        Entry::just(write_tool("w1", "a.txt")), // Turn 1 Pass 1: the write lands.
        Entry::just(insistent_reply("x1")),     // Turn 1 Pass 2: refused, caps unverified.
        Entry::just(text_end("recovered and done")), // The Recovery Turn.
    ];
    let agent = start(session, FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("write the file").await.unwrap();

    // The capped Turn settles, then the harness opens the Recovery Turn.
    recv_match(&mut rx, is_turn_finished).await;
    let recovery = recv_match(&mut rx, is_recovery_turn).await;
    let prompt = match &recovery {
        Event::RecoveryTurn { shape, text } => {
            assert_eq!(*shape, crate::session::RecoveryShape::Continuation);
            text.clone()
        }
        _ => unreachable!(),
    };
    assert_eq!(prompt, voice::recovery_prompt(false));
    recv_match(&mut rx, is_turn_started).await;
    recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(agent.status().await, Status::Idle);

    // The Conversation was KEPT: the original prompt and the Voice's
    // recovery prompt both ride it, and the recovery's conclusion closes.
    let conv = agent.conversation().await;
    let texts = user_texts(&conv);
    assert!(texts.iter().any(|t| t == "write the file"));
    assert!(texts.contains(&prompt));
    assert!(conv.messages.iter().any(|m| {
        m.role == Role::Assistant
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "recovered and done"))
    }));

    // The Session Log knows one recovery served this request.
    let path = log::latest(&session_dir).expect("a log file");
    assert_eq!(log::recoveries_used(&path), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_recovery_limit_bounds_one_user_request_and_a_recovery_turn_never_resets_it() {
    let dir = TempDir::new().unwrap();
    let session = recovery_session(&dir, crate::session::RecoveryShape::Continuation);
    // Turn 1 caps unverified -> one recovery; the Recovery Turn ALSO caps
    // unverified, but the request's budget (limit 1) is spent.
    let script = vec![
        Entry::just(write_tool("w1", "a.txt")),
        Entry::just(insistent_reply("x1")),
        Entry::just(write_tool("w2", "b.txt")),
        Entry::just(insistent_reply("x2")),
    ];
    let agent = start(session, FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("write the files").await.unwrap();

    recv_match(&mut rx, is_turn_finished).await;
    recv_match(&mut rx, is_recovery_turn).await;
    recv_match(&mut rx, is_turn_finished).await;

    // No third Turn: the capped Recovery Turn settles and the Agent idles.
    refute_match(&mut rx, is_recovery_turn).await;
    refute_match(&mut rx, is_turn_started).await;
    assert_eq!(agent.status().await, Status::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_genuine_user_prompt_resets_the_recovery_count() {
    let dir = TempDir::new().unwrap();
    let session = recovery_session(&dir, crate::session::RecoveryShape::Continuation);
    let script = vec![
        // Request 1: cap -> recovery -> cap (budget spent).
        Entry::just(write_tool("w1", "a.txt")),
        Entry::just(insistent_reply("x1")),
        Entry::just(write_tool("w2", "b.txt")),
        Entry::just(insistent_reply("x2")),
        // Request 2: cap -> the reset budget grants a fresh recovery.
        Entry::just(write_tool("w3", "c.txt")),
        Entry::just(insistent_reply("x3")),
        Entry::just(text_end("second request recovered")),
    ];
    let agent = start(session, FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("first request").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;
    recv_match(&mut rx, is_recovery_turn).await;
    recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(agent.status().await, Status::Idle);

    agent.submit("second request").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;
    recv_match(&mut rx, is_recovery_turn).await;
    recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(agent.status().await, Status::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handoff_recovery_seeds_a_fresh_conversation_with_the_mechanical_facts() {
    let dir = TempDir::new().unwrap();
    let session = recovery_session(&dir, crate::session::RecoveryShape::Handoff);
    let session_dir = session.session_dir.clone();
    let script = vec![
        // Turn 1's single Pass: set the Plan, write, and run a failing
        // verification. The write is the evidence the dangling-failure
        // recovery arm now requires (ADR-0028 addendum 2026-07-14).
        Entry::just(Response {
            content: vec![
                ContentBlock::tool_use("p1", "plan", json!({ "plan": "Goal: fix. 1. run [ ]" })),
                ContentBlock::tool_use(
                    "w1",
                    "write_file",
                    json!({ "path": "a.txt", "content": "hi" }),
                ),
                ContentBlock::tool_use("r1", "run_command", json!({ "command": "false" })),
            ],
            stop_reason: RStop::ToolUse,
            usage: Usage::default(),
            error: None,
        }),
        // Turn 1 Pass 2: a tool-insistent reply, refused, caps the Turn with
        // the write unverified and the verification dangling.
        Entry::just(insistent_reply("x1")),
        // The Handoff's summarize call.
        Entry::just(text_end("## Task\nnarrative-of-dying-turn")),
        // The Recovery Turn over the seeded Conversation.
        Entry::just(text_end("handoff recovered")),
    ];
    let agent = start(session.clone(), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("fix the failing tests").await.unwrap();

    // Approve the failing verification.
    let req = recv_match(&mut rx, |e| matches!(e, Event::ApprovalRequest { .. })).await;
    let id = match req {
        Event::ApprovalRequest { approval_id, .. } => approval_id,
        _ => unreachable!(),
    };
    agent.approve(id, Decision::Approve).await;

    // The verification result the seed must carry verbatim.
    let result = recv_match(
        &mut rx,
        |e| matches!(e, Event::ToolResult { id, .. } if id == "r1"),
    )
    .await;
    let verification = match result {
        Event::ToolResult { content, .. } => content,
        _ => unreachable!(),
    };

    recv_match(&mut rx, is_turn_finished).await;
    let recovery = recv_match(&mut rx, is_recovery_turn).await;
    assert!(matches!(
        &recovery,
        Event::RecoveryTurn { shape, text }
            if *shape == crate::session::RecoveryShape::Handoff
                && text == voice::recovery_prompt(true)
    ));
    recv_match(&mut rx, is_turn_started).await;
    recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(agent.status().await, Status::Idle);

    // The Conversation was RETIRED: one seed message (task verbatim,
    // narrative, final verification verbatim, recovery prompt) plus the
    // recovery's conclusion.
    let conv = agent.conversation().await;
    assert_eq!(conv.messages.len(), 2);
    let seed = user_texts(&conv).join("\n");
    assert!(seed.contains("fix the failing tests"));
    assert!(seed.contains("narrative-of-dying-turn"));
    assert!(seed.contains(&verification));
    assert!(seed.contains(voice::recovery_prompt(true)));
    assert_eq!(
        conv.messages[1],
        Message::assistant_from(
            vec![ContentBlock::text("handoff recovered")],
            session.model.provenance()
        )
    );

    // The Plan is harness-owned and survives the retirement verbatim.
    assert_eq!(agent.plan().await.as_deref(), Some("Goal: fix. 1. run [ ]"));

    // The whole session round-trips: a Resume rebuilds the seeded
    // Conversation byte-identically, and the recovery count survives.
    let live = agent.conversation().await;
    drop(agent);
    let path = log::latest(&session_dir).expect("a log file");
    assert_eq!(log::recoveries_used(&path), 1);
    let resumed = AgentHandle::start(
        StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
            .with_system_prompt("You are a test agent.")
            .with_resume(Resume::Path(path)),
    )
    .expect("resumes");
    assert_eq!(resumed.conversation().await.messages, live.messages);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_handoff_summarization_degrades_to_the_mechanical_skeleton() {
    let dir = TempDir::new().unwrap();
    let session = recovery_session(&dir, crate::session::RecoveryShape::Handoff);
    let script = vec![
        Entry::just(write_tool("w1", "a.txt")), // Turn 1 Pass 1: the write lands.
        Entry::just(insistent_reply("x1")),     // Turn 1 Pass 2: refused, caps unverified.
        Entry::error("summarizer down"),        // The Handoff's LLM call fails.
        Entry::just(text_end("recovered anyway")),
    ];
    let agent = start(session, FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("write the file").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;
    recv_match(&mut rx, is_recovery_turn).await;
    recv_match(&mut rx, is_turn_finished).await;

    // The recovery still happened, on the mechanical skeleton alone.
    let conv = agent.conversation().await;
    assert_eq!(conv.messages.len(), 2);
    let seed = user_texts(&conv).join("\n");
    assert!(seed.contains(voice::handoff_no_narrative()));
    assert!(seed.contains("write the file")); // task verbatim
    assert!(seed.contains(voice::recovery_prompt(false)));
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_limit_zero_leaves_a_capped_unfinished_turn_alone() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_string_lossy().into_owned();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    let session = Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            turn_limit: Some(2),
            recovery_limit: Some(0),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .unwrap();
    let agent = start(
        session,
        FakeLlm::script(vec![
            Entry::just(write_tool("w1", "a.txt")),
            Entry::just(insistent_reply("x1")),
        ]),
    );
    let mut rx = agent.subscribe();

    agent.submit("write the file").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;
    refute_match(&mut rx, is_recovery_turn).await;
    refute_match(&mut rx, is_turn_started).await;
    assert_eq!(agent.status().await, Status::Idle);
}

// ---- riders in the Session Log (Anchors + Endgame prompts) ------------

// Session facts tuned so riders fire: turn_limit 4 puts the wrap-up
// warning on Pass 2 and the final-Pass prompt on Pass 3; anchor_interval 2
// places an Anchor on Pass 2.
fn rider_session(dir: &TempDir) -> Session {
    let root = dir.path().to_string_lossy().into_owned();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            turn_limit: Some(4),
            anchor_interval: Some(2),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .expect("session builds")
}

// [`rider_session`] with the Recovery Turn disabled: a Turn that caps
// with unverified writes now settles as a recovery close (ADR-0028
// addendum), which has its own coverage - the test on this fixture
// asserts rider logging and byte-for-byte Resume only.
fn rider_session_no_recovery(dir: &TempDir) -> Session {
    let root = dir.path().to_string_lossy().into_owned();
    let session_dir = dir.path().join("sessions").to_string_lossy().into_owned();
    Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            turn_limit: Some(4),
            anchor_interval: Some(2),
            recovery_limit: Some(0),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .expect("session builds")
}

// Three exploration Passes then a conclusion: crosses the anchor cadence,
// the Endgame schedule, and (on Pass 3) the Explore Nudge.
fn exploring_script() -> Vec<Entry> {
    vec![
        Entry::just(tool_use_result("t1", "list_files", json!({ "path": "." }))),
        Entry::just(tool_use_result("t2", "list_files", json!({ "path": "." }))),
        Entry::just(tool_use_result("t3", "list_files", json!({ "path": "." }))),
        Entry::just(text_end("done")),
    ]
}

// The (entry, tag) shape of a Session Log file, header line skipped.
fn log_shape(path: &str) -> Vec<(String, Option<String>)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (
                v["e"].as_str().unwrap().to_string(),
                v.get("tag").and_then(|t| t.as_str()).map(String::from),
            )
        })
        .collect()
}

fn shape(pairs: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    pairs
        .iter()
        .map(|(e, t)| (e.to_string(), t.map(String::from)))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn riders_are_logged_in_linear_position_as_they_are_injected() {
    let dir = TempDir::new().unwrap();
    let session = rider_session(&dir);
    let session_dir = session.session_dir.clone();
    let agent = start(session, FakeLlm::script(exploring_script()));
    let mut rx = agent.subscribe();

    agent.submit("look around").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;
    drop(agent);

    let path = log::latest(&session_dir).expect("a log file");
    assert_eq!(
        log_shape(&path),
        shape(&[
            ("user_text", None),
            // Pass 1: an ordinary tool Pass, nothing rides.
            ("assistant_blocks", None),
            ("tool_result", None),
            // Pass 2: the Anchor (interval 2) and the wrap-up warning
            // (2 Passes remaining) ride the results tail, in merge order.
            ("assistant_blocks", None),
            ("tool_result", None),
            ("rider", Some("anchor")),
            ("rider", Some("wrap_up_warning")),
            // Pass 3: the Explore Nudge (3rd exploration Pass) rides
            // before the final-Pass prompt (1 remaining).
            ("assistant_blocks", None),
            ("tool_result", None),
            ("nudge", None),
            ("rider", Some("final_pass")),
            // Pass 4: the tool-less final Pass concludes.
            ("assistant_blocks", None),
            ("settled", None),
        ])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_carried_riders_resumes_byte_for_byte() {
    let dir = TempDir::new().unwrap();
    let session = rider_session(&dir);
    let session_dir = session.session_dir.clone();
    let agent = start(session.clone(), FakeLlm::script(exploring_script()));
    let mut rx = agent.subscribe();

    agent.submit("look around").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;

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
    // read live - Anchor and Endgame prompts included.
    assert_eq!(resumed.conversation().await.messages, live.messages);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unverified_write_logs_the_verification_pass_prompt_and_resumes_byte_for_byte() {
    let dir = TempDir::new().unwrap();
    let session = rider_session_no_recovery(&dir);
    let session_dir = session.session_dir.clone();
    // A successful write and no run_command: the Verification Pass prompt
    // subsumes the wrap-up warning at 2 remaining, and the Verify Nudge
    // (a standalone finish Nudge, not a rider) fires on the early finish.
    let script = vec![
        Entry::just(tool_use_result(
            "w1",
            "write_file",
            json!({ "path": "new.txt", "content": "hello" }),
        )),
        Entry::just(tool_use_result("t2", "list_files", json!({ "path": "." }))),
        Entry::just(text_end("not verified yet")),
        Entry::just(text_end("done")),
    ];
    let agent = start(session.clone(), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("write it").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;

    let live = agent.conversation().await;
    drop(agent);

    let path = log::latest(&session_dir).expect("a log file");
    assert_eq!(
        log_shape(&path),
        shape(&[
            ("user_text", None),
            ("assistant_blocks", None),
            ("tool_result", None),
            ("assistant_blocks", None),
            ("tool_result", None),
            ("rider", Some("anchor")),
            ("rider", Some("verification_pass")),
            ("assistant_blocks", None),
            ("nudge", None),
            ("assistant_blocks", None),
            ("settled", None),
        ])
    );

    let resumed = AgentHandle::start(
        StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
            .with_system_prompt("You are a test agent.")
            .with_resume(Resume::Path(path)),
    )
    .expect("resumes");
    assert_eq!(resumed.conversation().await.messages, live.messages);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plan_survives_a_turn_boundary_and_is_restored_on_resume() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let session_dir = session.session_dir.clone();
    let script = vec![
        Entry::just(tool_use_result(
            "p1",
            "plan",
            json!({ "plan": "Goal: Y. 1. do [ ]" }),
        )),
        Entry::just(text_end("planned")),
    ];
    let first = start(session.clone(), FakeLlm::script(script));
    let mut rx = first.subscribe();

    first.submit("do Y").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;

    assert_eq!(first.plan().await.as_deref(), Some("Goal: Y. 1. do [ ]"));
    drop(first);

    let path = log::latest(&session_dir).expect("a log file");
    let resumed = AgentHandle::start(
        StartOpts::new(session, Arc::new(FakeLlm::script(vec![])))
            .with_system_prompt("You are a test agent.")
            .with_resume(Resume::Path(path)),
    )
    .expect("resumes");

    assert_eq!(resumed.plan().await.as_deref(), Some("Goal: Y. 1. do [ ]"));
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
            // Tuned so THREE small Turns cross the Compaction Target and
            // two do not: the tool-spec overhead rides the estimate, so
            // this number tracks the registry (web_fetch, ADR-0024, moved
            // it from 4000; run_command's pipefail description moved it
            // from 4200; the no-invented-line-numbers Voice rule moved it
            // from 4230; the grow-in-verified-steps Voice rule moved it
            // from 4320; the run-commands-whole Voice rule moved it from
            // 4480).
            context_budget: Some(4640),
            eviction_slack: Some(0.3),
            compaction_keep: Some(0.1),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .expect("session builds");
    let session_dir = session.session_dir.clone();

    // Adaptation of baud's mid-test `Baud.FakeLLM.script(...)` re-scripting:
    // the Rust FakeLlm is per-instance with a fixed queue (ADR-0020), so all
    // entries ride ONE script up front - three small Turns to build history
    // past the compaction target, then the proactive summarization call
    // (popped FIRST on the next submit) and that Turn's own reply.
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
        recv_match(&mut rx, is_turn_finished).await;
    }
    // The next submit trips proactive compaction before its own reply.
    agent.submit("keep going").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;

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
// turns": in tokio a dropped broadcast Receiver auto-cleans on the next
// send, so there is no monitor/DOWN to model. We DROP a Receiver (the tokio
// analog of the subscriber process dying), then run a full Turn and assert a
// live subscriber still gets every event and the Agent stays healthy.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_subscriber_is_pruned_and_does_not_break_later_turns() {
    let dir = TempDir::new().unwrap();
    let agent = start(
        session_in(&dir),
        FakeLlm::script(vec![Entry::response(
            vec![Delta::Text("ok".into())],
            text_end("ok"),
        )]),
    );

    // A subscriber that immediately goes away.
    let dead = agent.subscribe();
    drop(dead);

    let mut rx = agent.subscribe();
    agent.submit("still alive?").await.unwrap();
    recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(agent.status().await, Status::Idle);
}

// ---- streaming responsiveness ----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn tool_use_during_streaming_steer_then_unblock_no_crash() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut inflight) = Entry::barrier();
    let script = vec![barrier, Entry::just(text_end("done"))];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.submit("test streaming").await.unwrap();

    // The first model call parks in-flight (mid-Turn). The first Pass's
    // MessageStart has already gone out; steer NOW - the Turn is running but
    // has not reached its drain point - then release into a tool_use.
    // `steer().await` round-trips through the Agent, so the text is queued
    // before the tool batch runs and the drain delivers it (this removes
    // baud's implicit scheduler race while preserving the observable
    // behavior: steering issued mid-Turn, delivered after the tool batch, no
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
    recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(agent.status().await, Status::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_during_streaming_does_not_crash() {
    let dir = TempDir::new().unwrap();
    let (barrier, mut inflight) = Entry::barrier();
    let agent = start(session_in(&dir), FakeLlm::script(vec![barrier]));
    let mut rx = agent.subscribe();

    agent.submit("cancel me").await.unwrap();

    let _inflight = inflight.recv().await.expect("blocked in llm");
    agent.cancel().await;
    // The barrier drops its release when the test ends; the parked call is
    // aborted at the await.

    recv_match(&mut rx, |e| matches!(e, Event::TurnCancelled)).await;
    assert_eq!(agent.status().await, Status::Idle);
}

// ---- Active Model / set_model (ADR-0033) --------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn set_model_changes_what_active_model_returns() {
    let dir = TempDir::new().unwrap();
    let agent = start(session_in(&dir), FakeLlm::script(vec![]));

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
    let dir = TempDir::new().unwrap();
    let agent = start(session_in(&dir), FakeLlm::script(vec![]));
    let before = agent.active_model().await;

    let err = agent.set_model("nowhere/model".into()).await.unwrap_err();
    assert!(err.contains("nowhere"), "error was: {err}");
    assert_eq!(agent.active_model().await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_spawned_after_set_model_uses_the_new_model() {
    // The next Turn captures the Agent's mutable Model, so the boundary call
    // carries the new one - not the Session's launch-time one (ADR-0033).
    let dir = TempDir::new().unwrap();
    let (model_tx, mut model_rx) = mpsc::unbounded_channel::<Model>();
    let script = vec![Entry::dynamic(
        vec![],
        move |_req: &LlmRequest, model: &Model| {
            let _ = model_tx.send(model.clone());
            text_end("done")
        },
    )];
    let agent = start(session_in(&dir), FakeLlm::script(script));
    let mut rx = agent.subscribe();

    agent.set_model("local/picked-model".into()).await.unwrap();
    agent.submit("go").await.unwrap();

    let captured = model_rx.recv().await.expect("model");
    assert_eq!(captured.scoped_id(), "local/picked-model");
    assert_eq!(captured.id, "picked-model");

    recv_match(&mut rx, is_turn_finished).await;
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
async fn set_model_rejects_a_pick_that_cannot_fit_and_keeps_the_active_model() {
    // The per-Model budget check at the swap (ADR-0037): with a 2_000 global
    // cap, a pick synthesized at the config max_tokens knob (8_000) cannot
    // fit - it is rejected with the reason, and nothing changes.
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
        .set_model("local/way-too-big".into())
        .await
        .unwrap_err();
    assert!(err.contains("leave room"), "error was: {err}");
    assert!(err.contains("local/way-too-big"), "error was: {err}");
    assert_eq!(agent.active_model().await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_budget_follows_the_captured_model_across_a_swap() {
    // The budget derives from the Model each Turn captures (ADR-0037): the
    // first Turn runs at the launch Model's window, and after a `/model` swap
    // to a narrower Provider the NEXT Turn runs at the picked window - visible
    // on TurnFinished, which carries the settling Conversation's budget.
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
            context_window: Some(4_000),
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
        Event::TurnFinished { context_budget, .. } => Some(*context_budget),
        _ => None,
    };

    agent.submit("first").await.unwrap();
    let ev = recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(finished_budget(&ev), Some(64_000), "the launch window");

    agent.set_model("tiny/m".into()).await.unwrap();
    agent.submit("second").await.unwrap();
    let ev = recv_match(&mut rx, is_turn_finished).await;
    assert_eq!(finished_budget(&ev), Some(4_000), "the captured window");
}
