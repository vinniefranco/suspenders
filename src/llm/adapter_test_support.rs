//! Shared assertion and execution helpers for LLM adapter integration tests.
//!
//! Both adapter test modules (anthropic_messages and openai_completions) test
//! the same error-algebra guarantees with adapter-specific setup (different
//! Provider api fields and wire paths). The assertions and common execution
//! patterns live here so each adapter calls rather than copies them.

use serde_json::Value;
use wiremock::MockServer;

use crate::content::ContentBlock;
use crate::llm::model::Model;
use crate::llm::response::{Response, StopReason};
use crate::llm::{Dispatcher, Llm, LlmRequest, StreamEvent};

/// Runs `req` through `dispatcher` collecting streaming events, returns them
/// together with the final Response.
pub async fn run_and_collect(
    dispatcher: Dispatcher,
    req: &LlmRequest,
    model: &Model,
) -> (Vec<StreamEvent>, Response) {
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut on_event = |ev: &StreamEvent| events.push(ev.clone());
    let result = dispatcher.complete(req, model, &mut on_event).await;
    (events, result)
}

/// Runs `req` through `dispatcher` with a no-op callback, returns the first
/// captured request body as parsed JSON so the caller can assert wire fields.
pub async fn body_for(
    dispatcher: Dispatcher,
    req: LlmRequest,
    model: &Model,
    server: &MockServer,
) -> Value {
    let mut no_op = |_ev: &StreamEvent| {};
    dispatcher.complete(&req, model, &mut no_op).await;
    let received = &server.received_requests().await.unwrap()[0];
    received.body_json().unwrap()
}

/// Asserts the error-algebra contract for a connection-refused scenario:
/// the stop reason is Error, the content is empty, and an error string is set.
pub fn assert_complete_error_response_empty(result: Response) {
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.content, Vec::<ContentBlock>::new());
    assert!(result.error.is_some());
}

/// Asserts the error-algebra contract for an HTTP 500 scenario:
/// the stop reason is Error and an error string is set.
pub fn assert_complete_is_error(result: Response) {
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result.error.is_some());
}

/// Asserts the list_models error contract: the result is an Err containing
/// the `request_failed` tag, matching the error algebra (ADR-0002 amendment).
pub fn assert_list_models_request_failed(result: Result<Vec<String>, String>) {
    assert!(result.is_err(), "expected Err, got {result:?}");
    assert!(result.unwrap_err().contains("request_failed"));
}
