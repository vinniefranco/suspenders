use super::codec::decode_line;
use super::*;
use crate::content::Role;
use crate::session::{Session, SessionConfig, SessionOpts};
use crate::voice;
use serde_json::json;
use tempfile::TempDir;

// A user message from content blocks - the fold's own `user_message` helper is
// private to the `fold` submodule now, so the test keeps its own trivial copy.
fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::user(content)
}

// A Session rooted at `dir`, session_dir under it, no-env config path.
fn session_in(dir: &std::path::Path) -> Session {
    session_with(dir, None, None)
}

fn session_with(
    dir: &std::path::Path,
    context_budget: Option<u64>,
    run_limit: Option<u64>,
) -> Session {
    let root = dir.to_string_lossy().into_owned();
    let session_dir = dir.join("sessions").to_string_lossy().into_owned();
    Session::build(
        SessionOpts {
            root: Some(root),
            session_dir: Some(session_dir),
            context_budget,
            run_limit,
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .unwrap()
}

// Returns (tmp, session, log) - `tmp` must stay alive for the duration of
// the test so the temp directory is not deleted while the log is open.
fn open_log() -> (TempDir, Session, Log) {
    let tmp = TempDir::new().unwrap();
    let session = session_in(tmp.path());
    let log = Log::open(&session).unwrap();
    (tmp, session, log)
}

fn tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::tool_use(id, name, input)
}

fn tool_result(id: &str, content: &str) -> ContentBlock {
    ContentBlock::tool_result(id, content, false)
}

fn tool_result_err(id: &str, content: &str, is_error: bool) -> ContentBlock {
    ContentBlock::tool_result(id, content, is_error)
}

fn text(t: &str) -> ContentBlock {
    ContentBlock::text(t)
}

// ---- round trip ----

#[test]
fn a_settled_run_folds_back_into_the_exact_conversation_shape() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("list the files".into()));
    log.append(Entry::assistant_blocks(vec![
        text("Let me look."),
        tool_use("t1", "list_directory", json!({"path": "."})),
    ]));
    log.append(Entry::ToolResult(tool_result("t1", "a.txt\nb.txt")));
    log.append(Entry::Steering("also check the README".into()));
    log.append(Entry::assistant_blocks(vec![text("Two files.")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, drift) = resume(&log.path, &session).unwrap();
    assert_eq!(drift, Vec::new());

    assert_eq!(
        messages,
        vec![
            user_message(vec![text("list the files")]),
            Message::assistant(vec![
                text("Let me look."),
                tool_use("t1", "list_directory", json!({"path": "."})),
            ]),
            user_message(vec![
                tool_result("t1", "a.txt\nb.txt"),
                text("also check the README"),
            ]),
            Message::assistant(vec![text("Two files.")]),
        ]
    );
}

#[test]
fn a_mixed_batch_keeps_answered_tool_calls_and_drops_unanswered_ones() {
    // ADR-0009 keeps a tool_use whose result landed; ADR-0004 drops one that
    // never answered. A batch with both must keep t1 (+ its result) and drop
    // t2 entirely.
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("go".into()));
    log.append(Entry::assistant_blocks(vec![
        tool_use("t1", "read_file", json!({"path": "a.rs"})),
        tool_use("t2", "read_file", json!({"path": "b.rs"})),
    ]));
    log.append(Entry::ToolResult(tool_result("t1", "ok")));
    log.append(Entry::assistant_blocks(vec![text("done")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, _) = resume(&log.path, &session).unwrap();

    assert_eq!(
        messages,
        vec![
            user_message(vec![text("go")]),
            // t2 is gone; only the answered t1 survives.
            Message::assistant(vec![tool_use("t1", "read_file", json!({"path": "a.rs"}))]),
            user_message(vec![tool_result("t1", "ok")]),
            Message::assistant(vec![text("done")]),
        ]
    );
}

#[test]
fn an_all_unanswered_batch_collapses_to_the_empty_response_marker() {
    // Every tool_use dropped (ADR-0004) leaves no assistant content, so the
    // batch close emits the empty-response marker instead of an empty message.
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("go".into()));
    log.append(Entry::assistant_blocks(vec![
        tool_use("t1", "read_file", json!({"path": "a.rs"})),
        tool_use("t2", "read_file", json!({"path": "b.rs"})),
    ]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, _) = resume(&log.path, &session).unwrap();

    assert_eq!(
        messages,
        vec![
            user_message(vec![text("go")]),
            Message::assistant(vec![text(voice::Marker::EmptyResponse.text())]),
        ]
    );
}

// ---- user media prompt (ADR-0068) ----

fn image(mime: &str, data: &str) -> ContentBlock {
    ContentBlock::image(mime, data)
}

#[test]
fn user_content_rides_the_wire_as_a_tagged_block_list_and_round_trips() {
    // A media prompt (ADR-0068) persists as `user_content{blocks}`, the blocks
    // in their tagged serde shape, and decodes back to the same entry.
    let entry = Entry::UserContent(vec![text("look at "), image("image/png", "AAAA")]);
    let value = entry.to_json();
    assert_eq!(value["e"], "user_content");
    assert_eq!(
        value["blocks"],
        json!([
            { "type": "text", "text": "look at " },
            { "type": "image", "mime": "image/png", "data": "AAAA" },
        ])
    );
    assert_eq!(Entry::from_json(&value), Some(entry));
}

#[test]
fn a_user_content_prompt_folds_back_into_one_media_user_message() {
    // Resume must rebuild the media user turn: the block list becomes ONE user
    // Message, order preserved (the Image survives to reach the model).
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserContent(vec![
        text("what is in "),
        image("image/png", "AAAA"),
    ]));
    log.append(Entry::assistant_blocks(vec![text("a cat")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, drift) = resume(&log.path, &session).unwrap();
    assert_eq!(drift, Vec::new());
    assert_eq!(
        messages,
        vec![
            user_message(vec![text("what is in "), image("image/png", "AAAA")]),
            Message::assistant(vec![text("a cat")]),
        ]
    );
}

#[test]
fn an_old_user_text_log_line_still_resumes_as_a_single_text_block() {
    // Backward compat (ADR-0068): the pre-P2 `user_text` entry is kept readable
    // and folds into a single Text user message, identical to before - an old
    // log resumes unchanged alongside the new `user_content` variant.
    let raw = r#"{"e":"user_text","text":"do the thing"}"#;
    let entry = Entry::from_json(&decode_line(raw).unwrap()).unwrap();
    assert_eq!(entry, Entry::UserText("do the thing".into()));

    let (_tmp, session, mut log) = open_log();
    log.append(Entry::UserText("do the thing".into()));
    log.append(Entry::assistant_blocks(vec![text("done")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, _) = resume(&log.path, &session).unwrap();
    assert_eq!(
        messages,
        vec![
            user_message(vec![text("do the thing")]),
            Message::assistant(vec![text("done")]),
        ]
    );
}

// ---- Plan survives Resume ----

#[test]
fn plan_restores_the_last_logged_plan_which_never_enters_the_folded_messages() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("do the thing".into()));
    log.append(Entry::Plan("Goal: A. 1. read [x] 2. edit [ ]".into()));
    log.append(Entry::assistant_blocks(vec![text("planned")]));
    log.append(Entry::Plan(
        "Goal: A. 1. read [x] 2. edit [x] 3. verify [ ]".into(),
    ));
    log.append(Entry::assistant_blocks(vec![text("done step 2")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    assert_eq!(
        plan(&log.path),
        Some("Goal: A. 1. read [x] 2. edit [x] 3. verify [ ]".to_string())
    );

    let (messages, _) = resume(&log.path, &session).unwrap();
    assert!(!messages.iter().any(|m| m.content.iter().any(|b| matches!(
        b,
        ContentBlock::Text { text } if text.contains("Goal: A.")
    ))));
}

#[test]
fn a_log_with_no_plan_entry_restores_a_nil_plan() {
    let (_tmp, _session, mut log) = open_log();

    log.append(Entry::UserText("hi".into()));
    log.append(Entry::assistant_blocks(vec![text("hello")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    assert_eq!(plan(&log.path), None);
}

// ---- the fold's close rules ----

#[test]
fn an_adr_0009_truncated_batch_folds_back_intact() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("go".into()));
    log.append(Entry::assistant_blocks(vec![tool_use(
        "t1",
        "write_file",
        json!({"path": "a"}),
    )]));
    log.append(Entry::ToolResult(tool_result_err(
        "t1",
        "[response was cut...]",
        true,
    )));
    log.append(Entry::assistant_blocks(vec![text("re-issued")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, _) = resume(&log.path, &session).unwrap();

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].role, Role::Assistant);
    assert!(matches!(&messages[1].content[0], ContentBlock::ToolUse { id, .. } if id == "t1"));
    assert_eq!(messages[2].role, Role::User);
    assert!(matches!(
        &messages[2].content[0],
        ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1"
    ));
    assert_eq!(messages[3].role, Role::Assistant);
    assert!(matches!(&messages[3].content[0], ContentBlock::Text { text } if text == "re-issued"));
}

#[test]
fn a_log_ending_mid_run_settles_as_failed_dangling_tool_use_dropped_marker_appended() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("go".into()));
    log.append(Entry::assistant_blocks(vec![
        text("thinking..."),
        tool_use("t1", "grep_search", json!({})),
    ]));
    // No tool_result, no settled: the app died mid-batch.

    let (messages, _) = resume(&log.path, &session).unwrap();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(
        messages[1].content,
        vec![text("thinking..."), text("[turn failed]")]
    );
}

// Shared setup: one completed run with a tool call, closed by a Settled entry.
// Returns the last reconstructed message after resume.
fn settled_last_message(outcome: Settled, stop_reason: StopReason) -> Message {
    let (_tmp, session, mut log) = open_log();
    log.append(Entry::UserText("go".into()));
    log.append(Entry::assistant_blocks(vec![tool_use(
        "t1",
        "grep_search",
        json!({}),
    )]));
    log.append(Entry::ToolResult(tool_result("t1", "hits")));
    log.append(Entry::Settled {
        outcome,
        stop_reason,
        reason: None,
    });
    let (messages, _) = resume(&log.path, &session).unwrap();
    messages.into_iter().last().unwrap()
}

#[test]
fn a_run_limit_settlement_restores_the_closing_marker() {
    let last = settled_last_message(Settled::Completed, StopReason::RunLimit);
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(
        last.content,
        vec![text("[turn limit reached - reply to continue]")]
    );
}

#[test]
fn a_cancelled_settlement_closes_with_the_cancelled_marker() {
    let last = settled_last_message(Settled::Cancelled, StopReason::Unknown);
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.content, vec![text("[turn cancelled by user]")]);
}

#[test]
fn a_failed_settlement_carries_its_reason_string_forensically_the_fold_ignores_it() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("go".into()));
    log.append(Entry::assistant_blocks(vec![text("partial")]));
    log.append(Entry::Settled {
        outcome: Settled::Failed,
        stop_reason: StopReason::Error,
        reason: Some(r#"{:llm_error, "connection refused"}"#.into()),
    });

    let (messages, _) = resume(&log.path, &session).unwrap();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0], user_message(vec![text("go")]));
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(
        messages[1].content,
        vec![text("partial"), text("[turn failed]")]
    );
}

#[test]
fn a_pre_unification_settled_line_still_parses_with_its_wire_names() {
    // Backward compat (ADR-0069): the settled entry's stop_reason names
    // predate the one-vocabulary unification, so a log written before it must
    // still Resume. This line is byte-for-byte what the old code wrote for a
    // Run-Limit stop.
    let raw = r#"{"e":"settled","outcome":"completed","stop_reason":"turn_limit","reason":null}"#;
    let entry = Entry::from_json(&decode_line(raw).unwrap()).unwrap();
    assert_eq!(
        entry,
        Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::RunLimit,
            reason: None,
        }
    );
}

