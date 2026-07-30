# Providers: one Api adapter per wire protocol, hosts as data

Suspenders grows from one connection to many hosts - Anthropic's API, the
local LM Studio default, and any OpenAI-compatible server - by porting the
provider architecture of pi's `packages/ai` (studied at earendil-works/pi,
2026-07). Its load-bearing insight: **the seam is the wire protocol, not the
host**. pi serves ~38 hosts with 10 hand-written protocol adapters because
most hosts speak one of a few protocols; everything host-specific is data.

No backwards compatibility is kept: the `Llm` trait signature, the config
schema, and the Session Log format all change in one move.

## Vocabulary (now in CONTEXT.md)

- **Api** - a wire protocol an adapter speaks (`anthropic-messages`,
  `openai-completions`). One hand-written adapter per Api.
- **Provider** - a host: identifier, base URL, credential, and the Api its
  models speak. Pure data that selects an adapter.
- **Model** - the facts of one model at one Provider: scoped id, Api,
  context window, output cap, pricing, compat quirks.
- **Catalog** - the generated registry of known Providers and Models.
- **Provenance** - the Provider and Model stamped on every assistant
  message, read at request-shaping for cross-Provider handoff.

## The seam moves: wire format behind the trait

Before: `request::build_request` produced Anthropic wire JSON *outside* the
trait, and `complete` took a `serde_json::Value` - every caller knew a wire
format existed, and the one real adapter made the seam hypothetical. Now:

```rust
pub trait Llm: Send + Sync {
    async fn complete(
        &self,
        request: &LlmRequest,   // typed: system, messages, tools, options
        model: &Model,
        on_event: &mut OnEvent<'_>,
    ) -> Response;

    async fn list_models(&self, provider: &Provider) -> Result<Vec<String>, String>;
}
```

Callers (Run and Compaction) speak only typed domain structs. Each Api
adapter owns end to end: native request building, transport and headers,
its SSE dialect's decoding (a pure fold, as before), stop-reason mapping,
usage extraction, and error capture. The production implementation of `Llm`
is a dispatcher that routes on `model.api` to the adapter; `FakeLlm`
implements the trait as before (ADR-0020) and gets simpler - it scripts
against the typed request, never wire JSON.

ADR-0002's error algebra is reaffirmed unchanged: the boundary never
returns `Err` and never panics; failure is a Response carrying an `Error`
stop reason plus partial content. (This is exactly pi's
errors-as-terminal-events design - a failed or aborted Pass is itself a
persistable message.)

First cut ships two adapters:

- `anthropic_messages` - the existing request builder and SSE fold, moved
  behind the seam.
- `openai_completions` - new: Chat Completions request shape, its SSE
  dialect, and compat handling (`max_tokens` vs `max_completion_tokens`,
  thinking-format quirks such as `chat_template_kwargs`). Covers LM Studio,
  llama.cpp, DeepSeek, Groq, OpenRouter, and most OpenAI-compatible hosts.

`openai-responses` and `google-generative-ai` are deferred, not foreclosed:
Api is an open set, and a new adapter is one module plus catalog data.

## Providers and the Catalog

A Provider is data - no subclassing, one shared shape:

```json
{
  "model": "lmstudio/qwen3.6-27b",
  "providers": {
    "lmstudio": {
      "base_url": "http://localhost:1234/v1",
      "api": "openai-completions",
      "context_window": 32768
    }
  }
}
```

- **Built-in Providers** (anthropic, openai, deepseek, …) come from the
  Catalog and need zero config beyond their own environment key
  (`ANTHROPIC_API_KEY`, …). Auth is per-Provider API keys only; OAuth is
  deferred (see rejected options).
- **Custom Providers** are declared in the `providers` table and discover
  their models live via `GET {base_url}/models` (the ADR-0002 amendment
  machinery, now per Provider), synthesizing Models whose window comes from
  config. Host variance within an Api is expressed as per-Model compat
  facts, following pi's compat-flag design.

The Catalog is generated from models.dev by a committed generator binary
(`cargo run --bin generate-models`), filtered to Providers whose models
speak an implemented Api, written as per-Provider JSON under the llm
module, and embedded with `include_str!`. **Divergence from pi**: pi
gitignores the generated data and regenerates before every build; we commit
it. Builds stay offline and reproducible, and a regeneration is a
reviewable diff. Model pricing rides along, so usage folding can price a
Pass; surfacing cost in the UI is deferred.

## Provenance and cross-Provider handoff

Every assistant message is stamped with the Provider and Model that
produced it, persisted in the Session Log. At request-shaping, a pure
transform pass compares each message's Provenance to the target Model:

