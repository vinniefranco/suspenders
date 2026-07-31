//! `ask_user_question(questions)`: puts one or more structured questions to the
//! user mid-run and reads back their picks (faithful to qwen askUserQuestion.ts).
//! The model reaches for this when it needs to gather a preference, clarify an
//! ambiguous instruction, or choose between implementation directions - the
//! structured clarification UX instead of asking in plain prose.
//!
//! Unlike a gated command (which passes the Approval gate), a question is NOT an
//! approval: it always opens a modal - there is no Standing-Approval / auto path.
//! The tool validates the questions (1-4 questions, each header <= 12 chars, 2-4
//! options, non-empty strings) BEFORE reaching the [`crate::tool::caps::Questioner`]
//! capability, because suspenders' schema-level `tool::validate` only checks
//! required/unknown/string-type - the shape rules qwen enforces in
//! `validateToolParams` are re-implemented here in `run`, every message VERBATIM.
//!
//! The capability yields the user's `(question_index, answer_value)` picks, which
//! this formats VERBATIM as qwen does (`**{header}**: {value}` joined by newlines,
//! wrapped in `User has provided the following answers:\n\n...`). A headless/test
//! host answers through [`crate::tool::caps::DecliningQuestioner`], which returns
//! the VERBATIM non-interactive string; a user cancel returns the VERBATIM decline
//! string. The tool is kept ALWAYS-VISIBLE (never deferred, qwen `shouldDefer:
//! false`) so the model reaches for the structured UX instead of asking in prose.

use crate::tool::caps::{Question, QuestionOption};
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

pub struct AskUserQuestion;

/// The most questions one call may put to the user (qwen's `maxItems: 4`).
const MAX_QUESTIONS: usize = 4;
/// The longest a question's `header` chip may be (qwen's `max 12 chars`).
const MAX_HEADER_CHARS: usize = 12;
/// The fewest options a question must offer (qwen's `minItems: 2`).
const MIN_OPTIONS: usize = 2;
/// The most options a question may offer (qwen's `maxItems: 4`).
const MAX_OPTIONS: usize = 4;

/// The tool description, VERBATIM from qwen askUserQuestion.ts.
const DESCRIPTION: &str = "Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.

Usage notes:
- Users will always be able to select \"Other\" to provide custom text input
- Use multiSelect: true to allow multiple answers to be selected for a question
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label

Plan mode note: In plan mode, use this tool to clarify requirements or choose between approaches BEFORE finalizing your plan. Do NOT use this tool to ask \"Is this plan ready?\" or \"Should I proceed?\" - use ExitPlanMode for plan approval.
";

