//! The LLM client boundary (ADR-0002, ADR-0037).
//!
//! Callers - the Run, the Scout, and Compaction - speak only the typed shapes
//! here: an [`LlmRequest`] plus the captured [`Model`]. Wire building, headers,
//! SSE decoding, stop-reason mapping, and usage extraction all live behind the
//! [`Llm`] trait, one adapter module per [`Api`]:
//!
//! - [`anthropic_messages`] - the Anthropic Messages API adapter (request
//!   building, transport, SSE fold).
//! - [`openai_completions`] - the OpenAI Chat Completions adapter: LM Studio,
//!   llama.cpp, DeepSeek, Groq, OpenRouter, and most OpenAI-compatible hosts.
//!
//! [`Dispatcher`] is the production [`Llm`]: it holds the Session's resolved
//! Provider set and routes each call on the captured Model's Api. Logic tests
//! inject a [`crate::test_support::FakeLlm`] behind the same trait (ADR-0020).
//!
//! Emit pacing (the ~30fps UI accommodation) lives in [`throttle`] - a pure
//! decision over caller-supplied clock ticks, protocol-agnostic.
//!
//! ## Error algebra (ADR-0002)
//!
//! [`Llm::complete`] NEVER returns `Err` and NEVER panics. Connection refused,
//! a non-2xx status, an SSE parse failure, mid-stream death, an unknown
//! Provider, and an Api with no adapter ALL yield a [`Response`] with
//! `stop_reason: Error`, `error` set, and whatever partial content had
//! streamed. Failure is data the Run loop reads.

pub mod anthropic_messages;
pub mod catalog;
pub mod cost;
pub mod metered;
pub mod model;
pub mod openai_completions;
pub mod provider;
pub mod response;
pub mod throttle;
pub mod transform;

#[cfg(test)]
pub mod adapter_test_support;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::content::{ContentBlock, Message};
use crate::tool::ToolSpec;
use model::{Api, Model};
use provider::Provider;
use response::Response;

/// A typed request as the caller assembles it. The adapter selected by the
/// captured Model's Api renders it to that protocol's wire payload.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// The sampling temperature; `None` leaves sampling to the server's own
    /// defaults. Resolved once by the Session (ADR-0037: temperature belongs
    /// to the request options, not a Connection).
    pub temperature: Option<f64>,
    /// The break-glass no-think rescue flag (DESIGN.md: Empty-response Nudge)
    /// and the Scout default (ADR-0014).
    pub no_think: bool,
}

impl LlmRequest {
    pub fn new(system: impl Into<String>, messages: Vec<Message>, tools: Vec<ToolSpec>) -> Self {
        LlmRequest {
            system: system.into(),
            messages,
            tools,
            temperature: None,
            no_think: false,
        }
    }

    pub fn with_no_think(mut self, no_think: bool) -> Self {
        self.no_think = no_think;
        self
    }

    pub fn with_temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }
}

/// A single streaming delta, tagged by kind. Part of the boundary vocabulary
/// every adapter emits.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Thinking(String),
    Text(String),
}

/// The per-delta streaming snapshot the boundary emits to its callback: the
/// delta itself plus the content accumulated so far (open blocks included).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamEvent {
    pub delta: Delta,
    pub content: Vec<ContentBlock>,
}

/// The sentinel key wrapping raw JSON that failed to parse, so a mangled
/// tool call stays distinguishable from a valid empty-input call. Private to
/// the boundary: callers read the fact through [`malformed_tool_input`], never
/// the wire string.
const MALFORMED_INPUT_SENTINEL: &str = "__suspenders_malformed_input__";

