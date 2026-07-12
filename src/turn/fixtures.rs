//! Shared test fixtures for the split Loop's tests: Session/Conversation
//! builders, FakeLlm script entries, the `run_with` harness, and event
//! inspectors. Today `loop_`'s test module is the only consumer — `batch` and
//! `finish` are covered through the loop's integration tests — but any test
//! module they grow draws on this one fixture set instead of drifting copies.
//! `#[cfg(test)]`-gated in `turn.rs`; never compiled into non-test builds.

use serde_json::Value;
use tempfile::TempDir;

use crate::content::{ContentBlock, Message, Usage};
use crate::conversation::Conversation;
use crate::event::Event;
use crate::llm::response::{Response, StopReason};
use crate::plugins::Registered;
use crate::session::{Session, SessionConfig, SessionOpts};
use crate::test_support::{Entry, FakeDeps, FakeLlm};
use crate::tool::ToolCtx;
use crate::turn::loop_::{Outcome, OutcomeStop, RunOpts, run};

pub(super) fn session_with(root: &std::path::Path, opts: SessionOpts) -> Session {
    let mut opts = opts;
    opts.root = Some(root.to_string_lossy().into_owned());
    Session::build(opts, &SessionConfig::test_defaults()).expect("session builds")
}

pub(super) fn session(root: &std::path::Path) -> Session {
    session_with(root, SessionOpts::default())
}

pub(super) fn conversation(session: &Session, prompt: &str) -> Conversation {
    let mut conv = Conversation::new(
        "You are a test agent.",
        crate::conversation::ConversationOpts::new(
            session.context_budget,
            session.connection.max_tokens,
        )
        .eviction_slack(session.eviction_slack)
        .dead_mass_fraction(session.dead_mass_fraction)
        .compaction_keep(session.compaction_keep),
    );
    conv.add_user_text(prompt);
    conv
}

pub(super) fn tool_ctx(session: &Session) -> ToolCtx {
    session.tool_ctx()
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
    let plugins: Vec<Registered> = Vec::new();
    let ctx = tool_ctx(session);
    let outcome = run(conv, session, &plugins, &ctx, &mut deps, RunOpts::default()).await;
    (outcome, deps)
}

pub(super) fn deps_for(session: &Session, entries: Vec<Entry>) -> FakeDeps {
    FakeDeps::new(FakeLlm::script(entries), session.connection.clone())
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
