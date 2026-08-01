
use super::*;
use crate::content::ContentBlock;
use serde_json::json;

fn tool_use(id: &str, name: &str) -> ContentBlock {
    ContentBlock::tool_use(id, name, json!({}))
}

fn tool_use_input(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::tool_use(id, name, input)
}

fn tool_result(id: &str, content: &str) -> ContentBlock {
    ContentBlock::tool_result(id, content, false)
}

fn tool_result_err(id: &str, content: &str, is_error: bool) -> ContentBlock {
    ContentBlock::tool_result(id, content, is_error)
}

// A minimal started conversation: system prompt "sys", budget 1000,
// one user message "hi". Used wherever a test just needs a conversation
// with at least one turn before appending assistant or result messages.
fn started_conv() -> Conversation {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
    conv.add_user_text("hi");
    conv
}

// ---- new/2 ----

#[test]
fn new_builds_empty_conversation_from_explicit_values() {
    let conv = Conversation::new("You are Baud.", ConversationOpts::new(32_000, 45));
    assert_eq!(conv.system_prompt, "You are Baud.");
    assert!(conv.messages.is_empty());
    assert_eq!(conv.last_usage, None);
    assert_eq!(conv.context_budget, 32_000);
    assert_eq!(conv.max_tokens_reserve, 45);
}

// NOTE: baud's "context_budget and max_tokens_reserve are required
// (KeyError)" test is enforced here by the type system: ConversationOpts
// makes both fields non-optional, so a caller cannot omit them. No runtime
// assertion is possible or needed - the equivalent guarantee is a compile
// error. (Documented judgment call.)

#[test]
fn new_overhead_chars_defaults_to_0_and_is_settable() {
    let base = Conversation::new("sys", ConversationOpts::new(123, 0));
    let with_overhead = Conversation::new("sys", ConversationOpts::new(123, 0).overhead_chars(700));
    assert_eq!(base.overhead_chars, 0);
    assert_eq!(with_overhead.overhead_chars, 700);
}

#[test]
fn new_compaction_slack_defaults_to_zero_and_is_settable() {
    let base = Conversation::new("sys", ConversationOpts::new(123, 0));
    let with_slack = Conversation::new("sys", ConversationOpts::new(123, 0).compaction_slack(0.5));
    assert_eq!(base.compaction_slack, 0.0);
    assert_eq!(with_slack.compaction_slack, 0.5);
}

#[test]
fn new_compaction_keep_defaults_to_half_and_is_settable() {
    let base = Conversation::new("sys", ConversationOpts::new(123, 0));
    let with_keep = Conversation::new("sys", ConversationOpts::new(123, 0).compaction_keep(0.3));
    assert_eq!(base.compaction_keep, 0.5);
    assert_eq!(with_keep.compaction_keep, 0.3);
}

// ---- message appending ----

#[test]
fn add_user_text_appends_user_message_with_single_text_block() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
    conv.add_user_text("hello");
    assert_eq!(
        conv.messages,
        vec![Message::user(vec![ContentBlock::text("hello")])]
    );
}

#[test]
fn add_assistant_blocks_appends_one_message_with_blocks_as_given() {
    let blocks = vec![
        ContentBlock::text("reading"),
        tool_use_input("t1", "read_file", json!({"path": "a"})),
    ];
    let mut conv = started_conv();
    conv.add_assistant_blocks(blocks.clone());
    assert_eq!(conv.messages.last().unwrap(), &Message::assistant(blocks));
}

#[test]
fn add_assistant_response_stamps_the_message_add_assistant_blocks_does_not() {
    let stamp = Provenance::new("anthropic", "claude-fable-5");
    let mut conv = started_conv();
    conv.add_assistant_response(vec![ContentBlock::text("reply")], stamp.clone());
    conv.add_assistant_blocks(vec![ContentBlock::text("[marker]")]);
    assert_eq!(conv.messages[1].provenance, Some(stamp));
    assert_eq!(conv.messages[2].provenance, None);
}

