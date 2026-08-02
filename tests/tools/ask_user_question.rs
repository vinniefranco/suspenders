use super::*;
use crate::tool::caps::{Capabilities, Questioner};
use std::sync::Arc;

// A scripted Questioner that returns fixed picks and records the questions it
// received, standing in for the tx-backed AgentQuestioner.
struct ScriptedQuestioner {
    answers: Result<Vec<(usize, String)>, String>,
}

#[async_trait::async_trait]
impl Questioner for ScriptedQuestioner {
    async fn ask(&self, _questions: Vec<Question>) -> Result<Vec<(usize, String)>, String> {
        self.answers.clone()
    }
}

fn ctx_with(answers: Result<Vec<(usize, String)>, String>) -> ToolCtx {
    let caps = Capabilities::for_test_with_questioner(Arc::new(ScriptedQuestioner { answers }));
    let mut ctx = ToolCtx::for_test("/nowhere".into(), 100_000);
    ctx.caps = caps;
    ctx
}

// A ctx whose questioner is the degraded DecliningQuestioner (the headless
// posture), for the non-interactive path.
fn declining_ctx() -> ToolCtx {
    ToolCtx::for_test("/nowhere".into(), 100_000)
}

fn one_question() -> Value {
    json!({
        "questions": [{
            "question": "Which library should we use for date formatting?",
            "header": "Library",
            "options": [
                { "label": "chrono", "description": "the de-facto standard" },
                { "label": "time", "description": "leaner, no C deps" }
            ]
        }]
    })
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    AskUserQuestion.run(&input, ctx).await
}

#[test]
fn spec_is_ask_user_question_and_not_deferred() {
    let spec = AskUserQuestion.spec();
    assert_eq!(spec.name, "ask_user_question");
    // Always-visible (qwen shouldDefer:false): the model must see it on the
    // wire list, not discover it via tool_search.
    assert!(!AskUserQuestion.should_defer());
    assert!(!AskUserQuestion.always_load());
}

// --- validation messages (all VERBATIM from qwen validateToolParams) -----

#[tokio::test]
async fn questions_must_be_an_array() {
    let ctx = declining_ctx();
    let err = run(json!({ "questions": "nope" }), &ctx).await.unwrap_err();
    assert_eq!(err, "Parameter \"questions\" must be an array.");
}

#[tokio::test]
async fn between_one_and_four_questions() {
    let ctx = declining_ctx();
    let empty = run(json!({ "questions": [] }), &ctx).await.unwrap_err();
    assert_eq!(
        empty,
        "Parameter \"questions\" must contain between 1 and 4 questions."
    );
    // Five questions overshoots.
    let q = json!({
        "question": "q?",
        "header": "h",
        "options": [
            { "label": "a", "description": "d" },
            { "label": "b", "description": "d" }
        ]
    });
    let five = json!({ "questions": [q, q, q, q, q] });
    let err = run(five, &ctx).await.unwrap_err();
    assert_eq!(
        err,
        "Parameter \"questions\" must contain between 1 and 4 questions."
    );
}

#[tokio::test]
async fn question_text_must_be_non_empty() {
    let ctx = declining_ctx();
    let err = run(
        json!({ "questions": [{
                "question": "   ",
                "header": "h",
                "options": [
                    { "label": "a", "description": "d" },
                    { "label": "b", "description": "d" }
                ]
            }] }),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(err, "Question 1: \"question\" must be a non-empty string.");
}

#[tokio::test]
async fn header_must_be_non_empty() {
    let ctx = declining_ctx();
    let err = run(
        json!({ "questions": [{
                "question": "q?",
                "header": "",
                "options": [
                    { "label": "a", "description": "d" },
                    { "label": "b", "description": "d" }
                ]
            }] }),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(err, "Question 1: \"header\" must be a non-empty string.");
}

#[tokio::test]
async fn header_must_be_twelve_characters_or_less() {
    let ctx = declining_ctx();
    let err = run(
        json!({ "questions": [{
                "question": "q?",
                "header": "thirteen chrs",
                "options": [
                    { "label": "a", "description": "d" },
                    { "label": "b", "description": "d" }
                ]
            }] }),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(err, "Question 1: \"header\" must be 12 characters or less.");
}

#[tokio::test]
async fn options_must_be_two_to_four() {
    let ctx = declining_ctx();
    let one = run(
        json!({ "questions": [{
                "question": "q?",
                "header": "h",
                "options": [ { "label": "a", "description": "d" } ]
            }] }),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        one,
        "Question 1: \"options\" must contain between 2 and 4 options."
    );
}

#[tokio::test]
async fn option_label_and_description_must_be_non_empty() {
    let ctx = declining_ctx();
    let bad_label = run(
        json!({ "questions": [{
                "question": "q?",
                "header": "h",
                "options": [
                    { "label": "", "description": "d" },
                    { "label": "b", "description": "d" }
                ]
            }] }),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        bad_label,
        "Question 1, Option 1: \"label\" must be a non-empty string."
    );

    let bad_desc = run(
        json!({ "questions": [{
                "question": "q?",
                "header": "h",
                "options": [
                    { "label": "a", "description": "d" },
                    { "label": "b", "description": "  " }
                ]
            }] }),
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        bad_desc,
        "Question 1, Option 2: \"description\" must be a non-empty string."
    );
}

// --- answer formatting (VERBATIM qwen shape) -----------------------------

#[tokio::test]
async fn formats_the_answers_verbatim() {
    let ctx = ctx_with(Ok(vec![(0, "chrono".into())]));
    let out = run(one_question(), &ctx).await.unwrap();
    assert_eq!(
        out,
        "User has provided the following answers:\n\n**Library**: chrono"
    );
}

#[tokio::test]
async fn multiple_answers_join_by_newline() {
    let two = json!({
        "questions": [
            {
                "question": "Which library?",
                "header": "Library",
                "options": [
                    { "label": "chrono", "description": "d" },
                    { "label": "time", "description": "d" }
                ]
            },
            {
                "question": "Which runtime?",
                "header": "Runtime",
                "options": [
                    { "label": "tokio", "description": "d" },
                    { "label": "async-std", "description": "d" }
                ]
            }
        ]
    });
    let ctx = ctx_with(Ok(vec![(0, "chrono".into()), (1, "tokio".into())]));
    let out = run(two, &ctx).await.unwrap();
    assert_eq!(
        out,
        "User has provided the following answers:\n\n**Library**: chrono\n**Runtime**: tokio"
    );
}

// --- decline + degraded paths (VERBATIM strings) -------------------------

#[tokio::test]
async fn decline_returns_the_verbatim_decline_string() {
    let ctx = ctx_with(Err("User declined to answer the questions.".to_string()));
    let out = run(one_question(), &ctx).await.unwrap();
    assert_eq!(out, "User declined to answer the questions.");
}

#[tokio::test]
async fn degraded_headless_ctx_returns_the_verbatim_non_interactive_string() {
    // for_test seeds a DecliningQuestioner, so a headless/test host answers
    // with the VERBATIM non-interactive string as the tool's content.
    let ctx = declining_ctx();
    let out = run(one_question(), &ctx).await.unwrap();
    assert_eq!(
        out,
        "Cannot ask user questions in non-interactive mode without ACP support. \
             Please run in interactive mode or enable ACP mode to use this tool."
    );
}
