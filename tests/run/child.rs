use super::*;
use std::sync::{Arc, Mutex};

use crate::llm::model::Api;

fn model() -> Model {
    Model::new("local", "m", Api::AnthropicMessages, 64_000, 100)
}

fn deps(sink: Option<ChildSink>) -> ChildDeps {
    ChildDeps {
        llm: Arc::new(crate::test_support::FakeLlm::script(vec![])),
        model: model(),
        temperature: None,
        thinking_budget: None,
        tool_call_style: ToolCallStyle::default(),
        sink,
    }
}

#[test]
fn no_sink_emitter_is_a_no_op() {
    let mut d = deps(None);
    let mut emitter = d.emitter();
    // Emitting through a no-op Emitter neither panics nor reaches anywhere.
    emitter.emit(Event::run_started("ref"));
}

#[test]
fn a_sink_receives_the_childs_events() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let sink: ChildSink = Box::new(move |event| sink_seen.lock().unwrap().push(event));
    let mut d = deps(Some(sink));
    let mut emitter = d.emitter();
    emitter.emit(Event::run_started("ref"));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn request_approval_denies() {
    let mut d = deps(None);
    assert!(!d.request_approval("id".into(), "rm -rf /".into()).await);
}

#[tokio::test]
async fn drain_steering_is_empty() {
    let mut d = deps(None);
    assert!(d.drain_steering().await.is_empty());
}

#[test]
fn provenance_is_the_models() {
    let d = deps(None);
    assert_eq!(d.provenance(), model().provenance());
}
