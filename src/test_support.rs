//! Shared test fakes (ADR-0020, ADR-0021), gated `#[cfg(test)]` so other
//! modules' tests can drive the LLM boundary without a network.
//!
//! [`FakeLlm`] owns its OWN script queue (`Arc<Mutex<VecDeque<Entry>>>`)
//! supplied per instance - NOT a global registry - so the suite runs in
//! parallel with no shared mutable state (ADR-0020).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::request::{self, LlmRequest};
use crate::llm::response::{Response, StopReason};
use crate::llm::stream::{Delta, StreamEvent};
use crate::llm::{Llm, OnEvent};
use crate::session::connection::Connection;
use crate::turn::deps::{AfterPass, CompactError, Emitter, TurnDeps};

/// A callback that inspects the outgoing request and produces a [`Response`].
/// The extension point for Phase 5 request-inspecting / barrier-blocking
/// entries (busy/cancel handshakes): a closure sees the request and may block.
pub type ResponseFn = Box<dyn Fn(&Value) -> Response + Send + Sync>;

/// The release a [`Barrier`] entry awaits: the deltas to fire and the Response
/// to return once the test lets the parked `complete` proceed. Sent back over
/// the oneshot the entry hands to the test.
///
/// [`Barrier`]: Entry::Barrier
pub struct Release {
    pub deltas: Vec<Delta>,
    pub response: Response,
}

/// The in-flight signal a [`Barrier`] entry sends the test the moment `complete`
/// is entered: the built request (so the test can inspect it, e.g. Rollover's
/// carried prompt) and the oneshot the test answers with a [`Release`] to
/// unpark the call. Dropping the oneshot without answering leaves `complete`
/// parked forever - which is exactly what `JoinHandle::abort()` cancels for the
/// cancellation tests.
///
/// [`Barrier`]: Entry::Barrier
pub struct InFlight {
    pub request: Value,
    pub release: tokio::sync::oneshot::Sender<Release>,
}

/// One scripted interaction. Designed to grow: today a canned response (with
/// optional deltas) or an error; tomorrow a request-inspecting closure that
/// blocks on a barrier - the [`Entry::Dynamic`] variant is that clean seam.
pub enum Entry {
    /// Fire `deltas` through `on_event` (each with the accumulated snapshot),
    /// then return `response`.
    Response {
        deltas: Vec<Delta>,
        response: Response,
    },
    /// Normalized to a [`Response`] with an `Error` stop_reason (honoring the
    /// must-not-fail error algebra).
    Error { reason: String },
    /// A closure that inspects the request and returns a Response. May block
    /// (e.g. on a barrier) to drive busy/cancel handshakes.
    Dynamic { deltas: Vec<Delta>, respond: ResponseFn },
    /// The tokio analog of baud's blocking script closure: on `complete`, sends
    /// an [`InFlight`] (the request + a release oneshot) to `signal` so the test
    /// observes the Turn parked mid-call, then awaits the [`Release`]. The test
    /// answers to unpark it (busy / steer-while-running / streaming handshakes),
    /// or never answers so `JoinHandle::abort()` cancels the call at this await
    /// (the cancellation tests).
    Barrier {
        signal: tokio::sync::mpsc::UnboundedSender<InFlight>,
    },
}

impl Entry {
    /// A scripted response with the given deltas fired first.
    pub fn response(deltas: Vec<Delta>, response: Response) -> Self {
        Entry::Response { deltas, response }
    }

    /// A scripted response with no intermediate deltas.
    pub fn just(response: Response) -> Self {
        Entry::Response {
            deltas: Vec::new(),
            response,
        }
    }

    /// An error entry, normalized to an `Error` Response.
    pub fn error(reason: impl Into<String>) -> Self {
        Entry::Error {
            reason: reason.into(),
        }
    }

    /// A request-inspecting closure entry (Phase 5 extension point).
    pub fn dynamic(
        deltas: Vec<Delta>,
        respond: impl Fn(&Value) -> Response + Send + Sync + 'static,
    ) -> Self {
        Entry::Dynamic {
            deltas,
            respond: Box::new(respond),
        }
    }