#[test]
fn a_hook_custom_stop_reason_round_trips_the_settled_entry_verbatim() {
    // The Hook's atom (ADR-0066) is a first-class stop reason (ADR-0069): it
    // serializes as itself and parses back as itself, never as `unknown`.
    let entry = Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::Custom("budget_hook".into()),
        reason: None,
    };
    let value = entry.to_json();
    assert_eq!(value["stop_reason"], "budget_hook");
    assert_eq!(Entry::from_json(&value), Some(entry));
}

// ---- crash modes ----

#[test]
fn a_torn_last_line_is_dropped() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("go".into()));
    log.append(Entry::assistant_blocks(vec![text("done")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&log.path)
        .unwrap();
    f.write_all(br#"{"e": "user_text", "tex"#).unwrap();

    let (messages, _) = resume(&log.path, &session).unwrap();
    assert_eq!(messages.len(), 2);
}

// ---- resume rules ----

#[test]
fn a_different_project_root_refuses_to_resume() {
    let (tmp, session, mut log) = open_log();
    log.append(Entry::UserText("go".into()));

    let other_root = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&other_root).unwrap();
    let other = Session::build(
        SessionOpts {
            root: Some(other_root.to_string_lossy().into_owned()),
            session_dir: Some(session.session_dir.clone()),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .unwrap();

    assert_eq!(resume(&log.path, &other), Err(ResumeError::RootMismatch));
}

#[test]
fn every_other_fact_yields_reported_as_drift() {
    let (tmp, session, mut log) = open_log();
    log.append(Entry::UserText("go".into()));

    // A budget cap BELOW the model's window, so the derived launch budget
    // actually changes (a cap above the window is a no-op, ADR-0037).
    let logged_budget = session.context_budget_for(&session.model);
    let changed = session_with(
        tmp.path(),
        Some(logged_budget / 2),
        Some(session.run_limit + 5),
    );

    let (_messages, drift) = resume(&log.path, &changed).unwrap();

    assert!(drift.contains(&Drift {
        key: "context_budget",
        logged: logged_budget.to_string(),
        current: changed.context_budget_for(&changed.model).to_string(),
    }));
    assert!(drift.contains(&Drift {
        key: "turn_limit",
        logged: session.run_limit.to_string(),
        current: changed.run_limit.to_string(),
    }));
}

#[test]
fn a_setpoint_like_compaction_slack_yields_on_resume_and_never_drifts() {
    // compaction_slack is a Setpoint (ADR-0031), not a durable header fact: it
    // is never persisted, so a resuming Session with a different value
    // reports NO drift for it and simply keeps its own value.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_string_lossy().into_owned();
    let session_dir = tmp.path().join("sessions").to_string_lossy().into_owned();

    let build = |slack: f64| {
        Session::build(
            SessionOpts {
                root: Some(root.clone()),
                session_dir: Some(session_dir.clone()),
                compaction_slack: Some(slack),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .unwrap()
    };

    let logged = build(0.15);
    let mut log = Log::open(&logged).unwrap();
    log.append(Entry::UserText("go".into()));

    let resuming = build(0.25);
    let (_messages, drift) = resume(&log.path, &resuming).unwrap();

    assert!(!drift.iter().any(|d| d.key == "compaction_slack"));
    // The resuming Session keeps its own Setpoint; the logged 0.15 is gone.
    assert_eq!(resuming.compaction_slack, 0.25);
}

#[test]
fn a_compaction_fold_discards_raw_entries_before_the_compacted_marker() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("turn 1".into()));
    log.append(Entry::assistant_blocks(vec![text("old response")]));
    log.append(Entry::UserText("turn 2".into()));
    log.append(Entry::assistant_blocks(vec![text("compacted response")]));
    log.append(Entry::ToolResult(tool_result("t1", "compacted result")));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });
    log.append(Entry::Compacted {
        summary: "Summary of old turns".into(),
        skip_count: 5,
        tokens_before: 100,
        file_ops: FileOps::default(),
        original_task: Some("the original task".into()),
    });
    log.append(Entry::UserText("turn 3".into()));
    log.append(Entry::assistant_blocks(vec![text("new response")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, drift) = resume(&log.path, &session).unwrap();
    assert_eq!(drift, Vec::new());

    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, Role::User);
    assert!(
        matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("Summary of old turns"))
    );
    assert_eq!(messages[1].role, Role::User);
    assert_eq!(messages[2].role, Role::Assistant);
}

#[test]
fn a_compaction_fold_reconstructs_the_mechanical_facts_task_and_file_ops() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("turn 1".into()));
    log.append(Entry::assistant_blocks(vec![text("old response")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });
    log.append(Entry::Compacted {
        summary: "narrative from the model".into(),
        skip_count: 3,
        tokens_before: 100,
        file_ops: FileOps {
            read_files: vec!["lib/a.ex".into()],
            modified_files: vec!["lib/b.ex".into()],
        },
        original_task: Some("verbatim original task".into()),
    });
    log.append(Entry::UserText("turn 2".into()));
    log.append(Entry::assistant_blocks(vec![text("new response")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, _) = resume(&log.path, &session).unwrap();

    let summary_text: String = messages[0]
        .content
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(summary_text.contains("narrative from the model"));
    assert!(summary_text.contains("verbatim original task"));
    assert!(summary_text.contains("lib/a.ex"));
    assert!(summary_text.contains("lib/b.ex"));
}

// The Compaction<->Resume fidelity invariant (ADR-0012's "byte-identical
// summary message", ADR-0021's test-as-spec): a LIVE compaction and the
// fold of its logged `Compacted` entry must reconstruct byte-identical
// Conversation messages. Both sides compose the summary through the single
// shared `compose_summary` helper - this crosses the seam BETWEEN them
// (each side is tested alone above; nothing exercised the round trip). The
// test drives the same builder ops into the Conversation and the Session
// Log in lockstep - exactly the "append every event as it happens" contract
// of ADR-0010 - runs a real `Compaction::run` over the head, then logs the
// `Compacted` entry via the production path (`session_log_entry`, converted
// the way `agent.rs` does) followed by the surviving tail. If someone
// changed one composition path without the other (e.g. `apply_compaction`
// prepended a marker the fold did not), the summary message would diverge
// and this assertion would fail.
#[tokio::test]
async fn a_live_compaction_and_its_logged_fold_reconstruct_byte_identical_messages() {
    use crate::compaction::Compaction;
    use crate::conversation::{Conversation, ConversationOpts};
    use crate::llm::model::{Api, Model};
    use crate::llm::response::{Response, StopReason as LlmStop};
    use crate::test_support::{Entry as ScriptEntry, FakeLlm};

    // Arrange: a Conversation of several Runs (user text + assistant text),
    // fat enough that the Compaction Keep leaves a real head to summarize.
    // The same ops feed the Session Log so the log mirrors the live events.
    let (_tmp, session, mut log) = open_log();

    let opts = ConversationOpts {
        compaction_slack: 0.0,
        ..ConversationOpts::new(2000, 500)
    };
    let mut conv = Conversation::new("You are Baud.", opts);
    for i in (1..=5).rev() {
        let body = format!("{}: turn {i}", "line ".repeat(50));
        conv.add_user_text(body.clone());
        conv.add_assistant_blocks(vec![ContentBlock::text(body.clone())]);
        log.append(Entry::UserText(body.clone()));
        log.append(Entry::assistant_blocks(vec![text(&body)]));
    }
    // The Run that triggers Compaction settles first, like the real path.
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    // Act (live): a real compaction cycle with a scripted narrative.
    let narrative = "## Goal\nPin the compaction seam\n## Progress\n### Done\n- traced";
    let fake = FakeLlm::script(vec![ScriptEntry::just(Response {
        content: vec![ContentBlock::text(narrative)],
        stop_reason: LlmStop::EndTurn,
        usage: crate::content::Usage::default(),
        error: None,
    })]);
    let model = Model::new("local", "test-model", Api::AnthropicMessages, 64_000, 4000);
    let before = conv.clone();
    let (compacted, new_state) = Compaction::new()
        .run(&conv, &fake, &model, None)
        .await
        .unwrap();
    // Sanity: compaction actually folded something into one summary message.
    assert!(compacted.messages.len() < before.messages.len());

    // Act (log): append the `Compacted` entry through the production path -
    // `session_log_entry` then the exact usize/Option conversion agent.rs
    // performs - followed by the surviving tail as it would have been logged
    // by the Runs that ran after the Compaction.
    let skip = Compaction::skip_count(&before, &compacted);
    let entry = new_state.session_log_entry(skip, 0);
    log.append(Entry::Compacted {
        summary: entry.summary.unwrap_or_default(),
        skip_count: entry.skip_count as u64,
        tokens_before: entry.tokens_before,
        file_ops: entry.file_ops,
        original_task: entry.original_task,
    });
    // The live-compacted tail (everything after the summary message) is
    // what later Runs appended; replay each surviving message as its entry.
    for msg in &compacted.messages[1..] {
        match msg.role {
            Role::User => {
                let text = match &msg.content[0] {
                    ContentBlock::Text { text } => text.clone(),
                    other => panic!("unexpected tail user block: {other:?}"),
                };
                log.append(Entry::UserText(text));
            }
            Role::Assistant => {
                log.append(Entry::assistant_blocks(msg.content.clone()));
            }
        }
    }
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    // Assert: the folded Conversation equals the live-compacted one, message
    // for message, byte for byte - the summary message at index 0 in
    // particular (where the two composition paths meet).
    let (folded, drift) = resume(&log.path, &session).unwrap();
    assert_eq!(drift, Vec::new());
    assert_eq!(folded, compacted.messages);
}

// ---- retry entries (ADR-0030) ----

#[test]
fn retry_entries_round_trip_the_file_with_their_error_attempt_and_budget() {
    let (_tmp, _session, mut log) = open_log();

    log.append(Entry::Retry {
        error: "api_stream_error: Failed to generate a valid tool call".into(),
        attempt: 1,
        budget: 3,
    });
    log.append(Entry::Retry {
        error: "api_stream_error: Failed to generate a valid tool call".into(),
        attempt: 2,
        budget: 3,
    });

    let content = std::fs::read_to_string(&log.path).unwrap();
    let entries: Vec<Entry> = content
        .lines()
        .skip(1)
        .filter_map(|l| decode_line(l).and_then(|v| Entry::from_json(&v)))
        .collect();
    assert_eq!(
        entries,
        vec![
            Entry::Retry {
                error: "api_stream_error: Failed to generate a valid tool call".into(),
                attempt: 1,
                budget: 3,
            },
            Entry::Retry {
                error: "api_stream_error: Failed to generate a valid tool call".into(),
                attempt: 2,
                budget: 3,
            },
        ]
    );
}

#[test]
fn a_retry_entry_is_silent_to_the_folded_conversation() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("go".into()));
    // A retryable draw failed and was re-drawn silently; the re-issued
    // request succeeded and the Run completed.
    log.append(Entry::Retry {
        error: "api_stream_error: Failed to generate a valid tool call".into(),
        attempt: 1,
        budget: 3,
    });
    log.append(Entry::assistant_blocks(vec![text("re-drawn answer")]));
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, _) = resume(&log.path, &session).unwrap();

    // The retry never enters the Conversation: user prompt then the reply.
    assert_eq!(
        messages,
        vec![
            user_message(vec![text("go")]),
            Message::assistant(vec![text("re-drawn answer")]),
        ]
    );
}

