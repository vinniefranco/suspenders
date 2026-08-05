use super::*;
use crate::content::Usage;
use crate::content::{ContentBlock, Role};
use crate::llm::model::{Api, Model};
use crate::llm::response::{Response, StopReason};
use crate::llm::{Delta, malformed_input_marker};
use crate::run::deps::{AfterPass, CompactError};
use crate::run::fixtures::{
    count_voiced, deps_for, empty, events, find_tool_result, just, last_message,
    next_speaker_verdict, ok, root, run_with, session, session_next_speaker, session_with,
    session_with_limit, text_end, text_result, tool_ctx, tool_use_result, write,
};
use crate::session::SessionOpts;
use crate::test_support::Entry;
use serde_json::json;
use std::sync::{Arc, Mutex};

// The harness fixtures (session builders, Response builders, `run_with`,
// event inspectors) live in `crate::run::fixtures`, one set for the split
// Loop's tests (these integration tests cover `batch` and `finish` too).

// ---- tool loop --------------------------------------------------------

#[tokio::test]
async fn runs_the_tool_emits_events_checkpoints_and_feeds_result_back() {
    let root = root();
    write(&root, "marker.txt", "");
    // list_files requires an ABSOLUTE path (qwen ls contract).
    let dir = root.path().to_string_lossy().into_owned();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "tu_1",
                "list_directory",
                json!({"path": dir}),
            )),
            just(text_end("Here are the files.")),
        ],
    );

    let (outcome, deps) = run_with(&session, "list the files", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::EndTurn);

    let evs = events(&deps);
    // tool_call for tu_1
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::ToolCall { id, name, .. }
            if id == "tu_1" && name == "list_directory"))
    );
    // tool_result for tu_1, not error, listing contains marker.txt
    let listing = evs.iter().find_map(|e| match e {
        Event::ToolResult {
            id,
            is_error,
            content,
            ..
        } if id == "tu_1" => {
            assert!(!is_error);
            Some(content.clone())
        }
        _ => None,
    });
    assert!(listing.unwrap().contains("marker.txt"));

    // The checkpoint after the result holds the answered pair.
    let checkpoints = deps.checkpoints.lock().unwrap();
    let cp = checkpoints.first().expect("a checkpoint");
    let tail = &cp.messages[cp.messages.len() - 2..];
    assert!(matches!(&tail[0].role, Role::Assistant));
    assert!(matches!(&tail[0].content[0], ContentBlock::ToolUse { id, .. } if id == "tu_1"));
    assert!(matches!(&tail[1].role, Role::User));
    assert!(
        matches!(&tail[1].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tu_1")
    );

    // The second request carried tools and the tool_result went back.
    let requests = deps.requests.lock().unwrap();
    let second = &requests[1];
    assert!(!second.tools.is_empty());
    let last = second.messages.last().unwrap();
    assert!(matches!(&last.role, Role::User));
    assert!(
        matches!(&last.content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "tu_1" && !is_error)
    );

    // The conversation ends on the model's reply.
    let lm = last_message(conv);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "Here are the files."));
}

#[tokio::test]
async fn a_multi_tool_pass_checkpoints_once_with_the_whole_answered_batch() {
    let root = root();
    write(&root, "a.txt", "");
    write(&root, "b.txt", "");
    let session = session(root.path());
    // One Pass emitting two Tool Calls: the batch is checkpointed once, after
    // both are answered (per-batch, not per-tool - ADR-0010's per-event
    // tool_result log entries carry crash recency; this checkpoint is only
    // the settlement fallback).
    let two_tool_pass = Response {
        content: vec![
            ContentBlock::tool_use("tu_1", "list_directory", json!({"path": "."})),
            ContentBlock::tool_use("tu_2", "list_directory", json!({"path": "."})),
        ],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        error: None,
    };
    let deps = deps_for(
        &session,
        vec![Entry::just(two_tool_pass), just(text_end("Done."))],
    );

    let (outcome, deps) = run_with(&session, "list twice", deps).await;
    ok(&outcome);

    let checkpoints = deps.checkpoints.lock().unwrap();
    // Exactly one checkpoint for the two-tool batch (plus the finish
    // checkpoint on end-of-Run) - never one per tool.
    assert_eq!(checkpoints.len(), 2, "one per batch, not one per tool");

    // The batch checkpoint carries both answered Tool Calls paired with
    // their results, and no unanswered tool_use block.
    let cp = &checkpoints[0];
    let tail = &cp.messages[cp.messages.len() - 2..];
    let assistant = &tail[0];
    assert!(matches!(assistant.role, Role::Assistant));
    let tool_use_ids: Vec<&str> = assistant
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_use_ids, vec!["tu_1", "tu_2"]);

    let user = &tail[1];
    assert!(matches!(user.role, Role::User));
    let result_ids: Vec<&str> = user
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(result_ids, vec!["tu_1", "tu_2"]);
}

// ---- Provenance stamping (ADR-0037) ------------------------------------

#[tokio::test]
async fn assistant_messages_enter_stamped_with_the_captured_models_provenance() {
    let root = root();
    write(&root, "marker.txt", "");
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(text_end("done")),
        ],
    );

    let (outcome, _deps) = run_with(&session, "go", deps).await;
    let (conv, _) = ok(&outcome);

    // Both the tool-answering Pass and the finish reply are stamped with
    // the Run's captured Model; user messages carry no Provenance.
    let expected = Some(session.model.provenance());
    let assistants: Vec<_> = conv
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .collect();
    assert_eq!(assistants.len(), 2);
    for m in &assistants {
        assert_eq!(m.provenance, expected);
    }
    assert!(
        conv.messages
            .iter()
            .filter(|m| m.role == Role::User)
            .all(|m| m.provenance.is_none())
    );
}

#[tokio::test]
async fn a_voice_authored_close_marker_carries_no_provenance() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![just(tool_use_result(
            "t1",
            "list_directory",
            json!({"path": "."}),
        ))],
    )
    .with_after_pass(|_r, _c| AfterPass::Stop("budget_hook".to_string()));

    let (outcome, _deps) = run_with(&session, "look", deps).await;
    let (conv, _) = ok(&outcome);
    let lm = last_message(conv);
    assert!(
        matches!(&lm.content[0], ContentBlock::Text { text } if text == "[turn stopped - reply to continue]")
    );
    assert_eq!(lm.provenance, None, "the Voice's marker is not the model's");
}

