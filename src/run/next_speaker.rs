//! Next-speaker check - who speaks after a no-tool-call Pass (qwen-code parity).
//!
//! qwen-code's agentic loop keeps going as long as the model emits Tool Calls;
//! the interesting case is a Pass with ZERO Tool Calls. There, before handing
//! control back to the user, qwen-code asks a cheap question: given only the
//! model's last reply, who should logically speak next - the `user` or the
//! `model`? A reply that announces a next action ("Now I'll...") or that was
//! cut off mid-thought means the model should continue; a reply that asks the
//! user a question or completes a thought means the user speaks next. This is
//! `checkNextSpeaker` (nextSpeakerChecker.ts), ported.
//!
//! Why this exists: a reasoning model streaming ONLY a thinking block (dropped
//! from the returned content, so the Pass looks empty) or a plain "let me do X
//! next" text reply would otherwise END the Run - forcing the user to type
//! "continue". The check auto-continues in exactly that case (ADR-0043).
//!
//! ## The two paths
//!
//! * **Short-circuit (no model call).** When the response's final content is
//!   EMPTY - zero content blocks, or only `Thinking` blocks (which never enter
//!   the Conversation and are already dropped from the returned content) - the
//!   model plainly produced nothing to hand back, so [`check_next_speaker`]
//!   returns [`NextSpeaker::Model`] without spending a request. This alone
//!   fixes the observed empty/thinking-only death.
//! * **Side-query (one cheap model call).** Otherwise a transient
//!   [`LlmRequest`] carrying `[assistant(the reply), user(CHECK_PROMPT)]` -
//!   NO tools, Thinking disabled, tiny expected output - goes through
//!   `deps.complete`. Its reply text is parsed leniently for
//!   `"next_speaker": "user" | "model"`. An unparseable reply defaults to
//!   [`NextSpeaker::User`] (END the Run) - a bad parse must never risk an
//!   infinite loop.
//!
//! The side-query is a genuine side-query: it builds its own [`LlmRequest`] and
//! never mutates the main Conversation, so nothing it says is checkpointed or
//! logged.

use crate::content::{ContentBlock, Message};
use crate::llm::response::Response;
use crate::llm::{LlmRequest, StreamEvent};
use crate::run::deps::RunDeps;

/// Who should speak next after a no-tool-call Pass. `Model` means the Run
/// continues (the model prompts itself to go on); `User` means the Run ends
/// and control returns to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextSpeaker {
    /// The model continues - the Run loop injects a "Please continue." user
    /// message and advances.
    Model,
    /// The user speaks next - the Run finishes as it does today.
    User,
}

/// qwen-code's `CHECK_PROMPT` (nextSpeakerChecker.ts), verbatim, with a final
/// instruction pinning the reply to a bare JSON object. The bare-JSON tail
/// replaces qwen-code's forced-schema response (suspenders has no structured
/// side-query channel); it was validated to produce clean output on the target
/// model with Thinking off.
const CHECK_PROMPT: &str = "Analyze *only* the content and structure of your immediately preceding response (your last turn in the conversation history). Based *strictly* on that response, determine who should logically speak next: the 'user' or the 'model' (you).
**Decision Rules (apply in order):**
1.  **Model Continues:** If your last response explicitly states an immediate next action *you* intend to take (e.g., \"Next, I will...\", \"Now I'll process...\", \"Moving on to analyze...\", indicates an intended tool call that didn't execute), OR if the response seems clearly incomplete (cut off mid-thought without a natural conclusion), then the **'model'** should speak next.
2.  **Question to User:** If your last response ends with a direct question specifically addressed *to the user*, then the **'user'** should speak next.
3.  **Waiting for User:** If your last response completed a thought, statement, or task *and* does not meet the criteria for Rule 1 (Model Continues) or Rule 2 (Question to User), it implies a pause expecting user input or reaction. In this case, the **'user'** should speak next.
Reply with ONLY a JSON object: {\"next_speaker\": \"user\"} or {\"next_speaker\": \"model\"}.";

/// Decides whether the model should keep going after a no-tool-call Pass
/// (qwen-code's `checkNextSpeaker`). `response` is the Pass whose stop had NO
/// Tool Calls; `deps` is the Run's effect bundle (the side-query travels
/// through its captured Model).
///
/// The short-circuit returns [`NextSpeaker::Model`] with no model call when the
/// reply carries no speakable content (empty, or only Thinking). Otherwise a
/// single transient completion decides; an unparseable reply ends the Run.
pub async fn check_next_speaker<D: RunDeps>(deps: &mut D, response: &Response) -> NextSpeaker {
    let speakable = speakable_content(&response.content);

    // Short-circuit: a reply with nothing speakable (empty, or only Thinking
    // blocks, which are dropped from the returned content) means the model
    // produced nothing to hand back - it should continue. No model call.
    if speakable.is_empty() {
        return NextSpeaker::Model;
    }

    // The side-query: a transient two-message context asking the model who
    // speaks next, Thinking disabled (a reasoning model with Thinking on would
    // spend its whole budget thinking and never answer - validated against the
    // user's endpoint). NO tools; empty system.
    let request = LlmRequest::new(
        "",
        vec![
            Message::assistant(speakable),
            Message::user(vec![ContentBlock::text(CHECK_PROMPT)]),
        ],
        Vec::new(),
    )
    .with_no_think(true);

    // A no-op sink: this side-query is invisible to the operator's Transcript -
    // no MessageStart/Update/End grammar, nothing streamed.
    let mut sink = |_ev: &StreamEvent| {};
    let reply = deps.complete(request, &mut sink).await;

    parse_next_speaker(&reply_text(&reply.content))
}

/// The reply's non-thinking, non-tool-use content: what actually enters the
/// Conversation and what the side-query echoes back to the model. Thinking is
/// already dropped from a returned Response's content, but filtering it here
/// keeps the emptiness test honest regardless.
fn speakable_content(blocks: &[ContentBlock]) -> Vec<ContentBlock> {
    blocks
        .iter()
        .filter(|b| !b.is_tool_use() && !matches!(b, ContentBlock::Thinking { .. }))
        .cloned()
        .collect()
}

/// The concatenated text of a side-query reply's Text blocks - what the lenient
/// parser reads.
fn reply_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Leniently parses a side-query reply for `"next_speaker": "user" | "model"`.
/// An unparseable reply is [`NextSpeaker::User`] - defaulting to ENDING the Run,
/// so a bad parse never risks an infinite loop.
fn parse_next_speaker(text: &str) -> NextSpeaker {
    // A hand-rolled scan for `"next_speaker"` then the next `"user"`/`"model"`
    // token - lenient enough to survive extra prose, whitespace, or a reasoning
    // preamble the model tacks on, without pulling in a regex dependency.
    let Some(rest) = text.split("\"next_speaker\"").nth(1) else {
        return NextSpeaker::User;
    };
    // After the key, find the first quoted value.
    let after_colon = rest.trim_start().strip_prefix(':').unwrap_or(rest);
    let mut quoted = after_colon.split('"');
    // `split('"')` yields [before, value, ...]; the value is the second piece.
    let _before = quoted.next();
    match quoted.next().map(str::trim) {
        Some("model") => NextSpeaker::Model,
        Some("user") => NextSpeaker::User,
        _ => NextSpeaker::User,
    }
}

#[cfg(test)]
mod tests {
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
        assert!(matches!(req.messages[0].role, crate::content::Role::Assistant));
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
        let mut deps =
            deps_scripting(vec![text(r#"{"next_speaker": "user"}"#, StopReason::EndTurn)]);
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
}