#[test]
fn add_tool_results_appends_all_results_as_one_user_message() {
    let results = vec![tool_result("t1", "one"), tool_result_err("t2", "two", true)];
    let mut conv = started_conv();
    conv.add_assistant_blocks(vec![
        tool_use("t1", "grep_search"),
        tool_use("t2", "grep_search"),
    ]);
    conv.add_tool_results(results.clone(), vec![]);
    assert_eq!(conv.messages.len(), 3);
    assert_eq!(conv.messages.last().unwrap(), &Message::user(results));
}

#[test]
fn messages_accumulate_in_order() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
    conv.add_user_text("first");
    conv.add_assistant_blocks(vec![ContentBlock::text("ok")]);
    conv.add_user_text("second");
    let roles: Vec<Role> = conv.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
}

#[test]
fn note_usage_stores_the_usage() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
    let usage = Usage::with_input_tokens(10);
    conv.note_usage(usage.clone());
    assert_eq!(conv.last_usage, Some(usage));
}

// ---- token_estimate/1 ----

#[test]
fn token_estimate_is_ceil_chars_over_35() {
    // 4 (system) + 5 (text) = 9 chars -> ceil(9 / 3.5) = 3
    let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0));
    conv.add_user_text("hello");
    assert_eq!(conv.token_estimate(), 3);
}

#[test]
fn token_estimate_counts_overhead_chars() {
    // 4 + 5 + 26 = 35 -> 35 / 3.5 = 10
    let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0).overhead_chars(26));
    conv.add_user_text("hello");
    assert_eq!(conv.token_estimate(), 10);
}

#[test]
fn token_estimate_counts_tool_use_and_tool_result_content() {
    let base = Conversation::new("", ConversationOpts::new(1000, 0));
    let mut with_blocks = base.clone();
    with_blocks.add_assistant_blocks(vec![tool_use_input(
        "t1",
        "grep_search",
        json!({"pattern": "x"}),
    )]);
    with_blocks.add_tool_results(vec![tool_result("t1", &"r".repeat(40))], vec![]);

    assert_eq!(base.token_estimate(), 0);
    assert!(with_blocks.token_estimate() > base.token_estimate());
    assert!(with_blocks.token_estimate() >= 10);
}

#[test]
fn token_estimate_uses_input_tokens_when_larger() {
    let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0));
    conv.add_user_text("hello");
    conv.note_usage(Usage::with_input_tokens(500));
    assert_eq!(conv.token_estimate(), 500);
}

#[test]
fn token_estimate_keeps_char_estimate_when_input_tokens_smaller() {
    // 400 chars -> ceil(400 / 3.5) = 115
    let mut conv = Conversation::new("s".repeat(400), ConversationOpts::new(1000, 0));
    conv.note_usage(Usage::with_input_tokens(1));
    assert_eq!(conv.token_estimate(), 115);
}

#[test]
fn token_estimate_accepts_atom_keyed_usage() {
    // In baud this distinguished atom- vs string-keyed maps; in Rust the
    // Usage type unifies both, so this simply confirms input_tokens wins.
    let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0));
    conv.add_user_text("hello");
    conv.note_usage(Usage::with_input_tokens(500));
    assert_eq!(conv.token_estimate(), 500);
}

#[test]
fn token_estimate_ignores_usage_without_input_tokens() {
    // 4 chars -> ceil(4 / 3.5) = 2
    let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0));
    conv.note_usage(Usage::default());
    assert_eq!(conv.token_estimate(), 2);
}

#[test]
fn token_estimate_floors_at_the_cache_inclusive_sum() {
    // Warm cache: a tiny uncached remainder over a six-figure cached
    // prefix. The floor holds at the cache-inclusive sum, not at
    // input_tokens (ADR-0036).
    let mut conv = Conversation::new("abcd", ConversationOpts::new(200_000, 0));
    conv.add_user_text("hello");
    conv.note_usage(Usage {
        input_tokens: Some(200),
        output_tokens: Some(300),
        cache_read_input_tokens: Some(90_000),
        cache_creation_input_tokens: None,
    });
    assert_eq!(conv.token_estimate(), 90_500);
}

