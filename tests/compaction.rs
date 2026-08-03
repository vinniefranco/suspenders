use super::*;
use crate::content::{ContentBlock, Role, Usage};
use crate::conversation::ConversationOpts;
use crate::llm::model::Api;
use crate::llm::response::{Response, StopReason};
use crate::test_support::{Entry, FakeLlm};

fn test_model() -> Model {
    Model::new("local", "test-model", Api::AnthropicMessages, 64_000, 4000)
}

fn ok_response(text: &str) -> Response {
    Response {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
        error: None,
    }
}

fn opts() -> ConversationOpts {
    ConversationOpts {
        compaction_slack: 0.0,
        ..ConversationOpts::new(2000, 500)
    }
}

fn conversation_with_runs(count: u64) -> Conversation {
    let mut conv = Conversation::new("You are Baud.", opts());
    add_runs(&mut conv, count);
    conv
}

fn add_runs(conv: &mut Conversation, n: u64) {
    for i in (1..=n).rev() {
        let content = format!("{}: turn {i}", "line ".repeat(50));
        conv.add_user_text(content.clone());
        conv.add_assistant_blocks(vec![ContentBlock::text(content)]);
    }
}

fn conversation_with_file_ops() -> Conversation {
    let mut conv = Conversation::new("You are Baud.", opts());
    for _ in 0..10 {
        conv.add_user_text("edit a file with many lines of content to fill budget");
        conv.add_assistant_blocks(vec![ContentBlock::tool_use(
            "t1",
            "edit",
            serde_json::json!({"file_path": "lib/foo.ex"}),
        )]);
        conv.add_tool_results(
            vec![ContentBlock::tool_result("t1", "edited".repeat(100), false)],
            Vec::new(),
        );
        conv.add_assistant_blocks(vec![ContentBlock::text("done ".repeat(50))]);
    }
    conv
}

fn message_text(msg: &crate::content::Message) -> String {
    msg.content
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- new ----

#[test]
fn new_returns_fresh_state_with_no_summary_and_empty_file_ops() {
    let state = Compaction::new();
    assert_eq!(state.previous_summary, None);
    assert_eq!(state.file_ops, FileOps::default());
}

// ---- proactive ----

#[test]
fn fires_at_the_compaction_target_not_the_budget_target() {
    let mut conv = Conversation::new(
        "",
        ConversationOpts {
            compaction_slack: 0.3,
            ..ConversationOpts::new(1000, 200)
        },
    );
    conv.add_user_text("a".repeat(2100));

    assert!(conv.token_estimate() > conv.compaction_target());
    assert!(conv.token_estimate() <= 800);
    assert!(Compaction::proactive(&conv));
}

#[test]
fn returns_true_when_estimate_exceeds_target() {
    let mut conv = Conversation::new("You are Baud.", ConversationOpts::new(1000, 200));
    conv.add_user_text("a ".repeat(700));
    conv.add_assistant_blocks(vec![ContentBlock::text("b ".repeat(700))]);
    assert!(Compaction::proactive(&conv));
}

#[test]
fn holds_at_the_target_exactly_and_fires_one_token_over() {
    // The trigger is strict: AT the Compaction Target the Conversation
    // still fits, so nothing fires. The estimate rides the usage floor
    // (`token_estimate` is the char estimate floored by the usage's
    // context floor), the binding term at Run start when the previous
    // Run's usage is on record.
    let mut conv = Conversation::new(
        "",
        ConversationOpts {
            compaction_slack: 0.0,
            ..ConversationOpts::new(1000, 200)
        },
    );
    conv.add_user_text("short");
    assert_eq!(conv.compaction_target(), 800);

    conv.note_usage(Usage::with_input_tokens(800));
    assert!(!Compaction::proactive(&conv));

    conv.note_usage(Usage::with_input_tokens(801));
    assert!(Compaction::proactive(&conv));
}

#[test]
fn returns_false_when_estimate_is_within_budget() {
    let mut conv = Conversation::new("You are Baud.", ConversationOpts::new(100_000, 4000));
    conv.add_user_text("short");
    assert!(!Compaction::proactive(&conv));
}

// ---- run ----

#[tokio::test]
async fn returns_nothing_to_compact_for_a_bare_conversation() {
    let conv = Conversation::new("You are Baud.", ConversationOpts::new(2000, 500));
    let fake = FakeLlm::script(Vec::<Entry>::new());
    let result = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await;
    assert_eq!(result, Err("nothing_to_compact".to_string()));
}

#[tokio::test]
async fn returns_ok_compacted_new_state_with_a_scripted_llm() {
    let fake = FakeLlm::script(vec![Entry::just(ok_response(
        "## Goal\nTest compaction\n## Progress\n### Done\n- completed",
    ))]);
    let conv = conversation_with_runs(5);
    let (compacted, new_state) = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();

    assert!(compacted.messages.len() < conv.messages.len());

    let summary_msg = &compacted.messages[0];
    assert_eq!(summary_msg.role, Role::User);
    assert!(
        summary_msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { .. }))
    );

    assert!(new_state.previous_summary.is_some());
    assert!(
        new_state
            .previous_summary
            .as_deref()
            .unwrap()
            .contains("Goal")
    );
    assert_eq!(new_state.file_ops, FileOps::default());
}

