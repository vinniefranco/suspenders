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
#[path = "../tests/unit/extensions.rs"]
mod tests;
