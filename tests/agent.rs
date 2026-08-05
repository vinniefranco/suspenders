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
use super::background::BackgroundStatus;
use super::*;
use crate::approvals::ApprovalMode;
use crate::content::{ContentBlock, Message, Role, Usage, UserPrompt};
use crate::llm::model::Api;

impl AgentState {
    /// A minimal [`AgentState`] for the background-registry unit tests (P4b,
    /// ADR-0063): a real Session/Model/Llm and the `self_tx` a detached child
    /// posts `BackgroundDone` back over, everything else empty/default. Returns
    /// the state alongside the receiver end of its own mpsc, so a test can drain
    /// the `BackgroundDone` the child posts.
    fn for_test(session: Session, llm: Arc<dyn Llm>) -> (AgentState, mpsc::UnboundedReceiver<Msg>) {
        let model = session.model.clone();
        let (self_tx, rx) = mpsc::unbounded_channel();
        let (events, _rx0) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let subagents = Arc::new(crate::subagents::SubagentRegistry::new(
            crate::subagents::builtins(),
        ));
        let skill_manager = Arc::new(crate::skills::SkillManager::default());
        let conversation = Conversation::new(
            "sys",
            crate::conversation::ConversationOpts::new(10_000, 1_000),
        );
        let state = AgentState {
            run_provenance: model.provenance(),
            model,
            llm,
            conversation,
            log: None,
            resume_info: None,
            plan: None,
            events,
            task: None,
            cancel_flag: false,
            settlement: Settlement::new(),
            approvals: Approvals::new(),
            approval_mode: Arc::new(crate::approvals::AtomicApprovalMode::default()),
            plan_exit_notice: Arc::new(crate::approvals::PendingManualPlanExit::empty()),
            approval_replies: HashMap::new(),
            question_replies: HashMap::new(),
            plan_replies: HashMap::new(),
            pre_plan_mode: None,
            approval_mode_revision: 0,
            steering: Vec::new(),
            compaction: Compaction::new(),
            self_tx,
            mcp: crate::mcp::manager::McpManager::default(),
            session_tools: crate::tools::tools().into(),
            skill_manager,
            hook_manager: Arc::new(crate::hooks::HookManager::default()),
            subagents,
            background: HashMap::new(),
            notifications: Vec::new(),
            background_counter: 0,
            background_shells: HashMap::new(),
            background_shell_counter: 0,
            session,
        };
        (state, rx)
    }
}
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

// Reads the latest session log and returns every JSONL entry whose `e` kind
// matches - the one place the tests parse the log file (settled/compacted).
fn log_entries_of(session_dir: &str, kind: &str) -> Vec<Value> {
    let path = log::latest(session_dir).expect("a log file");
    let content = std::fs::read_to_string(&path).unwrap();
    content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["e"] == kind)
        .collect()
}

// Waits for the successful ToolResult carrying `id` (the non-error tool return
// the tests assert after an approval/run). Returns the event for content checks.
async fn recv_tool_ok(rx: &mut broadcast::Receiver<Event>, id: &str) -> Event {
    recv_match(
        rx,
        |e| matches!(e, Event::ToolResult { id: got, is_error: false, .. } if got == id),
    )
    .await
}

// A steering harness: the FIRST scripted call parks (a barrier), the SECOND
// captures the LlmRequest it saw (over `req_rx`) so a test can inspect the
// boundary request the Run built - e.g. the drained steering riding the
// tool-result message. Submits `prompt`, awaits the parked first call, and
// hands back its `release` (drop the returned dir to end the test).
struct Steering {
    _dir: TempDir,
    agent: AgentHandle,
    rx: broadcast::Receiver<Event>,
    req_rx: mpsc::UnboundedReceiver<LlmRequest>,
    release: oneshot::Sender<Release>,
}

async fn steering_harness(prompt: &str, reply: &'static str) -> Steering {
    let (barrier, mut inflight) = Entry::barrier();
    let (req_tx, req_rx) = mpsc::unbounded_channel::<LlmRequest>();
    let (dir, agent, rx) = harness(vec![
        barrier,
        Entry::dynamic(vec![], move |req: &LlmRequest, _model: &Model| {
            let _ = req_tx.send(req.clone());
            text_end(reply)
        }),
    ]);
    agent.submit(prompt).await.unwrap();
    let InFlight { release, .. } = inflight.recv().await.expect("first call parked");
    Steering {
        _dir: dir,
        agent,
        rx,
        req_rx,
        release,
    }
}

// Waits for the SteeringQueued event carrying `text` (the observable that the
// steered text was accepted mid-Run before the drain delivers it).
async fn recv_steering_queued(rx: &mut broadcast::Receiver<Event>, text: &str) {
    recv_match(
        rx,
        |e| matches!(e, Event::SteeringQueued { text: t } if t == text),
    )
    .await;
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
        assert_eq!(stop_reason, crate::stop_reason::StopReason::EndTurn);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_submitted_media_prompt_reaches_the_conversation_as_one_user_message() {
    // ADR-0068 P2: a UserPrompt carrying Text + an Image block enters the
    // Conversation as ONE user Message, blocks in order - the media survives
    // the submit pipe (Composer -> Agent -> start_run -> add_user_content).
    let dir = TempDir::new().unwrap();
    let fake = FakeLlm::script(vec![Entry::response(
        vec![Delta::Text("a cat".into())],
        text_end("a cat"),
    )]);
    let session = session_in(&dir);
    let agent = start(session, fake);
    let mut rx = agent.subscribe();

    agent
        .submit(UserPrompt::from_blocks(vec![
            ContentBlock::text("what is in "),
            ContentBlock::image("image/png", "AAAA"),
        ]))
        .await
        .unwrap();

    recv_match(&mut rx, is_run_finished).await;

    let conv = agent.conversation().await;
    assert_eq!(
        conv.messages.first(),
        Some(&Message::user(vec![
            ContentBlock::text("what is in "),
            ContentBlock::image("image/png", "AAAA"),
        ]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_steered_media_prompt_delivers_its_text_projection_to_the_run() {
    // ADR-0068 P2: Steering is delivered as trailing user-text on the Pass's
    // Tool Result message, so a steered media prompt contributes its TEXT
    // projection today (the queue is text; a mid-Run media turn is P3). The
    // projection rides the next request's tool-result user message, unadorned.
    let Steering {
        agent,
        mut rx,
        mut req_rx,
        release,
        ..
    } = steering_harness("look around", "done").await;

    agent
        .steer(UserPrompt::from_blocks(vec![
            ContentBlock::text("also see "),
            ContentBlock::image("image/png", "AAAA"),
        ]))
        .await
        .unwrap();
    // BUG 4: the QUEUED (display) event shows the CLEAN typed text, never the raw
    // `[image: <mime>]` media projection - that placeholder only rides the WIRE
    // (asserted below via SteeringDelivered + the request message).
    recv_steering_queued(&mut rx, "also see ").await;

    release
        .send(Release {
            deltas: vec![],
            response: tool_use_result("t1", "list_directory", json!({ "path": "." })),
        })
        .ok();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::SteeringDelivered { text } if text == "also see [image: image/png]"),
    )
    .await;

    let request = req_rx.recv().await.expect("second request");
    let last = request.messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(
        last.content[1],
        ContentBlock::text("also see [image: image/png]")
    );

    recv_match(&mut rx, is_run_finished).await;
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
            "run_shell_command",
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
                    if tool_use_id == "tu_run"
                        && crate::content::result_blocks_text(content) == "[command denied by user]")
            })
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn approved_run_command_executes_and_returns_its_output() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "tu_run",
            "run_shell_command",
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
    let result = recv_tool_ok(&mut rx, "tu_run").await;
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
    recv_match(
        &mut rx,
        |e| matches!(e, Event::QuestionResolved { question_id } if *question_id == id),
    )
    .await;

    // The tool result is the VERBATIM formatted answer string.
    let result = recv_tool_ok(&mut rx, "tu_ask").await;
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

// ---- plan mode (ADR-0067) ---------------------------------------------

// A Run whose model calls `enter_plan_mode` (userRequested true): the Agent flips
// the mode to Plan, broadcasts ApprovalModeChanged(Plan), writes the mirror, and
// the tool result is the plan-mode reminder VERBATIM.
#[tokio::test(flavor = "multi_thread")]
async fn enter_plan_mode_flips_the_mode_and_returns_the_reminder() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "tu_enter",
            "enter_plan_mode",
            json!({ "userRequested": true }),
        )),
        Entry::just(text_end("planning now")),
    ]);

    agent.submit("switch to plan mode").await.unwrap();

    // The Agent broadcasts the mode change to Plan.
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalModeChanged { mode } if *mode == ApprovalMode::Plan),
    )
    .await;

    // The tool result is the plan-mode reminder verbatim.
    let result = recv_tool_ok(&mut rx, "tu_enter").await;
    if let Event::ToolResult { content, .. } = result {
        assert_eq!(content, crate::voice::plan_mode_reminder());
    }
    recv_match(&mut rx, is_run_finished).await;
}