#[tokio::test]
async fn strips_the_analysis_scratchpad_from_the_stored_summary() {
    // qwen's compression prompt asks the model to wrap its chain-of-thought in
    // an <analysis> block; it must be stripped before the summary enters history.
    let fake = FakeLlm::script(vec![Entry::just(ok_response(
        "<analysis>secret drafting scratchpad</analysis>\n<state_snapshot>real summary</state_snapshot>",
    ))]);
    let conv = conversation_with_runs(5);
    let (compacted, new_state) = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();

    let narrative = new_state.previous_summary.as_deref().unwrap();
    assert!(!narrative.contains("scratchpad"), "analysis block leaked");
    assert!(narrative.contains("<state_snapshot>real summary</state_snapshot>"));
    // The re-injected message also excludes the scratchpad.
    assert!(!message_text(&compacted.messages[0]).contains("scratchpad"));
}

#[tokio::test]
async fn falls_back_to_raw_when_the_model_returns_only_an_analysis_block() {
    let fake = FakeLlm::script(vec![Entry::just(ok_response(
        "<analysis>only scratchpad, no summary</analysis>",
    ))]);
    let conv = conversation_with_runs(5);
    let (_compacted, new_state) = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();

    // Stripping everything would lose the whole response; fall back to raw.
    assert!(
        new_state
            .previous_summary
            .as_deref()
            .unwrap()
            .contains("only scratchpad")
    );
}

#[tokio::test]
async fn returns_error_when_llm_fails() {
    let fake = FakeLlm::script(vec![Entry::error("server_busy")]);
    let conv = conversation_with_runs(5);
    let result = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await;
    assert_eq!(result, Err("server_busy".to_string()));
}

#[tokio::test]
async fn merges_file_ops_from_the_compacted_messages() {
    let fake = FakeLlm::script(vec![Entry::just(ok_response("summary of edits"))]);
    let conv = conversation_with_file_ops();

    let prev_state = Compaction {
        previous_summary: None,
        original_task: None,
        file_ops: FileOps {
            read_files: vec!["lib/other.ex".to_string()],
            modified_files: Vec::new(),
        },
    };

    let (_compacted, new_state) = prev_state
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();

    assert_eq!(
        new_state.file_ops.read_files,
        vec!["lib/other.ex".to_string()]
    );
    assert_eq!(
        new_state.file_ops.modified_files,
        vec!["lib/foo.ex".to_string()]
    );
}

// ---- original task capture and mechanical facts ----

#[tokio::test]
async fn captures_the_verbatim_first_user_text_on_the_first_compaction() {
    let fake = FakeLlm::script(vec![Entry::just(ok_response("narrative summary"))]);
    let task = "Implement the widget factory in lib/widget.ex";

    let mut conv = Conversation::new("You are Baud.", opts());
    conv.add_user_text(task);
    add_runs(&mut conv, 5);

    let (_compacted, new_state) = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();
    assert_eq!(new_state.original_task.as_deref(), Some(task));
}

#[tokio::test]
async fn the_summary_message_carries_the_verbatim_task_even_when_the_model_returns_garbage() {
    let fake = FakeLlm::script(vec![Entry::just(ok_response(
        "!!! nonsense narrative that ignores the template",
    ))]);
    let task = "Rename the User struct to Account everywhere";

    let mut conv = Conversation::new("You are Baud.", opts());
    conv.add_user_text(task);
    add_runs(&mut conv, 5);

    let (compacted, _new_state) = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();

    let summary_text = message_text(&compacted.messages[0]);
    assert!(summary_text.contains(task));
}