#[tokio::test]
async fn emits_message_grammar_per_pass_including_errored_responses() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![Entry::response(
            vec![
                Delta::Thinking("hm".into()),
                Delta::Text("hi ".into()),
                Delta::Text("there".into()),
            ],
            text_end("hi there"),
        )],
    );

    let (outcome, deps) = run_with(&session, "hello", deps).await;
    ok(&outcome);
    let evs = events(&deps);

    assert!(matches!(evs[0], Event::MessageStart { pass: 1 }));
    // First update: thinking delta + snapshot with thinking block.
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::MessageUpdate { delta, content }
            if *delta == Delta::Thinking("hm".into())
            && matches!(content.first(), Some(ContentBlock::Thinking { text }) if text == "hm")))
    );
    // A text delta "hi ".
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::MessageUpdate { delta, .. }
            if *delta == Delta::Text("hi ".into())))
    );
    // "there" update: snapshot has accumulated "hi there".
    assert!(evs.iter().any(|e| matches!(e, Event::MessageUpdate { delta, content }
            if *delta == Delta::Text("there".into())
            && content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "hi there")))));
    // message_end with text content and end_turn.
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::MessageEnd { content, stop_reason }
            if stop_reason == &StopReason::EndTurn
            && matches!(content.first(), Some(ContentBlock::Text { .. }))))
    );
}

#[tokio::test]
async fn streaming_updates_are_emitted_live_during_complete_not_after() {
    let root = root();
    let session = session(root.path());

    // A shared events log created UP FRONT, so the Dynamic entry can drop a
    // sentinel into it from INSIDE `complete` - after every delta has gone
    // through the streaming sink, immediately before `complete` returns.
    // If the loop buffered deltas and emitted after the call (the defect
    // ADR-0025 removes), every MessageUpdate would land AFTER the sentinel.
    let events_log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let sentinel = "__complete_returning__";
    let sentinel_log = Arc::clone(&events_log);
    let entry = Entry::dynamic(
        vec![Delta::Text("hi ".into()), Delta::Text("there".into())],
        move |_req, _model| {
            sentinel_log
                .lock()
                .unwrap()
                .push(Event::steering_delivered(sentinel));
            text_end("hi there")
        },
    );

    let mut deps = deps_for(&session, vec![entry]);
    // Point the fake's recorder at the pre-shared log; the Emitter it hands
    // out clones this same Arc, so updates and sentinel share one ordering.
    deps.events = events_log;

    let (outcome, deps) = run_with(&session, "hello", deps).await;
    ok(&outcome);
    let evs = events(&deps);

    let sentinel_at = evs
        .iter()
        .position(|e| matches!(e, Event::SteeringDelivered { text } if text == sentinel))
        .expect("the sentinel was recorded inside complete");
    let update_positions: Vec<usize> = evs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| matches!(e, Event::MessageUpdate { .. }).then_some(i))
        .collect();
    assert_eq!(update_positions.len(), 2);
    assert!(
        update_positions.iter().all(|&i| i < sentinel_at),
        "every MessageUpdate must precede the sentinel - updates are emitted \
             DURING complete, not after it returns (updates at {update_positions:?}, \
             sentinel at {sentinel_at})"
    );
}

// ---- context pressure -------------------------------------------------

#[tokio::test]
async fn emits_live_numbers_after_every_pass_once_usage_noted() {
    let root = root();
    write(&root, "marker.txt", "");
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with(&session, "list the files", deps).await;
    ok(&outcome);

    let evs = events(&deps);
    let pressures: Vec<(u64, u64, u64)> = evs
        .iter()
        .filter_map(|e| match e {
            Event::ContextPressure {
                token_estimate,
                context_budget,
                max_tokens_reserve,
            } => Some((*token_estimate, *context_budget, *max_tokens_reserve)),
            _ => None,
        })
        .collect();
    assert_eq!(pressures.len(), 2);
    assert_eq!(pressures[0].1, session.context_budget_for(&session.model));
    assert_eq!(pressures[0].2, session.model.max_tokens);
    // Pressure grows Pass to Pass.
    assert!(pressures[1].0 >= pressures[0].0);
}

#[tokio::test]
async fn context_pressure_never_enters_the_conversation() {
    let root = root();
    write(&root, "marker.txt", "");
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, _deps) = run_with(&session, "list the files", deps).await;
    let (conv, _) = ok(&outcome);
    assert!(!conv.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("context_pressure")))
    }));
}

// ---- steering ---------------------------------------------------------

#[tokio::test]
async fn drained_steering_rides_tool_results_message_and_is_announced() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(text_end("done")),
        ],
    )
    .with_steering(vec![vec!["also check the README".to_string()], vec![]]);

    let (outcome, deps) = run_with(&session, "look around", deps).await;
    ok(&outcome);

    let evs = events(&deps);
    assert!(evs.iter().any(
        |e| matches!(e, Event::SteeringDelivered { text } if text == "also check the README")
    ));

    let requests = deps.requests.lock().unwrap();
    let last = requests[1].messages.last().unwrap();
    assert!(matches!(&last.role, Role::User));
    assert!(
        matches!(&last.content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")
    );
    assert!(
        matches!(&last.content[1], ContentBlock::Text { text } if text == "also check the README")
    );
}

// ---- background notifications (P4b, ADR-0063) -------------------------

#[tokio::test]
async fn drained_notification_rides_the_tool_results_message_and_is_announced() {
    // A background `<task-notification>` that settles mid-Run merges into the
    // next request's tool-results user message (the PARALLEL channel to
    // Steering) and is announced as a BackgroundNotification event. The FIRST
    // seeded batch is empty - the Run-start drain consumes it - so the note in
    // the SECOND batch is the one the between-Passes drain (`next_pass`)
    // delivers, which is the merge this test covers.
    let root = root();
    let session = session(root.path());
    let note = "<task-notification>\n<task-id>scout-1</task-id>\n<status>completed</status>\n<summary>Agent \"explore\" completed.</summary>\n<result>found it</result>\n</task-notification>";
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(text_end("done")),
        ],
    )
    .with_notifications(vec![vec![], vec![note.to_string()]]);

    let (outcome, deps) = run_with(&session, "look around", deps).await;
    ok(&outcome);

    // Announced as a BackgroundNotification event.
    let evs = events(&deps);
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::BackgroundNotification { text } if text == note))
    );

    // The notification rides the next request's tool-results user message as
    // a trailing text block (after the tool result).
    let requests = deps.requests.lock().unwrap();
    let last = requests[1].messages.last().unwrap();
    assert!(matches!(&last.role, Role::User));
    assert!(
        matches!(&last.content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")
    );
    assert!(matches!(&last.content[1], ContentBlock::Text { text } if text == note));
}

