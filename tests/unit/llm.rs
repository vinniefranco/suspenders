use super::*;
use crate::llm::response::StopReason;

fn provider(id: &str, api: Api) -> Provider {
    Provider {
        id: id.into(),
        base_url: "http://localhost:0/v1".into(),
        token: "".into(),
        api,
        context_window: Some(64_000),
        custom: true,
    }
}

fn builtin(id: &str, token: &str) -> Provider {
    Provider {
        token: token.into(),
        custom: false,
        ..provider(id, Api::AnthropicMessages)
    }
}

fn model(provider: &str, api: Api) -> Model {
    Model::new(provider, "m", api, 64_000, 100)
}

fn no_op() -> impl FnMut(&StreamEvent) + Send {
    |_ev: &StreamEvent| {}
}

fn simple_request() -> LlmRequest {
    LlmRequest::new(
        "You are Baud.",
        vec![Message::user(vec![ContentBlock::text("hi")])],
        vec![],
    )
}

// ------------------------------------------------------------------
// Dispatcher routing (the error algebra: never Err, never panic)
// ------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_provider_yields_an_error_response() {
    let dispatcher = Dispatcher::new(vec![]);
    let result = dispatcher
        .complete(
            &simple_request(),
            &model("nowhere", Api::AnthropicMessages),
            &mut no_op(),
        )
        .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result.error.unwrap().contains("unknown_provider"));
}

// ------------------------------------------------------------------
// offerings - the multi-Provider /model listing (ADR-0037)
// ------------------------------------------------------------------

#[tokio::test]
async fn offerings_discovers_customs_live_and_lists_credentialed_builtins_from_the_catalog() {
    use crate::test_support::FakeLlm;

    let fake = FakeLlm::script(std::iter::empty())
        .with_models(vec![Ok(vec!["m1".to_string(), "m2".to_string()])]);
    let providers = vec![
        provider("local", Api::AnthropicMessages),
        builtin("anthropic", "sk-test"),
    ];

    let listings = offerings(&fake, &providers).await;
    assert_eq!(listings.len(), 2, "grouped by Provider, in set order");
    assert_eq!(listings[0].provider, "local");
    assert_eq!(listings[0].models, vec!["m1".to_string(), "m2".to_string()]);
    assert_eq!(listings[1].provider, "anthropic");
    assert!(
        listings[1].models.iter().any(|m| m == "claude-fable-5"),
        "builtins list the Catalog's models"
    );
}

#[tokio::test]
async fn offerings_marks_a_builtin_without_a_credential_with_env_keys_and_its_catalog() {
    use crate::test_support::FakeLlm;

    // A built-in whose environment key is unset does not vanish from the
    // selector: nothing pickable, its availability naming the keys to
    // export and carrying the Catalog's ids for the greyed preview. Not
    // a failure, so the listing is error-free.
    let fake = FakeLlm::script(std::iter::empty());
    let providers = vec![builtin("anthropic", "")];
    let listings = offerings(&fake, &providers).await;
    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0].provider, "anthropic");
    assert_eq!(listings[0].models, Vec::<String>::new());
    let Availability::MissingCredential { env, catalog } = &listings[0].availability else {
        panic!(
            "expected MissingCredential, got {:?}",
            listings[0].availability
        );
    };
    assert_eq!(env, &vec!["ANTHROPIC_API_KEY".to_string()]);
    assert!(
        catalog.iter().any(|m| m == "claude-fable-5"),
        "the Catalog's ids ride along for the greyed rows: {catalog:?}"
    );
}

#[tokio::test]
async fn offerings_with_every_discovery_failed_still_lists_every_signpost() {
    use crate::test_support::FakeLlm;

    // The worst morning: the one custom host down, no credential set
    // anywhere. The listings still come back whole - the down host as
    // its unreachable note, every built-in as its env-key signpost -
    // because an error screen here would hide the very map (what exists,
    // which key to export) the user needs to get out.
    let fake = FakeLlm::script(std::iter::empty()).with_models(vec![Err("refused".to_string())]);
    let providers = vec![
        provider("local", Api::AnthropicMessages),
        builtin("anthropic", ""),
        builtin("openrouter", ""),
    ];
    let listings = offerings(&fake, &providers).await;
    assert_eq!(listings.len(), 3, "nothing vanishes");
    assert_eq!(listings[0].provider, "local");
    assert_eq!(listings[0].availability, Availability::Unreachable);
    for listing in &listings[1..] {
        assert!(
            matches!(listing.availability, Availability::MissingCredential { .. }),
            "{} keeps its signpost",
            listing.provider
        );
    }
}

#[tokio::test]
async fn offerings_marks_a_failed_discovery_unreachable_instead_of_dropping_it() {
    use crate::test_support::FakeLlm;

    // Two customs: the first fails, the second lists. The selector shows
    // what it can, and the down host stays visible as unavailable rather
    // than vanishing silently (ADR-0037 Stage F).
    let fake = FakeLlm::script(std::iter::empty())
        .with_models(vec![Err("down".to_string()), Ok(vec!["m1".to_string()])]);
    let providers = vec![
        provider("a-local", Api::AnthropicMessages),
        provider("b-local", Api::AnthropicMessages),
    ];
    let listings = offerings(&fake, &providers).await;
    assert_eq!(listings.len(), 2);
    assert_eq!(listings[0].provider, "a-local");
    assert_eq!(listings[0].models, Vec::<String>::new());
    assert_eq!(listings[0].availability, Availability::Unreachable);
    assert_eq!(listings[1].provider, "b-local");
    assert_eq!(listings[1].models, vec!["m1".to_string()]);
    assert_eq!(listings[1].availability, Availability::Available);
}