// The full exit round-trip: enter plan, then exit_plan_mode raises a PlanRequest;
// a driven AnswerPlan(ProceedOnce) flips the mode to Default, saves the plan to
// disk, and the tool result is the VERBATIM "User approved." content.
#[tokio::test(flavor = "multi_thread")]
async fn exit_plan_mode_round_trips_and_flips_to_the_target_mode() {
    let (_dir, session, agent, mut rx) = harness_with_session(vec![
        Entry::just(tool_use_result(
            "tu_enter",
            "enter_plan_mode",
            json!({ "userRequested": true }),
        )),
        Entry::just(tool_use_result(
            "tu_exit",
            "exit_plan_mode",
            json!({ "plan": "1. write the code\n2. test it" }),
        )),
        Entry::just(text_end("coding now")),
    ]);

    agent.submit("plan then code").await.unwrap();

    // Step the two model calls through their own recv budgets (the reminder is a
    // large payload, so each stage gets a fresh deadline): the enter flips to
    // Plan, then the exit tool opens the plan dialog.
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalModeChanged { mode } if *mode == ApprovalMode::Plan),
    )
    .await;

    // The exit tool opened the plan dialog: a PlanRequest carrying the id, the
    // plan text, and the pre_plan_mode (Default, the mode Plan was entered from).
    let req = recv_match(&mut rx, |e| matches!(e, Event::PlanRequest { .. })).await;
    let (id, plan, pre) = match req {
        Event::PlanRequest {
            plan_id,
            plan,
            pre_plan_mode,
        } => (plan_id, plan, pre_plan_mode),
        other => panic!("expected a PlanRequest, got {other:?}"),
    };
    assert_eq!(plan, "1. write the code\n2. test it");
    assert_eq!(pre, ApprovalMode::Default);

    // Answer it as the UI would: "Yes, and manually approve edits" -> Default.
    agent
        .answer_plan(id.clone(), crate::approvals::PlanDecision::ProceedOnce)
        .await;

    // The mode flips to the target (Default) on approval.
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalModeChanged { mode } if *mode == ApprovalMode::Default),
    )
    .await;

    // The Agent emits PlanResolved once the tool reads the reply.
    recv_match(
        &mut rx,
        |e| matches!(e, Event::PlanResolved { plan_id } if *plan_id == id),
    )
    .await;

    // The tool result is the VERBATIM approved content.
    let result = recv_tool_ok(&mut rx, "tu_exit").await;
    if let Event::ToolResult { content, .. } = result {
        assert_eq!(
            content,
            "User approved. You can now start coding. Start with updating your todo list if applicable."
        );
    }
    recv_match(&mut rx, is_run_finished).await;

    // The plan was saved to <session_dir>/plan.md.
    let saved = std::fs::read_to_string(std::path::Path::new(&session.session_dir).join("plan.md"))
        .expect("plan.md written");
    assert_eq!(saved, "1. write the code\n2. test it");
}

// The keep-planning path: AnswerPlan(Cancel) stays in Plan, no mode change, and
// the tool result is the VERBATIM "Remaining in plan mode." content.
#[tokio::test(flavor = "multi_thread")]
async fn exit_plan_mode_cancel_stays_in_plan() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "tu_enter",
            "enter_plan_mode",
            json!({ "userRequested": true }),
        )),
        Entry::just(tool_use_result(
            "tu_exit",
            "exit_plan_mode",
            json!({ "plan": "the plan" }),
        )),
        Entry::just(text_end("still planning")),
    ]);

    agent.submit("plan").await.unwrap();

    // Step through the enter (a large reminder payload) so the PlanRequest recv
    // gets a fresh deadline.
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalModeChanged { mode } if *mode == ApprovalMode::Plan),
    )
    .await;
    let id = match recv_match(&mut rx, |e| matches!(e, Event::PlanRequest { .. })).await {
        Event::PlanRequest { plan_id, .. } => plan_id,
        other => panic!("expected a PlanRequest, got {other:?}"),
    };

    agent
        .answer_plan(id.clone(), crate::approvals::PlanDecision::Cancel)
        .await;

    let result = recv_tool_ok(&mut rx, "tu_exit").await;
    if let Event::ToolResult { content, .. } = result {
        assert_eq!(
            content,
            "Plan execution was not approved. Remaining in plan mode."
        );
    }
    recv_match(&mut rx, is_run_finished).await;
}