#[tokio::test]
async fn a_queued_notification_reaches_a_pure_text_runs_first_request() {
    // The delivery-gap fix (P4b, ADR-0063): a notification queued between
    // Runs must reach the model even when the next Run's ONLY Pass is pure
    // text (no tool call). `next_pass` never runs on a pure-text Run, so the
    // Run-start drain is the only delivery point - it merges the note into
    // the FIRST request's user turn.
    let root = root();
    let session = session(root.path());
    let note = "<task-notification>\n<task-id>scout-1</task-id>\n<status>completed</status>\n<summary>Agent \"explore\" completed.</summary>\n<result>found it</result>\n</task-notification>";
    // A single pure-text reply: the whole Run is one no-tool-call Pass.
    let deps = deps_for(&session, vec![just(text_end("here is my answer"))])
        .with_notifications(vec![vec![note.to_string()]]);

    let (outcome, deps) = run_with(&session, "what did the agent find?", deps).await;
    ok(&outcome);

    // Announced as a BackgroundNotification event even though no tool ran.
    let evs = events(&deps);
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::BackgroundNotification { text } if text == note))
    );

    // The one-and-only request carried the note merged into the prompt's
    // user message (a trailing text block), so the model read it on its
    // very first request.
    let requests = deps.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one pure-text Pass, one request");
    let first = requests.last().unwrap();
    let user = first.messages.last().unwrap();
    assert!(matches!(&user.role, Role::User));
    assert!(
        user.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == note)),
        "the queued note rode the first request's user turn: {:?}",
        user.content
    );
}

// ---- after-Pass hook --------------------------------------------------

#[tokio::test]
async fn after_pass_stop_closes_the_run_with_the_stopped_marker() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![just(tool_use_result(
            "t1",
            "list_directory",
            json!({"path": "."}),
        ))],
    )
    .with_after_pass(|_r, _c| AfterPass::Stop("budget_hook".to_string()));

    let (outcome, _deps) = run_with(&session, "look", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(
        *stop,
        crate::stop_reason::StopReason::Custom("budget_hook".to_string())
    );
    let lm = last_message(conv);
    assert!(
        matches!(&lm.content[0], ContentBlock::Text { text } if text == "[turn stopped - reply to continue]")
    );
}

#[tokio::test]
async fn after_pass_inject_appends_a_user_message_and_loops() {
    let root = root();
    let session = session(root.path());
    let injected = Arc::new(Mutex::new(vec![
        AfterPass::Continue,
        AfterPass::Inject("remember the budget".to_string()),
    ]));
    let inj = Arc::clone(&injected);
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(text_end("done")),
        ],
    )
    .with_after_pass(move |_r, _c| inj.lock().unwrap().pop().unwrap());

    let (outcome, deps) = run_with(&session, "look", deps).await;
    ok(&outcome);
    let requests = deps.requests.lock().unwrap();
    let last = requests[1].messages.last().unwrap();
    assert!(matches!(&last.role, Role::User));
    assert!(matches!(&last.content[0], ContentBlock::ToolResult { .. }));
    assert!(
        matches!(&last.content[1], ContentBlock::Text { text } if text == "remember the budget")
    );
}

// ---- error algebra ----------------------------------------------------

#[tokio::test]
async fn errored_response_settles_failed_keeping_partial_text() {
    let root = root();
    let session = session(root.path());
    let errored = Response {
        content: vec![
            ContentBlock::text("partial thought"),
            ContentBlock::tool_use("t1", "grep_search", json!({"pattern": "x"})),
        ],
        stop_reason: StopReason::Error,
        usage: Usage::default(),
        error: Some("request_failed: closed".to_string()),
    };
    let deps = deps_for(&session, vec![just(errored)]);

    let (outcome, deps) = run_with(&session, "go", deps).await;
    let conv = match &outcome {
        Outcome::Failed(reason, conv) => {
            // The LLM error string rides the Reason verbatim.
            assert_eq!(reason.inspect(), "request_failed: closed");
            conv
        }
        other => panic!("expected Failed, got {other:?}"),
    };
    // Grammar stays well-formed on the error path.
    let evs = events(&deps);
    assert!(evs.iter().any(
        |e| matches!(e, Event::MessageEnd { stop_reason, .. } if stop_reason == &StopReason::Error)
    ));
    // Partial text survives; tool_use dropped; failed marker closes.
    let lm = last_message(conv);
    assert_eq!(lm.content.len(), 2);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "partial thought"));
    assert!(matches!(&lm.content[1], ContentBlock::Text { text } if text == "[turn failed]"));
}

// ---- malformed-tool-call re-draw (ADR-0030) ---------------------------

// The server's constrained-decoding miss, as `llm/stream.rs` wraps it.
fn malformed_error() -> Response {
    Response {
        content: vec![],
        stop_reason: StopReason::Error,
        usage: Usage::default(),
        error: Some("api_stream_error: Failed to generate a valid tool call".to_string()),
    }
}

