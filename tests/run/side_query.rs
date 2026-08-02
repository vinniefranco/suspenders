use super::*;
use std::sync::Mutex;

use crate::llm::model::Api;
use crate::llm::response::{Response, StopReason};
use crate::test_support::{Entry, FakeLlm};

fn model() -> Model {
    Model::new("local", "m", Api::AnthropicMessages, 64_000, 100)
}

fn text_response(text: &str) -> Response {
    Response {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::EndTurn,
        usage: Default::default(),
        error: None,
    }
}

// A recording of one captured (request, model) pair - `Entry::dynamic` writes
// it as the side-query calls `complete` (FakeLlm records nothing itself).
type Captured = Arc<Mutex<Vec<(LlmRequest, Model)>>>;

// A `dynamic` entry that records the request+model it saw, then returns
// `reply`.
fn recording(captured: Captured, reply: Response) -> Entry {
    Entry::dynamic(vec![], move |req: &LlmRequest, model: &Model| {
        captured.lock().unwrap().push((req.clone(), model.clone()));
        reply.clone()
    })
}

// A side-query over a scripted FakeLlm, on the default main model.
fn side_query_over(entries: Vec<Entry>) -> LlmSideQuery {
    LlmSideQuery {
        llm: Arc::new(FakeLlm::script(entries)),
        model: model(),
        temperature: None,
    }
}

// Run a standard two-attempt request against the scripted side-query and
// surface the error - the shared body of the retry-exhaustion tests.
async fn err_after(entries: Vec<Entry>) -> String {
    side_query_over(entries)
        .run(SideQueryRequest {
            system: "sys".into(),
            user_content: "content".into(),
            model: None,
            max_attempts: 2,
        })
        .await
        .unwrap_err()
}

// The side-query runs the captured Llm with the request's system + single
// user part, NO tools, Thinking off - and returns the concatenated reply.
#[tokio::test]
async fn runs_the_llm_and_returns_the_reply() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeLlm::script(vec![recording(
        Arc::clone(&captured),
        text_response("extracted"),
    )]);
    let sq = LlmSideQuery {
        llm: Arc::new(fake),
        model: model(),
        temperature: Some(0.3),
    };

    let out = sq
        .run(SideQueryRequest {
            system: "extract it".into(),
            user_content: "the content".into(),
            model: None,
            max_attempts: 1,
        })
        .await
        .unwrap();
    assert_eq!(out, "extracted");

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let (req, seen_model) = &captured[0];
    assert_eq!(req.system, "extract it");
    assert!(req.tools.is_empty());
    assert!(req.no_think);
    assert_eq!(req.temperature, Some(0.3));
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(req.messages[0].role, crate::content::Role::User));
    assert!(
        matches!(&req.messages[0].content[0], ContentBlock::Text { text } if text == "the content")
    );
    // `None` on the request defaulted to the captured main Model.
    assert_eq!(seen_model.scoped_id(), model().scoped_id());
}

// An empty reply retries up to `max_attempts`; a later non-empty reply wins.
#[tokio::test]
async fn retries_on_empty_up_to_max_attempts_then_succeeds() {
    let fake = FakeLlm::script(vec![
        Entry::just(text_response("")),
        Entry::just(text_response("second")),
    ]);
    let sq = LlmSideQuery {
        llm: Arc::new(fake),
        model: model(),
        temperature: None,
    };

    let out = sq
        .run(SideQueryRequest {
            system: "sys".into(),
            user_content: "content".into(),
            model: None,
            max_attempts: 2,
        })
        .await
        .unwrap();
    assert_eq!(out, "second");
}

// Every attempt empty: the bounded loop gives up with an Err, never looping
// forever.
#[tokio::test]
async fn all_attempts_empty_errs() {
    let err = err_after(vec![
        Entry::just(text_response("")),
        Entry::just(text_response("")),
    ])
    .await;
    assert_eq!(err, "side query returned no text");
}

// An error Response retries too, then surfaces the last error string.
#[tokio::test]
async fn an_error_response_retries_then_surfaces_the_error() {
    let err = err_after(vec![Entry::error("boom"), Entry::error("boom again")]).await;
    assert_eq!(err, "boom again");
}

// A pinned Model on the request overrides the captured main Model.
#[tokio::test]
async fn a_pinned_request_model_overrides_the_captured_main_model() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeLlm::script(vec![recording(Arc::clone(&captured), text_response("ok"))]);
    let sq = LlmSideQuery {
        llm: Arc::new(fake),
        model: model(),
        temperature: None,
    };
    let pinned = Model::new("other", "fast", Api::OpenaiCompletions, 32_000, 100);

    sq.run(SideQueryRequest {
        system: "sys".into(),
        user_content: "content".into(),
        model: Some(pinned.clone()),
        max_attempts: 1,
    })
    .await
    .unwrap();

    assert_eq!(
        captured.lock().unwrap()[0].1.scoped_id(),
        pinned.scoped_id()
    );
}