// ---- standing approval ------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn approve_always_records_the_command_the_identical_command_is_auto_approved() {
    let (_dir, agent, mut rx) = harness(vec![
        Entry::just(tool_use_result(
            "r1",
            "run_shell_command",
            json!({ "command": "echo hi" }),
        )),
        Entry::just(tool_use_result(
            "ls",
            "list_directory",
            json!({ "path": "." }),
        )),
        Entry::just(tool_use_result(
            "r2",
            "run_shell_command",
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
    recv_tool_ok(&mut rx, "r1").await;

    // The identical second command: no modal, an approval_auto, still runs.
    recv_match(
        &mut rx,
        |e| matches!(e, Event::ApprovalAuto { command } if command == "echo hi"),
    )
    .await;
    let r2 = recv_tool_ok(&mut rx, "r2").await;
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
            "run_shell_command",
            json!({ "command": "echo hi" }),
        )),
        Entry::just(tool_use_result(
            "r2",
            "run_shell_command",
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
    recv_tool_ok(&mut rx, "r1").await;

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
            "run_shell_command",
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
    let result = recv_tool_ok(&mut rx, "y1").await;
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
    let Steering {
        agent,
        mut rx,
        mut req_rx,
        release,
        ..
    } = steering_harness("look around", "done").await;

    // First call is parked; steer, then release into a tool_use.
    agent.steer("also check the README").await.unwrap();
    recv_steering_queued(&mut rx, "also check the README").await;

    release
        .send(Release {
            deltas: vec![],
            response: tool_use_result("t1", "list_directory", json!({ "path": "." })),
        })
        .ok();

    recv_match(
        &mut rx,
        |e| matches!(e, Event::SteeringDelivered { text } if text == "also check the README"),
    )
    .await;

    // Unadorned, riding the SAME user message as the tool results.
    let request = req_rx.recv().await.expect("second request");
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
    let Steering {
        agent,
        mut rx,
        mut req_rx,
        release,
        ..
    } = steering_harness("first thing", "second done").await;

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

    let request = req_rx.recv().await.expect("rollover request");
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
        Message::assistant(vec![ContentBlock::text(voice::Marker::RunCancelled.text())])
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
    // list_files requires an ABSOLUTE path (qwen ls contract); build the dir
    // first so the tool_use can carry its absolute path.
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let path = dir.path().to_string_lossy().into_owned();
    let (agent, mut rx) = session_harness(
        session,
        vec![
            Entry::just(tool_use_result(
                "t1",
                "list_directory",
                json!({ "path": path }),
            )),
            barrier,
        ],
    );

    agent.submit("explore then hang").await.unwrap();

    // The tool ran; only then cancel (its result is on disk/in the conv).
    recv_tool_ok(&mut rx, "t1").await;
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
        Message::assistant(vec![ContentBlock::text(voice::Marker::RunCancelled.text())])
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
            vec![ContentBlock::text(voice::Marker::RunFailed.text())],
            provenance
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_llm_error_after_a_tool_ran_keeps_the_partial_run_under_the_failure_marker() {
    let (_dir, session, agent, mut rx) = harness_with_session(vec![
        Entry::just(tool_use_result(
            "t1",
            "list_directory",
            json!({ "path": "." }),
        )),
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
            vec![ContentBlock::text(voice::Marker::RunFailed.text())],
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

    let settled = log_entries_of(&session_dir, "settled");
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
        Entry::just(tool_use_result(
            "t1",
            "list_directory",
            json!({ "path": "." }),
        )),
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
        Entry::just(tool_use_result(
            "t1",
            "list_directory",
            json!({ "path": "." }),
        )),
        Entry::just(tool_use_result(
            "t2",
            "list_directory",
            json!({ "path": "." }),
        )),
        Entry::just(tool_use_result(
            "t3",
            "list_directory",
            json!({ "path": "." }),
        )),
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
            json!({ "todos": [{ "id": "1", "content": "do Y", "status": "in_progress" }] }),
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

    // The proactive-compaction target rides the per-request token estimate, which
    // includes tool-spec + system-prompt overhead - a figure that shifts whenever
    // a tool description changes. Rather than hardcode a budget that needs
    // re-tuning on every prompt edit, PROBE the settled estimates after Run 2 and
    // Run 3 with a budget so large compaction never fires, then derive the real
    // budget so the target lands strictly between them (only the third Run
    // crosses). estimate = ceil((overhead + system + messages)/3.5); the target is
    // budget - reserve(200) - trunc(0.3*budget), i.e. about 0.7*budget - 200, so
    // budget = (target + 200) / 0.7 recovers the knob from a chosen target.
    let reply = "word ".repeat(250);
    let probe_dir = TempDir::new().unwrap();
    let probe = start_voiced(
        Session::build(
            SessionOpts {
                root: Some(probe_dir.path().to_string_lossy().into_owned()),
                session_dir: Some(
                    probe_dir
                        .path()
                        .join("sessions")
                        .to_string_lossy()
                        .into_owned(),
                ),
                model: Some(model.clone()),
                // Under the 64k model window and far above any run estimate, so
                // the probe never compacts and measures clean settled estimates.
                context_budget: Some(60_000),
                compaction_slack: Some(0.3),
                compaction_keep: Some(0.066),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .expect("probe session builds"),
        FakeLlm::script(vec![
            Entry::just(text_end(&format!("{reply} 1"))),
            Entry::just(text_end(&format!("{reply} 2"))),
            Entry::just(text_end(&format!("{reply} 3"))),
        ]),
    );
    let mut prx = probe.subscribe();
    let mut est = [0u64; 3];
    for (n, slot) in est.iter_mut().enumerate() {
        probe.submit(format!("step {}", n + 1)).await.unwrap();
        if let Event::RunFinished { token_estimate, .. } =
            recv_match(&mut prx, is_run_finished).await
        {
            *slot = token_estimate;
        }
    }
    drop(probe);
    // Midpoint target between the post-Run2 and post-Run3 estimates, inverted
    // through target ~= 0.7*budget - 200 to recover the budget knob.
    let budget = ((est[1] + est[2]) / 2 + 200) * 10 / 7;

    let session = Session::build(
        SessionOpts {
            root: Some(dir.path().to_string_lossy().into_owned()),
            session_dir: Some(dir.path().join("sessions").to_string_lossy().into_owned()),
            model: Some(model),
            // Derived from the probe above: the budget slides the Compaction
            // Target (~0.7*budget - 200) to sit between the post-Run2 and
            // post-Run3 estimates, so exactly the third Run crosses. Replaces a
            // hand-tuned constant that drifted on every tool-description / prompt
            // edit (the estimate carries the tool-spec + system-prompt overhead).
            context_budget: Some(budget),
            compaction_slack: Some(0.3),
            // `compaction_keep` is the fraction of `budget - reserve` kept as raw
            // chars past the cut. At keep 0.066 over these ~1250-char replies the
            // absolute Keep (~2k chars) survives roughly one reply, so the third
            // Run still cuts to a summary head rather than retaining everything.
            compaction_keep: Some(0.066),
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
    let compacted = log_entries_of(&session_dir, "compacted");
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
            response: tool_use_result("t1", "list_directory", json!({ "path": "." })),
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
    let fake = FakeLlm::script(vec![]).with_models(vec![Ok(vec![
        crate::llm::DiscoveredModel {
            id: "a/model".into(),
            context_window: None,
        },
        crate::llm::DiscoveredModel {
            id: "b/model".into(),
            context_window: None,
        },
    ])]);
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
            // enough to hold the tool-spec overhead (~34.5k chars ~= 9.9k tokens
            // after the ADR-0067 plan-mode tools joined the base wire list -
            // enter_plan_mode is always-visible and exit_plan_mode is
            // always-declared) plus the reserve and a small Conversation -
            // otherwise the post-swap Run can never fit its request and this test
            // would hang. The value only has to be distinct from the launch
            // window for the assertion.
            context_window: Some(24_000),
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
    assert_eq!(finished_budget(&ev), Some(24_000), "the captured window");
}

// ===========================================================================
// Background subagent registry (P4b/4c/4d, ADR-0063). These drive the Agent's
// registry handlers directly on an `AgentState::for_test`, so the mint/notify/
// abort mechanics are observable without wiring a whole Run through the mpsc.
// ===========================================================================

use crate::tool::caps::{SubagentRequest, SubagentResult};

fn bg_request(subagent_type: &str) -> SubagentRequest {
    SubagentRequest {
        subagent_type: subagent_type.into(),
        prompt: "do the task".into(),
        model: None,
    }
}

// Builds a registered BackgroundTask over a never-firing spawned child, tagged
// with the given status + description. Returns the JoinHandle too so a test can
// assert the child was aborted (`handle.is_finished()`) after a stop.
fn bg_task(
    status: BackgroundStatus,
    description: &str,
) -> (BackgroundTask, tokio::task::JoinHandle<()>) {
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    let task = BackgroundTask {
        abort: handle.abort_handle(),
        status,
        description: description.into(),
    };
    (task, handle)
}

// The live Approval-mode mirror (ADR-0067): `cycle_approval_mode` writes the
// shared atomic in lockstep with the pure `approvals` fold, so an in-flight Run's
// batch gate reads the operator's live mode. This pins that the cycle mutation
// path updates the mirror (Phase 2's enter/exit tools will get their own pins).
#[tokio::test]
async fn cycle_approval_mode_writes_the_shared_atomic_mirror() {
    let dir = TempDir::new().unwrap();
    let (mut state, _rx) =
        super::AgentState::for_test(session_in(&dir), Arc::new(FakeLlm::script(vec![])));

    // At launch the mirror agrees with the fold's default.
    assert_eq!(state.approvals.mode, ApprovalMode::Default);
    assert_eq!(state.approval_mode.load(), ApprovalMode::Default);

    // Each cycle advances BOTH the fold and the mirror, in lockstep, through the
    // full qwen order: default -> auto-edit -> auto -> yolo -> plan -> default.
    for expected in [
        ApprovalMode::AutoEdit,
        ApprovalMode::Auto,
        ApprovalMode::Yolo,
        ApprovalMode::Plan,
        ApprovalMode::Default,
    ] {
        let landed = super::cycle_approval_mode(&mut state);
        assert_eq!(landed, expected);
        assert_eq!(state.approvals.mode, expected, "fold advanced");
        assert_eq!(
            state.approval_mode.load(),
            expected,
            "mirror advanced in lockstep with the fold"
        );
    }
}

// ---- plan mode: Agent-side state mutation (ADR-0067) ------------------

use crate::approvals::PlanDecision;
use crate::tool::caps::{EnterPlanOutcome, PlanExitOutcome};

fn fresh_state(dir: &TempDir) -> super::AgentState {
    super::AgentState::for_test(session_in(dir), Arc::new(FakeLlm::script(vec![]))).0
}

// enter_plan_mode from Default: flips to Plan, records prePlanMode = Default,
// bumps the revision, writes the mirror, and returns the reminder outcome.
#[tokio::test]
async fn enter_plan_mode_records_pre_plan_mode_and_bumps_revision() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    let rev0 = state.approval_mode_revision;

    let outcome = super::enter_plan_mode(&mut state, true);

    assert!(matches!(outcome, EnterPlanOutcome::Entered { .. }));
    assert_eq!(state.approvals.mode, ApprovalMode::Plan);
    assert_eq!(
        state.approval_mode.load(),
        ApprovalMode::Plan,
        "mirror written"
    );
    assert_eq!(
        state.pre_plan_mode,
        Some(ApprovalMode::Default),
        "prePlanMode recorded"
    );
    assert_eq!(state.approval_mode_revision, rev0 + 1, "revision bumped");
}

// A model-initiated entry from YOLO (userRequested false) is a no-op: StayedYolo,
// no mode change, no prePlanMode, no revision bump.
#[tokio::test]
async fn enter_plan_mode_from_yolo_without_user_request_is_a_no_op() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::set_approval_mode(&mut state, ApprovalMode::Yolo, false);
    let rev = state.approval_mode_revision;

    let outcome = super::enter_plan_mode(&mut state, false);

    assert!(matches!(outcome, EnterPlanOutcome::StayedYolo));
    assert_eq!(state.approvals.mode, ApprovalMode::Yolo, "stayed in YOLO");
    assert_eq!(state.pre_plan_mode, None);
    assert_eq!(state.approval_mode_revision, rev, "no revision bump");
}

// An explicit user request DOES enter Plan from YOLO (the userRequested override).
#[tokio::test]
async fn enter_plan_mode_from_yolo_with_user_request_enters_plan() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::set_approval_mode(&mut state, ApprovalMode::Yolo, false);

    let outcome = super::enter_plan_mode(&mut state, true);

    assert!(matches!(outcome, EnterPlanOutcome::Entered { .. }));
    assert_eq!(state.approvals.mode, ApprovalMode::Plan);
    assert_eq!(
        state.pre_plan_mode,
        Some(ApprovalMode::Yolo),
        "prePlanMode is YOLO"
    );
}

// Idempotent: entering plan while already in plan never overwrites prePlanMode
// and does not bump the revision (qwen's `getApprovalMode() !== PLAN` guard).
#[tokio::test]
async fn enter_plan_mode_is_idempotent_and_preserves_pre_plan_mode() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    // Enter from AutoEdit so prePlanMode = AutoEdit.
    super::set_approval_mode(&mut state, ApprovalMode::AutoEdit, false);
    super::enter_plan_mode(&mut state, true);
    assert_eq!(state.pre_plan_mode, Some(ApprovalMode::AutoEdit));
    let rev = state.approval_mode_revision;

    // A second enter while already in Plan.
    let outcome = super::enter_plan_mode(&mut state, true);

    assert!(matches!(outcome, EnterPlanOutcome::Entered { .. }));
    assert_eq!(state.approvals.mode, ApprovalMode::Plan);
    assert_eq!(
        state.pre_plan_mode,
        Some(ApprovalMode::AutoEdit),
        "prePlanMode preserved"
    );
    assert_eq!(state.approval_mode_revision, rev, "no extra revision bump");
}

// Leaving Plan (any path) clears prePlanMode: a Shift+Tab cycle out of Plan
// through set_approval_mode drops it (qwen's `mode !== PLAN && fromMode === PLAN`).
#[tokio::test]
async fn leaving_plan_clears_pre_plan_mode() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::enter_plan_mode(&mut state, true);
    assert_eq!(state.pre_plan_mode, Some(ApprovalMode::Default));

    super::set_approval_mode(&mut state, ApprovalMode::Default, false);
    assert_eq!(
        state.pre_plan_mode, None,
        "prePlanMode cleared on leaving Plan"
    );
}

// A MANUAL exit (from_approved_plan_exit == false, the Shift+Tab path) queues the
// one-shot manual-exit notice carrying the new mode (ADR-0067, qwen's
// 'manual-exit' notice event). The loop takes it in shape_request.
#[tokio::test]
async fn manual_leave_of_plan_queues_the_exit_notice() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::enter_plan_mode(&mut state, true);
    assert_eq!(
        state.plan_exit_notice.take(),
        None,
        "nothing queued while in Plan"
    );

    // The Shift+Tab cycle path leaves Plan with from_approved_plan_exit == false.
    super::set_approval_mode(&mut state, ApprovalMode::Default, false);

    assert_eq!(
        state.plan_exit_notice.take(),
        Some(ApprovalMode::Default),
        "a manual exit queues the notice carrying the new mode"
    );
    assert_eq!(
        state.plan_exit_notice.take(),
        None,
        "one-shot: gone after the take"
    );
}

// The Shift+Tab cycle helper itself queues the notice when it leaves Plan (it
// routes through set_approval_mode with from_approved_plan_exit == false).
#[tokio::test]
async fn cycle_out_of_plan_queues_the_exit_notice() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::enter_plan_mode(&mut state, true);

    // Plan -> Default in the cycle order (qwen APPROVAL_MODES).
    let new_mode = super::cycle_approval_mode(&mut state);
    assert_eq!(new_mode, ApprovalMode::Default, "Plan cycles to Default");

    assert_eq!(
        state.plan_exit_notice.take(),
        Some(ApprovalMode::Default),
        "a Shift+Tab cycle out of Plan queues the manual-exit notice"
    );
}

// An APPROVED exit (answer_plan, from_approved_plan_exit == true) does NOT queue
// the manual-exit notice (qwen's fromApprovedPlanExit -> 'clear'): the standing
// plan reminder simply stops, no manual-exit reminder is injected.
#[tokio::test]
async fn approved_exit_does_not_queue_the_exit_notice() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::enter_plan_mode(&mut state, true);

    let (reply, rx) = tokio::sync::oneshot::channel();
    super::request_plan_exit(&mut state, "the plan".into(), reply);
    let id = state
        .plan_replies
        .keys()
        .next()
        .cloned()
        .expect("one parked exit");

    super::answer_plan(&mut state, id, PlanDecision::ProceedOnce);
    assert_eq!(rx.await.unwrap(), PlanExitOutcome::Approved);
    assert_ne!(
        state.approvals.mode,
        ApprovalMode::Plan,
        "left Plan on approval"
    );

    assert_eq!(
        state.plan_exit_notice.take(),
        None,
        "an approved exit_plan_mode never queues the manual-exit notice"
    );
}

// The `/plan` command's ENTER path (enter_plan_mode_requested): a user request,
// so it enters Plan even from Yolo, records prePlanMode, and returns Plan.
#[tokio::test]
async fn plan_command_enter_enters_plan_even_from_yolo() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::set_approval_mode(&mut state, ApprovalMode::Yolo, false);

    let mode = super::enter_plan_mode_requested(&mut state);

    assert_eq!(mode, ApprovalMode::Plan, "returned the new (Plan) mode");
    assert_eq!(state.approvals.mode, ApprovalMode::Plan);
    assert_eq!(
        state.pre_plan_mode,
        Some(ApprovalMode::Yolo),
        "prePlanMode is the mode we came from"
    );
}

// The `/plan exit` command's EXIT path (exit_plan_mode_restore): restores the
// pre-plan mode AND, because it routes through the MANUAL-exit path
// (from_approved_plan_exit == false), queues the one-shot manual-exit notice -
// exactly as a Shift+Tab cycle out of Plan does.
#[tokio::test]
async fn plan_command_exit_restores_pre_plan_mode_and_queues_the_manual_exit_notice() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    // Enter from AutoEdit so the restore target is AutoEdit, not the Default default.
    super::set_approval_mode(&mut state, ApprovalMode::AutoEdit, false);
    super::enter_plan_mode_requested(&mut state);
    assert_eq!(state.pre_plan_mode, Some(ApprovalMode::AutoEdit));

    let restored = super::exit_plan_mode_restore(&mut state);

    assert_eq!(
        restored,
        ApprovalMode::AutoEdit,
        "restored the pre-plan mode"
    );
    assert_eq!(state.approvals.mode, ApprovalMode::AutoEdit);
    assert_eq!(
        state.pre_plan_mode, None,
        "prePlanMode cleared on leaving Plan"
    );
    assert_eq!(
        state.plan_exit_notice.take(),
        Some(ApprovalMode::AutoEdit),
        "a /plan exit is a MANUAL exit: it queues the one-shot manual-exit notice"
    );
}