// ---- plan read under the fold's torn-line tolerance ----

// A torn line is a truncated JSON object (a crash mid-write), the shape the
// fold's `a_torn_last_line_is_dropped` uses. The header const and the
// `write_log` raw-file helper both live lower in this module.
const TORN_LINE: &str = r#"{"e": "plan", "tex"#;

#[test]
fn plan_is_none_for_a_missing_file() {
    assert_eq!(plan("/definitely/not/here.jsonl"), None);
}

#[test]
fn plan_is_none_for_an_empty_file() {
    let tmp = TempDir::new().unwrap();
    let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[]);
    assert_eq!(plan(&path), None);
}

#[test]
fn plan_is_none_for_a_header_only_log() {
    let tmp = TempDir::new().unwrap();
    let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[TEST_HEADER]);
    assert_eq!(plan(&path), None);
}

// A torn HEADER line: [`plan`] skips line 1 unconditionally (header
// validation is [`resume`]'s job, not its), so a single torn header alone
// leaves nothing to scan - it reads empty, never inventing an entry from a
// log with no valid entries.
#[test]
fn plan_is_none_when_the_header_line_is_torn() {
    let tmp = TempDir::new().unwrap();
    let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[TORN_LINE]);
    assert_eq!(plan(&path), None);
}

#[test]
fn plan_returns_the_last_plan_before_a_tear_never_one_after() {
    let tmp = TempDir::new().unwrap();
    let path = write_log(
        tmp.path(),
        "20260101-000000-1.jsonl",
        &[
            TEST_HEADER,
            r#"{"e":"user_text","text":"go"}"#,
            r#"{"e":"plan","text":"before the tear"}"#,
            TORN_LINE,
            r#"{"e":"plan","text":"after the tear"}"#,
        ],
    );
    // The tear stops the scan: the Plan after it is never observed, exactly
    // as the resumed Conversation would never see it.
    assert_eq!(plan(&path), Some("before the tear".to_string()));
}

