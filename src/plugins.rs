//! Runs the Plugin pipeline (ADR-0007) around Tool execution.
//!
//! Plugins are a static, ordered list resolved once per Session. Ordering is
//! onion-style: `pre_run` folds in registration order, `post_run` in reverse,
//! so the first-registered plugin wraps the rest.
//!
//! Every stage call is wrapped: a panic skips that
//! plugin's effect - the token passes through unchanged from before it ran -
//! and comes back as a [`Failure`] for the caller to report. Fail-open with
//! visibility (ADR-0007 / ADR-0018): the model never sees it, the Turn never
//! fails. In Rust the isolation is `std::panic::catch_unwind` around each
//! synchronous stage call (`AssertUnwindSafe`, since the token/plugin are not
//! `UnwindSafe`); the panic message is recovered from the panic payload.
//!
//! [`execute`] is the Turn's dispatch seam: [`crate::tools::execute`] (raw,
//! unshaped), then the `post_run` fold, then Shaping - so plugins transform
//! the model-facing content BEFORE the Result Cap, and whatever they append is
//! capped like any other content. Artifacts bypass Shaping entirely; they
//! never enter the Conversation. [`crate::tools::run`] remains the
//! plugin-free equivalent.
//!
//! [`present`] folds the PURE Presentment stage over a Transcript Item in
//! registration order. Like the other stages it is fail-open: a panicking
//! `present` is skipped and the item from before that plugin is kept, with a
//! [`Failure`] recorded. It reads only the Artifacts riding the `:tool_result`
//! event and does no IO.

pub mod diff;

use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::Value;

use std::collections::HashMap;

use crate::event::Stage;
use crate::plugin::token::TokenResult;
use crate::plugin::{Plugin, Token};
use crate::tools::{self, shaping};
use crate::ui::transcript::TranscriptItem;

/// One registered Plugin: a name (used to attribute a
/// [`Failure`]), the plugin itself, and its registration options.
pub struct Registered {
    pub name: String,
    pub plugin: Box<dyn Plugin>,
    pub opts: Value,
}

impl Registered {
    pub fn new(name: impl Into<String>, plugin: Box<dyn Plugin>, opts: Value) -> Self {
        Registered {
            name: name.into(),
            plugin,
            opts,
        }
    }
}

/// A Plugin stage that was skipped fail-open, with enough to report an info
/// line. The model never sees this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub plugin: String,
    pub stage: Stage,
    pub message: String,
}

/// The shaped outcome of the back half of the pipeline: the model-facing
/// content, its error flag, and the display-side Artifacts.
/// Artifacts are keyed by `String` and hold arbitrary `Value`,
/// mirroring the Token.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineResult {
    pub content: String,
    pub is_error: bool,
    pub artifacts: std::collections::HashMap<String, Value>,
}

/// Folds `pre_run` in registration order. A halted token short-circuits the
/// remaining plugins. Returns the token and any failures
/// in registration order.
pub fn pre_run(plugins: &[Registered], token: Token) -> (Token, Vec<Failure>) {
    let mut token = token;
    let mut failures = Vec::new();

    for reg in plugins {
        if token.halted {
            break;
        }
        token = stage(reg, Stage::PreRun, token, &mut failures, |reg, tok| {
            reg.plugin.pre_run(tok, &reg.opts)
        });
    }

    (token, failures)
}

/// Executes the Tool and runs the back half of the pipeline: raw execution,
/// `post_run` fold in REVERSE registration order, then Shaping.
pub async fn execute(plugins: &[Registered], token: Token) -> (PipelineResult, Vec<Failure>) {
    // Raw, unshaped execution - the plugin-facing result the post_run fold
    // rewrites.
    let raw = tools::execute(&token.tool, &token.input, &token.ctx).await;

    let mut token = token;
    token.result = Some(TokenResult {
        content: raw.content,
        is_error: raw.is_error,
    });

    // post_run folds in reverse registration order (onion): the last-registered
    // plugin runs first, the first-registered wraps it.
    let mut failures = Vec::new();
    for reg in plugins.iter().rev() {
        token = stage(reg, Stage::PostRun, token, &mut failures, |reg, tok| {
            reg.plugin.post_run(tok, &reg.opts)
        });
    }
    // Reversed registration order produced reversed failures; restore
    // registration order for the caller.
    failures.reverse();

    let result = token.result.expect("result set before post_run fold");
    let content = shaping::shape(
        &token.tool,
        &result.content,
        token.ctx.result_cap,
        read_start_line(&token.tool, &token.input),
    );

    (
        PipelineResult {
            content,
            is_error: result.is_error,
            artifacts: token.artifacts,
        },
        failures,
    )
}

