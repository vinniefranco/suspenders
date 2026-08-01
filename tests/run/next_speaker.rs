
use super::*;
use crate::content::Usage;
use crate::llm::response::StopReason;
use crate::run::fixtures::{FakeDeps, session};
use crate::test_support::{Entry, FakeLlm};
use tempfile::TempDir;

// A FakeDeps whose `complete` is scripted with the side-query replies (the
// check itself is the only caller of `complete` in these unit tests).
fn deps_scripting(replies: Vec<Response>) -> FakeDeps {
    let root = TempDir::new().unwrap();
    let session = session(root.path());
    let entries: Vec<Entry> = replies.into_iter().map(Entry::just).collect();
    FakeDeps::new(FakeLlm::script(entries), session.model.clone())
}

fn text(content: &str, stop: StopReason) -> Response {
    Response {
        content: vec![ContentBlock::text(content)],
        stop_reason: stop,
        usage: Usage::default(),
        error: None,
    }
}

// A response with nothing speakable - empty content, or only Thinking -
// continues WITHOUT any model call (the cheap short-circuit).
#[tokio::test]
async fn empty_response_continues_without_a_model_call() {
    let mut deps = deps_scripting(vec![]);
    let empty = Response {
        content: vec![],
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
        error: None,
    };
    let verdict = check_next_speaker(&mut deps, &empty).await;
    assert_eq!(verdict, NextSpeaker::Model);
    // No side-query was issued: the short-circuit spent no request.
    assert_eq!(deps.requests.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn thinking_only_response_continues_without_a_model_call() {
    let mut deps = deps_scripting(vec![]);
    let thinking_only = Response {
        content: vec![ContentBlock::Thinking {
            text: "let me think".into(),
        }],
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
        error: None,
    };
    let verdict = check_next_speaker(&mut deps, &thinking_only).await;
    assert_eq!(verdict, NextSpeaker::Model);
    assert_eq!(deps.requests.lock().unwrap().len(), 0);
}

// A textful reply issues the side-query; a {"next_speaker":"model"} reply
// means continue.
#[tokio::test]
async fn side_query_model_verdict_continues() {
    let mut deps = deps_scripting(vec![text(
        r#"{"next_speaker": "model"}"#,
        StopReason::EndTurn,
    )]);
    let reply = text("Next, I will read the file.", StopReason::EndTurn);
    let verdict = check_next_speaker(&mut deps, &reply).await;
    assert_eq!(verdict, NextSpeaker::Model);

    // The side-query carried the reply as the assistant turn, the
    // CHECK_PROMPT as the user turn, no tools, and Thinking disabled.
    let requests = deps.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert!(req.tools.is_empty());
    assert!(req.no_think);
    assert!(req.system.is_empty());
    assert_eq!(req.messages.len(), 2);
    assert!(matches!(
        req.messages[0].role,
        crate::content::Role::Assistant
    ));
    assert!(
        matches!(&req.messages[0].content[0], ContentBlock::Text { text } if text == "Next, I will read the file.")
    );
    assert!(matches!(req.messages[1].role, crate::content::Role::User));
    assert!(
        matches!(&req.messages[1].content[0], ContentBlock::Text { text } if text.contains("who should logically speak next"))
    );
}

#[tokio::test]
async fn side_query_user_verdict_ends() {
    let mut deps = deps_scripting(vec![text(
        r#"{"next_speaker": "user"}"#,
        StopReason::EndTurn,
    )]);
    let reply = text("All done. Anything else?", StopReason::EndTurn);
    let verdict = check_next_speaker(&mut deps, &reply).await;
    assert_eq!(verdict, NextSpeaker::User);
}

// An unparseable side-query reply defaults to ENDING the Run - never risk
// an infinite loop on a bad parse.
#[tokio::test]
async fn unparseable_side_query_reply_ends() {
    let mut deps = deps_scripting(vec![text(
        "I think the model should keep going, honestly.",
        StopReason::EndTurn,
    )]);
    let reply = text("Now I'll analyze the results.", StopReason::EndTurn);
    let verdict = check_next_speaker(&mut deps, &reply).await;
    assert_eq!(verdict, NextSpeaker::User);
}

// The lenient parser survives a reasoning preamble and extra prose around
// the JSON.
#[tokio::test]
async fn parser_tolerates_prose_around_the_json() {
    let mut deps = deps_scripting(vec![text(
        "The response announces a next action.\n{\"next_speaker\": \"model\"}\n",
        StopReason::EndTurn,
    )]);
    let reply = text("Moving on to analyze the output.", StopReason::EndTurn);
    assert_eq!(
        check_next_speaker(&mut deps, &reply).await,
        NextSpeaker::Model
    );
}