// With no prePlanMode recorded, exit restores Default (qwen's getPrePlanMode
// `?? DEFAULT`).
#[tokio::test]
async fn plan_command_exit_defaults_to_default_when_no_pre_plan_mode() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    // Force Plan without recording a prePlanMode (a resumed-in-Plan edge).
    state.approvals.mode = ApprovalMode::Plan;
    state.approval_mode.store(ApprovalMode::Plan);
    state.pre_plan_mode = None;

    let restored = super::exit_plan_mode_restore(&mut state);

    assert_eq!(restored, ApprovalMode::Default, "defaults to Default");
    assert_eq!(state.approvals.mode, ApprovalMode::Default);
}

// request_plan_exit outside plan mode returns NotInPlanMode WITHOUT raising a
// dialog (no PlanRequest, nothing parked).
#[tokio::test]
async fn request_plan_exit_outside_plan_is_not_in_plan_mode() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    let (reply, rx) = tokio::sync::oneshot::channel();

    super::request_plan_exit(&mut state, "a plan".into(), reply);

    let outcome = rx.await.unwrap();
    assert!(
        matches!(outcome, PlanExitOutcome::NotInPlanMode { current_mode } if current_mode == "default")
    );
    assert!(state.plan_replies.is_empty(), "no dialog parked");
}