#[tokio::test]
async fn a_retryable_error_re_draws_in_band_and_the_run_completes() {
    let root = root();
    let session = session(root.path());
    // A retryable draw fails, then the re-draw succeeds - the Run
    // continues and completes rather than failing.
    let deps = deps_for(
        &session,
        vec![just(malformed_error()), just(text_end("the good answer"))],
    );

    let (outcome, deps) = run_with(&session, "go", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::EndTurn);

    // The Conversation ends on the re-drawn reply; the failed draw left
    // nothing behind (no [run failed] marker).
    let lm = last_message(conv);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "the good answer"));
    assert!(!conv.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "[turn failed]"))
    }));

    let evs = events(&deps);
    // A retry event was produced (visible + durable), naming attempt 1/3.
    assert!(evs.iter().any(|e| matches!(
        e,
        Event::Retry { attempt: 1, budget: 3, error }
        if error.contains("Failed to generate a valid tool call")
    )));

    // The re-draw did NOT advance the Pass: both the failed draw and the
    // successful re-draw carry MessageStart { pass: 1 } - no extra Pass.
    let starts: Vec<u32> = evs
        .iter()
        .filter_map(|e| match e {
            Event::MessageStart { pass } => Some(*pass),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![1, 1]);
    // Two model calls: the failed draw and its re-draw.
    assert_eq!(deps.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_exhausted_budget_falls_to_finish_fail_as_before() {
    let root = root();
    // Budget 1: the first draw fails and re-draws once; a second failure
    // has no budget left, so it fails loud exactly as today.
    let session = session_with(
        root.path(),
        SessionOpts {
            malformed_retry_budget: Some(1),
            ..Default::default()
        },
    );
    let deps = deps_for(
        &session,
        vec![just(malformed_error()), just(malformed_error())],
    );

    let (outcome, deps) = run_with(&session, "go", deps).await;
    match &outcome {
        Outcome::Failed(reason, _) => {
            assert!(
                reason
                    .inspect()
                    .contains("Failed to generate a valid tool call")
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let evs = events(&deps);
    // Exactly one re-draw was spent before the budget ran out.
    assert_eq!(count_voiced(&evs, |e| matches!(e, Event::Retry { .. })), 1);
    assert_eq!(deps.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_zero_budget_disables_the_re_draw_entirely() {
    let root = root();
    let session = session_with(
        root.path(),
        SessionOpts {
            malformed_retry_budget: Some(0),
            ..Default::default()
        },
    );
    let deps = deps_for(&session, vec![just(malformed_error())]);

    let (outcome, deps) = run_with(&session, "go", deps).await;
    assert!(matches!(&outcome, Outcome::Failed(_, _)));
    let evs = events(&deps);
    assert_eq!(count_voiced(&evs, |e| matches!(e, Event::Retry { .. })), 0);
    assert_eq!(deps.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_non_retryable_error_fails_immediately_without_re_drawing() {
    let root = root();
    let session = session(root.path());
    // Context-exceeded is fail-loud by default: no re-draw, even with
    // budget to spare.
    let context_exceeded = Response {
        content: vec![],
        stop_reason: StopReason::Error,
        usage: Usage::default(),
        error: Some("api_stream_error: Context size has been exceeded".to_string()),
    };
    let deps = deps_for(&session, vec![just(context_exceeded)]);

    let (outcome, deps) = run_with(&session, "go", deps).await;
    match &outcome {
        Outcome::Failed(reason, _) => {
            assert!(reason.inspect().contains("Context size has been exceeded"));
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    let evs = events(&deps);
    assert_eq!(count_voiced(&evs, |e| matches!(e, Event::Retry { .. })), 0);
    // Failed on the first draw: no re-request.
    assert_eq!(deps.requests.lock().unwrap().len(), 1);
}

// ---- loop guards ------------------------------------------------------

#[tokio::test]
async fn tool_use_stop_with_zero_blocks_ends_as_end_turn() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![just(Response {
            content: vec![ContentBlock::text("hmm")],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            error: None,
        })],
    );
    let (outcome, _deps) = run_with(&session, "hi", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::EndTurn);
    let lm = last_message(conv);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "hmm"));
}

#[tokio::test]
async fn truncated_batch_answers_every_call_executes_nothing() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(Response {
                content: vec![
                    ContentBlock::text("partial answer"),
                    ContentBlock::tool_use(
                        "t1",
                        "write_file",
                        json!({"path": "a.txt", "content": "trunca"}),
                    ),
                ],
                stop_reason: StopReason::MaxTokens,
                usage: Usage::default(),
                error: None,
            }),
            just(text_end("re-issued and done")),
        ],
    );
    let (outcome, deps) = run_with(&session, "go", deps).await;
    ok(&outcome);
    let evs = events(&deps);
    let tr = find_tool_result(&evs, "t1").unwrap();
    assert!(
        matches!(tr, Event::ToolResult { is_error, content, .. } if *is_error && content.contains("re-issue"))
    );
    // Nothing touched disk.
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    // The batch went back intact.
    let requests = deps.requests.lock().unwrap();
    let msgs = &requests[1].messages;
    let tail = &msgs[msgs.len() - 2..];
    assert!(matches!(&tail[0].role, Role::Assistant));
    assert!(matches!(&tail[0].content[0], ContentBlock::Text { .. }));
    assert!(matches!(&tail[0].content[1], ContentBlock::ToolUse { id, .. } if id == "t1"));
    assert!(
        matches!(&tail[1].content[0], ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == "t1" && *is_error)
    );
}

#[tokio::test]
async fn reissued_call_after_truncation_executes_not_duplicate() {
    let root = root();
    let session = session(root.path());
    // write_file takes an absolute file_path (qwen contract).
    let target = root.path().join("a.txt").to_string_lossy().into_owned();
    let input = json!({"file_path": target, "content": "hello"});
    let deps = deps_for(
        &session,
        vec![
            just(Response {
                content: vec![ContentBlock::tool_use("t1", "write_file", input.clone())],
                stop_reason: StopReason::MaxTokens,
                usage: Usage::default(),
                error: None,
            }),
            just(tool_use_result("t2", "write_file", input.clone())),
            just(text_end("done")),
            just(text_end("declining to verify")),
        ],
    );
    let (outcome, deps) = run_with(&session, "write it", deps).await;
    ok(&outcome);
    let evs = events(&deps);
    assert!(
        matches!(find_tool_result(&evs, "t1").unwrap(), Event::ToolResult { is_error, .. } if *is_error)
    );
    assert!(
        matches!(find_tool_result(&evs, "t2").unwrap(), Event::ToolResult { is_error, .. } if !is_error)
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn max_tokens_with_no_tool_use_closes_with_text() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![just(text_result("partial answer", StopReason::MaxTokens))],
    );
    let (outcome, _deps) = run_with(&session, "go", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::MaxTokens);
    let lm = last_message(conv);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "partial answer"));
}

#[tokio::test]
async fn max_tokens_with_no_content_closes_with_truncation_marker() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(&session, vec![just(empty(StopReason::MaxTokens))]);
    let (outcome, _deps) = run_with(&session, "go", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::MaxTokens);
    let lm = last_message(conv);
    assert!(
        matches!(&lm.content[0], ContentBlock::Text { text } if text == "[response truncated by max_tokens]")
    );
}

#[tokio::test]
async fn run_limit_stops_the_loop_after_n_passes() {
    let root = root();
    let session = session_with_limit(root.path(), 2);
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(tool_use_result(
                "t2",
                "list_directory",
                json!({"path": "lib"}),
            )),
        ],
    );
    let (outcome, _deps) = run_with(&session, "explore", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::RunLimit);
    let lm = last_message(conv);
    assert!(
        matches!(&lm.content[0], ContentBlock::Text { text } if text == "[turn limit reached - reply to continue]")
    );
    let penult = &conv.messages[conv.messages.len() - 2];
    assert!(matches!(&penult.role, Role::User));
    assert!(
        matches!(&penult.content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t2")
    );
}