#[test]
fn plan_is_none_for_a_header_only_log_again() {
    let tmp = TempDir::new().unwrap();
    let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[TEST_HEADER]);
    assert_eq!(plan(&path), None);
}

// ---- resume_governed folds the Plan once ----
//
// The single fold's `plan` MUST equal the standalone `plan` query on the
// same file: same last-Plan semantics, same torn-line tolerance.

#[test]
fn resume_governed_plan_matches_the_standalone_query() {
    let (_tmp, session, mut log) = open_log();

    log.append(Entry::UserText("go".into()));
    log.append(Entry::Plan("Goal: A. 1. read [x]".into()));
    // A fresh prompt then a new Plan; the later Plan is the one that resumes.
    log.append(Entry::UserText("now do B".into()));
    log.append(Entry::Plan("Goal: B. 1. edit [ ]".into()));

    let r = resume_governed(&log.path, &session).unwrap();

    assert_eq!(r.plan, plan(&log.path));
    assert_eq!(r.plan, Some("Goal: B. 1. edit [ ]".to_string()));
}

#[test]
fn resume_governed_plan_matches_the_standalone_query_under_a_tear() {
    let tmp = TempDir::new().unwrap();
    // The header's root is `/r`; resume needs a Session rooted there.
    let session = Session::build(
        SessionOpts {
            root: Some("/r".into()),
            session_dir: Some(tmp.path().to_string_lossy().into_owned()),
            ..Default::default()
        },
        &SessionConfig::test_defaults(),
    )
    .unwrap();
    let path = write_log(
        tmp.path(),
        "20260101-000000-1.jsonl",
        &[
            TEST_HEADER,
            r#"{"e":"user_text","text":"go"}"#,
            r#"{"e":"plan","text":"before the tear"}"#,
            TORN_LINE,
            r#"{"e":"plan","text":"after the tear"}"#,
        ],
    );

    let r = resume_governed(&path, &session).unwrap();

    // The tear stops the scan for both derivations identically: the fold
    // drops the post-tear messages, and `plan` never observes the Plan after
    // it.
    assert_eq!(r.plan, plan(&path));
    assert_eq!(r.plan, Some("before the tear".to_string()));
}