// The proceed resolution flips to the decision's target and saves the plan;
// RestorePrevious lands on prePlanMode.
#[tokio::test]
async fn answer_plan_restore_previous_flips_to_pre_plan_mode() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    // Enter Plan from AutoEdit so prePlanMode = AutoEdit.
    super::set_approval_mode(&mut state, ApprovalMode::AutoEdit, false);
    super::enter_plan_mode(&mut state, true);

    let (reply, rx) = tokio::sync::oneshot::channel();
    super::request_plan_exit(&mut state, "the plan".into(), reply);
    let id = state
        .plan_replies
        .keys()
        .next()
        .cloned()
        .expect("one parked exit");

    super::answer_plan(&mut state, id, PlanDecision::RestorePrevious);

    assert_eq!(rx.await.unwrap(), PlanExitOutcome::Approved);
    assert_eq!(
        state.approvals.mode,
        ApprovalMode::AutoEdit,
        "restored to prePlanMode"
    );
    assert!(
        state.pre_plan_mode.is_none(),
        "prePlanMode cleared on leaving Plan"
    );
    let saved =
        std::fs::read_to_string(std::path::Path::new(&state.session.session_dir).join("plan.md"))
            .expect("plan saved");
    assert_eq!(saved, "the plan");
}

// The stale-guard: if the mode changed (revision moved) between raising the
// dialog and resolving it, the resolution takes NO action and returns Stale.
#[tokio::test]
async fn answer_plan_is_stale_when_the_revision_moved() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::enter_plan_mode(&mut state, true);

    let (reply, rx) = tokio::sync::oneshot::channel();
    super::request_plan_exit(&mut state, "the plan".into(), reply);
    let id = state
        .plan_replies
        .keys()
        .next()
        .cloned()
        .expect("one parked exit");

    // The mode changes out from under the pending exit (a Shift+Tab cycle),
    // bumping the revision. The exit's snapshot is now stale. (The cycle out of
    // Plan leaves the mode non-Plan, which the guard also catches; re-enter Plan
    // so we test the REVISION arm specifically, not the mode arm.)
    super::set_approval_mode(&mut state, ApprovalMode::Default, false); // rev++ (leaves Plan)
    super::set_approval_mode(&mut state, ApprovalMode::Plan, false); // rev++ (re-enters Plan)
    assert_eq!(
        state.approvals.mode,
        ApprovalMode::Plan,
        "back in Plan, but revision moved"
    );

    super::answer_plan(&mut state, id, PlanDecision::ProceedOnce);

    assert_eq!(rx.await.unwrap(), PlanExitOutcome::Stale);
    assert_eq!(
        state.approvals.mode,
        ApprovalMode::Plan,
        "no action taken, still in Plan"
    );
    assert!(
        !std::path::Path::new(&state.session.session_dir)
            .join("plan.md")
            .exists(),
        "no plan saved on a stale resolution"
    );
}

