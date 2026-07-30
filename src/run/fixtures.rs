//! Shared test fixtures for the split Loop's tests: Session/Conversation
//! builders, FakeLlm script entries, the `run_with` harness, and event
//! inspectors. Today `loop_`'s test module is the only consumer - `batch` and
//! `finish` are covered through the loop's integration tests - but any test
//! module they grow draws on this one fixture set instead of drifting copies.
//! `#[cfg(test)]`-gated in `run.rs`; never compiled into non-test builds.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tempfile::TempDir;

use crate::content::{ContentBlock, Message, Usage};
use crate::conversation::Conversation;
use crate::event::Event;
use crate::extensions::Registered;
use crate::llm::model::Model;
use crate::llm::response::{Response, StopReason};
use crate::llm::{Llm, LlmRequest, StreamEvent};
use crate::run::deps::{AfterPass, CompactError, Emitter, RunDeps};
use crate::run::loop_::{Outcome, OutcomeStop, RunEnv, RunOpts, run};
use crate::session::{Session, SessionConfig, SessionOpts};
use crate::test_support::{Entry, FakeLlm};
use crate::tool::ToolCtx;

pub(super) fn session_with(root: &std::path::Path, opts: SessionOpts) -> Session {
    let mut opts = opts;
    opts.root = Some(root.to_string_lossy().into_owned());
    Session::build(opts, &SessionConfig::test_defaults()).expect("session builds")
}

/// A Session on `root` with the Run Limit set to `limit` and every other fact
/// at its default - the common test preamble.
#[allow(clippy::field_reassign_with_default)] // matches loop_'s test module: names the knob
pub(super) fn session_with_limit(root: &std::path::Path, limit: u64) -> Session {
    let mut opts = SessionOpts::default();
    opts.run_limit = Some(limit);
    session_with(root, opts)
}

pub(super) fn session(root: &std::path::Path) -> Session {
    session_with(root, SessionOpts::default())
}

/// A Session on `root` with the next-speaker check ENABLED (the test defaults
/// turn it off; ADR-0043). The `run_limit` bounds any auto-continuation.
#[allow(clippy::field_reassign_with_default)] // matches loop_'s test module: names the knob
pub(super) fn session_next_speaker(root: &std::path::Path, run_limit: u64) -> Session {
    let mut opts = SessionOpts::default();
    opts.skip_next_speaker = Some(false);
    opts.run_limit = Some(run_limit);
    session_with(root, opts)
}

/// A side-query reply scripting the next-speaker verdict `speaker`
/// (`"user"` ends the Run, `"model"` continues it): a bare JSON object, the
/// lenient parser's happy path.
pub(super) fn next_speaker_verdict(speaker: &str) -> Response {
    text_end(&format!("{{\"next_speaker\": \"{speaker}\"}}"))
}

pub(super) fn conversation(session: &Session, prompt: &str) -> Conversation {
    let mut conv = Conversation::new(
        "You are a test agent.",
        crate::conversation::ConversationOpts::new(
            session.context_budget_for(&session.model),
            session.model.max_tokens,
        )
        .compaction_slack(session.compaction_slack)
        .compaction_keep(session.compaction_keep),
    );
    conv.add_user_text(prompt);
    conv
}

pub(super) fn tool_ctx(session: &Session) -> ToolCtx {
    session.tool_ctx(&session.model, crate::tool::caps::Capabilities::for_test())
}

// Response builders mirroring baud's text_result / tool_use_result.
pub(super) fn text_result(text: &str, stop: StopReason) -> Response {
    Response {
        content: vec![ContentBlock::text(text)],
        stop_reason: stop,
        usage: Usage::default(),
        error: None,
    }
}

pub(super) fn text_end(text: &str) -> Response {
    text_result(text, StopReason::EndTurn)
}

pub(super) fn tool_use_result(id: &str, name: &str, input: Value) -> Response {
    Response {
        content: vec![ContentBlock::tool_use(id, name, input)],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        error: None,
    }
}

pub(super) fn empty(stop: StopReason) -> Response {
    Response {
        content: vec![],
        stop_reason: stop,
        usage: Usage::default(),
        error: None,
    }
}

pub(super) fn just(r: Response) -> Entry {
    Entry::just(r)
}

// Runs the loop to completion with the given script and (optional) deps
// customization, on a fresh temp root. Returns (outcome, deps) so the test
// can inspect recorded events/checkpoints/requests/plans.
pub(super) async fn run_with(
    session: &Session,
    prompt: &str,
    mut deps: FakeDeps,
) -> (Outcome, FakeDeps) {
    let conv = conversation(session, prompt);
    let extensions: Vec<Registered> = Vec::new();
    let ctx = tool_ctx(session);
    let outcome = run(
        conv,
        session,
        RunEnv {
            extensions: &extensions,
            tool_ctx: &ctx,
        },
        &mut deps,
        RunOpts::default(),
    )
    .await;
    (outcome, deps)
}

