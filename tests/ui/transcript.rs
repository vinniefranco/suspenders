use super::*;
use crate::presenter::Presenter;
use crate::view_model::{DiffHunk, DiffLine, DiffSide};
use serde_json::json;

// --- helpers ------------------------------------------------------------

fn fresh() -> Transcript {
    Transcript::new(Vec::new())
}

fn user(text: &str) -> TranscriptItem {
    TranscriptItem::User { text: text.into() }
}
fn assistant(text: &str) -> TranscriptItem {
    TranscriptItem::Assistant { text: text.into() }
}
fn thinking(text: &str) -> TranscriptItem {
    TranscriptItem::Thinking { text: text.into() }
}
fn info(text: &str) -> TranscriptItem {
    TranscriptItem::Info { text: text.into() }
}
fn marker(text: &str, tone: Tone) -> TranscriptItem {
    TranscriptItem::Marker {
        text: text.into(),
        tone,
    }
}
fn tool_call_item(id: &str, name: &str, summary: &str) -> TranscriptItem {
    TranscriptItem::ToolCall {
        id: id.into(),
        name: name.into(),
        summary: summary.into(),
    }
}
fn tool_result_item(name: &str, summary: &str, is_error: bool) -> TranscriptItem {
    TranscriptItem::ToolResult {
        name: name.into(),
        summary: summary.into(),
        is_error,
        key_arg: None,
    }
}
fn tool_result_merged(name: &str, summary: &str, is_error: bool, key_arg: &str) -> TranscriptItem {
    TranscriptItem::ToolResult {
        name: name.into(),
        summary: summary.into(),
        is_error,
        key_arg: Some(key_arg.into()),
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.into() }
}
fn thinking_block(text: &str) -> ContentBlock {
    ContentBlock::Thinking { text: text.into() }
}

// --- streaming lifecycle -------------------------------------------------

#[test]
fn message_end_materializes_thinking_then_text() {
    let mut t = fresh();
    t.message_start();
    t.message_update(vec![thinking_block("hmm"), text_block("reading")]);
    t.message_end(&[text_block("reading")]);
    assert_eq!(t.items(), vec![thinking("hmm"), assistant("reading")]);
    assert!(t.streaming_text().is_empty() && t.streaming_thinking().is_empty());
}

#[test]
fn no_thinking_in_snapshot_yields_no_thinking_item() {
    let mut t = fresh();
    t.message_start();
    t.message_end(&[text_block("no thinking here")]);
    assert_eq!(t.items(), vec![assistant("no thinking here")]);
}

#[test]
fn discard_streaming_drops_the_snapshot_without_settling_it() {
    let mut t = fresh();
    t.message_start();
    t.message_update(vec![text_block("stale")]);
    t.discard_streaming();
    assert!(t.streaming_text().is_empty());
    // Nothing to salvage on a later close: the snapshot is gone.
    t.close(None);
    assert!(t.items().is_empty());
}

// --- close (the flush-before-note ordering) -------------------------------

#[test]
fn close_settles_the_live_snapshot_then_records_the_note() {
    let mut t = fresh();
    t.message_start();
    t.message_update(vec![thinking_block("mid"), text_block("partial")]);
    t.close(Some("turn cancelled".into()));
    assert_eq!(
        t.items(),
        vec![
            thinking("mid"),
            assistant("partial"),
            info("turn cancelled")
        ]
    );
    // The snapshot emptied: a second close salvages nothing and records
    // only its note.
    t.close(None);
    assert_eq!(t.items().len(), 3);
}

#[test]
fn a_clean_close_with_no_note_is_silent_when_idle() {
    let mut t = fresh();
    t.close(None);
    assert!(t.items().is_empty());
}

// --- tool_call summaries --------------------------------------------------

#[test]
fn key_arg_picks_the_salient_arg_by_tool() {
    // path for the file tools, command for run_command, pattern/query for
    // grep/search.
    assert_eq!(
        key_arg(
            "read_file",
            &json!({"path": "src/foo.rs", "start_line": 10})
        ),
        Some("src/foo.rs".to_string())
    );
    assert_eq!(
        key_arg("run_shell_command", &json!({"command": "cargo test"})),
        Some("cargo test".to_string())
    );
    assert_eq!(
        key_arg("grep_search", &json!({"pattern": "TODO", "path": "src"})),
        Some("TODO".to_string())
    );
    assert_eq!(
        key_arg("search", &json!({"query": "needle"})),
        Some("needle".to_string())
    );
}

