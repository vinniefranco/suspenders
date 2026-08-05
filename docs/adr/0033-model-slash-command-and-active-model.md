# `/model`: switch the Active Model, live and sticky

The model was a Session fixed fact (`connection.model`), resolved and validated
once at launch and frozen in an `Arc<Session>` the Agent never mutates
(ADR-0017). We want to list the models a server offers, pick one, use it this
Session, and have the choice stick to the next Session. `/model` is the first
Slash Command (ADR-0032).

## Decision

**List via the boundary, live on every open.** Committing `/model` lists
models from `GET {base_url}/models` (ADR-0002 amendment) - a fresh fetch each
time the selector opens, off the event loop so the UI never blocks (ADR-0011),
with a `Loading` state until the reply lands. No cache: it is a localhost call,
and the server's live answer is always the truth. We deliberately did **not**
build pi's hand-authored model registry: Suspenders has exactly one connection,
so the server's own endpoint is the source of truth, and a models catalog could
not live in `config.json` anyway (ADR-0031 locks that schema to the env-settable
key set). A thin curation overlay (aliases/hide/pin) is left for later and is
orthogonal to this decision.

**The Active Model is mutable Agent state** (CONTEXT.md: **Active Model**),
seeded from the connection at launch and living beside the fixed Session facts
like the Conversation does. A new `Command::SetModel(String)` on the
`AgentHandle` swaps it. Each Run captures the model when it begins, so a change
lands on the **next Run**; an in-flight Run finishes on the model it started
with. No mid-stream swap, no synchronization.

**Only the model identifier changes.** The identifier is inert everywhere except
`request::build_request` (the wire `model` field); `result_cap`, the reply
reserve, and the budget invariants all derive from `max_tokens` and
`context_budget`, never the model name. So swapping the identifier alone needs
**no re-validation**. The price: switching to a model with a smaller context
window than the configured budget does **not** auto-shrink anything - that stays
the user's call via config/env.

**Sticky, via a sparse write.** A selection persists by a read-modify-write of
`config.json`: parse the existing file (or start empty), set only `model`, write
it back - never touching the user's other keys and never introducing `token`.
The file is **created if absent**: this is the one sanctioned exception to
ADR-0031's no-auto-create, because an explicit `/model` pick is a deliberate act
(the spirit of `--write-config`), not launch fabricating a file. When
`SUSPENDERS_MODEL` is set in the environment it shadows the file (ADR-0031's
precedence), so the write is accompanied by a Transcript warning that next
launch will override it; the live swap still applies this Session.

Re-selecting the current model is a no-op - no swap, no write, no warning.

## Considered options

- **pi's configured registry** (hand-authored `models.json`) - rejected: earns
  its keep across many cloud providers with per-provider auth; pure overhead for
  one local server, and it collides with ADR-0031's locked config schema.
- **Rebuild the whole `Arc<Session>` on change** - rejected: heavier, re-runs
  full validation, and breaks "fixed facts resolved once at launch." A mutable
  Active Model beside the fixed facts is the smaller, honest change.
- **Swap the whole Connection (incl. `max_tokens`)** - rejected: `max_tokens`
  feeds `result_cap` and the budget invariants, so changing it would force
  re-derivation and re-validation mid-Session. Swapping only the identifier keeps
  the change cheap and total.
- **An endpoint-pinned cache of the model list** (`{ endpoint, ids }` under
  `$XDG_CACHE_HOME`, invalidated when `base_url` changes) - rejected. `base_url`
  is a *fixed* Session fact, so the endpoint-switch invalidation can never fire
  mid-Session (dead code), and with no refresh affordance the cache goes stale
  across Sessions whenever the server's model set changes - the exact
  local-server workflow this feature serves. It would guard only a ~50ms
  localhost call. The correct version, if instant cross-Session paint is ever
  wanted, is stale-while-revalidate (show cached rows, always re-fetch,
  reconcile with a request-generation token) - a deliberate future add, not a
  bare cache.

## Consequences

- The model leaves the Session's fixed-facts set; CONTEXT.md's Session and the
  new Active Model entries reflect this. Every Run reads the model from the
  Agent's mutable state, not the frozen Session value.
- ADR-0031 gains a cross-referenced amendment: `/model` may create `config.json`
  and writes the `model` key by sparse merge; `token` is still never written.
- The `/model` command's adapter logic (fetch orchestration + pick policy) lives
  in one module (`ui::model_command`); the pick policy (no-op-if-unchanged, the
  env-shadow warning, the persist-failure message) is pure and unit-tested there,
  keeping decisions out of the untested adapter. A registry-coverage test asserts
  every `slash::COMMANDS` entry has an adapter handler, so adding a command
  without wiring it fails loudly.
- The Active Model shows in the footer (ADR-0053) - the mutable fact is
  surfaced, not just the fixed Session facts.

## Amendment (ADR-0037): the single-connection premise is reversed

This ADR rejected pi's model registry because "Suspenders has exactly one
connection." Providers (ADR-0037) remove that premise, and with it two
decisions here:

- **"Only the model identifier changes" is superseded.** The Active Model
  becomes a scoped `provider/model-id`, and each Run captures the whole
  Model - window, output cap, pricing, compat - not just the id. The
  Context Budget, Result Cap, and reply reserve derive from that capture
  at Run start, so a cross-Provider switch lands as ordinary budget
  pressure on the next Run instead of a silent mismatch.
- **The registry rejection is reversed.** A generated Catalog (models.dev)
  is the source of truth for built-in Providers; the live `GET /models`
  listing this ADR built survives for custom Providers, whose Models are
  synthesized from discovery plus config.

What survives unchanged: the Active Model as mutable Agent state beside the
fixed Session facts, change-on-next-Run semantics, no mid-stream swap, the
no-op on re-selection, and the sticky sparse write of the `model` key (now
scoped) with its env-shadow warning.

## Amendment (ADR-0051): the `/model` DIALOG is System A + a filter

The one-widget convergence with `/theme` is superseded: `/model` is now a
System-A numbered `›` DIALOG (`ui::selection::SelectionList`), not the retired
group-aware `Selector`. Provider HEADER rows survive (richer than qwen's per-row
`[authType]` badge for suspenders' N providers); headers and greyed catalog rows
are disabled (skipped by nav, dim in render); switch is on Enter only (no live
switch), matching the change-on-next-Run rule above. `model_rows`, `pick`, and
`applied_line` are unchanged.

**The `/model` dialog KEEPS an editable fuzzy filter (deliberate divergence from
qwen's filter-less dialog).** Suspenders surfaces hundreds of catalog models, so
typing after `/model ` narrows the rows (a case-insensitive whole-row filter that
retains matching groups' headers and notes). This is the only filtered dialog;
`/theme` stays frozen-draft. Navigation now WRAPS (qwen `useSelectionList`) where
the retired selector saturated.