pub(super) fn deps_for(session: &Session, entries: Vec<Entry>) -> FakeDeps {
    FakeDeps::new(FakeLlm::script(entries), session.model.clone())
}

// Inspectors over recorded events.
pub(super) fn events(deps: &FakeDeps) -> Vec<Event> {
    deps.events.lock().unwrap().clone()
}

pub(super) fn find_tool_result<'a>(evs: &'a [Event], id: &str) -> Option<&'a Event> {
    evs.iter()
        .find(|e| matches!(e, Event::ToolResult { id: i, .. } if i == id))
}

pub(super) fn count_voiced(evs: &[Event], f: impl Fn(&Event) -> bool) -> usize {
    evs.iter().filter(|e| f(e)).count()
}

pub(super) fn last_message(conv: &Conversation) -> &Message {
    conv.messages.last().expect("has a message")
}

pub(super) fn ok(outcome: &Outcome) -> (&Conversation, &OutcomeStop) {
    match outcome {
        Outcome::Ok(c, s) => (c, s),
        other => panic!("expected Ok, got {other:?}"),
    }
}

// A test root.
pub(super) fn root() -> TempDir {
    TempDir::new().unwrap()
}

pub(super) fn write(root: &TempDir, name: &str, content: &str) {
    std::fs::write(root.path().join(name), content).unwrap();
}

// ---------------------------------------------------------------------------
// FakeDeps - the Run loop's dependency bundle for tests (baud's loop_test fake
// Deps). Records emitted events, checkpoints, set_plan calls, and each built
// request into shared handles the test inspects; scripts `complete` through an
// owned [`FakeLlm`]; answers Approvals from a canned per-call queue OR an
// approval channel (mid-flight); yields canned Steering batches; and lets a
// test override `after_pass`/`compact` (including a panicking/erroring variant
// to prove they are control-bearing and fail-loud, ADR-0011).
// ---------------------------------------------------------------------------

/// An override closure for the `after_pass` hook.
pub(super) type AfterPassFn = Box<dyn FnMut(&Response, &Conversation) -> AfterPass + Send>;

/// An override closure for the `compact` Dep.
pub(super) type CompactFn =
    Box<dyn FnMut(Conversation) -> Result<Conversation, CompactError> + Send>;

/// One Approval request as recorded / forwarded: the per-call id and the
/// command the user is asked to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApprovalAsk {
    pub id: String,
    pub command: String,
}

/// The Run loop test double. All recording handles are `Arc<Mutex<..>>` so a
/// test can clone a handle before the run consumes `&mut deps` and inspect it
/// after (or concurrently, from a spawned answerer).
pub(super) struct FakeDeps {
    llm: FakeLlm,
    model: Model,

    /// Every emitted [`Event`], in order.
    pub events: Arc<Mutex<Vec<Event>>>,
    /// Every checkpointed Conversation, in order.
    pub checkpoints: Arc<Mutex<Vec<Conversation>>>,
    /// Every `set_plan` payload, in order.
    pub plans: Arc<Mutex<Vec<String>>>,
    /// Every request handed to `complete`, in order (inspect `.tools`,
    /// `.messages`, `.no_think`).
    pub requests: Arc<Mutex<Vec<LlmRequest>>>,

    /// Canned per-call Approval answers, popped front-to-back. Exhausted ⇒
    /// denied (false).
    approvals: Arc<Mutex<VecDeque<bool>>>,
    /// Optional mid-flight approval channel: the ask goes out here and the
    /// answer comes back on the paired oneshot. When set, it takes precedence
    /// over the canned queue.
    approval_tx: Option<
        tokio::sync::mpsc::UnboundedSender<(ApprovalAsk, tokio::sync::oneshot::Sender<bool>)>,
    >,

    /// Canned Steering batches, one popped per `drain_steering`. Exhausted ⇒
    /// empty.
    steering: Arc<Mutex<VecDeque<Vec<String>>>>,

    after_pass: Option<AfterPassFn>,
    compact: Option<CompactFn>,
}