#[test]
fn key_arg_falls_back_to_the_first_sorted_value_and_none_when_empty() {
    // No named arg for this tool: the first value in sorted key order.
    assert_eq!(
        key_arg("mystery_tool", &json!({"zeta": "z", "alpha": "a"})),
        Some("a".to_string())
    );
    // An empty / non-object input has no salient arg.
    assert_eq!(key_arg("read_file", &json!({})), None);
    assert_eq!(key_arg("read_file", &json!("not an object")), None);
}

// The single emptiness rule (Nit-1): a salient arg that formats empty yields
// None, so the live call line falls back to the full summary rather than a
// dangling `name  ` with a blank arg.
#[test]
fn key_arg_maps_an_empty_formatted_value_to_none() {
    assert_eq!(key_arg("run_shell_command", &json!({"command": ""})), None);
    // The live call line then falls back to summarize_input, not a blank arg.
    let mut t = fresh();
    t.tool_call(
        "t1".into(),
        "run_shell_command".into(),
        &json!({"command": ""}),
    );
    assert_eq!(
        t.items(),
        vec![tool_call_item("t1", "run_shell_command", "command=")]
    );
}

// The raw-JSON leak fix (P0): a `todo_write` NEVER shows its `todos` array in
// a summary - not the in-flight call, not a schema-passing-but-semantically-
// malformed result that drops all items (no Todo artifact, so the Tool Result
// passes through). Both paths draw their summary from `call_summary`, which is
// empty for `todo_write` (the list is the Todo body, not a description).
#[test]
fn todo_write_never_leaks_raw_json_in_any_summary() {
    // The salient-arg pick and its fallback BOTH JSON-format the array; the
    // structural fix is a clean empty summary.
    let malformed = json!({"todos": [{"content": "", "status": "bogus"}]});
    assert_eq!(call_summary("todo_write", &malformed), "");
    // The raw pick DOES carry the JSON (this is what would have leaked).
    assert!(key_arg("mystery", &malformed).unwrap().contains("content"));

    // (b) The in-flight call reads a bare `todo_write` (empty summary).
    let mut t = fresh();
    t.tool_call("t1".into(), "todo_write".into(), &malformed);
    assert_eq!(t.items(), vec![tool_call_item("t1", "todo_write", "")]);

    // (a) A malformed result that passes through (no Todo artifact) recovers
    // the call's clean summary as its key_arg - no `todos=[...]` anywhere.
    t.tool_result(
        "t1",
        "todo_write".into(),
        "Recorded.",
        false,
        &HashMap::new(),
    );
    let rendered = format!("{:?}", t.items());
    assert!(
        !rendered.contains("todos") && !rendered.contains("content:"),
        "todo_write summary leaked raw JSON: {rendered}"
    );
}

#[test]
fn a_live_tool_call_line_reads_name_then_key_arg_not_key_equals_value() {
    let mut t = fresh();
    t.tool_call(
        "t1".into(),
        "read_file".into(),
        &json!({"path": "src/foo.rs"}),
    );
    assert_eq!(
        t.items(),
        vec![tool_call_item("t1", "read_file", "src/foo.rs")]
    );
}

// --- call/result pairing merge ---------------------------------------------

#[test]
fn tool_result_appends_summary_with_error_flag() {
    let mut t = fresh();
    t.tool_result(
        "t1",
        "grep_search".into(),
        "a\nb\nc",
        false,
        &HashMap::new(),
    );
    assert_eq!(
        t.items(),
        vec![tool_result_item("grep_search", "a (+2 more lines)", false)]
    );
}