// The turn counter bounds the Run: after `max_turns` tool-answering Passes
// the loop closes on the run-limit marker at the top of the next iteration,
// without ever building another request. (Group F wires the loop-detector;
// this is the plain bound.)
#[tokio::test]
async fn turn_counter_bounds_the_run_at_max_turns() {
    let root = root();
    let session = session_with_limit(root.path(), 3);
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(tool_use_result(
                "t2",
                "list_directory",
                json!({"path": "."}),
            )),
            just(tool_use_result(
                "t3",
                "list_directory",
                json!({"path": "."}),
            )),
            // A fourth model reply must never be requested.
            just(text_end("should never be reached")),
        ],
    );
    let (outcome, deps) = run_with(&session, "list forever", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::RunLimit);
    // Exactly three requests: one per answered Pass, none for the bound.
    assert_eq!(deps.requests.lock().unwrap().len(), 3);
    let lm = last_message(conv);
    assert!(
        matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::Marker::RunLimit.text())
    );
}

// ---- loop-detector (the passive circuit breaker) ----------------------

// The model stuck on the IDENTICAL Tool Call batch trips the loop-detector:
// after `loop_stall_limit` consecutive identical batches the Run terminates
// on the loop-stall marker with the `turn_limit_stuck` reason - and, the
// whole point of the passive design, NO steering text was injected into the
// Conversation. Only the close marker rides.
#[tokio::test]
async fn a_stuck_identical_batch_trips_the_loop_detector_without_injecting_text() {
    let root = root();
    let mut opts = SessionOpts::default();
    opts.loop_stall_limit = Some(3);
    // A run limit generous enough that the detector, not the bound, fires.
    opts.run_limit = Some(50);
    let session = session_with(root.path(), opts);

    // The same call, Pass after Pass: three identical batches trip the cap.
    let same = || {
        just(tool_use_result(
            "t1",
            "list_directory",
            json!({"path": "."}),
        ))
    };
    let deps = deps_for(
        &session,
        vec![
            same(),
            same(),
            same(),
            // A fourth reply must never be requested - the detector closes
            // at the third identical batch.
            just(text_end("should never be reached")),
        ],
    );
    let (outcome, deps) = run_with(&session, "loop forever", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::RunLimitStuck);

    // Exactly three model calls: the detector closed on the third identical
    // batch before a fourth request could be built.
    assert_eq!(deps.requests.lock().unwrap().len(), 3);

    // The Run closes on the loop-stall marker.
    let lm = last_message(conv);
    assert!(
        matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::Marker::LoopStall.text())
    );

    // The passive invariant: NO loop-detector steering text entered the
    // Conversation. Every unstamped (Voice-authored, no Provenance)
    // assistant text block must be exactly the close marker - the detector
    // appends that one marker and nothing else (the passive design,
    // ADR-0045).
    let voice_texts: Vec<&str> = conv
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant && m.provenance.is_none())
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        voice_texts,
        vec![voice::Marker::LoopStall.text()],
        "the detector appends only the close marker, no steering text"
    );

    // The operator DID get an event: the detector is silent to the model,
    // never to the operator.
    let evs = events(&deps);
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::LoopStall { count } if *count == 3))
    );
}

// Different Tool Calls each Pass reset the detector: a model making genuine
// progress never trips it, even past the stall limit in raw Pass count.
#[tokio::test]
async fn distinct_batches_each_pass_never_trip_the_detector() {
    let root = root();
    let mut opts = SessionOpts::default();
    opts.loop_stall_limit = Some(2);
    opts.run_limit = Some(50);
    let session = session_with(root.path(), opts);

    // Four DIFFERENT calls in a row, then a clean finish: each batch differs
    // from the last, so the identical-count resets to 1 every Pass and the
    // cap of 2 is never reached.
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(tool_use_result(
                "t2",
                "list_directory",
                json!({"path": "src"}),
            )),
            just(tool_use_result(
                "t3",
                "list_directory",
                json!({"path": "lib"}),
            )),
            just(tool_use_result(
                "t4",
                "list_directory",
                json!({"path": "docs"}),
            )),
            just(text_end("done exploring")),
        ],
    );
    let (outcome, deps) = run_with(&session, "explore around", deps).await;
    let (conv, stop) = ok(&outcome);
    // A clean end_turn - the detector never fired.
    assert_eq!(*stop, crate::stop_reason::StopReason::EndTurn);
    let evs = events(&deps);
    assert!(!evs.iter().any(|e| matches!(e, Event::LoopStall { .. })));
    let lm = last_message(conv);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "done exploring"));
}

// Every request offers the FULL Tool registry - there is no per-Pass
// narrowing (ADR-0045).
#[tokio::test]
async fn every_request_offers_the_full_tool_registry() {
    let root = root();
    let session = session_with_limit(root.path(), 3);
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(tool_use_result(
                "t2",
                "list_directory",
                json!({"path": "."}),
            )),
            just(text_end("done")),
        ],
    );
    let (outcome, deps) = run_with(&session, "list twice", deps).await;
    ok(&outcome);
    let full: Vec<String> = crate::tools::specs().into_iter().map(|s| s.name).collect();
    let requests = deps.requests.lock().unwrap();
    // Three requests, each carrying the identical full registry - no
    // narrowing on any Pass, near the limit or not.
    assert_eq!(requests.len(), 3);
    for req in requests.iter() {
        let names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, full);
    }
}

// ---- next-speaker check (ADR-0043) ------------------------------------

// A thinking-only reply (empty final content) auto-continues WITHOUT a
// side-query: the short-circuit injects "Please continue." and loops, and
// the next Pass finishes the Run. This is the #1 pain the check fixes.
#[tokio::test]
async fn a_thinking_only_reply_auto_continues_via_the_short_circuit() {
    let root = root();
    let session = session_next_speaker(root.path(), 50);
    // First reply: only a thinking block (dropped from final content -> the
    // Pass looks empty). Second reply (after "Please continue."): a real
    // answer, then its next-speaker verdict ends the Run.
    let thinking_only = Response {
        content: vec![ContentBlock::Thinking {
            text: "let me reason".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
        error: None,
    };
    let deps = deps_for(
        &session,
        vec![
            just(thinking_only),
            just(text_end("here is the answer")),
            just(next_speaker_verdict("user")),
        ],
    );
    let (outcome, deps) = run_with(&session, "think then answer", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::EndTurn);

    // A "Please continue." user message was injected between the two Passes,
    // and announced as a delivered-steering event.
    assert!(conv.messages.iter().any(|m| m.role == Role::User
            && matches!(&m.content[0], ContentBlock::Text { text } if text == voice::please_continue())));
    let evs = events(&deps);
    assert!(evs.iter().any(
        |e| matches!(e, Event::SteeringDelivered { text } if text == voice::please_continue())
    ));

    // The Run ends on the real answer.
    let lm = last_message(conv);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "here is the answer"));
}