/// Folds `present` over a Transcript Item in registration order (Presentment,
/// CONTEXT.md). Each call is wrapped fail-open (ADR-0007): a panicking `present`
/// is skipped and the item from before that plugin is kept, with a [`Failure`]
/// recorded. Returns the presented item and any failures in registration order.
pub fn present(
    plugins: &[Registered],
    item: TranscriptItem,
    artifacts: &HashMap<String, Value>,
) -> (TranscriptItem, Vec<Failure>) {
    let mut item = item;
    let mut failures = Vec::new();

    for reg in plugins {
        // The pre-stage item survives a panic; clone it so the closure can
        // consume its copy while we retain the fallback.
        let fallback = item.clone();
        let result = catch_unwind(AssertUnwindSafe(|| {
            reg.plugin.present(item, artifacts, &reg.opts)
        }));

        item = match result {
            Ok(item) => item,
            Err(payload) => {
                failures.push(Failure {
                    plugin: reg.name.clone(),
                    stage: Stage::Present,
                    message: panic_message(&payload),
                });
                fallback
            }
        };
    }

    (item, failures)
}

/// One entry in the configured Plugin list: a plugin name and its registration
/// options. The Session carries plugins as these lightweight specs (a name plus
/// opts) and resolves them into live [`Registered`] instances at Turn/UI start.
/// This is the analogue of the config `:plugins` entries - a bare `Module` or a
/// `{Module, opts}` tuple.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginSpec {
    pub name: String,
    pub opts: Value,
}

impl PluginSpec {
    /// A bare name entry (no opts), the analogue of a bare `Module`.
    pub fn bare(name: impl Into<String>) -> Self {
        PluginSpec {
            name: name.into(),
            opts: Value::Array(Vec::new()),
        }
    }

    /// A `{name, opts}` entry.
    pub fn with_opts(name: impl Into<String>, opts: Value) -> Self {
        PluginSpec {
            name: name.into(),
            opts,
        }
    }
}

impl From<&str> for PluginSpec {
    fn from(name: &str) -> Self {
        PluginSpec::bare(name)
    }
}

impl From<String> for PluginSpec {
    fn from(name: String) -> Self {
        PluginSpec::bare(name)
    }
}

/// Normalizes a plugin entry into a `{name, opts}` spec. A bare name entry
/// gets an empty opts list; a `{name, opts}` entry passes its opts through.
/// This is the direct analogue of the config-list normalization: bare `Module`
/// or `{Module, opts}` both land as `{module, opts}`.
pub fn normalize(entry: impl Into<PluginSpec>) -> PluginSpec {
    entry.into()
}

/// Resolves the Session's ordered Plugin names into live [`Registered`]
/// instances, in registration order. Each name maps to its plugin
/// implementation ([`build`]); an unknown name is skipped (it cannot be
/// registered, so it has no effect and cannot fail a stage). This is the
/// production registry: the shipped config resolves `["diff"]` into the one
/// Diff plugin, so the live app runs the Turn/Presentment pipeline with Diff.
pub fn configured(names: &[String]) -> Vec<Registered> {
    names
        .iter()
        .filter_map(|name| build(&normalize(name.clone())))
        .collect()
}

/// Builds the [`Registered`] plugin for one normalized spec, or `None` if the
/// name has no registered implementation.
fn build(spec: &PluginSpec) -> Option<Registered> {
    match spec.name.as_str() {
        "diff" => Some(Registered::new(
            "diff",
            Box::new(diff::Diff),
            spec.opts.clone(),
        )),
        _ => None,
    }
}