// The paired call+result collapse to ONE result item carrying the call's
// key_arg; the redundant call line is removed and the revision bumps.
#[test]
fn a_result_merges_with_its_call_removing_the_call_and_bumping_revision() {
    let mut t = fresh();
    t.tool_call(
        "t1".into(),
        "read_file".into(),
        &json!({"path": "src/foo.rs"}),
    );
    let rev_after_call = t.revision();
    t.tool_result(
        "t1",
        "read_file".into(),
        "340 lines",
        false,
        &HashMap::new(),
    );
    // Call gone, ONE merged result with the recovered key_arg.
    assert_eq!(
        t.items(),
        vec![tool_result_merged(
            "read_file",
            "340 lines",
            false,
            "src/foo.rs"
        )]
    );
    // The removal is a non-append edit: revision moved.
    assert_eq!(t.revision(), rev_after_call + 1);
}

// An in-flight call with no result yet renders alone and never bumps.
#[test]
fn an_in_flight_call_renders_alone_without_bumping_revision() {
    let mut t = fresh();
    t.tool_call(
        "t1".into(),
        "run_shell_command".into(),
        &json!({"command": "ls"}),
    );
    assert_eq!(
        t.items(),
        vec![tool_call_item("t1", "run_shell_command", "ls")]
    );
    assert_eq!(t.revision(), 0);
}

// Parallel/interleaved ids pair by id, not by position: the second result
// matches the first call.
#[test]
fn parallel_calls_pair_by_id_not_by_position() {
    let mut t = fresh();
    t.tool_call("a".into(), "read_file".into(), &json!({"path": "a.rs"}));
    t.tool_call("b".into(), "read_file".into(), &json!({"path": "b.rs"}));
    // Result for the FIRST call arrives second.
    t.tool_result("a", "read_file".into(), "10 lines", false, &HashMap::new());
    // Call `a` merged away; call `b` still pending; result carries a.rs.
    assert_eq!(
        t.items(),
        vec![
            tool_call_item("b", "read_file", "b.rs"),
            tool_result_merged("read_file", "10 lines", false, "a.rs"),
        ]
    );
}

// A result with no live call (a Voice answer to an orphaned call) removes
// nothing, does not bump, and carries no key_arg.
#[test]
fn an_unpaired_result_does_not_bump_and_has_no_key_arg() {
    let mut t = fresh();
    t.tool_result(
        "orphan",
        "run_shell_command".into(),
        "injected",
        false,
        &HashMap::new(),
    );
    assert_eq!(
        t.items(),
        vec![tool_result_item("run_shell_command", "injected", false)]
    );
    assert_eq!(t.revision(), 0);
}

// An error result keeps is_error, still removes the call, stamps key_arg,
// and bumps.
#[test]
fn an_error_result_merges_keeping_the_error_flag_and_key_arg() {
    let mut t = fresh();
    t.tool_call(
        "t1".into(),
        "run_shell_command".into(),
        &json!({"command": "cargo test"}),
    );
    t.tool_result(
        "t1",
        "run_shell_command".into(),
        "boom",
        true,
        &HashMap::new(),
    );
    assert_eq!(
        t.items(),
        vec![tool_result_merged(
            "run_shell_command",
            "boom",
            true,
            "cargo test"
        )]
    );
    assert_eq!(t.revision(), 1);
}

// --- steering (the marker equality, queued ↔ delivered) --------------------

#[test]
fn steering_queued_shows_the_pending_marker_delivered_promotes_it_to_user() {
    let mut t = fresh();
    t.steering_queued("check the README");
    // A Steering-toned marker in the plane, not a plain Info line.
    assert_eq!(
        t.items(),
        vec![marker("↳ queued: check the README", Tone::Steering)]
    );

    t.steering_delivered("check the README");
    assert_eq!(t.items(), vec![user("check the README")]);
}

// A Steering marker whose text matches an existing Info line is NOT
// removed by delivery: the anchor targets the Marker variant only, so a
// look-alike Info cannot be superseded by a Steering delivery.
#[test]
fn steering_delivered_anchors_on_the_marker_variant_not_a_look_alike_info() {
    let mut t = fresh();
    t.info("↳ queued: not really steering");
    t.steering_queued("not really steering");
    // Info first, then the real Steering marker.
    assert_eq!(
        t.items(),
        vec![
            info("↳ queued: not really steering"),
            marker("↳ queued: not really steering", Tone::Steering),
        ]
    );

    t.steering_delivered("not really steering");
    // The Marker was removed and promoted; the Info look-alike survives.
    assert_eq!(
        t.items(),
        vec![
            info("↳ queued: not really steering"),
            user("not really steering"),
        ]
    );
}