// A stale/duplicate answer_plan (no matching id) is dropped, mirroring
// answer_question's ignore path.
#[tokio::test]
async fn answer_plan_with_unknown_id_is_dropped() {
    let dir = TempDir::new().unwrap();
    let mut state = fresh_state(&dir);
    super::enter_plan_mode(&mut state, true);
    // No parked exit; a stray answer_plan is a no-op (no panic, mode unchanged).
    super::answer_plan(&mut state, "plan-999".into(), PlanDecision::ProceedOnce);
    assert_eq!(state.approvals.mode, ApprovalMode::Plan);
}

#[tokio::test]
async fn spawn_background_mints_the_id_and_registers_running() {
    let dir = TempDir::new().unwrap();
    // A FakeLlm the detached child will settle against (a plain final text).
    let fake = FakeLlm::script(vec![Entry::just(text_end("done"))]);
    let (mut state, _rx) = super::AgentState::for_test(session_in(&dir), Arc::new(fake));

    let id = state.spawn_background(bg_request("general-purpose"), "find the bug".into());
    assert_eq!(id, "general-purpose-1", "the minted id is {{type}}-{{n}}");
    assert!(state.background.contains_key("general-purpose-1"));
    // A second launch increments the per-Session counter.
    let id2 = state.spawn_background(bg_request("Explore"), "explore".into());
    assert_eq!(id2, "Explore-2");
}

#[tokio::test]
async fn a_child_settlement_queues_the_completed_notification() {
    let dir = TempDir::new().unwrap();
    let (mut state, _rx) =
        super::AgentState::for_test(session_in(&dir), Arc::new(FakeLlm::script(vec![])));
    // Register a Running entry by hand (a spawned never-firing task for the abort
    // handle), then feed a GOAL settlement.
    let (task, _handle) = bg_task(BackgroundStatus::Running, "find the bug");
    state.background.insert("general-purpose-1".into(), task);

    state.background_done(
        "general-purpose-1".into(),
        SubagentResult {
            terminate_reason: "GOAL".into(),
            result: "the findings".into(),
        },
    );

    assert_eq!(state.notifications.len(), 1);
    let note = &state.notifications[0];
    assert!(note.contains("<task-id>general-purpose-1</task-id>"));
    assert!(note.contains("<status>completed</status>"));
    assert!(note.contains("<summary>Agent \"find the bug\" completed.</summary>"));
    assert!(note.contains("<result>the findings</result>"));
}

#[tokio::test]
async fn stop_background_aborts_the_running_child_and_sets_stopped() {
    let dir = TempDir::new().unwrap();
    let (mut state, _rx) =
        super::AgentState::for_test(session_in(&dir), Arc::new(FakeLlm::script(vec![])));
    let (task, handle) = bg_task(BackgroundStatus::Running, "explore api");
    state.background.insert("scout-1".into(), task);

    let wording = state.stop_background("scout-1".into()).unwrap();
    assert_eq!(
        wording,
        "Cancellation requested for background agent \"scout-1\". A final \
         task-notification carrying the agent's last result will follow.\n\
         Description: explore api"
    );
    // The entry is now Stopped and the child task was aborted.
    assert!(matches!(
        state.background.get("scout-1").unwrap().status,
        super::background::BackgroundStatus::Stopped
    ));
    // Yield so the abort lands on the child task before asserting it finished.
    tokio::task::yield_now().await;
    assert!(handle.is_finished());
    // The `was cancelled` notification was queued synchronously.
    assert!(
        state.notifications[0].contains("<summary>Agent \"explore api\" was cancelled.</summary>")
    );
}

#[tokio::test]
async fn stop_background_unknown_id_returns_none_for_the_dual_registry_fallthrough() {
    let dir = TempDir::new().unwrap();
    let (mut state, _rx) =
        super::AgentState::for_test(session_in(&dir), Arc::new(FakeLlm::script(vec![])));
    // An id no SUBAGENT owns yields `None`, so the dual-registry handler can fall
    // through to the shell registry before synthesizing the verbatim not-found.
    assert_eq!(state.stop_background("ghost-9".into()), None);
}

#[tokio::test]
async fn stop_background_of_a_settled_task_is_the_not_running_wording() {
    let dir = TempDir::new().unwrap();
    let (mut state, _rx) =
        super::AgentState::for_test(session_in(&dir), Arc::new(FakeLlm::script(vec![])));
    let (task, _handle) = bg_task(BackgroundStatus::Done, "explore api");
    state.background.insert("scout-1".into(), task);
    let wording = state.stop_background("scout-1".into()).unwrap();
    assert_eq!(
        wording,
        "Error: Background agent \"scout-1\" is not running (status: completed)."
    );
}

#[tokio::test]
async fn a_settlement_for_a_stopped_task_is_dropped_no_double_notify() {
    let dir = TempDir::new().unwrap();
    let (mut state, _rx) =
        super::AgentState::for_test(session_in(&dir), Arc::new(FakeLlm::script(vec![])));
    let (task, _handle) = bg_task(BackgroundStatus::Stopped, "explore api");
    state.background.insert("scout-1".into(), task);
    // The child's late BackgroundDone lands after the stop: it must be dropped,
    // not queued as a second notification (idle-completion no panic).
    state.background_done(
        "scout-1".into(),
        SubagentResult {
            terminate_reason: "GOAL".into(),
            result: "partial".into(),
        },
    );
    assert!(
        state.notifications.is_empty(),
        "a Stopped entry drops the racing result"
    );
}

