use super::*;
use crate::tool::ToolCtx;
use crate::view_model::{DiffHunk, DiffLine, DiffSide, TranscriptItem};
use serde_json::json;
use std::collections::HashMap;

// ---- Test ctx: root "/nowhere", result_cap 10_000 ----
fn ctx() -> ToolCtx {
    ToolCtx::for_test("/nowhere".into(), 10_000)
}

fn ctx_with_cap(cap: usize) -> ToolCtx {
    ToolCtx {
        result_cap: cap,
        ..ctx()
    }
}

// baud's `token/1` helper: Token.new("bogus_tool", %{}, @ctx).
fn token() -> Token {
    Token::new("bogus_tool", json!({}), ctx())
}

// ---- Test extensions, defined inline as baud does ----

// Records the order stages ran in, tagged by the :id opt, appending to a
// `trace` list in assigns. baud's Recorder implements both pre_run and
// post_run; here it does too, as a Middleware.
struct Recorder;
impl Recorder {
    fn record(token: Token, opts: &Value, stage: &str) -> Token {
        let id = opts.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let mut trace = token
            .assigns
            .get("trace")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        trace.push(json!([id, stage]));
        token.assign("trace", Value::Array(trace))
    }
}
impl Middleware for Recorder {
    fn pre_run(&self, token: Token, opts: &Value) -> Token {
        Self::record(token, opts, "pre_run")
    }
    fn post_run(&self, token: Token, opts: &Value) -> Token {
        Self::record(token, opts, "post_run")
    }
}

// Halts in pre_run.
struct Halter;
impl Middleware for Halter {
    fn pre_run(&self, token: Token, _opts: &Value) -> Token {
        token.halt("[blocked by halter]")
    }
}

// Panics in both execution stages and in present (baud's Crasher raises
// "pre boom"/"post boom"/"present boom"). It composes both roles.
struct Crasher;
impl Middleware for Crasher {
    fn pre_run(&self, _token: Token, _opts: &Value) -> Token {
        panic!("pre boom")
    }
    fn post_run(&self, _token: Token, _opts: &Value) -> Token {
        panic!("post boom")
    }
}
impl Presenter for Crasher {
    fn present(
        &self,
        _item: TranscriptItem,
        _artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        panic!("present boom")
    }
}

// Appends " <id>" to the result content in post_run.
struct ContentTagger;
impl Middleware for ContentTagger {
    fn post_run(&self, mut token: Token, opts: &Value) -> Token {
        let id = opts.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(result) = token.result.as_mut() {
            let tagged = format!("{} <{}>", result.text_of(), id);
            result.set_text(tagged);
        }
        token
    }
}

// Attaches an artifact in post_run; in present, replaces a tool_result with
// a diff when the `mark: seen` artifact is present (baud's
// ArtifactPresenter). It composes both roles.
struct ArtifactPresenter;
impl Middleware for ArtifactPresenter {
    fn post_run(&self, token: Token, _opts: &Value) -> Token {
        token.put_artifact("mark", json!("seen"))
    }
}
impl Presenter for ArtifactPresenter {
    fn present(
        &self,
        item: TranscriptItem,
        artifacts: &HashMap<String, Value>,
        _opts: &Value,
    ) -> TranscriptItem {
        match (&item, artifacts.get("mark")) {
            (TranscriptItem::ToolResult { name, .. }, Some(mark)) if mark == &json!("seen") => {
                TranscriptItem::Diff {
                    title: format!("presented {name}"),
                    lang: None,
                    hunks: vec![DiffHunk {
                        header: None,
                        lines: vec![DiffLine::new(DiffSide::Added, "line")],
                    }],
                    elided: 0,
                }
            }
            _ => item,
        }
    }
}

// ---- reg helpers: register a double under the role(s) it implements ----

// A Middleware-only double.
fn mw(name: &str, middleware: Box<dyn Middleware>, opts: Value) -> Registered {
    Registered::new(name, opts).with_middleware(middleware)
}

// A Presenter-only double.
#[allow(dead_code)]
fn pres(name: &str, presenter: Box<dyn Presenter>, opts: Value) -> Registered {
    Registered::new(name, opts).with_presenter(presenter)
}

