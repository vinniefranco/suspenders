//! Runs the Extension pipeline (ADR-0007) around Tool execution.
//!
//! Extensions are a static, ordered list resolved once per Session. Each
//! Extension composes two roles (ADR-0042): an optional Middleware
//! (execution-path: `pre_run`/`post_run`) and an optional Presenter
//! (display-path: `present`). Ordering is onion-style: `pre_run` folds in
//! registration order, `post_run` in reverse, so the first-registered
//! Extension wraps the rest.
//!
//! Every stage call is wrapped: a panic skips that
//! Extension's effect - the token passes through unchanged from before it ran -
//! and comes back as a [`Failure`] for the caller to report. Fail-open with
//! visibility (ADR-0007 / ADR-0018): the model never sees it, the Run never
//! fails. In Rust the isolation is `std::panic::catch_unwind` around each
//! synchronous stage call (`AssertUnwindSafe`, since the token/extension are not
//! `UnwindSafe`); the panic message is recovered from the panic payload.
//!
//! [`execute`] is the Run's dispatch seam: [`crate::tools::execute`] (raw,
//! unshaped), then the `post_run` fold, then Shaping - so Middlewares transform
//! the model-facing content BEFORE the Result Cap, and whatever they append is
//! capped like any other content. Artifacts bypass Shaping entirely; they
//! never enter the Conversation. [`crate::tools::run`] remains the
//! extension-free equivalent.
//!
//! [`present`] folds the PURE Presentment stage over a Transcript Item in
//! registration order. Like the other stages it is fail-open: a panicking
//! `present` is skipped and the item from before that Extension is kept, with a
//! [`Failure`] recorded. It reads only the Artifacts riding the `:tool_result`
//! event and does no IO.

pub mod condense;
pub mod diff;
pub mod run_command;
pub mod todo;

use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::Value;

use std::collections::HashMap;

use crate::content::{ResultBlock, result_blocks_text};
use crate::event::Stage;
use crate::middleware::token::TokenResult;
use crate::middleware::{Middleware, Token};
use crate::presenter::Presenter;
use crate::tools::{self, shaping};
use crate::view_model::TranscriptItem;

/// One registered Extension (ADR-0042): a name (used to attribute a
/// [`Failure`]), its optional Middleware and Presenter roles, and its
/// registration options. An Extension carries `Some` for each role it
/// implements and `None` for the roles it does not - a role-less slot the
/// pipeline skips, observably identical to the old identity default.
pub struct Registered {
    pub name: String,
    pub middleware: Option<Box<dyn Middleware>>,
    pub presenter: Option<Box<dyn Presenter>>,
    pub opts: Value,
}

impl Registered {
    /// A registration with no roles yet; add them with [`with_middleware`] /
    /// [`with_presenter`].
    ///
    /// [`with_middleware`]: Registered::with_middleware
    /// [`with_presenter`]: Registered::with_presenter
    pub fn new(name: impl Into<String>, opts: Value) -> Self {
        Registered {
            name: name.into(),
            middleware: None,
            presenter: None,
            opts,
        }
    }

    /// Attaches the Middleware (execution-path) role.
    pub fn with_middleware(mut self, middleware: Box<dyn Middleware>) -> Self {
        self.middleware = Some(middleware);
        self
    }

    /// Attaches the Presenter (display-path) role.
    pub fn with_presenter(mut self, presenter: Box<dyn Presenter>) -> Self {
        self.presenter = Some(presenter);
        self
    }
}

/// An Extension stage that was skipped fail-open, with enough to report an info
/// line. The model never sees this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub extension: String,
    pub stage: Stage,
    pub message: String,
}

/// The shaped outcome of the back half of the pipeline: the model-facing
/// content (a [`ResultBlock`] list, ADR-0059), its error flag, and the
/// display-side Artifacts. Artifacts are keyed by `String` and hold arbitrary
/// `Value`, mirroring the Token.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineResult {
    pub content: Vec<ResultBlock>,
    pub is_error: bool,
    pub artifacts: std::collections::HashMap<String, Value>,
}