/// The boundary's semantic verdict on a Tool Call's decoded `input`: if the
/// input JSON never parsed, [`malformed_tool_input`] returns the raw unparsed
/// text (`Some`); a valid input (including a valid empty map) returns `None`.
///
/// This is how the stream-decoding fact that a tool_use's input was mangled
/// crosses the LLM boundary as a domain signal. The sentinel string that
/// carries it stays private to this module - domain code (the Run batch, the
/// tool registry) gates on this accessor without knowing the representation.
///
/// ADR-0002: malformation is DATA folded into the content path, so it rides in
/// the durable `ContentBlock::ToolUse.input` `Value` unchanged and is
/// interpreted here - never surfaced as an `Err`.
pub fn malformed_tool_input(input: &Value) -> Option<&str> {
    // The key's presence is the verdict; its value carries the raw unparsed
    // text (always a string from the decoder, "" defensively otherwise).
    input
        .get(MALFORMED_INPUT_SENTINEL)
        .map(|raw| raw.as_str().unwrap_or(""))
}

/// Builds the malformed-input marker `Value` from raw unparsed text - the
/// counterpart to [`malformed_tool_input`]. An adapter's decoder produces
/// these when input JSON fails to decode; construction stays here so no
/// caller (or test) spells the sentinel itself.
pub fn malformed_input_marker(raw: &str) -> Value {
    json!({ MALFORMED_INPUT_SENTINEL: raw })
}

/// Decodes a Tool Call's accumulated input JSON - shared by every adapter's
/// stream fold. Empty accumulation is a valid empty map; a non-object or
/// unparseable accumulation becomes the malformed-input marker, so a mangled
/// Tool Call stays distinguishable from a valid empty-input call.
pub(crate) fn decode_tool_input(json: &str) -> Value {
    if json.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(json) {
        Ok(v) if v.is_object() => v,
        _ => malformed_input_marker(json),
    }
}