#[test]
fn seeded_message_entries_replay_verbatim() {
    let (_tmp, session, mut log) = open_log();

    let seeded = Message::assistant(vec![text("from a previous life")]);
    log.append(Entry::Message(seeded.clone()));

    let (messages, drift) = resume(&log.path, &session).unwrap();
    assert_eq!(messages, vec![seeded]);
    assert_eq!(drift, Vec::new());
}

// ---- Provenance persistence (ADR-0037) ----

#[test]
fn assistant_provenance_round_trips_through_the_log_and_fold() {
    let (_tmp, session, mut log) = open_log();

    let stamp = Provenance::new("anthropic", "claude-fable-5");
    log.append(Entry::UserText("go".into()));
    log.append(Entry::AssistantBlocks {
        blocks: vec![tool_use("t1", "grep_search", json!({}))],
        provenance: Some(stamp.clone()),
    });
    log.append(Entry::ToolResult(tool_result("t1", "hits")));
    log.append(Entry::AssistantBlocks {
        blocks: vec![text("done")],
        provenance: Some(stamp.clone()),
    });
    log.append(Entry::Settled {
        outcome: Settled::Completed,
        stop_reason: StopReason::EndTurn,
        reason: None,
    });

    let (messages, _) = resume(&log.path, &session).unwrap();

    assert_eq!(messages[1].provenance, Some(stamp.clone()));
    assert_eq!(messages[3].provenance, Some(stamp));
    assert_eq!(messages[0].provenance, None, "user messages carry none");
    assert_eq!(messages[2].provenance, None);
}