impl PipelineResult {
    /// The text projection of the shaped blocks (ADR-0059): what the UI event
    /// and the loop read where the old `content: String` was read.
    pub fn text(&self) -> String {
        result_blocks_text(&self.content)
    }
}

/// Folds `pre_run` in registration order over the Extensions that carry a
/// Middleware role (a Presenter-only Extension is skipped). A halted token
/// short-circuits the remaining Extensions. Returns the token and any failures
/// in registration order.
pub fn pre_run(extensions: &[Registered], token: Token) -> (Token, Vec<Failure>) {
    let mut token = token;
    let mut failures = Vec::new();

    for reg in extensions {
        if token.halted {
            break;
        }
        let Some(middleware) = reg.middleware.as_ref() else {
            continue;
        };
        token = stage(reg, Stage::PreRun, token, &mut failures, |_reg, tok| {
            middleware.pre_run(tok, &reg.opts)
        });
    }

    (token, failures)
}

/// Executes the Tool and runs the back half of the pipeline: raw execution,
/// `post_run` fold in REVERSE registration order (Middleware role only), then
/// Shaping.
pub async fn execute(extensions: &[Registered], token: Token) -> (PipelineResult, Vec<Failure>) {
    // Raw, unshaped execution - the Middleware-facing result the post_run fold
    // rewrites.
    let raw = tools::execute(&token.tool, &token.input, &token.ctx).await;

    let mut token = token;
    token.result = Some(TokenResult {
        content: raw.content,
        is_error: raw.is_error,
    });
    // `raw.content` is already a `Vec<ResultBlock>` (a text tool's one Text
    // block, ADR-0059); the text-editing Middleware rewrite the text through
    // `TokenResult::set_text` and media rides through.

    // post_run folds in reverse registration order (onion): the last-registered
    // Extension runs first, the first-registered wraps it. A Presenter-only
    // Extension has no Middleware role, so it is skipped.
    let mut failures = Vec::new();
    for reg in extensions.iter().rev() {
        let Some(middleware) = reg.middleware.as_ref() else {
            continue;
        };
        token = stage(reg, Stage::PostRun, token, &mut failures, |_reg, tok| {
            middleware.post_run(tok, &reg.opts)
        });
    }
    // Reversed registration order produced reversed failures; restore
    // registration order for the caller.
    failures.reverse();

    let result = token.result.expect("result set before post_run fold");
    let content = shaping::shape(
        &token.tool,
        &token.input,
        result.content,
        token.ctx.result_cap,
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
/// CONTEXT.md) over the Extensions that carry a Presenter role (a
/// Middleware-only Extension is skipped). Each call is wrapped fail-open
/// (ADR-0007): a panicking `present` is skipped and the item from before that
/// Extension is kept, with a [`Failure`] recorded. Returns the presented item
/// and any failures in registration order.
pub fn present(
    extensions: &[Registered],
    item: TranscriptItem,
    artifacts: &HashMap<String, Value>,
) -> (TranscriptItem, Vec<Failure>) {
    let mut item = item;
    let mut failures = Vec::new();

    for reg in extensions {
        let Some(presenter) = reg.presenter.as_ref() else {
            continue;
        };
        // The pre-stage item survives a panic; clone it so the closure can
        // consume its copy while we retain the fallback.
        let fallback = item.clone();
        let result = catch_unwind(AssertUnwindSafe(|| {
            presenter.present(item, artifacts, &reg.opts)
        }));

        item = match result {
            Ok(item) => item,
            Err(payload) => {
                failures.push(Failure {
                    extension: reg.name.clone(),
                    stage: Stage::Present,
                    message: panic_message(&payload),
                });
                fallback
            }
        };
    }

    (item, failures)
}

/// One entry in the configured Extension list: an extension name and its
/// registration options. The Session carries extensions as these lightweight
/// specs (a name plus opts) and resolves them into live [`Registered`]
/// instances at Run/UI start. Each configured entry is a bare name or a name
/// paired with opts.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionSpec {
    pub name: String,
    pub opts: Value,
}

impl ExtensionSpec {
    /// A bare name entry (no opts), the analogue of a bare `Module`.
    pub fn bare(name: impl Into<String>) -> Self {
        ExtensionSpec {
            name: name.into(),
            opts: Value::Array(Vec::new()),
        }
    }

    /// A `{name, opts}` entry.
    // qual:test_helper
    pub fn with_opts(name: impl Into<String>, opts: Value) -> Self {
        ExtensionSpec {
            name: name.into(),
            opts,
        }
    }
}

impl From<&str> for ExtensionSpec {
    fn from(name: &str) -> Self {
        ExtensionSpec::bare(name)
    }
}

impl From<String> for ExtensionSpec {
    fn from(name: String) -> Self {
        ExtensionSpec::bare(name)
    }
}

/// Normalizes an extension entry into a `{name, opts}` spec. A bare name entry
/// gets an empty opts list; a `{name, opts}` entry passes its opts through.
/// This is the direct analogue of the config-list normalization: bare `Module`
/// or `{Module, opts}` both land as `{module, opts}`.
pub fn normalize(entry: impl Into<ExtensionSpec>) -> ExtensionSpec {
    entry.into()
}

/// Resolves the Session's ordered Extension names into live [`Registered`]
/// instances, in registration order. Each name maps to its extension
/// implementation ([`build`]); an unknown name is skipped (it cannot be
/// registered, so it has no effect and cannot fail a stage). This is the
/// production registry: the shipped config resolves
/// `["diff", "run_shell_command", "condense", "todo"]` into the Diff extension, the
/// run_command exit-badge extension, the condense noise-collapse extension, and
/// the Todo display extension (ADR-0048), so the live app runs the
/// Run/Presentment pipeline with all four.
pub fn configured(names: &[String]) -> Vec<Registered> {
    names
        .iter()
        .filter_map(|name| build(&normalize(name.clone())))
        .collect()
}

/// Builds the [`Registered`] extension for one normalized spec, or `None` if
/// the name has no registered implementation. Maps each name to the role(s) it
/// composes: `diff` is Middleware + Presenter, `run_command` is Middleware +
/// Presenter, `condense` is Middleware-only, `todo` is Middleware + Presenter.
/// The concrete structs are ZSTs, so boxing one twice as two trait objects is
/// free.
fn build(spec: &ExtensionSpec) -> Option<Registered> {
    match spec.name.as_str() {
        "diff" => Some(
            Registered::new("diff", spec.opts.clone())
                .with_middleware(Box::new(diff::Diff))
                .with_presenter(Box::new(diff::Diff)),
        ),
        "run_shell_command" => Some(
            Registered::new("run_shell_command", spec.opts.clone())
                .with_middleware(Box::new(run_command::RunCommand))
                .with_presenter(Box::new(run_command::RunCommand)),
        ),
        "condense" => Some(
            Registered::new("condense", spec.opts.clone())
                .with_middleware(Box::new(condense::Condense)),
        ),
        "todo" => Some(
            Registered::new("todo", spec.opts.clone())
                .with_middleware(Box::new(todo::Todo))
                .with_presenter(Box::new(todo::Todo)),
        ),
        _ => None,
    }
}

/// Runs one stage of one Extension fail-open. A panic in the stage skips its
/// effect (the token passes through unchanged) and records a [`Failure`];
/// otherwise the transformed token is returned. `AssertUnwindSafe` is required
/// because [`Token`] and the extension trait objects are not `UnwindSafe`; a
/// caught panic leaves no observable shared state (the pre-stage token is what
/// we keep).
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
                extension: reg.name.clone(),
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

#[cfg(test)]
mod tests {
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
            ExtensionSpec::with_opts("Recorder", json!([]))
        );
        assert_eq!(
            normalize(ExtensionSpec::with_opts("Halter", json!({ "a": 1 }))),
            ExtensionSpec::with_opts("Halter", json!({ "a": 1 }))
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
            Registered::new("ArtifactPresenter", json!({}))
                .with_presenter(Box::new(ArtifactPresenter)),
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
            Registered::new("ArtifactPresenter", json!({}))
                .with_presenter(Box::new(ArtifactPresenter)),
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
        let extensions =
            vec![Registered::new("Crasher", json!({})).with_presenter(Box::new(Crasher))];

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
}