/// Runs one stage of one plugin fail-open. A panic in the stage skips its
/// effect (the token passes through unchanged) and records a [`Failure`];
/// otherwise the transformed token is returned. `AssertUnwindSafe` is required
/// because [`Token`] and `dyn Plugin` are not `UnwindSafe`; a caught panic
/// leaves no observable shared state (the pre-stage token is what we keep).
fn stage<F>(
    reg: &Registered,
    stage_name: Stage,
    token: Token,
    failures: &mut Vec<Failure>,
    call: F,
) -> Token
where
    F: FnOnce(&Registered, Token) -> Token,
{
    // The pre-stage token is what survives a panic. Clone it so the closure can
    // consume its copy while we retain the fallback.
    let fallback = token.clone();
    let result = catch_unwind(AssertUnwindSafe(|| call(reg, token)));

    match result {
        Ok(token) => token,
        Err(payload) => {
            failures.push(Failure {
                plugin: reg.name.clone(),
                stage: stage_name,
                message: panic_message(&payload),
            });
            fallback
        }
    }
}

/// Recovers a human-readable message from a caught panic payload, the way
/// a raised exception surfaces its message. `panic!("msg")` and
/// `panic!("{}", s)` both land as a `String` or `&str` in the payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "plugin panicked".to_string()
    }
}