#[test]
fn a_seeded_message_entry_keeps_its_provenance_across_log_generations() {
    // Resume seeds a fresh log with `message` entries; the stamp must
    // survive so a twice-resumed history still normalizes correctly.
    let (_tmp, session, mut log) = open_log();

    let seeded = Message::assistant_from(
        vec![text("stamped reply")],
        Provenance::new("lmstudio", "qwen3.6-27b"),
    );
    log.append(Entry::Message(seeded.clone()));

    let (messages, _) = resume(&log.path, &session).unwrap();
    assert_eq!(messages, vec![seeded]);
}

// The documented decode choice: a logged assistant event MISSING the
// provenance fields decodes as `None` (unknown Provenance, a transform
// mismatch) rather than failing the fold - the same optional-field
// tolerance the settled entry's `reason` takes, and strictly safer than
// treating the line as torn (which would silently drop the rest of the
// log). No backwards compatibility is intended; unknown is simply the
// honest value for an unstamped line.
#[test]
fn a_line_missing_the_provenance_fields_decodes_as_unknown_provenance() {
    let raw = r#"{"e":"assistant_blocks","blocks":[{"type":"text","text":"old"}]}"#;
    let entry = Entry::from_json(&decode_line(raw).unwrap()).unwrap();
    assert_eq!(
        entry,
        Entry::AssistantBlocks {
            blocks: vec![text("old")],
            provenance: None,
        }
    );

    let raw = r#"{"e":"message","role":"assistant","content":[{"type":"text","text":"old"}]}"#;
    let entry = Entry::from_json(&decode_line(raw).unwrap()).unwrap();
    assert_eq!(entry, Entry::Message(Message::assistant(vec![text("old")])));
}