// A textful reply whose side-query returns {"next_speaker":"model"}
// auto-continues: the reply enters the Conversation (stamped), then
// "Please continue." nudges the model on.
#[tokio::test]
async fn a_model_verdict_continues_and_appends_the_reply_then_the_nudge() {
    let root = root();
    let session = session_next_speaker(root.path(), 50);
    let deps = deps_for(
        &session,
        vec![
            just(text_end("Next, I will read the config.")),
            just(next_speaker_verdict("model")),
            just(text_end("Done reading it.")),
            just(next_speaker_verdict("user")),
        ],
    );
    let (outcome, _deps) = run_with(&session, "go", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::EndTurn);

    // The announced-but-not-executed reply enters stamped with the Model's
    // Provenance; the nudge follows as an unstamped user message.
    let announce = conv
            .messages
            .iter()
            .position(|m| m.role == Role::Assistant
                && matches!(&m.content[0], ContentBlock::Text { text } if text == "Next, I will read the config."))
            .expect("the first reply is in the Conversation");
    assert_eq!(
        conv.messages[announce].provenance,
        Some(session.model.provenance())
    );
    let nudge = &conv.messages[announce + 1];
    assert_eq!(nudge.role, Role::User);
    assert!(
        matches!(&nudge.content[0], ContentBlock::Text { text } if text == voice::please_continue())
    );
    assert_eq!(
        nudge.provenance, None,
        "the nudge is Voice-authored, not the model's"
    );

    let lm = last_message(conv);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "Done reading it."));
}

// A {"next_speaker":"user"} verdict ends the Run exactly as before: the
// reply is the closing message, no nudge injected.
#[tokio::test]
async fn a_user_verdict_finishes_the_run_with_no_nudge() {
    let root = root();
    let session = session_next_speaker(root.path(), 50);
    let deps = deps_for(
        &session,
        vec![
            just(text_end("All set. Let me know if you need anything.")),
            just(next_speaker_verdict("user")),
        ],
    );
    let (outcome, deps) = run_with(&session, "finish up", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::EndTurn);

    // No "Please continue." was injected.
    assert!(!conv.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == voice::please_continue()))
    }));
    let evs = events(&deps);
    assert!(!evs.iter().any(
        |e| matches!(e, Event::SteeringDelivered { text } if text == voice::please_continue())
    ));

    let lm = last_message(conv);
    assert!(
        matches!(&lm.content[0], ContentBlock::Text { text } if text == "All set. Let me know if you need anything.")
    );
}

// The auto-continuation is BOUNDED by max_turns: a model that keeps
// producing empty replies (short-circuit -> always continue) cannot loop
// forever - the run-limit guard closes the Run.
#[tokio::test]
async fn the_continuation_is_bounded_by_max_turns() {
    let root = root();
    // run_limit 3: at most three no-tool Passes, then the bound closes it.
    let session = session_next_speaker(root.path(), 3);
    let always_empty = || {
        just(Response {
            content: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            error: None,
        })
    };
    let deps = deps_for(
        &session,
        vec![
            always_empty(),
            always_empty(),
            always_empty(),
            // A fourth reply must never be requested - the bound closes
            // after the third empty Pass.
            just(text_end("should never be reached")),
        ],
    );
    let (outcome, deps) = run_with(&session, "loop on empties", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::RunLimit);

    // Exactly three model calls (no side-query fires on the empty
    // short-circuit): the fourth was never requested.
    assert_eq!(deps.requests.lock().unwrap().len(), 3);
    let lm = last_message(conv);
    assert!(
        matches!(&lm.content[0], ContentBlock::Text { text } if text == voice::Marker::RunLimit.text())
    );
}

// `skip_next_speaker` restores the pre-check behavior: a no-tool reply
// finishes the Run immediately, with no side-query.
#[tokio::test]
async fn skip_next_speaker_finishes_without_the_check() {
    let root = root();
    let mut opts = SessionOpts::default();
    opts.skip_next_speaker = Some(true);
    let session = session_with(root.path(), opts);
    let deps = deps_for(&session, vec![just(text_end("done"))]);
    let (outcome, deps) = run_with(&session, "go", deps).await;
    let (conv, stop) = ok(&outcome);
    assert_eq!(*stop, crate::stop_reason::StopReason::EndTurn);
    // No side-query: exactly one model call.
    assert_eq!(deps.requests.lock().unwrap().len(), 1);
    let lm = last_message(conv);
    assert!(matches!(&lm.content[0], ContentBlock::Text { text } if text == "done"));
}

// A tool-call reply continues on tool PRESENCE even when the stop reason is
// NOT tool_use (qwen-code parity, the core inversion): an EndTurn stop that
// still carries a tool_use block executes it rather than ending the Run.
#[tokio::test]
async fn tool_use_with_a_non_tool_use_stop_reason_still_continues() {
    let root = root();
    write(&root, "marker.txt", "");
    let dir = root.path().to_string_lossy().into_owned();
    let session = session(root.path());
    // stop_reason EndTurn, but a tool_use block is present -> must execute.
    let end_turn_with_tool = Response {
        content: vec![ContentBlock::tool_use(
            "t1",
            "list_directory",
            json!({"path": dir}),
        )],
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
        error: None,
    };
    let deps = deps_for(
        &session,
        vec![just(end_turn_with_tool), just(text_end("done"))],
    );
    let (outcome, deps) = run_with(&session, "list", deps).await;
    ok(&outcome);
    // The tool ran despite the EndTurn stop reason.
    let evs = events(&deps);
    assert!(
        matches!(find_tool_result(&evs, "t1"), Some(Event::ToolResult { is_error, .. }) if !is_error)
    );
}

// ---- approval gate (ADR-0005) -----------------------------------------