#[tokio::test]
async fn abort_all_aborts_every_running_child_at_loop_exit() {
    let dir = TempDir::new().unwrap();
    let (mut state, _rx) =
        super::AgentState::for_test(session_in(&dir), Arc::new(FakeLlm::script(vec![])));
    let (task_a, h1) = bg_task(BackgroundStatus::Running, "one");
    let (task_b, h2) = bg_task(BackgroundStatus::Running, "two");
    state.background.insert("a-1".into(), task_a);
    state.background.insert("b-2".into(), task_b);

    state.abort_all_background();
    assert!(state.background.is_empty());
    // Yield so the aborts land, then both child tasks are finished.
    tokio::task::yield_now().await;
    assert!(h1.is_finished());
    assert!(h2.is_finished());
}

#[tokio::test]
async fn spawn_background_settles_through_the_mpsc_and_queues_the_notification() {
    // The end-to-end detached path: spawn_background spawns a real child Run over
    // the FakeLlm; when it settles it posts a BackgroundDone over self_tx, which
    // background_done then queues as a notification.
    let dir = TempDir::new().unwrap();
    let fake = FakeLlm::script(vec![Entry::just(text_end("the findings"))]);
    let (mut state, mut rx) = super::AgentState::for_test(session_in(&dir), Arc::new(fake));

    let id = state.spawn_background(bg_request("general-purpose"), "find the bug".into());
    assert_eq!(id, "general-purpose-1");

    // Await the child's BackgroundDone off the mpsc, then drive it into the
    // handler (the actor loop would do this).
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2000);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(Msg::Run(RunMsg::BackgroundDone { id, result }))) => {
                state.background_done(id, result);
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("mpsc closed"),
            Err(_) => panic!("timed out waiting for BackgroundDone"),
        }
    }
    assert_eq!(state.notifications.len(), 1);
    assert!(state.notifications[0].contains("<result>the findings</result>"));
}

// ===========================================================================
// Background SHELL registry (Phase 9, ADR-0063). Drive the Agent's shell handlers
// directly on an `AgentState::for_test`, gated behind #[cfg(unix)] for the process
// tests. Deterministic: no wall-clock sleeps - long-running shells gate on a MARKER
// FILE and settlements are awaited off the mpsc with a bounded tokio timeout.
// ===========================================================================

// Awaits a single BackgroundShellDone off the mpsc (bounded), returning the id +
// outcome so a test can drive it into the handler.
#[cfg(unix)]
async fn recv_shell_done(
    rx: &mut mpsc::UnboundedReceiver<Msg>,
) -> (String, super::background_shell::ShellOutcome) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(5000);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(Msg::Run(RunMsg::BackgroundShellDone { id, outcome }))) => {
                return (id, outcome);
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("mpsc closed"),
            Err(_) => panic!("timed out waiting for BackgroundShellDone"),
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_background_shell_runs_and_settles_completed_with_captured_output() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let root = session.root.clone();
    let (mut state, mut rx) =
        super::AgentState::for_test(session, Arc::new(FakeLlm::script(vec![])));

    let id = state.spawn_background_shell("echo hi".into(), root);
    assert_eq!(id, "bg_1", "the minted id is bg_{{n}}");
    // Synchronously Running, registered.
    assert!(matches!(
        state.background_shells.get("bg_1").unwrap().status,
        super::background_shell::ShellStatus::Running
    ));
    let output_path = state
        .background_shells
        .get("bg_1")
        .unwrap()
        .output_path
        .clone();

    // Await the child's exit off the mpsc, then drive it into the handler.
    let (done_id, outcome) = recv_shell_done(&mut rx).await;
    assert_eq!(done_id, "bg_1");
    state.background_shell_done(done_id, outcome);

    // Completed: exactly one notification, status completed.
    assert!(matches!(
        state.background_shells.get("bg_1").unwrap().status,
        super::background_shell::ShellStatus::Completed
    ));
    assert_eq!(state.notifications.len(), 1);
    assert!(state.notifications[0].contains("<status>completed</status>"));
    // The capture file holds the (ANSI-stripped) output.
    let captured = std::fs::read_to_string(&output_path).unwrap();
    assert!(captured.contains("hi"), "capture file: {captured:?}");
}