#[test]
fn provenance_rides_the_wire_as_flat_provider_and_model_keys() {
    // The greppable-log thesis of ADR-0010: a human can read the stamp.
    let entry = Entry::AssistantBlocks {
        blocks: vec![text("hi")],
        provenance: Some(Provenance::new("anthropic", "claude-fable-5")),
    };
    let value = entry.to_json();
    assert_eq!(value["provider"], "anthropic");
    assert_eq!(value["model"], "claude-fable-5");
    assert_eq!(Entry::from_json(&value), Some(entry));
}

// ---- latest/1 ----

#[test]
fn returns_the_newest_log_by_filename_error_when_none() {
    let tmp = TempDir::new().unwrap();
    let session = session_in(tmp.path());

    assert_eq!(latest(&session.session_dir), None);

    let first = Log::open(&session).unwrap();
    let second = Log::open(&session).unwrap();

    let got = latest(&session.session_dir).unwrap();
    assert!(got == first.path || got == second.path);

    let missing = std::path::Path::new(&session.session_dir)
        .join("missing")
        .to_string_lossy()
        .into_owned();
    assert_eq!(latest(&missing), None);
}

// ---- list/1 ----

const TEST_HEADER: &str =
    r#"{"type":"session","version":1,"root":"/r","model":"m","context_budget":1,"turn_limit":1}"#;