#[tokio::test]
async fn a_denied_run_command_answers_the_denial_and_never_runs() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "r1",
                "run_shell_command",
                json!({"command": "true"}),
            )),
            just(text_end("moved on")),
        ],
    )
    .with_approvals(vec![false]);
    let (outcome, deps) = run_with(&session, "run it", deps).await;
    ok(&outcome);
    let evs = events(&deps);
    // The Approval gate asked, then the denial became the is_error result.
    assert!(
        evs.iter()
            .any(|e| matches!(e, Event::ApprovalRequest { .. }))
    );
    assert!(
        find_tool_result(&evs, "r1")
            .map(|e| matches!(e, Event::ToolResult { is_error, content, .. }
                    if *is_error && content == voice::Marker::CommandDenied.text()))
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn context_budget_exhaustion_fails_before_any_request() {
    let root = root();
    let mut opts = SessionOpts::default();
    opts.context_budget = Some(60);
    opts.compaction_slack = Some(0.0);
    opts.model = Some(Model::new("local", "m", Api::AnthropicMessages, 64_000, 50));
    let session = session_with(root.path(), opts);
    // No script entries: any complete call would surface a different error.
    let deps = deps_for(&session, vec![]);
    let prompt = "pad ".repeat(50);
    let (outcome, _deps) = run_with(&session, &prompt, deps).await;
    assert_eq!(
        outcome,
        Outcome::Error(crate::run::settlement::Reason::atom(
            "context_budget_exhausted"
        ))
    );
}

// ---- malformed tool input ---------------------------------------------

#[tokio::test]
async fn malformed_input_becomes_error_result_never_executes() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(Response {
                content: vec![ContentBlock::tool_use(
                    "t1",
                    "write_file",
                    malformed_input_marker("{\"path\": \"oops"),
                )],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
                error: None,
            }),
            just(text_end("ok")),
        ],
    );
    let (outcome, deps) = run_with(&session, "write something", deps).await;
    ok(&outcome);
    let evs = events(&deps);
    let tr = evs
        .iter()
        .find(|e| matches!(e, Event::ToolResult { name, .. } if name == "write_file"))
        .unwrap();
    assert!(
        matches!(tr, Event::ToolResult { is_error, content, .. } if *is_error && content.contains("not valid JSON"))
    );
    // The error tool_result went back to the model.
    let requests = deps.requests.lock().unwrap();
    let last = requests[1].messages.last().unwrap();
    assert!(
        matches!(&last.content[0], ContentBlock::ToolResult { is_error, content, .. } if *is_error && crate::content::result_blocks_text(content).contains("not valid JSON"))
    );
    // Nothing executed.
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

// ---- tool display Artifacts on the tool_result event (ADR-0007) -------

// A tool that shapes its own transcript display attaches a display Artifact to
// its Tool Result; that Artifact must ride the `:tool_result` event to the UI
// (it never enters the Conversation). Exercised end-to-end through the loop with
// a real tool (todo_write attaches the `todos` Artifact), not a mock pipeline.
#[tokio::test]
async fn tool_display_artifacts_ride_the_tool_result_event() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "todo_write",
                json!({"todos": [
                    { "id": "1", "content": "read", "status": "in_progress" },
                    { "id": "2", "content": "edit", "status": "pending" },
                ]}),
            )),
            just(text_end("ok")),
        ],
    );
    let (outcome, deps) = run_with(&session, "make a plan", deps).await;
    ok(&outcome);
    let evs = events(&deps);
    let tr = evs
        .iter()
        .find(|e| matches!(e, Event::ToolResult { .. }))
        .unwrap();
    match tr {
        Event::ToolResult {
            is_error,
            artifacts,
            ..
        } => {
            assert!(!is_error);
            // The `todos` Artifact rides the event; parsing it back yields the
            // two parsed items the Transcript store swaps in as a Todo.
            let todos = crate::tools::todo_write::read_todos_artifact(artifacts)
                .expect("todos artifact present");
            assert_eq!(todos.items.len(), 2);
        }
        _ => unreachable!(),
    }
}

// A tool NEVER crashes the Run (ADR-0018 fail-open): a tool that returns `Err`
// (or an unknown tool) comes back as an is_error Tool Result, and the Run
// completes normally.
#[tokio::test]
async fn a_failing_tool_is_fail_open_and_the_run_completes() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            // A relative edit path is refused by edit_file (qwen's absolute-path
            // contract), so the tool returns Err -> an is_error result.
            just(tool_use_result(
                "t1",
                "edit",
                json!({"file_path": "rel.txt", "old_string": "a", "new_string": "b"}),
            )),
            just(text_end("ok")),
        ],
    );
    let (outcome, deps) = run_with(&session, "edit something", deps).await;
    ok(&outcome);
    let evs = events(&deps);
    let tr = evs
        .iter()
        .find(|e| matches!(e, Event::ToolResult { .. }))
        .unwrap();
    assert!(matches!(tr, Event::ToolResult { is_error, .. } if *is_error));
    // Nothing was written.
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

// ---- Plan storage -----------------------------------------------------

#[tokio::test]
async fn successful_plan_call_stores_plan_via_set_plan() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "p1",
                "todo_write",
                json!({"todos": [
                    { "id": "1", "content": "read", "status": "in_progress" },
                    { "id": "2", "content": "edit", "status": "pending" },
                ]}),
            )),
            just(text_end("planned, done")),
        ],
    );
    let (outcome, deps) = run_with(&session, "do X", deps).await;
    ok(&outcome);
    let plans = deps.plans.lock().unwrap();
    assert_eq!(plans.as_slice(), &["◐ read\n○ edit".to_string()]);
}

#[tokio::test]
async fn failed_plan_call_does_not_store_a_plan() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result("p1", "todo_write", json!({}))),
            just(text_end("recovered")),
        ],
    );
    let (outcome, deps) = run_with(&session, "do X", deps).await;
    ok(&outcome);
    assert!(deps.plans.lock().unwrap().is_empty());
}

// ---- proactive Compaction (ADR-0012) ----------------------------------

#[tokio::test]
async fn proactive_compacts_before_first_pass() {
    let root = root();
    let mut opts = SessionOpts::default();
    opts.run_limit = Some(50);
    let session = session_with(root.path(), opts);

    let compacted = Arc::new(Mutex::new(false));
    let c = Arc::clone(&compacted);
    let mut deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "t1",
                "list_directory",
                json!({"path": "."}),
            )),
            just(text_end("done")),
        ],
    )
    .with_compact(move |conv: Conversation| {
        *c.lock().unwrap() = true;
        let mut out = conv.clone();
        out.messages = vec![conv.messages[0].clone()];
        Ok(out)
    });

    // Geometry: budget 4000, reserve 100, slack 0.3. Target = 2700; the big
    // assistant blob puts the estimate over target but under the cliff.
    let mut conv = Conversation::new(
        "sys",
        crate::conversation::ConversationOpts {
            compaction_slack: 0.3,
            ..crate::conversation::ConversationOpts::new(4000, 100)
        },
    );
    conv.add_user_text("original task");
    conv.add_assistant_blocks(vec![ContentBlock::text("x".repeat(12_000))]);

    let ctx = tool_ctx(&session);
    let outcome = run(
        conv,
        &session,
        RunEnv {
            tool_ctx: &ctx,
            hooks: None,
            skill_activation: None,
        },
        &mut deps,
        RunOpts::default(),
    )
    .await;
    let (_conv, _) = ok(&outcome);
    assert!(*compacted.lock().unwrap());
    // Compaction ran before the first model call: the first recorded request
    // reflects the compacted (single-message) conversation.
    let requests = deps.requests.lock().unwrap();
    assert_eq!(requests[0].messages.len(), 1);
}