// The render cache's append-only contract: pushes leave the revision
// alone; a delivered steering's marker removal bumps it. A delivery whose
// marker was never queued removes nothing and must not bump.
#[test]
fn only_a_delivered_steering_removal_bumps_the_revision() {
    let mut t = fresh();
    t.user("hello");
    t.info("adapter news");
    assert_eq!(t.revision(), 0);

    // Queued (a push) does not bump; delivered (the remove) does.
    t.steering_queued("check the README");
    assert_eq!(t.revision(), 0);
    t.steering_delivered("check the README");
    assert_eq!(t.revision(), 1);

    // Delivered with no matching marker removes nothing: no bump.
    t.steering_delivered("never queued");
    assert_eq!(t.revision(), 1);
}

// --- presentment -----------------------------------------------------------

// A first-class Diff item as the Diff extension's Presenter would emit.
fn diff_item(name: &str) -> TranscriptItem {
    TranscriptItem::Diff {
        title: format!("diff {name}"),
        lang: None,
        hunks: vec![DiffHunk {
            header: None,
            lines: vec![DiffLine::new(DiffSide::Added, "new line")],
        }],
        elided: 0,
    }
}

struct DiffPresenter;
impl Presenter for DiffPresenter {
    fn present(
        &self,
        item: TranscriptItem,
        artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        match &item {
            TranscriptItem::ToolResult {
                name,
                is_error: false,
                ..
            } if artifacts.contains_key("diff") => diff_item(name),
            _ => item,
        }
    }
}

struct PresentCrasher;
impl Presenter for PresentCrasher {
    fn present(
        &self,
        _item: TranscriptItem,
        _artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        panic!("render boom")
    }
}

fn reg(name: &str, presenter: Box<dyn Presenter>) -> Registered {
    Registered::new(name, Value::Null).with_presenter(presenter)
}

fn diff_artifacts() -> HashMap<String, Value> {
    let mut artifacts = HashMap::new();
    artifacts.insert("diff".to_string(), json!("some_diff"));
    artifacts
}

#[test]
fn extension_replaces_tool_result_summary_using_artifacts() {
    let mut t = Transcript::new(vec![reg("DiffPresenter", Box::new(DiffPresenter))]);
    t.tool_result("t1", "edit".into(), "edited x", false, &diff_artifacts());
    assert_eq!(t.items(), vec![diff_item("edit")]);
}

#[test]
fn without_matching_artifact_default_summary_survives() {
    let mut t = Transcript::new(vec![reg("DiffPresenter", Box::new(DiffPresenter))]);
    t.tool_result("t1", "edit".into(), "edited x", false, &HashMap::new());
    assert_eq!(t.items(), vec![tool_result_item("edit", "edited x", false)]);
}

#[test]
fn tool_call_items_pass_through_present() {
    let mut t = Transcript::new(vec![reg("DiffPresenter", Box::new(DiffPresenter))]);
    t.tool_call("t1".into(), "grep_search".into(), &json!({}));
    assert_eq!(t.items(), vec![tool_call_item("t1", "grep_search", "")]);
}

#[test]
fn crashing_present_falls_back_to_default_with_info_line() {
    let mut t = Transcript::new(vec![reg("PresentCrasher", Box::new(PresentCrasher))]);
    t.tool_result("t1", "edit".into(), "edited x", false, &diff_artifacts());
    let items = t.items();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], tool_result_item("edit", "edited x", false));
    match &items[1] {
        TranscriptItem::Info { text } => {
            assert!(text.contains("PresentCrasher"));
            assert!(text.contains("present"));
            assert!(text.contains("render boom"));
        }
        other => panic!("expected info, got {other:?}"),
    }
}