#[async_trait::async_trait]
impl Tool for AskUserQuestion {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user_question".into(),
            description: DESCRIPTION.into(),
            // Schema VERBATIM from qwen askUserQuestion.ts: questions array
            // (minItems 1, maxItems 4); each {question, header (<= 12 chars),
            // options (minItems 2, maxItems 4) {label, description}, multiSelect
            // (default false)}. The optional `metadata.source` is accepted and
            // ignored (analytics-only in qwen).
            input_schema: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "description": "Questions to ask the user (1-4 questions)",
                        "minItems": 1,
                        "maxItems": 4,
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "description": "The complete question to ask the user. Should be clear, specific, and end with a question mark. Example: \"Which library should we use for date formatting?\" If multiSelect is true, phrase it accordingly, e.g. \"Which features do you want to enable?\"",
                                    "type": "string"
                                },
                                "header": {
                                    "description": "Very short label displayed as a chip/tag (max 12 chars). Examples: \"Auth method\", \"Library\", \"Approach\".",
                                    "type": "string"
                                },
                                "options": {
                                    "description": "The available choices for this question. Must have 2-4 options. Each option should be a distinct, mutually exclusive choice (unless multiSelect is enabled). There should be no 'Other' option, that will be provided automatically.",
                                    "minItems": 2,
                                    "maxItems": 4,
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "description": "The display text for this option that the user will see and select. Should be concise (1-5 words) and clearly describe the choice.",
                                                "type": "string"
                                            },
                                            "description": {
                                                "description": "Explanation of what this option means or what will happen if chosen. Useful for providing context about trade-offs or implications.",
                                                "type": "string"
                                            }
                                        },
                                        "required": ["label", "description"],
                                        "additionalProperties": false
                                    }
                                },
                                "multiSelect": {
                                    "description": "Set to true to allow the user to select multiple options instead of just one. Use when choices are not mutually exclusive.",
                                    "default": false,
                                    "type": "boolean"
                                }
                            },
                            "required": ["question", "header", "options"],
                            "additionalProperties": false
                        }
                    },
                    "metadata": {
                        "description": "Optional metadata for tracking and analytics purposes. Not displayed to user.",
                        "type": "object",
                        "properties": {
                            "source": {
                                "description": "Optional identifier for the source of this question (e.g., \"remember\" for /remember command). Used for analytics tracking.",
                                "type": "string"
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // TOOL-LEVEL validation (qwen `validateToolParams`): suspenders'
        // schema-level `tool::validate` only checks required/unknown/string-type,
        // so the shape rules qwen enforces are re-implemented here, every message
        // VERBATIM. A validation failure is a normal `Err` (an is_error result),
        // never a panic - it never reaches the capability.
        let questions = parse_and_validate(input)?;

        // The Question effect (ADR-0057): put the questions to the user and read
        // back their picks. A headless/test host answers through the
        // DecliningQuestioner (the VERBATIM non-interactive string); a cancel is
        // the VERBATIM decline string. Either `Err` is the tool's content
        // VERBATIM - qwen returns the same string as both llmContent and display.
        let answers = match ctx.caps.questioner.ask(questions.clone()).await {
            Ok(answers) => answers,
            Err(message) => return Ok(message),
        };

        Ok(format_answers(&questions, &answers))
    }
}

/// Parses the `questions` array into the capability's [`Question`]s, enforcing
/// qwen's `validateToolParams` rules with VERBATIM messages. The `metadata` field
/// is accepted and ignored (the schema permits it; it never reaches the modal).
fn parse_and_validate(input: &Value) -> Result<Vec<Question>, String> {
    let raw = match input.get("questions") {
        Some(Value::Array(qs)) => qs,
        _ => return Err("Parameter \"questions\" must be an array.".to_string()),
    };

    if raw.is_empty() || raw.len() > MAX_QUESTIONS {
        return Err("Parameter \"questions\" must contain between 1 and 4 questions.".to_string());
    }

    let mut questions = Vec::with_capacity(raw.len());
    for (i, q) in raw.iter().enumerate() {
        // 1-indexed for the VERBATIM messages (qwen `i + 1`).
        let n = i + 1;

        let question = non_empty_string(q.get("question"))
            .ok_or_else(|| format!("Question {n}: \"question\" must be a non-empty string."))?;

        let header = non_empty_string(q.get("header"))
            .ok_or_else(|| format!("Question {n}: \"header\" must be a non-empty string."))?;

        // qwen uses JS `.length` (UTF-16 code units); suspenders counts chars.
        // For the <= 12 rule the two agree on any realistic short header.
        if header.chars().count() > MAX_HEADER_CHARS {
            return Err(format!(
                "Question {n}: \"header\" must be 12 characters or less."
            ));
        }

        let raw_options = match q.get("options") {
            Some(Value::Array(opts)) => opts,
            _ => return Err(format!("Question {n}: \"options\" must be an array.")),
        };

        if raw_options.len() < MIN_OPTIONS || raw_options.len() > MAX_OPTIONS {
            return Err(format!(
                "Question {n}: \"options\" must contain between 2 and 4 options."
            ));
        }

        let mut options = Vec::with_capacity(raw_options.len());
        for (j, opt) in raw_options.iter().enumerate() {
            let m = j + 1;
            let label = non_empty_string(opt.get("label")).ok_or_else(|| {
                format!("Question {n}, Option {m}: \"label\" must be a non-empty string.")
            })?;
            let description = non_empty_string(opt.get("description")).ok_or_else(|| {
                format!("Question {n}, Option {m}: \"description\" must be a non-empty string.")
            })?;
            options.push(QuestionOption { label, description });
        }

        // `multiSelect` must be a boolean when present (qwen's last check),
        // defaulting to false.
        let multi_select = match q.get("multiSelect") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(format!("Question {n}: \"multiSelect\" must be a boolean.")),
        };

        questions.push(Question {
            question,
            header,
            options,
            multi_select,
        });
    }

    Ok(questions)
}

/// A trimmed-non-empty owned String from a `Value`, or `None` for a missing,
/// non-string, or blank value (qwen's `!x || typeof x !== 'string' || x.trim()
/// === ''`). The returned String is the ORIGINAL (untrimmed) text - qwen only
/// trims for the emptiness check.
fn non_empty_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Formats the user's picks VERBATIM as qwen does: one `**{header}**: {value}`
/// line per answer (keyed by question index), joined by newlines, wrapped in
/// `User has provided the following answers:\n\n{...}`. An index with no matching
/// question falls back to qwen's `Question {index + 1}` header.
fn format_answers(questions: &[Question], answers: &[(usize, String)]) -> String {
    let body = answers
        .iter()
        .map(|(index, value)| {
            let header = questions
                .get(*index)
                .map(|q| q.header.clone())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| format!("Question {}", index + 1));
            format!("**{header}**: {value}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("User has provided the following answers:\n\n{body}")
}

#[cfg(test)]
mod tests {
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
}