impl FakeDeps {
    /// A FakeDeps scripting `complete` from `llm`, with no approvals, steering,
    /// or overrides.
    pub fn new(llm: FakeLlm, model: Model) -> Self {
        FakeDeps {
            llm,
            model,
            events: Arc::new(Mutex::new(Vec::new())),
            checkpoints: Arc::new(Mutex::new(Vec::new())),
            plans: Arc::new(Mutex::new(Vec::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            approvals: Arc::new(Mutex::new(VecDeque::new())),
            approval_tx: None,
            steering: Arc::new(Mutex::new(VecDeque::new())),
            after_pass: None,
            compact: None,
        }
    }

    /// Seeds the canned Approval answers (popped one per `request_approval`).
    pub fn with_approvals(mut self, answers: impl IntoIterator<Item = bool>) -> Self {
        self.approvals = Arc::new(Mutex::new(answers.into_iter().collect()));
        self
    }

    /// Seeds the canned Steering batches (one popped per `drain_steering`).
    pub fn with_steering(mut self, batches: impl IntoIterator<Item = Vec<String>>) -> Self {
        self.steering = Arc::new(Mutex::new(batches.into_iter().collect()));
        self
    }

    /// Overrides the after-Pass hook.
    pub fn with_after_pass(
        mut self,
        f: impl FnMut(&Response, &Conversation) -> AfterPass + Send + 'static,
    ) -> Self {
        self.after_pass = Some(Box::new(f));
        self
    }

    /// Overrides the compact Dep.
    pub fn with_compact(
        mut self,
        f: impl FnMut(Conversation) -> Result<Conversation, CompactError> + Send + 'static,
    ) -> Self {
        self.compact = Some(Box::new(f));
        self
    }

    /// Installs a mid-flight approval channel and returns the receiver. A test
    /// spawns a task that receives `(ask, reply)` and answers via `reply`,
    /// letting the run block in `request_approval` until answered. The mid-flight
    /// handshake seam, mirroring [`FakeLlm`]'s barrier entry; unused today but
    /// wired end to end (`approval_tx` -> `request_approval`).
    #[allow(dead_code)] // retained seam: the only setter of `approval_tx`
    pub fn approval_channel(
        &mut self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<(ApprovalAsk, tokio::sync::oneshot::Sender<bool>)>
    {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.approval_tx = Some(tx);
        rx
    }
}

impl RunDeps for FakeDeps {
    async fn complete(
        &mut self,
        req: LlmRequest,
        on_event: &mut (dyn FnMut(&StreamEvent) + Send),
    ) -> Response {
        self.requests.lock().unwrap().push(req.clone());
        // `FakeLlm::complete` wants an `OnEvent` (FnMut); adapt the &dyn.
        let mut adapter = |ev: &StreamEvent| on_event(ev);
        self.llm.complete(&req, &self.model, &mut adapter).await
    }

    fn provenance(&self) -> crate::content::Provenance {
        self.model.provenance()
    }

    fn emitter(&mut self) -> Emitter {
        // The handle shares the SAME `Arc<Mutex<Vec<Event>>>` the fake records
        // into directly (approval request/resolved below), so a test sees one
        // ordered log regardless of which path emitted (ADR-0025) - and the
        // existing `deps.events` inspectors keep working unchanged.
        let events = Arc::clone(&self.events);
        Emitter::new(move |event| events.lock().unwrap().push(event))
    }

    async fn drain_steering(&mut self) -> Vec<String> {
        self.steering
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
    }

    async fn request_approval(&mut self, id: String, command: String) -> bool {
        // The shell (and baud's fake) emit the request/resolved events around
        // the block; mirror that so message-grammar assertions hold.
        self.events
            .lock()
            .unwrap()
            .push(Event::approval_request(id.clone(), command.clone()));

        let approved = if let Some(tx) = &self.approval_tx {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let ask = ApprovalAsk {
                id: id.clone(),
                command,
            };
            // If the receiver is gone, fall back to denial.
            if tx.send((ask, reply_tx)).is_err() {
                false
            } else {
                reply_rx.await.unwrap_or(false)
            }
        } else {
            self.approvals.lock().unwrap().pop_front().unwrap_or(false)
        };

        self.events
            .lock()
            .unwrap()
            .push(Event::approval_resolved(id, approved));
        approved
    }

    fn checkpoint(&mut self, conversation: &Conversation) {
        self.checkpoints.lock().unwrap().push(conversation.clone());
    }

    fn set_plan(&mut self, plan: String) {
        self.plans.lock().unwrap().push(plan);
    }

    async fn after_pass(&mut self, response: &Response, conversation: &Conversation) -> AfterPass {
        match &mut self.after_pass {
            Some(f) => f(response, conversation),
            None => AfterPass::Continue,
        }
    }

    async fn compact(&mut self, conversation: Conversation) -> Result<Conversation, CompactError> {
        match &mut self.compact {
            Some(f) => f(conversation),
            None => Err(CompactError("no_compactor".to_string())),
        }
    }
}
