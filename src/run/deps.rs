//! Run Deps - the static-dispatch dependency bundle for a Run (ADR-0011).
//!
//! Every effect the Run loop performs, as trait methods (baud's `Baud.Turn.Deps`
//! function captures). The shell builds a `RunDeps` wiring these to Agent
//! messages; tests build one whose methods record into fields the test inspects.
//! The loop itself spawns nothing and holds no pids.
//!
//! These are infrastructure, NOT Extensions: control-bearing and NOT fail-open
//! (ADR-0011). A `RunDeps` method that panics or returns an error fails the
//! Run honestly. Extensions remain the fail-open, tool-scoped unit of extension
//! (ADR-0007) - that isolation lives in `crate::extensions`, not here.
//!
//! ## Static dispatch, no `async_trait`
//!
//! The Loop is `async fn run<D: RunDeps>(...)`; `RunDeps` is monomorphised per
//! caller. The methods that do IO (`complete`, `request_approval`,
//! `drain_steering`, `compact`) return concrete futures via
//! `impl Future` in return position (edition 2024 RPITIT), so no boxing and no
//! `async_trait`. The fire-and-forget effects (`checkpoint`, `set_plan`) are
//! synchronous - the Loop never awaits them.
//!
//! Event emission is NOT a trait method: it is the owned [`Emitter`] handle
//! [`RunDeps::emitter`] hands out (ADR-0025), so the streaming sink can emit
//! while `complete` exclusively borrows the deps.

use std::future::Future;

use crate::content::Provenance;
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::response::Response;
use crate::llm::{LlmRequest, StreamEvent};

/// The detached emission handle (ADR-0025): an owned sink for run [`Event`]s,
/// obtained once from [`RunDeps::emitter`] and carried by the Loop alongside
/// the deps. Because it is a separate owned value - not a method on the deps -
/// the streaming sink inside `complete_and_emit` can emit MessageUpdates LIVE
/// while `complete` holds the exclusive `&mut D` borrow.
///
/// One boxed (dynamic) call per event is the accepted cost: an SSE delta
/// dwarfs a boxed-closure dispatch, so nothing that matters is spent here.
pub struct Emitter(Box<dyn FnMut(Event) + Send>);

impl Emitter {
    /// Wraps the sink function every emitted [`Event`] flows through.
    pub fn new(f: impl FnMut(Event) + Send + 'static) -> Self {
        Emitter(Box::new(f))
    }

    /// Emits one run Event (fire-and-forget).
    pub fn emit(&mut self, event: Event) {
        (self.0)(event)
    }
}

/// The after-Pass hook's verdict (baud's `after_pass_verdict`). The default
/// implementation always returns [`AfterPass::Continue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterPass {
    /// Keep looping.
    Continue,
    /// Close the Run with the stopped marker and this stop reason (an arbitrary
    /// atom in baud - carried here as a string, e.g. `"budget_hook"`).
    Stop(String),
    /// Merge this text into the trailing tool-results user message and loop.
    Inject(String),
}

/// The error a `compact` Dep returns when it cannot (or will not) compact. Its
/// only observable behaviour in the loop is falling through to the exhaustion
/// path; the reason rides for the caller's own logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactError(pub String);

/// Every effect the Run loop performs (ADR-0011). Static-dispatch: the Loop is
/// generic over `D: RunDeps` and monomorphises per caller. Methods that do IO
/// are async (return a `Future`); `checkpoint`/`set_plan` are fire-and-forget
/// and synchronous. Event emission goes through the [`Emitter`] handle that
/// [`RunDeps::emitter`] hands out (ADR-0025).
///
/// `complete` receives the fully-built [`LlmRequest`] (system, messages, tools,
/// no_think) - the shell renders it to wire JSON and calls the LLM boundary,
/// forwarding each streaming `StreamEvent` to `on_event`.
pub trait RunDeps: Send {
    /// Calls the model with the built request, forwarding every streaming delta
    /// snapshot to `on_event`, and yields the [`Response`] (the error algebra
    /// means this never "fails" - a failure is a `Response` with an `Error`
    /// stop reason).
    fn complete(
        &mut self,
        request: LlmRequest,
        on_event: &mut (dyn FnMut(&StreamEvent) + Send),
    ) -> impl Future<Output = Response> + Send;

    /// The Provenance of the Model this Run captured at spawn (ADR-0037,
    /// CONTEXT.md: Provenance): stamped on every assistant message a Response
    /// contributes to the Conversation. A fact of the capture, so it never
    /// changes mid-Run.
    fn provenance(&self) -> Provenance;

    /// Hands out the owned emission handle (ADR-0025). Emission alone is a
    /// handle rather than a trait method because of the borrow split: the
    /// streaming sink must emit MessageUpdate events WHILE `complete` holds the
    /// exclusive `&mut self` borrow, and a `deps.emit(..)` call inside the sink
    /// would be a second `&mut` borrow the compiler rightly rejects. The Loop
    /// calls this once at start and emits through the handle everywhere.
    fn emitter(&mut self) -> Emitter;

    /// Drains any queued Steering, yielding the texts in order.
    fn drain_steering(&mut self) -> impl Future<Output = Vec<String>> + Send;

    /// Requests the user's Approval for a command Tool Call; blocks until the
    /// user answers. `id` is the per-call reference (baud's `make_ref()`).
    fn request_approval(
        &mut self,
        id: String,
        command: String,
    ) -> impl Future<Output = bool> + Send;

    /// Checkpoints the Conversation (fire-and-forget). Called once per Tool Call
    /// batch with the answered calls; crash recency comes from the Session Log's
    /// per-event tool_result entries (ADR-0010), so this is the settlement
    /// fallback, not the durability path.
    fn checkpoint(&mut self, conversation: &Conversation);

    /// Stores the Plan text outside the Conversation (fire-and-forget). Called
    /// on a successful plan Tool Call with the model's verbatim Plan content.
    fn set_plan(&mut self, plan: String);

    /// The after-Pass control hook. Default: always [`AfterPass::Continue`].
    fn after_pass(
        &mut self,
        _response: &Response,
        _conversation: &Conversation,
    ) -> impl Future<Output = AfterPass> + Send {
        async { AfterPass::Continue }
    }

    /// Compacts the Conversation. Default: a no-op that reports no compactor
    /// (ADR-0012 - the real compaction effect is a later phase). A successful
    /// compaction returns the rewritten Conversation; an error falls through to
    /// the budget-exhaustion path.
    fn compact(
        &mut self,
        conversation: Conversation,
    ) -> impl Future<Output = Result<Conversation, CompactError>> + Send {
        async move {
            let _ = conversation;
            Err(CompactError("no_compactor".to_string()))
        }
    }
}