// ---- compaction_target/1 ----

#[test]
fn compaction_target_is_live_window_minus_slack() {
    let conv = Conversation::new(
        "sys",
        ConversationOpts::new(1000, 200).compaction_slack(0.3),
    );
    assert_eq!(conv.compaction_target(), 500);
    assert_eq!(compaction_target(1000, 200, 0.3), 500);
}

#[test]
fn compaction_target_with_no_slack_equals_budget_target() {
    let conv = Conversation::new("sys", ConversationOpts::new(1000, 200));
    assert_eq!(conv.compaction_target(), 800);
}

#[test]
fn compaction_target_clamps_at_zero() {
    assert_eq!(compaction_target(1000, 900, 0.5), 0);
}

// ---- compaction_keep_amount/1 ----

#[test]
fn compaction_keep_amount_is_keep_fraction_of_live_window() {
    let conv = Conversation::new("sys", ConversationOpts::new(1000, 200).compaction_keep(0.5));
    assert_eq!(conv.compaction_keep_amount(), 400);
    assert_eq!(compaction_keep_amount(1000, 200, 0.5), 400);
}

#[test]
fn compaction_keep_amount_clamps_at_zero_when_reserve_exceeds_budget() {
    assert_eq!(compaction_keep_amount(1000, 1200, 0.5), 0);
}

// ---- for_request/1 ----

#[test]
fn for_request_returns_system_and_messages_wire_ready() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
    conv.add_user_text("hello");
    assert_eq!(
        conv.for_request(),
        Ok(Request {
            system: "sys".to_string(),
            messages: vec![Message::user(vec![ContentBlock::text("hello")])],
        })
    );
}

#[test]
fn for_request_errs_when_char_estimate_exceeds_the_live_window() {
    // Pure fit-check on the char estimate against `budget - reserve` - the
    // same final-fit threshold the retired Eviction path used, so the
    // Compaction trigger point (loop_ recovers on this Err) is unchanged.
    let mut conv = Conversation::new("sys", ConversationOpts::new(50, 5));
    conv.add_user_text("x".repeat(400));
    assert!(conv.char_estimate() > 50 - 5);
    assert_eq!(conv.for_request(), Err(ContextBudgetExhausted));
}

#[test]
fn for_request_reply_reserve_counts_against_budget() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(100, 99));
    conv.add_user_text("hello");
    assert_eq!(conv.for_request(), Err(ContextBudgetExhausted));
}

#[test]
fn for_request_last_usage_floor_does_not_fail_the_fit_check() {
    // The fit check uses the char estimate alone; a large usage floor does
    // not make a small request fail.
    let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
    conv.add_user_text("hello");
    conv.note_usage(Usage::with_input_tokens(5000));
    assert!(conv.for_request().is_ok());
}

// ---- keep_cutoff/2 ----

fn user_msg_of_chars(n: usize) -> Message {
    Message::user(vec![ContentBlock::text("x".repeat(n))])
}

#[test]
fn keep_cutoff_returns_the_index_of_the_crossing_message() {
    // Newest-first: 100 < 150, then 200 >= 150 crosses at index 1.
    let messages = vec![
        user_msg_of_chars(100),
        user_msg_of_chars(100),
        user_msg_of_chars(100),
    ];
    assert_eq!(keep_cutoff(&messages, 150), Some(1));
}

#[test]
fn keep_cutoff_with_zero_keep_returns_the_newest_index() {
    let messages = vec![user_msg_of_chars(10), user_msg_of_chars(10)];
    assert_eq!(keep_cutoff(&messages, 0), Some(1));
}

#[test]
fn keep_cutoff_returns_none_when_the_whole_history_fits_within_keep() {
    let messages = vec![user_msg_of_chars(100), user_msg_of_chars(100)];
    assert_eq!(keep_cutoff(&messages, 1000), None);
}