// The recursion bound: the fail-open report line is pushed RAW, never
// re-presented - a extension that panics on EVERY item (this one) would
// otherwise crash on its own failure report, report that, crash on the
// report of the report, and never terminate.
#[test]
fn a_presentment_failure_line_bypasses_presentment() {
    let mut t = Transcript::new(vec![reg("PresentCrasher", Box::new(PresentCrasher))]);
    t.info("news");
    let items = t.items();
    // The pre-stage item survives fail-open, then exactly ONE raw report.
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], info("news"));
    match &items[1] {
        TranscriptItem::Info { text } => assert!(text.contains("PresentCrasher")),
        other => panic!("expected info, got {other:?}"),
    }
}

// The diff redundancy case: because the paired call is removed, the Diff
// extension's Diff item (whose title summarizes the call) stands alone.
#[test]
fn a_diff_stands_alone_after_the_paired_call_is_removed() {
    let mut t = Transcript::new(vec![reg("DiffPresenter", Box::new(DiffPresenter))]);
    t.tool_call("t1".into(), "edit".into(), &json!({"path": "src/x.rs"}));
    t.tool_result("t1", "edit".into(), "edited", false, &diff_artifacts());
    // Only the Diff remains - the redundant call line is gone.
    assert_eq!(t.items(), vec![diff_item("edit")]);
    assert_eq!(t.revision(), 1);
}

// --- the revision contract, as a property ----------------------------------