/// Parses a models-list response body into `data[].id` (ADR-0002 amendment).
/// The wire shape (`{"data": [{"id": …}]}`) is common to the Anthropic and
/// OpenAI REST APIs, so one parse serves both adapters; entries without a
/// string `id` are skipped leniently, and missing or empty `data` is
/// `Ok(vec![])`. A body that isn't JSON is an `Err`.
pub(crate) fn models_from_body(body: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("request_failed: {e}"))?;
    Ok(value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.get("id").and_then(|id| id.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// The per-delta streaming callback. A named `for<'e>`-quantified trait object
/// so the `&StreamEvent` borrow stays independent of the async future's
/// lifetime (async-trait would otherwise unify an elided lifetime with the
/// future, breaking short-lived local borrows inside `complete`).
pub type OnEvent<'cb> = dyn FnMut(&StreamEvent) + Send + 'cb;

/// Emits one event through the callback. A free function so the `&StreamEvent`
/// borrow is a fresh, function-local lifetime rather than being unified with
/// the calling future's lifetime (which triggers a spurious borrow error).
/// Shared by every adapter's transport loop.
pub(crate) fn emit(on_event: &mut OnEvent<'_>, ev: StreamEvent) {
    on_event(&ev);
}

/// The LLM boundary seam. Object-safe so `Arc<dyn Llm>` works (ADR-0020).
///
/// `request` is the typed payload; `model` is the Model the caller captured
/// (ADR-0033 amendment) - together they say what to ask and whom to ask.
/// `on_event` gets one [`StreamEvent`] per text/thinking delta in arrival
/// order - the delta plus the snapshot of blocks accumulated so far (open
/// block included; consumers re-render statelessly). Snapshots carry the
/// accumulated thinking block so the UI can render Thinking without
/// bookkeeping; it is dropped from the returned content and never enters the
/// Conversation.
#[async_trait]
pub trait Llm: Send + Sync {
    async fn complete(
        &self,
        request: &LlmRequest,
        model: &Model,
        on_event: &mut OnEvent<'_>,
    ) -> Response;

    /// Lists the model identifiers `provider` offers
    /// (`GET {base_url}/models`).
    ///
    /// Unlike [`complete`], this RETURNS a `Result` rather than folding failure
    /// into a Response (ADR-0002 amendment, ADR-0033): it is a discrete,
    /// user-triggered query, not the streaming Run loop, so a plain `Err` the
    /// caller surfaces as an info line is simpler than the never-Err error
    /// algebra. Connection refused, a non-2xx status, an unparseable body,
    /// and a request over [`DISCOVERY_TIMEOUT`] are all `Err`; a well-formed
    /// response with no `data` is `Ok(vec![])`.
    ///
    /// [`complete`]: Llm::complete
    async fn list_models(&self, provider: &Provider) -> Result<Vec<String>, String>;
}

/// The total cap on one [`Llm::list_models`] request. Discovery is a
/// discrete, user-triggered query behind the `/model` overlay's "loading
/// models…" line (ADR-0033), so a blackholed host - a sleeping machine
/// dropping SYNs with no RST - must degrade into its group's unreachable
/// note within seconds, not sit on the OS TCP timeout's minutes. A live
/// `GET /models` answers well inside this. [`Llm::complete`] deliberately
/// carries no such cap: a Run streams for minutes by design.
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One Provider's offering for the `/model` selector (ADR-0037): the Provider
/// id, its PICKABLE bare model ids, and its [`Availability`]. The UI scopes
/// the ids (`provider/model-id`) when it builds rows, keeping the wire's bare
/// ids at the boundary. A configured Provider that lists nothing pickable
/// does not vanish: `availability` states why, as a fact the UI runs into
/// display strings, not a pre-rendered reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderModels {
    pub provider: String,
    pub models: Vec<String>,
    pub availability: Availability,
}

/// Why a configured Provider's listing holds what it holds - a fact, not a
/// display string (the RescueState precedent: the boundary states what
/// happened, the UI derives what to show).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// The listing's models are live and pickable.
    Available,
    /// Live discovery failed (host down, non-2xx, unparseable body).
    Unreachable,
    /// The host (or the Catalog) answered with no models.
    NoModels,
    /// A built-in whose credential did not resolve: a configuration fact,
    /// never a discovery failure. `env` names the keys (any one suffices)
    /// whose export would open the door; `catalog` carries the Catalog's
    /// model ids so the selector can show them greyed - what one export
    /// would unlock - without letting them count as listed.
    MissingCredential {
        env: Vec<String>,
        catalog: Vec<String>,
    },
}

impl ProviderModels {
    /// A listing from the ids a Provider offered; empty means the host (or
    /// the Catalog) answered with no models, marked so the selector never
    /// drops a configured Provider silently.
    fn listed(provider: &str, models: Vec<String>) -> Self {
        let availability = if models.is_empty() {
            Availability::NoModels
        } else {
            Availability::Available
        };
        ProviderModels {
            provider: provider.to_string(),
            models,
            availability,
        }
    }

    /// A listing for a Provider whose live discovery failed (host down,
    /// non-2xx, unparseable body): no models, marked unreachable.
    fn unreachable(provider: &str) -> Self {
        ProviderModels {
            provider: provider.to_string(),
            models: Vec::new(),
            availability: Availability::Unreachable,
        }
    }

    /// A listing for a built-in Provider whose credential did not resolve:
    /// nothing pickable, the environment keys and the Catalog's ids riding in
    /// the availability - the selector shows the way in rather than a
    /// mystery gap.
    fn missing_credential(provider: &str, env: Vec<String>, catalog: Vec<String>) -> Self {
        ProviderModels {
            provider: provider.to_string(),
            models: Vec::new(),
            availability: Availability::MissingCredential { env, catalog },
        }
    }
}