#[tokio::test]
async fn offerings_marks_an_empty_listing_no_models() {
    use crate::test_support::FakeLlm;

    // A reachable host answering an empty model list is configured but
    // unusable - visible as unavailable, not dropped.
    let fake = FakeLlm::script(std::iter::empty()).with_models(vec![Ok(vec![])]);
    let providers = vec![provider("local", Api::AnthropicMessages)];
    let listings = offerings(&fake, &providers).await;
    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0].availability, Availability::NoModels);
}

// ------------------------------------------------------------------
// The malformed-input sentinel accessors
// ------------------------------------------------------------------

#[test]
fn malformed_marker_round_trips_through_the_accessor() {
    let marker = malformed_input_marker("{\"path\": tru");
    assert_eq!(malformed_tool_input(&marker), Some("{\"path\": tru"));
    assert_eq!(malformed_tool_input(&json!({ "path": "." })), None);
    assert_eq!(malformed_tool_input(&json!({})), None);
}

#[test]
fn decode_tool_input_covers_empty_valid_and_malformed() {
    assert_eq!(decode_tool_input(""), json!({}));
    assert_eq!(
        decode_tool_input("{\"path\": \".\"}"),
        json!({ "path": "." })
    );
    // A non-object and unparseable JSON both mark as malformed.
    assert_eq!(
        decode_tool_input("[1, 2]"),
        malformed_input_marker("[1, 2]")
    );
    assert_eq!(
        decode_tool_input("{\"path\": tru"),
        malformed_input_marker("{\"path\": tru")
    );
}

// ------------------------------------------------------------------
// The shared models-list parse (ADR-0002 amendment)
// ------------------------------------------------------------------

#[test]
fn models_from_body_parses_ids_skipping_idless_entries() {
    let body = r#"{ "data": [{ "id": "a" }, { "id": 7 }, { "name": "no-id" }, { "id": "b" }] }"#;
    assert_eq!(
        models_from_body(body),
        Ok(vec!["a".to_string(), "b".to_string()])
    );
}

#[test]
fn models_from_body_missing_or_empty_data_is_ok_empty() {
    assert_eq!(models_from_body("{}"), Ok(vec![]));
    assert_eq!(models_from_body(r#"{ "data": [] }"#), Ok(vec![]));
}

#[test]
fn models_from_body_non_json_is_err() {
    assert!(models_from_body("not json").is_err());
}

// ------------------------------------------------------------------
// FakeLlm (ADR-0020) - exercised here so the seam is covered.
// ------------------------------------------------------------------

#[tokio::test]
async fn fake_llm_fires_deltas_and_returns_scripted_response() {
    use crate::test_support::{Entry, FakeLlm};

    let fake = FakeLlm::script(vec![Entry::response(
        vec![Delta::Text("Hel".into()), Delta::Text("lo".into())],
        Response {
            content: vec![ContentBlock::text("Hello")],
            stop_reason: StopReason::EndTurn,
            usage: Default::default(),
            error: None,
        },
    )]);

    let mut seen: Vec<Delta> = Vec::new();
    let mut on_event = |ev: &StreamEvent| seen.push(ev.delta.clone());

    let result = fake
        .complete(
            &simple_request(),
            &model("local", Api::AnthropicMessages),
            &mut on_event,
        )
        .await;

    assert_eq!(
        seen,
        vec![Delta::Text("Hel".into()), Delta::Text("lo".into())]
    );
    assert_eq!(result.content, vec![ContentBlock::text("Hello")]);
    assert_eq!(result.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn fake_llm_error_entry_normalizes_to_error_response() {
    use crate::test_support::{Entry, FakeLlm};

    let fake = FakeLlm::script(vec![Entry::error("boom")]);
    let result = fake
        .complete(
            &simple_request(),
            &model("local", Api::AnthropicMessages),
            &mut no_op(),
        )
        .await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(result.error.as_deref(), Some("boom"));
}

#[tokio::test]
async fn fake_llm_list_models_scripts_ok_then_err() {
    use crate::test_support::FakeLlm;

    let fake = FakeLlm::script(std::iter::empty()).with_models(vec![
        Ok(vec!["m1".to_string(), "m2".to_string()]),
        Err("boom".to_string()),
    ]);
    let p = provider("local", Api::AnthropicMessages);

    assert_eq!(
        fake.list_models(&p).await,
        Ok(vec!["m1".to_string(), "m2".to_string()])
    );
    assert_eq!(fake.list_models(&p).await, Err("boom".to_string()));
    // Exhausted queue falls back to the benign empty list.
    assert_eq!(fake.list_models(&p).await, Ok(vec![]));
}