#[tokio::test]
async fn the_summary_message_mechanically_appends_accumulated_file_ops() {
    let fake = FakeLlm::script(vec![Entry::just(ok_response("narrative"))]);
    let conv = conversation_with_file_ops();

    let prev_state = Compaction {
        previous_summary: None,
        original_task: Some("the original task".to_string()),
        file_ops: FileOps {
            read_files: vec!["lib/other.ex".to_string()],
            modified_files: Vec::new(),
        },
    };

    let (compacted, _new_state) = prev_state
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();

    let summary_text = message_text(&compacted.messages[0]);
    assert!(summary_text.contains("lib/other.ex"));
    assert!(summary_text.contains("lib/foo.ex"));
}

#[tokio::test]
async fn the_original_task_survives_a_second_compaction_unchanged() {
    let fake = FakeLlm::script(vec![
        Entry::just(ok_response("first")),
        Entry::just(ok_response("second")),
    ]);
    let task = "Add pagination to the reports endpoint";

    let mut conv = Conversation::new("You are Baud.", opts());
    conv.add_user_text(task);
    add_runs(&mut conv, 5);

    let (mut compacted1, state1) = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();
    assert_eq!(state1.original_task.as_deref(), Some(task));

    // Grow the conversation again and compact a second time. The summary
    // message the first compaction produced no longer starts with the
    // original user text, so only the carried state can preserve it.
    add_runs(&mut compacted1, 5);
    let (compacted2, state2) = state1
        .run(&compacted1, &fake, &test_model(), None)
        .await
        .unwrap();

    assert_eq!(state2.original_task.as_deref(), Some(task));

    let summary_text = message_text(&compacted2.messages[0]);
    assert!(summary_text.contains(task));
}

// ---- recovery_capture ----

#[tokio::test]
async fn recovery_capture_returns_ok_conversation_on_success() {
    let fake = FakeLlm::script(vec![Entry::just(ok_response("summary data"))]);
    let conv = conversation_with_runs(5);

    let compacted = Compaction::new()
        .recovery_capture(&conv, &fake, &test_model(), None)
        .await
        .unwrap();
    assert!(compacted.messages.len() < conv.messages.len());
}

#[tokio::test]
async fn recovery_capture_returns_error_on_llm_failure() {
    let fake = FakeLlm::script(vec![Entry::error("timeout")]);
    let conv = conversation_with_runs(5);
    let result = Compaction::new()
        .recovery_capture(&conv, &fake, &test_model(), None)
        .await;
    assert_eq!(result, Err("timeout".to_string()));
}

// ---- session_log_entry ----

#[test]
fn builds_the_compacted_log_entry_from_state_and_forensic_counts() {
    let state = Compaction {
        previous_summary: Some("the model narrative".to_string()),
        original_task: Some("the verbatim task".to_string()),
        file_ops: FileOps {
            read_files: vec!["lib/a.ex".to_string()],
            modified_files: vec!["lib/b.ex".to_string()],
        },
    };

    let entry = state.session_log_entry(7, 1234);
    assert_eq!(
        entry,
        SessionLogEntry {
            summary: Some("the model narrative".to_string()),
            skip_count: 7,
            tokens_before: 1234,
            file_ops: FileOps {
                read_files: vec!["lib/a.ex".to_string()],
                modified_files: vec!["lib/b.ex".to_string()],
            },
            original_task: Some("the verbatim task".to_string()),
        }
    );
}

// ---- skip_count ----

#[tokio::test]
async fn counts_the_messages_a_compaction_folded_into_the_single_summary() {
    let fake = FakeLlm::script(vec![Entry::just(ok_response("narr"))]);
    let conv = conversation_with_runs(5);
    let (compacted, _state) = Compaction::new()
        .run(&conv, &fake, &test_model(), None)
        .await
        .unwrap();

    let expected = conv.messages.len() - (compacted.messages.len() - 1);
    assert_eq!(Compaction::skip_count(&conv, &compacted), expected);
    assert!(Compaction::skip_count(&conv, &compacted) > 0);
}
