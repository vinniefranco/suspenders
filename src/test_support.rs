//! Shared test fakes (ADR-0020, ADR-0021), gated `#[cfg(test)]` so other
//! modules' tests can drive the LLM boundary without a network.
//!
//! [`FakeLlm`] owns its OWN script queue (`Arc<Mutex<VecDeque<Entry>>>`)
//! supplied per instance - NOT a global registry - so the suite runs in
//! parallel with no shared mutable state (ADR-0020).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::llm::model::{Api, Model};
use crate::llm::provider::Provider;
use crate::llm::response::{Response, StopReason};
use crate::llm::{Delta, Llm, LlmRequest, OnEvent, StreamEvent};

/// A callback that inspects the outgoing typed request (and the Model it is
/// bound for) and produces a [`Response`]. The extension point for
/// request-inspecting / barrier-blocking entries (busy/cancel handshakes):
/// a closure sees the request and may block.
pub type ResponseFn = Box<dyn Fn(&LlmRequest, &Model) -> Response + Send + Sync>;

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
/// is entered: the typed request and its Model (so the test can inspect them,
/// e.g. Rollover's carried prompt or the captured Active Model) and the
/// oneshot the test answers with a [`Release`] to unpark the call. Dropping
/// the oneshot without answering leaves `complete` parked forever - which is
/// exactly what `JoinHandle::abort()` cancels for the cancellation tests.
///
/// [`Barrier`]: Entry::Barrier
pub struct InFlight {
    pub request: LlmRequest,
    pub model: Model,
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
    Dynamic {
        deltas: Vec<Delta>,
        respond: ResponseFn,
    },
    /// The tokio analog of baud's blocking script closure: on `complete`, sends
    /// an [`InFlight`] (the request + a release oneshot) to `signal` so the test
    /// observes the Run parked mid-call, then awaits the [`Release`]. The test
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

    /// A request-inspecting closure entry.
    pub fn dynamic(
        deltas: Vec<Delta>,
        respond: impl Fn(&LlmRequest, &Model) -> Response + Send + Sync + 'static,
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

/// One scripted `list_models` answer: `Ok(ids)` or `Err(reason)`, mirroring the
/// boundary's `Result<Vec<String>, String>`.
pub type ModelsResult = Result<Vec<String>, String>;

/// A per-instance scripted LLM. Pops one [`Entry`] per `complete` call and one
/// [`ModelsResult`] per `list_models` call.
#[derive(Clone)]
pub struct FakeLlm {
    script: Arc<Mutex<VecDeque<Entry>>>,
    /// Scripted `list_models` answers, popped front-to-back. Its own queue -
    /// `list_models` is a discrete query, not part of the `complete` script.
    /// Exhausted ⇒ `Ok(vec![])`.
    models: Arc<Mutex<VecDeque<ModelsResult>>>,
}

impl FakeLlm {
    /// Builds a fake from a script of entries, consumed front-to-back.
    pub fn script(entries: impl IntoIterator<Item = Entry>) -> Self {
        FakeLlm {
            script: Arc::new(Mutex::new(entries.into_iter().collect())),
            models: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Seeds the scripted `list_models` answers (one popped per call), each
    /// either an `Ok(ids)` or an `Err(reason)`.
    pub fn with_models(self, answers: impl IntoIterator<Item = ModelsResult>) -> Self {
        *self.models.lock().unwrap() = answers.into_iter().collect();
        self
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
        request: &LlmRequest,
        model: &Model,
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
                respond(request, model)
            }
            Some(Entry::Barrier { signal }) => {
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                // Tell the test we are parked mid-call (with the request it may
                // inspect). If the receiver is already gone, fall through to an
                // error Response rather than blocking on nobody.
                if signal
                    .send(InFlight {
                        request: request.clone(),
                        model: model.clone(),
                        release: release_tx,
                    })
                    .is_err()
                {
                    return Response::error("fake_llm: barrier signal receiver dropped");
                }
                // Park until the test releases us - or forever, so an abort()
                // cancels the Run task exactly at this await.
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

    async fn list_models(&self, _provider: &Provider) -> Result<Vec<String>, String> {
        // An unseeded queue answers with an empty list (the benign default);
        // scripted entries drive the Ok/Err paths tests want to exercise.
        self.models
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;

    fn model() -> Model {
        Model::new("local", "m", Api::AnthropicMessages, 64_000, 100)
    }

    fn request() -> LlmRequest {
        LlmRequest::new("s", vec![], vec![])
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
        let r = fake.complete(&request(), &model(), &mut on_event).await;

        assert_eq!(deltas, vec![Delta::Text("hi".into())]);
        assert_eq!(r.content, vec![ContentBlock::text("hi")]);
    }

    #[tokio::test]
    async fn error_entry_becomes_error_response() {
        let fake = FakeLlm::script(vec![Entry::error("nope")]);
        let mut on_event = |_ev: &StreamEvent| {};
        let r = fake.complete(&request(), &model(), &mut on_event).await;
        assert_eq!(r.stop_reason, StopReason::Error);
        assert_eq!(r.error.as_deref(), Some("nope"));
    }

    #[tokio::test]
    async fn dynamic_entry_inspects_the_typed_request_and_model() {
        let fake = FakeLlm::script(vec![Entry::dynamic(
            vec![],
            |req: &LlmRequest, model: &Model| {
                let line = format!("{}@{}", req.system, model.scoped_id());
                Response {
                    content: vec![ContentBlock::text(line)],
                    stop_reason: StopReason::EndTurn,
                    usage: Default::default(),
                    error: None,
                }
            },
        )]);
        let mut on_event = |_ev: &StreamEvent| {};
        let r = fake.complete(&request(), &model(), &mut on_event).await;
        assert_eq!(r.content, vec![ContentBlock::text("s@local/m")]);
    }
}