/// Lists every Provider's models for the `/model` selector, grouped by
/// Provider in set order (ADR-0037): custom Providers by live discovery
/// ([`Llm::list_models`], the ADR-0002 amendment machinery, now across every
/// configured custom Provider), built-in Providers from the Catalog. A
/// built-in whose credential did not resolve still appears - nothing
/// pickable, its [`Availability::MissingCredential`] carrying the environment
/// keys to set and the Catalog's ids for the selector's greyed preview -
/// because a Provider the user could call with one export deserves a
/// signpost, not a silent gap. A custom Provider whose discovery fails (and
/// any Provider whose listing is empty) likewise appears marked, so a down
/// host is visible rather than silently missing. The listings ALWAYS come
/// back whole: a failed discovery IS its group's unreachable note, never an
/// error that swallows the other Providers' signposts - even with every
/// discovery down and no credential set, the selector shows what exists and
/// how to reach it. (The `/model` overlay's Failed state remains for the one
/// failure with no listings to show: the Agent itself gone - see
/// `Agent::list_models`.)
pub async fn offerings(llm: &dyn Llm, providers: &[Provider]) -> Vec<ProviderModels> {
    let mut listings: Vec<ProviderModels> = Vec::new();
    for p in providers {
        if p.custom {
            match llm.list_models(p).await {
                Ok(models) => listings.push(ProviderModels::listed(&p.id, models)),
                Err(_) => listings.push(ProviderModels::unreachable(&p.id)),
            }
        } else if !p.token.is_empty() {
            let models = catalog::provider(&p.id)
                .map(|c| c.models.iter().map(|m| m.id.clone()).collect())
                .unwrap_or_default();
            listings.push(ProviderModels::listed(&p.id, models));
        } else {
            let (env, catalog) = catalog::provider(&p.id)
                .map(|c| {
                    (
                        c.env.clone(),
                        c.models.iter().map(|m| m.id.clone()).collect(),
                    )
                })
                .unwrap_or_default();
            listings.push(ProviderModels::missing_credential(&p.id, env, catalog));
        }
    }
    listings
}

/// The production boundary (ADR-0037): holds the Session's resolved Provider
/// set and routes each call on the captured Model's Api to that Api's adapter.
/// Holds nothing else config-ish - the request and Model carry everything a
/// call needs (ADR-0002, ADR-0020).
#[derive(Debug, Clone)]
pub struct Dispatcher {
    providers: Vec<Provider>,
}

impl Dispatcher {
    pub fn new(providers: Vec<Provider>) -> Self {
        Dispatcher { providers }
    }
}

#[async_trait]
impl Llm for Dispatcher {
    async fn complete(
        &self,
        request: &LlmRequest,
        model: &Model,
        on_event: &mut OnEvent<'_>,
    ) -> Response {
        // An unknown Provider is data, not a panic (the error algebra): the
        // Session validates the set at launch, so this arm marks a harness bug
        // loudly without killing the Run.
        let Some(provider) = provider::find(&self.providers, &model.provider) else {
            return Response::error(format!("unknown_provider: {}", model.provider));
        };
        // The cross-Provider transform pass (ADR-0037), run ONCE here before
        // routing so both adapters get it for free: history whose Provenance
        // matches `model` replays verbatim; the rest is normalized to the
        // target Api's tool-id rules with orphaned Tool Calls answered.
        let request = transform::normalize_request(request, model);
        match model.api {
            Api::AnthropicMessages => {
                anthropic_messages::complete(&request, model, provider, on_event).await
            }
            Api::OpenaiCompletions => {
                openai_completions::complete(&request, model, provider, on_event).await
            }
        }
    }

    async fn list_models(&self, provider: &Provider) -> Result<Vec<String>, String> {
        match provider.api {
            Api::AnthropicMessages => anthropic_messages::list_models(provider).await,
            Api::OpenaiCompletions => openai_completions::list_models(provider).await,
        }
    }
}

#[cfg(test)]
mod tests {
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
        let fake =
            FakeLlm::script(std::iter::empty()).with_models(vec![Err("refused".to_string())]);
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
        let body =
            r#"{ "data": [{ "id": "a" }, { "id": 7 }, { "name": "no-id" }, { "id": "b" }] }"#;
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
}