    /// A barrier entry paired with the receiver the test watches. The test
    /// recvs an [`InFlight`] once `complete` parks, then answers its `release`
    /// oneshot with a [`Release`] to unpark - or drops it / never recvs, so an
    /// `abort()` cancels the parked call (the cancellation tests).
    pub fn barrier() -> (Self, tokio::sync::mpsc::UnboundedReceiver<InFlight>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Entry::Barrier { signal: tx }, rx)
    }
}

/// A per-instance scripted LLM. Pops one [`Entry`] per `complete` call.
#[derive(Clone)]
pub struct FakeLlm {
    script: Arc<Mutex<VecDeque<Entry>>>,
}

impl FakeLlm {
    /// Builds a fake from a script of entries, consumed front-to-back.
    pub fn script(entries: impl IntoIterator<Item = Entry>) -> Self {
        FakeLlm {
            script: Arc::new(Mutex::new(entries.into_iter().collect())),
        }
    }

    /// Fires `deltas` through `on_event`, each carrying a snapshot of the text
    /// accumulated so far (thinking rendered but never in the final content).
    fn fire_deltas(deltas: &[Delta], on_event: &mut OnEvent<'_>) {
        use crate::content::ContentBlock;
        let mut text = String::new();
        let mut thinking = String::new();
        for delta in deltas {
            match delta {
                Delta::Text(s) => text.push_str(s),
                Delta::Thinking(s) => thinking.push_str(s),
            }
            let mut content = Vec::new();
            if !thinking.is_empty() {
                content.push(ContentBlock::Thinking {
                    text: thinking.clone(),
                });
            }
            if !text.is_empty() {
                content.push(ContentBlock::Text { text: text.clone() });
            }
            on_event(&StreamEvent {
                delta: delta.clone(),
                content,
            });
        }
    }
}

#[async_trait]
impl Llm for FakeLlm {
    async fn complete(
        &self,
        request: Value,
        _connection: &Connection,
        on_event: &mut OnEvent<'_>,
    ) -> Response {
        let entry = self.script.lock().unwrap().pop_front();
        match entry {
            Some(Entry::Response { deltas, response }) => {
                Self::fire_deltas(&deltas, on_event);
                response
            }
            Some(Entry::Error { reason }) => Response::error(reason),
            Some(Entry::Dynamic { deltas, respond }) => {
                Self::fire_deltas(&deltas, on_event);
                respond(&request)
            }
            Some(Entry::Barrier { signal }) => {
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                // Tell the test we are parked mid-call (with the request it may
                // inspect). If the receiver is already gone, fall through to an
                // error Response rather than blocking on nobody.
                if signal
                    .send(InFlight {
                        request,
                        release: release_tx,
                    })
                    .is_err()
                {
                    return Response::error("fake_llm: barrier signal receiver dropped");
                }
                // Park until the test releases us - or forever, so an abort()
                // cancels the Turn task exactly at this await.
                match release_rx.await {
                    Ok(Release { deltas, response }) => {
                        Self::fire_deltas(&deltas, on_event);
                        response
                    }
                    // The test dropped the release without answering.
                    Err(_) => Response::error("fake_llm: barrier released without a response"),
                }
            }
            // An empty script is a test bug; surface it as an error Response
            // rather than panicking (the boundary must not fail).
            None => Response::error("fake_llm: script exhausted"),
        }
    }
}

// ---------------------------------------------------------------------------
// FakeDeps - the Turn loop's dependency bundle for tests (baud's loop_test fake
// Deps). Records emitted events, checkpoints, set_plan calls, and each built
// request into shared handles the test inspects; scripts `complete` through an
// owned [`FakeLlm`]; answers Approvals from a canned per-call queue OR an
// approval channel (mid-flight); yields canned Steering batches; and lets a
// test override `after_pass`/`compact` (including a panicking/erroring variant
// to prove they are control-bearing and fail-loud, ADR-0011).
// ---------------------------------------------------------------------------

/// An override closure for the `after_pass` hook.
pub type AfterPassFn = Box<dyn FnMut(&Response, &Conversation) -> AfterPass + Send>;

/// An override closure for the `compact` Dep.
pub type CompactFn = Box<dyn FnMut(Conversation) -> Result<Conversation, CompactError> + Send>;

/// One Approval request as recorded / forwarded: the per-call id and the
/// command the user is asked to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalAsk {
    pub id: String,
    pub command: String,
}

