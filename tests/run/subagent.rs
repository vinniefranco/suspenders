use super::*;
use std::sync::Mutex;

use crate::content::ContentBlock;
use crate::llm::LlmRequest;
use crate::llm::model::Api;
use crate::llm::provider::Provider;
use crate::llm::response::{Response, StopReason};
use crate::session::{Session, SessionConfig, SessionOpts};
use crate::subagents::{SubagentDef, SubagentModel, ToolSelector, builtins};
use crate::test_support::{Entry, FakeLlm};

fn session() -> Session {
    let tmp = std::env::temp_dir();
    let opts = SessionOpts {
        root: Some(tmp.to_string_lossy().to_string()),
        ..SessionOpts::default()
    };
    Session::build(opts, &SessionConfig::test_defaults()).expect("session builds")
}

fn model(provider: &str, id: &str) -> Model {
    Model::new(provider, id, Api::AnthropicMessages, 64_000, 100)
}

// A custom Provider carrying its own window, so a `Scoped` def naming it
// resolves through `Session::resolve_model` without a Catalog entry.
fn provider(id: &str) -> Provider {
    Provider {
        id: id.into(),
        base_url: "http://localhost:1234/v1".into(),
        token: String::new(),
        api: Api::AnthropicMessages,
        context_window: Some(32_000),
        custom: true,
    }
}

// A registry holding one `Scoped` def pinned to `provider/model-id`.
fn scoped_registry(scoped: &str) -> SubagentRegistry {
    SubagentRegistry::new(vec![SubagentDef {
        name: "scout".into(),
        description: "a scoped scout".into(),
        system_prompt: "explore".into(),
        model: SubagentModel::Scoped(scoped.into()),
        tools: ToolSelector::All,
    }])
}

fn text_response(text: &str) -> Response {
    Response {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::EndTurn,
        usage: Default::default(),
        error: None,
    }
}

type Captured = Arc<Mutex<Vec<Model>>>;

fn recording(captured: Captured, reply: Response) -> Entry {
    Entry::dynamic(vec![], move |_req: &LlmRequest, model: &Model| {
        captured.lock().unwrap().push(model.clone());
        reply.clone()
    })
}

fn spawner_with(llm: Arc<dyn Llm>, providers: Vec<Provider>) -> DirectSubagentSpawner {
    spawner_with_registry(llm, providers, SubagentRegistry::new(builtins()))
}

// As `spawner_with`, but over a caller-supplied registry so a test can pin a
// `Scoped` def and observe the resolved child Model.
fn spawner_with_registry(
    llm: Arc<dyn Llm>,
    providers: Vec<Provider>,
    registry: SubagentRegistry,
) -> DirectSubagentSpawner {
    let mut session = session();
    session.providers = providers;
    DirectSubagentSpawner {
        llm,
        parent_model: model("local", "main"),
        temperature: None,
        thinking_budget: None,
        tool_call_style: ToolCallStyle::default(),
        session,
        registry: Arc::new(registry),
        subagent_run_limit: 5,
    }
}

#[tokio::test]
async fn an_unknown_subagent_type_is_the_verbatim_not_found_error() {
    let spawner = spawner_with(Arc::new(FakeLlm::script(vec![])), vec![]);
    let err = spawner
        .spawn(SubagentRequest {
            subagent_type: "nope".into(),
            prompt: "do it".into(),
            model: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        err,
        "Subagent \"nope\" not found. Available subagents: general-purpose, Explore"
    );
}

#[tokio::test]
async fn an_inherit_subagent_runs_on_the_parent_model() {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeLlm::script(vec![recording(
        Arc::clone(&captured),
        text_response("the findings"),
    )]);
    let spawner = spawner_with(Arc::new(fake), vec![]);

    let out = spawner
        .spawn(SubagentRequest {
            subagent_type: "general-purpose".into(),
            prompt: "investigate".into(),
            model: None,
        })
        .await
        .unwrap();
    assert_eq!(out.terminate_reason, "GOAL");
    assert_eq!(out.result, "the findings");
    // The child completion ran on the parent Model (Inherit).
    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].scoped_id(), model("local", "main").scoped_id());
}

#[tokio::test]
async fn an_explicit_model_override_routes_the_child_to_that_model() {
    // Two-Provider set; the override pins the OTHER provider's model, and the
    // child's `complete` must be called with exactly that Model (the shared
    // Dispatcher routes it) - the Opus-main / Qwen-scout seam.
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeLlm::script(vec![recording(Arc::clone(&captured), text_response("ok"))]);
    let other = model("scout", "fast");
    let spawner = spawner_with(Arc::new(fake), vec![]);

    spawner
        .spawn(SubagentRequest {
            subagent_type: "general-purpose".into(),
            prompt: "explore".into(),
            model: Some(other.clone()),
        })
        .await
        .unwrap();

    let seen = captured.lock().unwrap();
    assert_eq!(seen[0].scoped_id(), other.scoped_id());
}

#[tokio::test]
async fn a_scoped_subagent_resolves_and_routes_to_the_scoped_model() {
    // A two-Provider set; the def is Scoped to the OTHER provider. The
    // resolve runs through `Session::resolve_model` (the canonical path, not
    // a hand-rolled 1-token fallback), and the child's `complete` is called
    // with exactly that resolved Model - the Opus-main / Qwen-scout seam over
    // a def-level pin.
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let fake = FakeLlm::script(vec![recording(
        Arc::clone(&captured),
        text_response("scouted"),
    )]);
    let providers = vec![provider("local"), provider("scout")];
    let spawner = spawner_with_registry(Arc::new(fake), providers, scoped_registry("scout/fast"));

    let out = spawner
        .spawn(SubagentRequest {
            subagent_type: "scout".into(),
            prompt: "explore".into(),
            model: None,
        })
        .await
        .unwrap();
    assert_eq!(out.result, "scouted");
    let seen = captured.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].scoped_id(), "scout/fast");
    // The resolved window is the scoped Provider's, NOT a 1-token fallback.
    assert_eq!(seen[0].context_window, 32_000);
}

#[tokio::test]
async fn a_scoped_subagent_naming_an_unknown_provider_surfaces_the_err() {
    // The Provider set has no `ghost`, so `Session::resolve_model` returns an
    // Err that `spawn` propagates rather than swallowing.
    let spawner = spawner_with_registry(
        Arc::new(FakeLlm::script(vec![])),
        vec![provider("local")],
        scoped_registry("ghost/model"),
    );
    let err = spawner
        .spawn(SubagentRequest {
            subagent_type: "scout".into(),
            prompt: "explore".into(),
            model: None,
        })
        .await
        .unwrap_err();
    assert!(
        err.contains("ghost"),
        "the unknown-provider Err surfaces up through spawn: {err}"
    );
}
