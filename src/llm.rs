//! The LLM client boundary (ADR-0002, ADR-0037).
//!
//! Callers - the Run and Compaction - speak only the typed shapes
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
#[path = "../tests/llm/adapter_test_support.rs"]
pub mod adapter_test_support;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::content::{ContentBlock, Message, ToolSpec};
use model::{Api, Model};
use provider::Provider;
use response::Response;

/// How a stream adapter resolves Tool Calls a model emits (qwen parity with
/// `QWEN_CODE_TOOL_CALL_STYLE`). Only the OpenAI-completions adapter reads it -
/// the Anthropic dialect has no text-markup fallback to gate.
///
/// `rename_all = "lowercase"` makes the serde forms the exact lowercase strings
/// the config env seam parses (`"auto"` / `"structured"` / `"text"`), so the
/// `FileConfig` serialization and [`ToolCallStyle::parse`]/[`as_str`] agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallStyle {
    /// Structured-first with a text-markup fallback: parse the content channel
    /// for Hermes/Qwen-coder markup ONLY when the structured `tool_calls[]`
    /// channel came back empty. The default - it recovers small-model calls
    /// without ever overriding a structured one.
    #[default]
    Auto,
    /// Never parse the text channel: the pre-fallback behavior, an escape hatch
    /// for a host whose prose trips the text parser as a false positive.
    Structured,
    /// Force the text-parse attempt. Structured Tool Calls still always win, so
    /// this only differs from [`Auto`](ToolCallStyle::Auto) once the attempt is
    /// gated on a model id (future work); for now `Text` == `Auto`.
    Text,
}

impl ToolCallStyle {
    /// The lowercase wire token, paired with [`parse`](ToolCallStyle::parse):
    /// `as_str` and `parse` are exact inverses over the wire tokens.
    pub fn as_str(self) -> &'static str {
        match self {
            ToolCallStyle::Auto => "auto",
            ToolCallStyle::Structured => "structured",
            ToolCallStyle::Text => "text",
        }
    }

    /// Parses a wire token; `None` on an unknown one (the config env seam turns
    /// that into a malformed-value error naming the accepted set).
    pub fn parse(s: &str) -> Option<ToolCallStyle> {
        match s {
            "auto" => Some(ToolCallStyle::Auto),
            "structured" => Some(ToolCallStyle::Structured),
            "text" => Some(ToolCallStyle::Text),
            _ => None,
        }
    }
}

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
    /// Nucleus-sampling cutoff; `None` leaves it to the server. Emitted only by
    /// the OpenAI-completions wire builder (Qwen3-Coder tuning), omitted when
    /// unset - the same omit-when-None discipline as `temperature`.
    pub top_p: Option<f64>,
    /// Top-k sampling cutoff; `None` leaves it to the server. Like `top_p`,
    /// only the OpenAI-completions wire builder emits it, and only when set.
    pub top_k: Option<u64>,
    /// The break-glass no-think request flag: when set, the wire builders tell
    /// the server to skip the model's Thinking for this request.
    pub no_think: bool,
    /// The extended-thinking token budget (qwen-code parity): when `Some(n)`,
    /// the Anthropic wire builder emits `thinking: {type: "enabled",
    /// budget_tokens: n}`, which keeps the local reasoning model producing a
    /// Thinking block THEN a Tool Call every turn (an unset budget lets it
    /// think and stop, an empty turn). `None` omits it. Mutually exclusive with
    /// `no_think`: a no-think request suppresses it (answer directly, no
    /// reasoning). Only the Anthropic-messages wire reads it - the OpenAI path
    /// gets reasoning via `reasoning_content` and needs no thinking param.
    pub thinking_budget: Option<u64>,
    /// How the stream adapter resolves Tool Calls (qwen parity). Only the
    /// OpenAI-completions adapter reads it; [`ToolCallStyle::Auto`] by default.
    pub tool_call_style: ToolCallStyle,
}