// Every verb BELOW either leaves the settled items a prefix of what they
// become (an append, revision still) or bumps the revision (a structural
// edit). This is THE contract the render cache keys on. The verb list is
// MANUAL - Rust cannot reflect over methods - so a new public verb must
// enroll here and revisit the bumps-count guard at the end (see the
// module-doc invariant list); the test cannot notice a verb it was never
// told about.
#[test]
fn every_verb_preserves_the_items_prefix_or_bumps_the_revision() {
    type Step = (&'static str, Box<dyn FnOnce(&mut Transcript)>);
    let steps: Vec<Step> = vec![
        ("info", Box::new(|t| t.info("news"))),
        (
            "header",
            Box::new(|t| t.header("suspenders", "0.1.0", "p/m", "~/x", "tip")),
        ),
        (
            "marker",
            Box::new(|t| t.marker("✂ evicted 3 stale results", Tone::Housekeeping)),
        ),
        ("user", Box::new(|t| t.user("hello"))),
        ("push", Box::new(|t| t.push(diff_item("edit")))),
        ("message_start", Box::new(|t| t.message_start())),
        (
            "message_update",
            Box::new(|t| t.message_update(vec![text_block("partial")])),
        ),
        (
            "message_end",
            Box::new(|t| t.message_end(&[text_block("done")])),
        ),
        (
            "steering_queued",
            Box::new(|t| t.steering_queued("steer me")),
        ),
        (
            "steering_delivered",
            Box::new(|t| t.steering_delivered("steer me")),
        ),
        (
            "tool_call",
            Box::new(|t| t.tool_call("t1".into(), "grep_search".into(), &json!({"pattern": "x"}))),
        ),
        (
            "tool_result",
            Box::new(|t| t.tool_result("t1", "grep_search".into(), "hit", false, &HashMap::new())),
        ),
        (
            "extension_failure",
            Box::new(|t| t.extension_failure("P", Stage::Present, "boom")),
        ),
        ("message_start (again)", Box::new(|t| t.message_start())),
        (
            "message_update (again)",
            Box::new(|t| t.message_update(vec![thinking_block("half")])),
        ),
        (
            "close",
            Box::new(|t| t.close(Some("turn cancelled".into()))),
        ),
        ("discard_streaming", Box::new(|t| t.discard_streaming())),
    ];

    let mut t = fresh();
    let mut bumps = 0;
    for (name, step) in steps {
        let rev_before = t.revision();
        let before = t.items().to_vec();
        step(&mut t);
        if t.revision() == rev_before {
            assert!(
                t.items().starts_with(&before),
                "{name} changed settled items without bumping the revision"
            );
        } else {
            assert!(
                t.revision() > rev_before,
                "{name} moved the revision backwards"
            );
            bumps += 1;
        }
    }
    // Both structural verbs actually removed something above - the prefix
    // half of the property was not satisfied vacuously.
    assert_eq!(bumps, 2, "steering_delivered and tool_result each bumped");
}

// --- markers -----------------------------------------------------------------

// A marker APPENDS with its carried tone and never bumps the revision - it
// is an ordinary append, not a structural edit.
#[test]
fn marker_appends_with_its_tone_and_does_not_bump() {
    let mut t = fresh();
    t.marker("⟨ compacted 41 messages → summary ⟩", Tone::Housekeeping);
    t.marker("⚑ plan refreshed", Tone::Aid);
    assert_eq!(
        t.items(),
        vec![
            marker("⟨ compacted 41 messages → summary ⟩", Tone::Housekeeping),
            marker("⚑ plan refreshed", Tone::Aid),
        ]
    );
    assert_eq!(t.revision(), 0);
}

// --- latest_todo (ADR-0048: the sticky box's single source of truth) ---------

fn todo_item(contents: &[&str]) -> TranscriptItem {
    TranscriptItem::Todo {
        items: contents
            .iter()
            .map(|c| crate::plan::TodoItem {
                content: (*c).into(),
                status: crate::plan::TodoStatus::Pending,
            })
            .collect(),
    }
}

#[test]
fn latest_todo_returns_the_newest_todo_with_its_index() {
    let mut t = fresh();
    assert_eq!(t.latest_todo(), None, "no todo yet");

    t.user("do it");
    t.push(todo_item(&["read", "edit"]));
    t.push(info("working"));
    // A later todo_write supersedes the earlier list (a fresh append, not a
    // structural edit) - latest_todo returns the NEWEST one and its index.
    t.push(todo_item(&["read", "edit", "ship"]));

    let (idx, items) = t.latest_todo().expect("a todo is on screen");
    assert_eq!(idx, 3, "the newest todo item's index");
    assert_eq!(items.len(), 3);
    assert_eq!(items[2].content, "ship");
}

// --- thought_subject (the spinner's rolling reasoning head) ------------------
// The pure parse ladder is tested in the `thought` child module; here we pin
// only the store read that threads the streaming snapshot through it.

// The store read threads the streaming snapshot through: subject shows
// mid-message, then empties to None between messages (clear-timing is free).
#[test]
fn thought_subject_reads_the_live_snapshot_and_clears_between_messages() {
    let mut t = fresh();
    assert_eq!(t.thought_subject(), None, "idle: no reasoning");
    t.message_start();
    t.message_update(vec![thinking_block("**Planning** the next move")]);
    assert_eq!(t.thought_subject(), Some("Planning".to_string()));
    // Settling the message empties the streaming snapshot; a new message
    // starts from nothing, so the subject clears with no manual reset.
    t.message_end(&[text_block("done")]);
    assert_eq!(t.thought_subject(), None);
}

// --- item vocabulary ---------------------------------------------------------

// The predicate and its title travel together in the pure core (S1): a
// non-empty Diff has both; everything else (empty Diff, one-line result)
// has neither, so the view never re-implements the fold rule.
#[test]
fn foldable_body_and_fold_title_agree_on_what_collapses() {
    let diff = TranscriptItem::Diff {
        title: "edit x (+1 -1)".into(),
        lang: None,
        hunks: vec![DiffHunk {
            header: None,
            lines: vec![DiffLine::new(DiffSide::Added, "a")],
        }],
        elided: 0,
    };
    assert!(diff.has_foldable_body());
    assert_eq!(diff.fold_title(), Some("edit x (+1 -1)"));

    // A Diff with no hunk lines: no body to fold. (It still HAS a title, but
    // the view gates on `has_foldable_body()`, so it never collapses.)
    let empty = TranscriptItem::Diff {
        title: "empty".into(),
        lang: None,
        hunks: vec![],
        elided: 0,
    };
    assert!(!empty.has_foldable_body());

    // A merged one-line ToolResult: neither.
    let result = TranscriptItem::ToolResult {
        name: "read_file".into(),
        summary: "340 lines".into(),
        is_error: false,
        key_arg: Some("src/foo.rs".into()),
    };
    assert!(!result.has_foldable_body());
    assert_eq!(result.fold_title(), None);
}