#[test]
fn keep_cutoff_returns_none_for_empty_messages() {
    assert_eq!(keep_cutoff(&[], 0), None);
    assert_eq!(keep_cutoff(&[], 100), None);
}

// ---- prepare_compaction/1 ----

#[test]
fn prepare_compaction_noop_for_empty() {
    let conv = Conversation::new("sys", ConversationOpts::new(32_000, 1000));
    assert_eq!(conv.prepare_compaction(), None);
}

#[test]
fn prepare_compaction_noop_for_one_user_message() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(32_000, 1000));
    conv.add_user_text("hello");
    assert_eq!(conv.prepare_compaction(), None);
}

#[test]
fn prepare_compaction_finds_cutoff_across_runs() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(200, 0).compaction_slack(0.0));
    for (u, a) in [("a", "b"), ("c", "d"), ("e", "f"), ("g", "h")] {
        conv.add_user_text(u.repeat(100));
        conv.add_assistant_blocks(vec![ContentBlock::text(a.repeat(100))]);
    }

    let (to_summarize, cutoff_idx, _) = conv.prepare_compaction().unwrap();
    assert!(to_summarize.len() >= 2);
    assert!(cutoff_idx > 0);
    assert!(cutoff_idx < conv.messages.len());

    let cutoff_msg = &conv.messages[cutoff_idx];
    assert_eq!(cutoff_msg.role, Role::User);
    assert!(matches!(
        cutoff_msg.content.first(),
        Some(ContentBlock::Text { .. })
    ));
}

// Multi-run conversation for compaction tests: N pairs of (user,assistant)
// messages, each padded to `chars_per_msg` characters, with the given opts.
fn multi_run_conv(
    opts: ConversationOpts,
    pairs: &[(&str, &str)],
    chars_per_msg: usize,
) -> Conversation {
    let mut conv = Conversation::new("sys", opts);
    for (u, a) in pairs {
        conv.add_user_text(u.repeat(chars_per_msg));
        conv.add_assistant_blocks(vec![ContentBlock::text(a.repeat(chars_per_msg))]);
    }
    conv
}

#[test]
fn prepare_compaction_keep_is_compaction_keep_of_window() {
    let pairs = [("a", "b"), ("c", "d"), ("e", "f"), ("g", "h")];
    let small = multi_run_conv(
        ConversationOpts::new(10_000, 0).compaction_keep(0.05),
        &pairs,
        700,
    );
    let large = multi_run_conv(
        ConversationOpts::new(10_000, 0).compaction_keep(0.3),
        &pairs,
        700,
    );

    let (_, small_cutoff, _) = small.prepare_compaction().unwrap();
    let (_, large_cutoff, _) = large.prepare_compaction().unwrap();
    assert!(small_cutoff > large_cutoff);
}

#[test]
fn prepare_compaction_walk_measures_keep_in_chars_not_tokens() {
    // Pins the flagged ambiguity in CONTEXT.md (preserved deliberately,
    // pending a tuning decision): the Compaction Keep amount is a
    // token-space figure, but the walk accumulates raw chars, so the
    // executed keep is ~3.5x smaller than configured. Keep amount =
    // 0.05 * 10_000 = 500. The newest message alone is 600 chars
    // (~172 tokens): a char walk crosses on it, snapping the cutoff to
    // the last run start (index 6); a token walk would need three
    // messages and snap to index 4.
    let pairs = [("a", "b"), ("c", "d"), ("e", "f"), ("g", "h")];
    let conv = multi_run_conv(
        ConversationOpts::new(10_000, 0).compaction_keep(0.05),
        &pairs,
        600,
    );

    let (to_summarize, cutoff_idx, _) = conv.prepare_compaction().unwrap();
    assert_eq!(cutoff_idx, 6);
    assert_eq!(to_summarize.len(), 6);
}