impl LlmRequest {
    pub fn new(system: impl Into<String>, messages: Vec<Message>, tools: Vec<ToolSpec>) -> Self {
        LlmRequest {
            system: system.into(),
            messages,
            tools,
            temperature: None,
            top_p: None,
            top_k: None,
            no_think: false,
            thinking_budget: None,
            tool_call_style: ToolCallStyle::Auto,
        }
    }

    pub fn with_no_think(mut self, no_think: bool) -> Self {
        self.no_think = no_think;
        self
    }

    pub fn with_thinking_budget(mut self, thinking_budget: Option<u64>) -> Self {
        self.thinking_budget = thinking_budget;
        self
    }

    pub fn with_temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn with_top_p(mut self, top_p: Option<f64>) -> Self {
        self.top_p = top_p;
        self
    }

    pub fn with_top_k(mut self, top_k: Option<u64>) -> Self {
        self.top_k = top_k;
        self
    }

    pub fn with_tool_call_style(mut self, style: ToolCallStyle) -> Self {
        self.tool_call_style = style;
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

/// One model a Provider's models endpoint reported (ADR-0037): its bare id and,
/// when the host surfaces it, its live context window (`meta.n_ctx`). llama.cpp
/// and LM Studio report the REAL loaded window here, so a custom Provider's Model
/// can take its window from the server rather than a config guess. `None` when
/// the host omits `meta.n_ctx` (most cloud hosts do), and the config/fallback
/// window covers that case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub context_window: Option<u64>,
}

/// Parses a models-list response body into its [`DiscoveredModel`]s (ADR-0002
/// amendment, ADR-0037): each entry's `data[].id` and, when present,
/// `data[].meta.n_ctx`. The wire shape (`{"data": [{"id": …, "meta": {"n_ctx":
/// …}}]}`) is common to the Anthropic and OpenAI REST APIs, so one parse serves
/// both adapters; entries without a string `id` are skipped leniently, order is
/// preserved, and missing or empty `data` is `Ok(vec![])`. A body that isn't JSON
/// is an `Err`.
pub(crate) fn models_from_body(body: &str) -> Result<Vec<DiscoveredModel>, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("request_failed: {e}"))?;
    Ok(value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let id = e.get("id").and_then(|id| id.as_str())?;
                    let context_window = e
                        .get("meta")
                        .and_then(|m| m.get("n_ctx"))
                        .and_then(|n| n.as_u64());
                    Some(DiscoveredModel {
                        id: id.to_string(),
                        context_window,
                    })
                })
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

    /// Lists the models `provider` offers (`GET {base_url}/models`) as
    /// [`DiscoveredModel`]s - each bare id plus its server-reported window when
    /// the host surfaces one.
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
    async fn list_models(&self, provider: &Provider) -> Result<Vec<DiscoveredModel>, String>;
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
                // The picker needs only the ids; the server-reported window rides
                // discovery for the launch/swap enrichment (ADR-0037), not the
                // selector rows.
                Ok(models) => {
                    let ids = models.into_iter().map(|m| m.id).collect();
                    listings.push(ProviderModels::listed(&p.id, ids));
                }
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
        // The media-degrade pass (ADR-0059), run right after normalize so both
        // adapters get it for free: a Tool Result image/PDF block the target
        // Model cannot accept becomes the verbatim unsupported-modality
        // placeholder. The cross-Model-history safety net (read-time degrade in
        // read_file is P3 3b).
        let request = transform::degrade_unsupported_media(request, model);
        match model.api {
            Api::AnthropicMessages => {
                anthropic_messages::complete(&request, model, provider, on_event).await
            }
            Api::OpenaiCompletions => {
                openai_completions::complete(&request, model, provider, on_event).await
            }
        }
    }

    async fn list_models(&self, provider: &Provider) -> Result<Vec<DiscoveredModel>, String> {
        match provider.api {
            Api::AnthropicMessages => anthropic_messages::list_models(provider).await,
            Api::OpenaiCompletions => openai_completions::list_models(provider).await,
        }
    }
}

#[cfg(test)]
#[path = "../tests/llm.rs"]
mod tests;