#[tokio::test]
async fn leaves_conversation_alone_under_the_target() {
    let root = root();
    let session = session(root.path());
    let compacted = Arc::new(Mutex::new(false));
    let c = Arc::clone(&compacted);
    let deps = deps_for(&session, vec![just(text_end("done"))]).with_compact(
        move |_conv: Conversation| {
            *c.lock().unwrap() = true;
            Err(CompactError("should_not_run".to_string()))
        },
    );
    let (outcome, _deps) = run_with(&session, "small task", deps).await;
    ok(&outcome);
    assert!(!*compacted.lock().unwrap());
}

// ---- Plan-mode request-shaping Voice (ADR-0067, Phase 4a) --------------

use crate::approvals::ApprovalMode;
use crate::run::fixtures::{run_with_mode, run_with_pending_exit_notice};

// A fragment unique to the standing plan-mode reminder (qwen's
// getPlanModeSystemReminder), and one unique to the manual-exit reminder, so the
// tests assert on the actual Voice strings without pasting the whole verbatim
// text.
// A fragment of the standing reminder that does NOT appear in the manual-exit
// reminder (the manual-exit text mentions "Plan mode is active" when it says it
// is no longer active, so that phrase is not distinctive).
const PLAN_REMINDER_MARK: &str = "The user indicated that they do not want you to execute yet";
const MANUAL_EXIT_MARK: &str =
    "The approval mode changed outside the approved exit_plan_mode flow.";

// While the live mode is Plan, the standing plan-mode reminder rides EVERY
// request's system text (qwen re-injects getPlanModeSystemReminder into every
// request in Plan - client.ts:2915). A two-Pass read-only script (read-only tools
// are allowed in Plan) proves it is re-added on BOTH requests.
#[tokio::test]
async fn plan_mode_injects_the_standing_reminder_into_every_request() {
    let root = root();
    let dir = root.path().to_string_lossy().into_owned();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "tu_1",
                "list_directory",
                json!({"path": dir}),
            )),
            just(text_end("done planning")),
        ],
    );

    let (outcome, deps) = run_with_mode(&session, "plan it", ApprovalMode::Plan, deps).await;
    ok(&outcome);

    let requests = deps.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "two Passes, two requests");
    for (i, req) in requests.iter().enumerate() {
        assert!(
            req.system.contains(PLAN_REMINDER_MARK),
            "request {i} must carry the standing plan reminder while in Plan"
        );
    }
}

// Outside Plan (the default mode) with no pending manual-exit notice, NEITHER
// reminder is injected - the reminder is a Plan-mode invariant, gone the moment
// the mode is not Plan (and it is ephemeral: never in the Conversation).
#[tokio::test]
async fn outside_plan_mode_injects_no_plan_reminder() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(&session, vec![just(text_end("done"))]);

    let (outcome, deps) = run_with(&session, "just answer", deps).await;
    ok(&outcome);

    let requests = deps.requests.lock().unwrap();
    let sys = &requests[0].system;
    assert!(
        !sys.contains(PLAN_REMINDER_MARK),
        "no plan reminder outside Plan"
    );
    assert!(
        !sys.contains(MANUAL_EXIT_MARK),
        "no manual-exit reminder with none pending"
    );
}

// The standing plan reminder is EPHEMERAL: it rides the request's system text but
// is never persisted into the Conversation the loop carries forward. After a
// Plan-mode Run, the returned Conversation's system prompt is clean.
#[tokio::test]
async fn plan_reminder_is_not_persisted_in_the_conversation() {
    let root = root();
    let session = session(root.path());
    let deps = deps_for(&session, vec![just(text_end("done planning"))]);

    let (outcome, _deps) = run_with_mode(&session, "plan it", ApprovalMode::Plan, deps).await;
    let (conv, _stop) = ok(&outcome);

    // The wire request carried the reminder, but the Conversation snapshot the
    // loop returns holds the ORIGINAL system prompt - the reminder never entered
    // the persisted history.
    let persisted = conv.for_request().unwrap().system;
    assert!(
        !persisted.contains(PLAN_REMINDER_MARK),
        "the ephemeral reminder must not be persisted in the Conversation"
    );
}

// A manual (Shift+Tab) exit queued a notice: the NEXT request carries the
// one-shot manual-exit reminder ONCE (qwen's takePendingManualPlanExitNotice -
// geminiChat.ts:2384), and the SECOND request does not (the take cleared it). A
// two-Pass read-only script proves the exactly-once semantics. The live mode is
// Default (already left Plan), so the standing plan reminder is absent.
#[tokio::test]
async fn manual_exit_injects_the_one_shot_reminder_exactly_once() {
    let root = root();
    let dir = root.path().to_string_lossy().into_owned();
    let session = session(root.path());
    let deps = deps_for(
        &session,
        vec![
            just(tool_use_result(
                "tu_1",
                "list_directory",
                json!({"path": dir}),
            )),
            just(text_end("done")),
        ],
    );

    let (outcome, deps) =
        run_with_pending_exit_notice(&session, "carry on", ApprovalMode::Default, deps).await;
    ok(&outcome);

    let requests = deps.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "two Passes, two requests");
    assert!(
        requests[0].system.contains(MANUAL_EXIT_MARK),
        "the first request after a manual exit carries the one-shot reminder"
    );
    // And it names the current mode's wire string (default), verbatim.
    assert!(
        requests[0]
            .system
            .contains("The current approval mode is: default.")
    );
    assert!(
        !requests[1].system.contains(MANUAL_EXIT_MARK),
        "the one-shot reminder must not repeat on the next request"
    );
    // A manual exit is not Plan, so the standing plan reminder never appears.
    assert!(!requests[0].system.contains(PLAN_REMINDER_MARK));
}