- **Same Model**: verbatim replay.
- **Different Model**: tool-call identifiers are rewritten to the target
  Api's alphabet and length limits (with the matching Tool Result ids
  rewritten in the same pass), and orphaned Tool Calls are answered with
  synthetic error Tool Results in the Voice (the ADR-0004/0009 machinery,
  relocated to this pass).

Suspenders' standing rule that Thinking never enters the Conversation
deletes the hardest part of pi's handoff (signature replay for encrypted
reasoning) outright - there is nothing to strip.

## The budget follows the captured Model

Supersedes ADR-0033's "only the model identifier changes." Each Run
captures the whole Model, not just its id, and the derived figures
recompute from that capture at Run start:

- context window → Context Budget (config `context_budget` remains as the
  window for catalog-less Models and as an optional global cap),
- output cap → the reply reserve (clamped to half the budget, below),
- Context Budget → the Result Cap.

A switch to a smaller window lands as ordinary budget pressure on the next
Run - Compaction already knows what to do. An in-flight Run finishes on the
Model it captured; nothing swaps mid-flight. Compaction runs on the same
captured figures.

The precedence (implemented in Stage E): a Model's window is the Catalog's
figure for a known built-in model, else its Provider's config
`context_window` (the per-provider entry beats the global figure for that
Provider's models), else the config `context_budget` figure, else the
64K default. The effective Context Budget for a Run is then
`min(context_budget, window)` when the config key is set, and the window
alone when it is not - the key is a cap and a fallback, never the budget
itself. A Provider is trusted to report whatever output ceiling it likes -
some report an output cap equal to the context window - but a request spends
`input + max_tokens` against the window, so sending that raw ceiling would
make the endpoint reject the moment the prompt is non-empty. The wire output
cap is therefore clamped at Model resolution to half the window, leaving the
other half for the prompt. The reply reserve is per-Model on top of that:
the wire cap clamped again to half the effective budget (which matters only
when the config caps the budget below the window), so a live window - and
therefore a usable `/model` switch - always survives. The one per-Model
budget invariant, checked at launch and at a `/model` swap, is that the
Compaction Keep sits below the compaction trigger at that reserve; a config
that fails it is rejected with the reason.

## Considered and rejected

- **Keeping `build_request` outside the trait with a protocol switch** -
  rejected: every caller learns there are N wire formats; the interface
  grows where it should deepen.
- **A trait per Api instead of a dispatcher** - rejected: Run and
  Compaction want one injected boundary (ADR-0020); which adapter speaks is
  the dispatcher's fact, routed on `model.api`.
- **Gitignored generated Catalog data (pi's way)** - rejected: puts the
  network in the build path and hides catalog changes from review.
- **No Catalog, config only** - rejected: without per-Model windows the
  budget is blind across a 10x window switch, and pricing is impossible.
- **OAuth now (Anthropic Pro/Max)** - deferred: auth stays declarative
  data per Provider, so OAuth slots in later without reshaping the seam.
- **`openai-responses` and `google-generative-ai` adapters now** -
  deferred: neither serves a host we use today; each is one module later.

## Consequences

- ADR-0002 amended: the single-protocol commitment is superseded; the
  one-boundary-per-protocol principle, the pure SSE fold, and the error
  algebra are reaffirmed.
- ADR-0033 amended: the single-connection premise that rejected a model
  registry is reversed; the Active Model becomes a scoped
  `provider/model-id`; per-Run capture widens from the id to the Model.
- ADR-0031 amended: `model` becomes scoped; a `providers` table joins the
  schema and is file-only (structure the env cannot express).
- CONTEXT.md gains Provider, Api, Model, Catalog, and Provenance; the
  Session, Active Model, Context Budget, and Result Cap entries change.
- `Connection` dissolves: base URL and credential belong to the Provider,
  window and output cap to the Model, temperature to the request options.
- The Session Log format gains Provenance fields on assistant messages.

## Amendment (ADR-0059): Model carries input_modalities; transform degrades media

The **Model** now carries an `input_modalities` fact (`{image, pdf}`, a copied
Catalog fact, all-false for synthesized Models and catalog misses), read from
the Catalog's `modalities.input` at resolve. `CatalogModel` gains the field
with `#[serde(default)]` so committed data written before it parses as
text-only; the generator populates it on the next regeneration. The Model
stamps it onto the `ToolCtx` at ctx-build (a copied fact like the Result Cap),
so read_file can gate media on it (P3 3b) without a `tool -> llm` edge.

The **transform pass** (the cross-Provider request-shaping normalizer) gains a
media-degrade step, `degrade_unsupported_media`, run right after
`normalize_request` in the Dispatcher's `complete` so both adapters get it for
free. For each Tool Result `Image`/`Document` block the target Model cannot
accept, it substitutes the verbatim unsupported-modality placeholder (ADR-0059)
as a Text block - the cross-Model-history safety net, since a request may carry
media a previous, more capable Model produced.