// Regression for the pipe-deadlock (Phase 9): a child that writes a large burst
// (> the ~64KB pipe buffer) to STDERR ONLY while STDOUT stays open-and-idle. A
// sequential drain loop deadlocks here (the idle-stdout read parks, the full-stderr
// pipe blocks the child, the shell never settles); the concurrent `select!` drain
// keeps reading stderr and the child settles. `recv_shell_done` is bounded by a
// tokio timeout, so a regression fails-by-timeout, it does NOT hang forever.
#[cfg(unix)]
#[tokio::test]
async fn a_large_stderr_only_burst_does_not_deadlock_the_drain() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let root = session.root.clone();
    let (mut state, mut rx) =
        super::AgentState::for_test(session, Arc::new(FakeLlm::script(vec![])));

    // 200000 bytes (>> 64KB) to stderr only; stdout is left open (bash keeps it
    // open for the duration) and idle. `head -c` from /dev/zero, redirected to fd 2.
    let id = state.spawn_background_shell("head -c 200000 /dev/zero 1>&2".into(), root);
    assert_eq!(id, "bg_1");
    let output_path = state
        .background_shells
        .get("bg_1")
        .unwrap()
        .output_path
        .clone();

    // Bounded await: with the old sequential loop this times out (deadlock); with
    // `select!` the child settles Completed.
    let (done_id, outcome) = recv_shell_done(&mut rx).await;
    assert_eq!(done_id, "bg_1");
    state.background_shell_done(done_id, outcome);
    assert!(matches!(
        state.background_shells.get("bg_1").unwrap().status,
        super::background_shell::ShellStatus::Completed
    ));

    // The whole burst reached the capture file (well past the 64KB pipe buffer).
    let captured = std::fs::read(&output_path).unwrap();
    assert_eq!(
        captured.len(),
        200000,
        "the full stderr burst was drained, not frozen at the pipe buffer"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stop_background_shell_cancels_synchronously_and_drops_the_late_done() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let root = session.root.clone();
    let (mut state, mut rx) =
        super::AgentState::for_test(session, Arc::new(FakeLlm::script(vec![])));

    // A long-running shell gated on a marker file (no wall-clock sleep): it spins
    // until the marker appears, which the test never creates, so a `task_stop`
    // killpg is what ends it.
    let marker = dir.path().join("marker");
    let cmd = format!("until [ -f {} ]; do :; done", marker.display());
    let id = state.spawn_background_shell(cmd, root);
    assert_eq!(id, "bg_1");
    assert!(matches!(
        state.background_shells.get("bg_1").unwrap().status,
        super::background_shell::ShellStatus::Running
    ));

    // Stop it: synchronous Cancelled + a `<status>cancelled</status>` notification
    // with NO `<result>` tag.
    let wording = state.stop_background_shell("bg_1".into()).unwrap();
    assert!(wording.starts_with("Cancellation requested for background agent \"bg_1\"."));
    assert!(matches!(
        state.background_shells.get("bg_1").unwrap().status,
        super::background_shell::ShellStatus::Cancelled
    ));
    assert_eq!(state.notifications.len(), 1);
    assert!(state.notifications[0].contains("<status>cancelled</status>"));
    assert!(
        !state.notifications[0].contains("<result>"),
        "a cancelled shell carries no result tag"
    );

    // The stop aborts the watcher, so no `BackgroundShellDone` will arrive. But if
    // one raced the abort (the child exited on its own just before the killpg), the
    // Cancelled entry must DROP it - no second notification. Drive that path
    // directly with a synthetic done.
    state.background_shell_done(
        "bg_1".into(),
        super::background_shell::ShellOutcome {
            exit_code: Some(0),
            signalled: false,
            spawn_error: None,
        },
    );
    assert_eq!(
        state.notifications.len(),
        1,
        "a Cancelled entry drops the racing done"
    );
    // No `BackgroundShellDone` reaches the mpsc (the watcher was aborted).
    let none = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(
        none.is_err() || !matches!(none, Ok(Some(Msg::Run(RunMsg::BackgroundShellDone { .. })))),
        "the aborted watcher posts no done"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stop_background_shell_of_a_settled_shell_is_the_not_running_wording() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let (mut state, _rx) = super::AgentState::for_test(session, Arc::new(FakeLlm::script(vec![])));
    // Register a settled (Completed) shell by hand over a never-firing task.
    let handle = tokio::spawn(async { std::future::pending::<()>().await });
    state.background_shells.insert(
        "bg_1".into(),
        super::background_shell::BackgroundShell {
            abort: handle.abort_handle(),
            pgid: None,
            status: super::background_shell::ShellStatus::Completed,
            command: "echo hi".into(),
            output_path: dir.path().join("bg_1.output"),
        },
    );
    let wording = state.stop_background_shell("bg_1".into()).unwrap();
    assert_eq!(
        wording,
        "Error: Background agent \"bg_1\" is not running (status: completed)."
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stop_background_shell_unknown_id_returns_none() {
    let dir = TempDir::new().unwrap();
    let (mut state, _rx) =
        super::AgentState::for_test(session_in(&dir), Arc::new(FakeLlm::script(vec![])));
    assert_eq!(state.stop_background_shell("ghost".into()), None);
}

// The dual-registry resolution the `StopBackground` handler drives (Phase 9): a
// subagent id hits the subagent wording, a shell id hits the shell wording, and an
// unknown id synthesizes the verbatim not-found ONCE - no string sniffing.
#[cfg(unix)]
#[tokio::test]
async fn stop_background_dual_registry_routes_by_id_space() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let root = session.root.clone();
    let (mut state, mut rx) =
        super::AgentState::for_test(session, Arc::new(FakeLlm::script(vec![])));

    // A running subagent and a running shell live side by side.
    let (task, _h) = bg_task(BackgroundStatus::Running, "explore api");
    state.background.insert("scout-1".into(), task);
    let marker = dir.path().join("marker");
    let cmd = format!("until [ -f {} ]; do :; done", marker.display());
    state.spawn_background_shell(cmd, root);

    // The handler's dual-registry resolution, replicated: subagent -> shell -> not-found.
    let resolve = |state: &mut AgentState, id: String| -> String {
        state
            .stop_background(id.clone())
            .or_else(|| state.stop_background_shell(id.clone()))
            .unwrap_or_else(|| format!("Error: No background task found with ID \"{id}\"."))
    };

    // Subagent id -> the subagent wording.
    let sub = resolve(&mut state, "scout-1".into());
    assert!(sub.starts_with("Cancellation requested for background agent \"scout-1\"."));
    assert!(sub.contains("Description: explore api"));
    // Shell id -> the shell wording (its Description is the command).
    let shell = resolve(&mut state, "bg_1".into());
    assert!(shell.starts_with("Cancellation requested for background agent \"bg_1\"."));
    assert!(shell.contains("until [ -f"));
    // Unknown id -> the verbatim not-found, synthesized once.
    assert_eq!(
        resolve(&mut state, "nope".into()),
        "Error: No background task found with ID \"nope\"."
    );

    // Drain any late shell done so the mpsc doesn't leak the child.
    let _ = tokio::time::timeout(Duration::from_millis(2000), rx.recv()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn abort_all_background_shells_clears_the_registry_at_loop_exit() {
    let dir = TempDir::new().unwrap();
    let session = session_in(&dir);
    let root = session.root.clone();
    let (mut state, mut rx) =
        super::AgentState::for_test(session, Arc::new(FakeLlm::script(vec![])));
    let marker = dir.path().join("marker");
    let cmd = format!("until [ -f {} ]; do :; done", marker.display());
    state.spawn_background_shell(cmd, root);
    assert_eq!(state.background_shells.len(), 1);

    state.abort_all_background_shells();
    assert!(state.background_shells.is_empty());
    // Drain any late done.
    let _ = tokio::time::timeout(Duration::from_millis(2000), rx.recv()).await;
}

// ---- live MCP ops (ADR-0065 Phase C) ----------------------------------

// A session carrying one WORKSPACE-sourced MCP server `gh`, so the enable/disable
// scope resolves to `<root>/.suspenders/config.json` (fully under the tmp dir, no
// XDG env). The transport is a command that does not exist, so any attach fails
// fast on spawn (no timeout wait) - the disable path never attaches anyway.
fn session_with_workspace_mcp(dir: &TempDir) -> Session {
    let mut session = session_in(dir);
    let cfg = crate::mcp::McpServerConfig::new(crate::mcp::McpTransport::Stdio {
        command: "suspenders-no-such-mcp-binary".to_string(),
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
    });
    session.mcp_servers.insert("gh".to_string(), cfg);
    session
        .mcp_sources
        .insert("gh".to_string(), crate::mcp::McpSource::Workspace);
    session
}

#[tokio::test]
async fn mcp_set_enabled_false_persists_to_the_workspace_scope_and_disables_the_view() {
    let dir = TempDir::new().unwrap();
    let session = session_with_workspace_mcp(&dir);
    let (agent, _rx) = session_harness(session, vec![]);

    // Disabling a Workspace server writes the workspace config's mcp_excluded and
    // marks the manager's view disabled (the next Run rebuilds without its tools).
    agent
        .mcp_set_enabled("gh", false)
        .await
        .expect("disable ok");

    let workspace_config = dir.path().join(".suspenders").join("config.json");
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(&workspace_config).unwrap()).unwrap();
    assert_eq!(written["mcp_excluded"], json!(["gh"]));

    let views = agent.mcp_views().await;
    let gh = views.iter().find(|v| v.name == "gh").expect("gh view");
    assert!(gh.is_disabled);
}

#[tokio::test]
async fn mcp_set_enabled_refuses_an_unknown_server() {
    let (_dir, agent, _rx) = harness(vec![]);
    // No such server is configured, so there is no scope to write - an Err, not a
    // silent no-op.
    let err = agent
        .mcp_set_enabled("ghost", false)
        .await
        .expect_err("unknown server is refused");
    assert!(err.contains("unknown MCP server"), "{err}");
}

#[tokio::test]
async fn mcp_reconnect_of_an_unknown_server_is_a_no_op_ok() {
    let (_dir, agent, _rx) = harness(vec![]);
    // A reconnect of a name the manager does not hold is absorbed as Ok (the
    // dialog only ever names a listed server; this is the defensive floor).
    agent.mcp_reconnect("ghost").await.expect("no-op reconnect");
    assert!(agent.mcp_views().await.is_empty());
}