/// The Turn loop test double. All recording handles are `Arc<Mutex<..>>` so a
/// test can clone a handle before the run consumes `&mut deps` and inspect it
/// after (or concurrently, from a spawned answerer).
pub struct FakeDeps {
    llm: FakeLlm,
    connection: Connection,

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
    approval_tx: Option<tokio::sync::mpsc::UnboundedSender<(ApprovalAsk, tokio::sync::oneshot::Sender<bool>)>>,

    /// Canned Steering batches, one popped per `drain_steering`. Exhausted ⇒
    /// empty.
    steering: Arc<Mutex<VecDeque<Vec<String>>>>,

    after_pass: Option<AfterPassFn>,
    compact: Option<CompactFn>,
}

impl FakeDeps {
    /// A FakeDeps scripting `complete` from `llm`, with no approvals, steering,
    /// or overrides.
    pub fn new(llm: FakeLlm, connection: Connection) -> Self {
        FakeDeps {
            llm,
            connection,
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
    /// letting the run block in `request_approval` until answered.
    pub fn approval_channel(
        &mut self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<(ApprovalAsk, tokio::sync::oneshot::Sender<bool>)> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.approval_tx = Some(tx);
        rx
    }

    // ---- convenience inspectors (clone the handles) ----

    pub fn events_handle(&self) -> Arc<Mutex<Vec<Event>>> {
        Arc::clone(&self.events)
    }
    pub fn checkpoints_handle(&self) -> Arc<Mutex<Vec<Conversation>>> {
        Arc::clone(&self.checkpoints)
    }
    pub fn plans_handle(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.plans)
    }
    pub fn requests_handle(&self) -> Arc<Mutex<Vec<LlmRequest>>> {
        Arc::clone(&self.requests)
    }
}

impl TurnDeps for FakeDeps {
    async fn complete(
        &mut self,
        req: LlmRequest,
        on_event: &mut (dyn FnMut(&StreamEvent) + Send),
    ) -> Response {
        self.requests.lock().unwrap().push(req.clone());
        // Render to wire JSON so a Dynamic entry can still inspect the request.
        let wire = request::build_request(&req, &self.connection);
        // `FakeLlm::complete` wants an `OnEvent` (FnMut); adapt the &dyn.
        let mut adapter = |ev: &StreamEvent| on_event(ev);
        self.llm.complete(wire, &self.connection, &mut adapter).await
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
        self.steering.lock().unwrap().pop_front().unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use serde_json::json;

    fn conn() -> Connection {
        Connection::new("http://x/v1", "t", "m", 100)
    }

    #[tokio::test]
    async fn response_entry_fires_deltas_and_returns() {
        let fake = FakeLlm::script(vec![Entry::response(
            vec![Delta::Text("hi".into())],
            Response {
                content: vec![ContentBlock::text("hi")],
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
                error: None,
            },
        )]);

        let mut deltas = Vec::new();
        let mut on_event = |ev: &StreamEvent| deltas.push(ev.delta.clone());
        let r = fake.complete(json!({}), &conn(), &mut on_event).await;

        assert_eq!(deltas, vec![Delta::Text("hi".into())]);
        assert_eq!(r.content, vec![ContentBlock::text("hi")]);
    }

    #[tokio::test]
    async fn error_entry_becomes_error_response() {
        let fake = FakeLlm::script(vec![Entry::error("nope")]);
        let mut on_event = |_ev: &StreamEvent| {};
        let r = fake.complete(json!({}), &conn(), &mut on_event).await;
        assert_eq!(r.stop_reason, StopReason::Error);
        assert_eq!(r.error.as_deref(), Some("nope"));
    }

    #[tokio::test]
    async fn dynamic_entry_inspects_request() {
        let fake = FakeLlm::script(vec![Entry::dynamic(vec![], |req: &Value| {
            let model = req["model"].as_str().unwrap_or("?").to_string();
            Response {
                content: vec![ContentBlock::text(model)],
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
                error: None,
            }
        })]);
        let mut on_event = |_ev: &StreamEvent| {};
        let r = fake
            .complete(json!({ "model": "m9" }), &conn(), &mut on_event)
            .await;
        assert_eq!(r.content, vec![ContentBlock::text("m9")]);
    }
}