#[test]
fn prepare_compaction_compaction_slack_no_longer_affects_cutoff() {
    let pairs = [("a", "b"), ("c", "d"), ("e", "f")];
    let make_opts = || ConversationOpts::new(1_000, 0).compaction_keep(0.5);
    let zero = multi_run_conv(make_opts().compaction_slack(0.0), &pairs, 300);
    let high = multi_run_conv(make_opts().compaction_slack(0.9), &pairs, 300);

    let (_, cutoff_zero, _) = zero.prepare_compaction().unwrap();
    let (_, cutoff_high, _) = high.prepare_compaction().unwrap();
    assert_eq!(cutoff_zero, cutoff_high);
}

#[test]
fn prepare_compaction_cutoff_lands_on_run_start_user_message() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1, 0));
    for (u, a) in [
        ("turn 1", "a"),
        ("turn 2", "b"),
        ("turn 3", "c"),
        ("turn 4", "d"),
    ] {
        conv.add_user_text(u);
        conv.add_assistant_blocks(vec![ContentBlock::text(a)]);
    }

    let (_, cutoff_idx, _) = conv.prepare_compaction().unwrap();
    let cutoff_msg = &conv.messages[cutoff_idx];
    assert_eq!(cutoff_msg.role, Role::User);
    assert!(matches!(
        cutoff_msg.content.first(),
        Some(ContentBlock::Text { .. })
    ));
}

// ---- apply_compaction/3 ----

#[test]
fn apply_compaction_replaces_old_messages_keeps_tail() {
    let mut conv = Conversation::new("sys", ConversationOpts::new(1, 0));
    conv.add_user_text("turn 1");
    conv.add_assistant_blocks(vec![ContentBlock::text("old response")]);
    conv.add_user_text("turn 2");
    conv.add_assistant_blocks(vec![ContentBlock::text("this should survive")]);

    let compacted = conv.apply_compaction("Summary of turn 1", 2);
    assert_eq!(compacted.messages.len(), 3);

    assert_eq!(compacted.messages[0].role, Role::User);
    match compacted.messages[0].content.first().unwrap() {
        ContentBlock::Text { text } => assert!(text.contains("Summary of turn 1")),
        _ => panic!("expected text summary block"),
    }

    assert_eq!(compacted.messages[1].role, Role::User);
    match compacted.messages[1].content.first().unwrap() {
        ContentBlock::Text { text } => assert_eq!(text, "turn 2"),
        _ => panic!("expected turn 2 text"),
    }

    assert_eq!(compacted.messages[2].role, Role::Assistant);
    match compacted.messages[2].content.first().unwrap() {
        ContentBlock::Text { text } => assert_eq!(text, "this should survive"),
        _ => panic!("expected surviving text"),
    }
}

// ---- extract_file_ops/1 ----

#[test]
fn extract_file_ops_extracts_read_and_write_ops() {
    let messages = vec![
        Message::user(vec![
            tool_use_input("_", "read_file", json!({"file_path": "foo.ex"})),
            tool_use_input("_", "write_file", json!({"file_path": "bar.ex"})),
        ]),
        Message::user(vec![
            tool_use_input("_", "edit", json!({"file_path": "bar.ex"})),
            tool_use_input("_", "list_directory", json!({"path": "lib/"})),
        ]),
    ];

    let ops = extract_file_ops(&messages);
    let mut reads = ops.read_files.clone();
    reads.sort();
    let mut mods = ops.modified_files.clone();
    mods.sort();
    assert_eq!(reads, vec!["foo.ex", "lib/"]);
    assert_eq!(mods, vec!["bar.ex"]);
}

#[test]
fn extract_file_ops_deduplicates() {
    let messages = vec![Message::user(vec![
        tool_use_input("_", "read_file", json!({"file_path": "foo.ex"})),
        tool_use_input("_", "read_file", json!({"file_path": "foo.ex"})),
    ])];
    let ops = extract_file_ops(&messages);
    assert_eq!(ops.read_files, vec!["foo.ex"]);
}