fn write_log(dir: &std::path::Path, name: &str, lines: &[&str]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n")).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn list_returns_newest_first_by_the_filename_stamp_latest_sorts_on() {
    let tmp = TempDir::new().unwrap();
    let older = write_log(
        tmp.path(),
        "20260101-090000-1.jsonl",
        &[TEST_HEADER, r#"{"e":"user_text","text":"older"}"#],
    );
    let newer = write_log(
        tmp.path(),
        "20260711-140205-1.jsonl",
        &[TEST_HEADER, r#"{"e":"user_text","text":"newer"}"#],
    );

    let entries = list(&tmp.path().to_string_lossy());

    assert_eq!(
        entries,
        vec![
            SessionEntry {
                path: newer,
                stamp: "2026-07-11 14:02".into(),
                label: "newer".into(),
            },
            SessionEntry {
                path: older,
                stamp: "2026-01-01 09:00".into(),
                label: "older".into(),
            },
        ]
    );
}

#[test]
fn list_labels_with_the_first_user_text_first_line_only() {
    let tmp = TempDir::new().unwrap();
    write_log(
        tmp.path(),
        "20260101-000000-1.jsonl",
        &[
            TEST_HEADER,
            r#"{"e":"plan","text":"not a label"}"#,
            "{\"e\":\"user_text\",\"text\":\"fix the bug\\nwith much more detail below\"}",
            r#"{"e":"user_text","text":"a later prompt"}"#,
        ],
    );

    let entries = list(&tmp.path().to_string_lossy());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].label, "fix the bug");
}

#[test]
fn list_char_truncates_long_labels_with_an_ellipsis() {
    let tmp = TempDir::new().unwrap();
    let long = "é".repeat(70);
    write_log(
        tmp.path(),
        "20260101-000000-1.jsonl",
        &[
            TEST_HEADER,
            &format!(r#"{{"e":"user_text","text":"{long}"}}"#),
        ],
    );

    let entries = list(&tmp.path().to_string_lossy());
    assert_eq!(entries[0].label.chars().count(), 61);
    assert!(entries[0].label.ends_with('…'));
    assert!(entries[0].label.starts_with(&"é".repeat(60)));
}

#[test]
fn list_labels_a_log_with_no_user_text_as_an_empty_session() {
    let tmp = TempDir::new().unwrap();
    let session = session_in(tmp.path());
    Log::open(&session).unwrap();

    let entries = list(&session.session_dir);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].label, "(empty session)");
}

#[test]
fn list_skips_torn_headers_and_foreign_files_without_panicking() {
    let tmp = TempDir::new().unwrap();
    write_log(
        tmp.path(),
        "20260101-000000-1.jsonl",
        &[r#"{"type": "sess"#],
    );
    write_log(tmp.path(), "20260102-000000-1.jsonl", &["not json at all"]);
    write_log(
        tmp.path(),
        "20260103-000000-1.jsonl",
        &[r#"{"type":"something_else"}"#],
    );
    write_log(tmp.path(), "notes.txt", &["a non-jsonl file"]);
    let good = write_log(
        tmp.path(),
        "20260104-000000-1.jsonl",
        &[TEST_HEADER, r#"{"e":"user_text","text":"survivor"}"#],
    );

    let entries = list(&tmp.path().to_string_lossy());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, good);
    assert_eq!(entries[0].label, "survivor");
}

#[test]
fn list_of_an_empty_or_missing_dir_is_empty() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(list(&tmp.path().to_string_lossy()), Vec::new());

    let missing = tmp.path().join("missing").to_string_lossy().into_owned();
    assert_eq!(list(&missing), Vec::new());
}