fn trace(token: &Token) -> Vec<(String, String)> {
    token
        .assigns
        .get("trace")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|e| {
                    let pair = e.as_array().unwrap();
                    (
                        pair[0].as_str().unwrap().to_string(),
                        pair[1].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

// ============================================================
// configured / normalize
// ============================================================

// The config-list normalization case: a bare name entry gets empty opts, a
// {name, opts} entry passes its opts through - the analogue of normalizing
// bare `Module` and `{Module, opts}` entries to `{module, opts}`.
#[test]
fn normalize_bare_and_name_opts_entries() {
    assert_eq!(
        normalize("Recorder"),
        ExtensionSpec {
            name: "Recorder".into(),
            opts: json!([]),
        }
    );
    assert_eq!(
        normalize(ExtensionSpec {
            name: "Halter".into(),
            opts: json!({ "a": 1 }),
        }),
        ExtensionSpec {
            name: "Halter".into(),
            opts: json!({ "a": 1 }),
        }
    );
}

// Resolving the shipped config list `["diff"]` yields exactly one extension,
// the Diff extension, registered under the name "diff".
#[test]
fn configured_resolves_diff_name_to_one_diff_extension() {
    let extensions = configured(&["diff".to_string()]);
    assert_eq!(extensions.len(), 1);
    assert_eq!(extensions[0].name, "diff");
}

// An unknown name has no implementation, so it is skipped (it cannot
// register and thus cannot run or fail a stage). The empty test-env list
// resolves to no extensions.
#[test]
fn configured_skips_unknown_names_and_empty_list_is_empty() {
    assert!(configured(&[]).is_empty());
    assert!(configured(&["nope".to_string()]).is_empty());
}

// ============================================================
// pre_run/2
// ============================================================

#[test]
fn pre_run_folds_in_registration_order() {
    let extensions = vec![
        mw("Recorder", Box::new(Recorder), json!({ "id": "a" })),
        mw("Recorder", Box::new(Recorder), json!({ "id": "b" })),
    ];

    let (token, failures) = pre_run(&extensions, token());

    assert!(failures.is_empty());
    assert_eq!(
        trace(&token),
        vec![
            ("a".to_string(), "pre_run".to_string()),
            ("b".to_string(), "pre_run".to_string()),
        ]
    );
}

#[test]
fn pre_run_halted_token_short_circuits_the_remaining_extensions() {
    let extensions = vec![
        mw("Halter", Box::new(Halter), json!({})),
        mw(
            "Recorder",
            Box::new(Recorder),
            json!({ "id": "after_halt" }),
        ),
    ];

    let (token, failures) = pre_run(&extensions, token());

    assert!(failures.is_empty());
    assert!(token.halted);
    assert_eq!(token.halt_reason.as_deref(), Some("[blocked by halter]"));
    assert!(!token.assigns.contains_key("trace"));
}

#[test]
fn pre_run_a_crashing_extension_is_skipped_and_reported_the_rest_still_run() {
    let extensions = vec![
        mw("Crasher", Box::new(Crasher), json!({})),
        mw("Recorder", Box::new(Recorder), json!({ "id": "survivor" })),
    ];

    let (token, failures) = pre_run(&extensions, token());

    assert_eq!(
        trace(&token),
        vec![("survivor".to_string(), "pre_run".to_string())]
    );
    assert_eq!(
        failures,
        vec![Failure {
            extension: "Crasher".to_string(),
            stage: Stage::PreRun,
            message: "pre boom".to_string(),
        }]
    );
}

// The "a stage returns the wrong shape at runtime" failure mode is
// unrepresentable here: a stage method's return type is `Token`, so the
// compiler enforces the contract statically. No test - the type system
// already guarantees it.

#[test]
fn pre_run_extensions_without_the_stage_are_skipped() {
    // ContentTagger only overrides post_run; its pre_run is the default
    // identity, so the token passes through unchanged with no failures.
    let extensions = vec![mw(
        "ContentTagger",
        Box::new(ContentTagger),
        json!({ "id": "x" }),
    )];

    let (token, failures) = pre_run(&extensions, token());

    assert!(failures.is_empty());
    assert!(!token.halted);
    assert!(!token.assigns.contains_key("trace"));
}

// ============================================================
// execute/2
// ============================================================

#[tokio::test]
async fn execute_runs_the_tool_and_returns_the_shaped_result_with_artifacts() {
    let extensions = vec![mw(
        "ArtifactPresenter",
        Box::new(ArtifactPresenter),
        json!({}),
    )];

    let (result, failures) = execute(&extensions, token()).await;

    assert!(failures.is_empty());
    assert!(result.is_error);
    assert!(result.text().contains("unknown tool"));
    let mut expected = HashMap::new();
    expected.insert("mark".to_string(), json!("seen"));
    assert_eq!(result.artifacts, expected);
}

#[tokio::test]
async fn execute_post_run_folds_in_reverse_registration_order_onion() {
    let extensions = vec![
        mw(
            "ContentTagger",
            Box::new(ContentTagger),
            json!({ "id": "outer" }),
        ),
        mw(
            "ContentTagger",
            Box::new(ContentTagger),
            json!({ "id": "inner" }),
        ),
    ];

    let (result, failures) = execute(&extensions, token()).await;

    assert!(failures.is_empty());
    // inner (last registered) runs first; outer wraps it.
    assert!(result.text().ends_with("<inner> <outer>"));
}

#[tokio::test]
async fn execute_post_run_output_is_shaped_to_the_result_cap() {
    let extensions = vec![mw(
        "ContentTagger",
        Box::new(ContentTagger),
        json!({ "id": "tag" }),
    )];
    let token = Token::new("bogus", json!({}), ctx_with_cap(10));

    let (result, failures) = execute(&extensions, token).await;

    assert!(failures.is_empty());
    assert!(result.text().contains("[truncated:"));
}

#[tokio::test]
async fn execute_a_crashing_post_run_is_skipped_and_reported() {
    let extensions = vec![
        mw(
            "ContentTagger",
            Box::new(ContentTagger),
            json!({ "id": "kept" }),
        ),
        mw("Crasher", Box::new(Crasher), json!({})),
    ];

    let (result, failures) = execute(&extensions, token()).await;

    // ContentTagger:kept survives; Crasher's post_run is skipped. Reverse
    // fold: Crasher runs first (panics, token unchanged), then kept tags.
    assert!(result.text().ends_with("<kept>"));
    assert_eq!(
        failures,
        vec![Failure {
            extension: "Crasher".to_string(),
            stage: Stage::PostRun,
            message: "post boom".to_string(),
        }]
    );
}

// ============================================================
// present/3
// ============================================================

#[test]
fn present_folds_over_the_item_extensions_that_pass_leave_it_unchanged() {
    let item = TranscriptItem::ToolResult {
        name: "edit".to_string(),
        summary: "edited x".to_string(),
        is_error: false,
        key_arg: None,
    };
    let extensions = vec![
        Registered::new("ArtifactPresenter", json!({})).with_presenter(Box::new(ArtifactPresenter)),
    ];

    // Empty artifacts: ArtifactPresenter's present leaves the item unchanged.
    let (presented, failures) = present(&extensions, item.clone(), &HashMap::new());

    assert!(failures.is_empty());
    assert_eq!(presented, item);
}

#[test]
fn present_an_extension_may_replace_the_item_using_the_artifacts() {
    let item = TranscriptItem::ToolResult {
        name: "edit".to_string(),
        summary: "edited x".to_string(),
        is_error: false,
        key_arg: None,
    };
    let extensions = vec![
        Registered::new("ArtifactPresenter", json!({})).with_presenter(Box::new(ArtifactPresenter)),
    ];
    let mut artifacts = HashMap::new();
    artifacts.insert("mark".to_string(), json!("seen"));

    let (presented, failures) = present(&extensions, item, &artifacts);

    assert!(failures.is_empty());
    assert_eq!(
        presented,
        TranscriptItem::Diff {
            title: "presented edit".to_string(),
            lang: None,
            hunks: vec![DiffHunk {
                header: None,
                lines: vec![DiffLine::new(DiffSide::Added, "line")],
            }],
            elided: 0,
        }
    );
}

#[test]
fn present_a_crashing_present_keeps_the_item_from_before_that_extension_and_reports() {
    let item = TranscriptItem::ToolCall {
        id: "t1".to_string(),
        name: "grep_search".to_string(),
        summary: "pattern=x".to_string(),
    };
    let extensions = vec![Registered::new("Crasher", json!({})).with_presenter(Box::new(Crasher))];

    let (presented, failures) = present(&extensions, item.clone(), &HashMap::new());

    assert_eq!(presented, item);
    assert_eq!(
        failures,
        vec![Failure {
            extension: "Crasher".to_string(),
            stage: Stage::Present,
            message: "present boom".to_string(),
        }]
    );
}