// Shaping reads read_file's `start_line` so a cut's resume marker is absolute
// (mirrors `tools::run`); every other tool passes `None`.
fn read_start_line(tool: &str, input: &Value) -> Option<i64> {
    if tool == "read_file" {
        input.get("start_line").and_then(|v| v.as_i64())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCtx;
    use crate::ui::transcript::{LineStyle, StyledLine, TranscriptItem};
    use serde_json::json;
    use std::collections::HashMap;

    // ---- The `@ctx` from plugins_test.exs: %{root: "/nowhere", result_cap: 10_000} ----
    fn ctx() -> ToolCtx {
        ToolCtx {
            root: "/nowhere".into(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
            scout: None,
        }
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

    // ---- Test plugins, defined inline as baud does ----

    // Records the order stages ran in, tagged by the :id opt, appending to a
    // `trace` list in assigns. baud's Recorder implements both pre_run and
    // post_run; here it does too.
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
    impl Plugin for Recorder {
        fn pre_run(&self, token: Token, opts: &Value) -> Token {
            Self::record(token, opts, "pre_run")
        }
        fn post_run(&self, token: Token, opts: &Value) -> Token {
            Self::record(token, opts, "post_run")
        }
    }

    // Halts in pre_run.
    struct Halter;
    impl Plugin for Halter {
        fn pre_run(&self, token: Token, _opts: &Value) -> Token {
            token.halt("[blocked by halter]")
        }
    }

    // Panics in both stages (baud's Crasher raises "pre boom"/"post boom").
    struct Crasher;
    impl Plugin for Crasher {
        fn pre_run(&self, _token: Token, _opts: &Value) -> Token {
            panic!("pre boom")
        }
        fn post_run(&self, _token: Token, _opts: &Value) -> Token {
            panic!("post boom")
        }
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
    impl Plugin for ContentTagger {
        fn post_run(&self, mut token: Token, opts: &Value) -> Token {
            let id = opts.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(result) = token.result.as_mut() {
                result.content = format!("{} <{}>", result.content, id);
            }
            token
        }
    }

    // Attaches an artifact in post_run; in present, replaces a tool_result with
    // a block when the `mark: seen` artifact is present (baud's ArtifactPresenter).
    struct ArtifactPresenter;
    impl Plugin for ArtifactPresenter {
        fn post_run(&self, token: Token, _opts: &Value) -> Token {
            token.put_artifact("mark", json!("seen"))
        }

        fn present(
            &self,
            item: TranscriptItem,
            artifacts: &HashMap<String, Value>,
            _opts: &Value,
        ) -> TranscriptItem {
            match (&item, artifacts.get("mark")) {
                (TranscriptItem::ToolResult { name, .. }, Some(mark))
                    if mark == &json!("seen") =>
                {
                    TranscriptItem::Block {
                        title: format!("presented {name}"),
                        lines: vec![StyledLine::new(LineStyle::Added, "+ line")],
                    }
                }
                _ => item,
            }
        }
    }

    fn reg(name: &str, plugin: Box<dyn Plugin>, opts: Value) -> Registered {
        Registered::new(name, plugin, opts)
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
            PluginSpec::with_opts("Recorder", json!([]))
        );
        assert_eq!(
            normalize(PluginSpec::with_opts("Halter", json!({ "a": 1 }))),
            PluginSpec::with_opts("Halter", json!({ "a": 1 }))
        );
    }

    // Resolving the shipped config list `["diff"]` yields exactly one plugin,
    // the Diff plugin, registered under the name "diff".
    #[test]
    fn configured_resolves_diff_name_to_one_diff_plugin() {
        let plugins = configured(&["diff".to_string()]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "diff");
    }

    // An unknown name has no implementation, so it is skipped (it cannot
    // register and thus cannot run or fail a stage). The empty test-env list
    // resolves to no plugins.
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
        let plugins = vec![
            reg("Recorder", Box::new(Recorder), json!({ "id": "a" })),
            reg("Recorder", Box::new(Recorder), json!({ "id": "b" })),
        ];

        let (token, failures) = pre_run(&plugins, token());

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
    fn pre_run_halted_token_short_circuits_the_remaining_plugins() {
        let plugins = vec![
            reg("Halter", Box::new(Halter), json!({})),
            reg("Recorder", Box::new(Recorder), json!({ "id": "after_halt" })),
        ];

        let (token, failures) = pre_run(&plugins, token());

        assert!(failures.is_empty());
        assert!(token.halted);
        assert_eq!(token.halt_reason.as_deref(), Some("[blocked by halter]"));
        assert!(!token.assigns.contains_key("trace"));
    }

    #[test]
    fn pre_run_a_crashing_plugin_is_skipped_and_reported_the_rest_still_run() {
        let plugins = vec![
            reg("Crasher", Box::new(Crasher), json!({})),
            reg("Recorder", Box::new(Recorder), json!({ "id": "survivor" })),
        ];

        let (token, failures) = pre_run(&plugins, token());

        assert_eq!(
            trace(&token),
            vec![("survivor".to_string(), "pre_run".to_string())]
        );
        assert_eq!(
            failures,
            vec![Failure {
                plugin: "Crasher".to_string(),
                stage: Stage::PreRun,
                message: "pre boom".to_string(),
            }]
        );
    }

    // baud's "a stage returning a non-token is a reported failure" case: in
    // baud a plugin can return the wrong shape at runtime (dynamic dispatch on
    // an untyped return). In Rust the trait method's return type is `Token`, so
    // this failure mode is impossible - the compiler enforces the contract that
    // baud's `token_stage` guards at runtime. SKIPPED as unrepresentable
    // (type-system-enforced), not deferred; noted in the report.

    #[test]
    fn pre_run_plugins_without_the_stage_are_skipped() {
        // ContentTagger only overrides post_run; its pre_run is the default
        // identity, so the token passes through unchanged with no failures.
        let plugins = vec![reg(
            "ContentTagger",
            Box::new(ContentTagger),
            json!({ "id": "x" }),
        )];

        let (token, failures) = pre_run(&plugins, token());

        assert!(failures.is_empty());
        assert!(!token.halted);
        assert!(!token.assigns.contains_key("trace"));
    }

    // ============================================================
    // execute/2
    // ============================================================

    #[tokio::test]
    async fn execute_runs_the_tool_and_returns_the_shaped_result_with_artifacts() {
        let plugins = vec![reg("ArtifactPresenter", Box::new(ArtifactPresenter), json!({}))];

        let (result, failures) = execute(&plugins, token()).await;

        assert!(failures.is_empty());
        assert!(result.is_error);
        assert!(result.content.contains("unknown tool"));
        let mut expected = HashMap::new();
        expected.insert("mark".to_string(), json!("seen"));
        assert_eq!(result.artifacts, expected);
    }

    #[tokio::test]
    async fn execute_post_run_folds_in_reverse_registration_order_onion() {
        let plugins = vec![
            reg("ContentTagger", Box::new(ContentTagger), json!({ "id": "outer" })),
            reg("ContentTagger", Box::new(ContentTagger), json!({ "id": "inner" })),
        ];

        let (result, failures) = execute(&plugins, token()).await;

        assert!(failures.is_empty());
        // inner (last registered) runs first; outer wraps it.
        assert!(result.content.ends_with("<inner> <outer>"));
    }

    #[tokio::test]
    async fn execute_post_run_output_is_shaped_to_the_result_cap() {
        let plugins = vec![reg("ContentTagger", Box::new(ContentTagger), json!({ "id": "tag" }))];
        let token = Token::new("bogus", json!({}), ctx_with_cap(10));

        let (result, failures) = execute(&plugins, token).await;

        assert!(failures.is_empty());
        assert!(result.content.contains("[truncated:"));
    }

    #[tokio::test]
    async fn execute_a_crashing_post_run_is_skipped_and_reported() {
        let plugins = vec![
            reg("ContentTagger", Box::new(ContentTagger), json!({ "id": "kept" })),
            reg("Crasher", Box::new(Crasher), json!({})),
        ];

        let (result, failures) = execute(&plugins, token()).await;

        // ContentTagger:kept survives; Crasher's post_run is skipped. Reverse
        // fold: Crasher runs first (panics, token unchanged), then kept tags.
        assert!(result.content.ends_with("<kept>"));
        assert_eq!(
            failures,
            vec![Failure {
                plugin: "Crasher".to_string(),
                stage: Stage::PostRun,
                message: "post boom".to_string(),
            }]
        );
    }

    // ============================================================
    // present/3
    // ============================================================

    #[test]
    fn present_folds_over_the_item_plugins_that_pass_leave_it_unchanged() {
        let item = TranscriptItem::ToolResult {
            name: "edit_file".to_string(),
            summary: "edited x".to_string(),
            is_error: false,
        };
        let plugins = vec![reg("ArtifactPresenter", Box::new(ArtifactPresenter), json!({}))];

        // Empty artifacts: ArtifactPresenter's present leaves the item unchanged.
        let (presented, failures) = present(&plugins, item.clone(), &HashMap::new());

        assert!(failures.is_empty());
        assert_eq!(presented, item);
    }

    #[test]
    fn present_a_plugin_may_replace_the_item_using_the_artifacts() {
        let item = TranscriptItem::ToolResult {
            name: "edit_file".to_string(),
            summary: "edited x".to_string(),
            is_error: false,
        };
        let plugins = vec![reg("ArtifactPresenter", Box::new(ArtifactPresenter), json!({}))];
        let mut artifacts = HashMap::new();
        artifacts.insert("mark".to_string(), json!("seen"));

        let (presented, failures) = present(&plugins, item, &artifacts);

        assert!(failures.is_empty());
        assert_eq!(
            presented,
            TranscriptItem::Block {
                title: "presented edit_file".to_string(),
                lines: vec![StyledLine::new(LineStyle::Added, "+ line")],
            }
        );
    }

    #[test]
    fn present_a_crashing_present_keeps_the_item_from_before_that_plugin_and_reports() {
        let item = TranscriptItem::ToolCall {
            name: "grep".to_string(),
            summary: "pattern=x".to_string(),
        };
        let plugins = vec![reg("Crasher", Box::new(Crasher), json!({}))];

        let (presented, failures) = present(&plugins, item.clone(), &HashMap::new());

        assert_eq!(presented, item);
        assert_eq!(
            failures,
            vec![Failure {
                plugin: "Crasher".to_string(),
                stage: Stage::Present,
                message: "present boom".to_string(),
            }]
        );
    }
}
