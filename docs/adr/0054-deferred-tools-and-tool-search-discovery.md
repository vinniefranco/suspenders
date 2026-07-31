# Deferred tools and on-demand discovery via tool_search

The wire tool list used to be a static `tools::specs()` - every tool's spec rode every request. That is fine at nine tools, but the port adds low-frequency built-ins, subagents, skills, and (later) an open-ended set of MCP tools. Sending every schema on every request would bloat the system prompt for a small local model that mostly needs a handful of core tools.

We adopt qwen-code's deferral model. Each tool carries three flags on the `Tool` trait (`should_defer`, `always_load`, `search_hint`, all defaulting off). A tool that is `should_defer && !always_load` is withheld from the wire list; its name and a one-line description appear instead in a "## Deferred Tools" section of the system prompt. The model reaches a deferred tool by calling `tool_search` (verbatim port), which matches by `select:<name>` or keyword score, reveals the matched tools, and returns their schemas in an informational `<functions>` block. A revealed tool joins the wire list on the very next request and stays for the rest of the Run.

Reveal state lives on a Run-scoped `ToolRegistry` (`src/tool/registry.rs`, re-exported at the crate root as `tool_registry`) that owns the tool set plus a `revealed` set. The registry is built once per Run and carried as an `Arc` on `ToolCtx`, so `tool_search` (which runs with a `ToolCtx` like any tool) reveals into the same registry the request builder reads from. Because the wire list is recomputed from the registry on every request, a reveal is picked up synchronously with no separate declaration cache to sync - so reveal is infallible. This is deliberately simpler than qwen's `setTools()`/client round-trip and its rollback path, which guard a gap that does not exist here. Reveals reset per Run (a fresh registry), matching qwen's clear-on-session-reset.

The registry handle is the first capability carried on `ToolCtx`; F1 (the capability-context refactor) adds the rest (approval, questioning, side queries, subagent spawning) to the same seat.

Consequences:

- The Agent's one-time tool-spec overhead estimate uses a per-session registry's `specs()` (the base wire list, nothing revealed yet). Reveals add token cost on demand that the estimate does not pre-count - the same property qwen has, and the reason the reveal-aware list is read only in the request builder.
- The `<function>` blocks `tool_search` returns serialize the native Anthropic `ToolSpec` (`input_schema`, ADR-0003), not qwen's Gemini `parametersJsonSchema` shape.

## Revision (F8 / ADR-0056: MCP tools land)

MCP tools (ADR-0056) are the first tools that are actually deferred, and they are *instance-dependent*: they register on a specific tool set that depends on the Session's configured servers, not on a static built-in list. Three things become live:

- **The Session-stable tool set is `Arc`-shared.** The Agent builds one `Arc<[Box<dyn Tool>]>` at startup (built-ins plus discovered `mcp__*` tools) and each Run's registry is built with `ToolRegistry::with_shared` over it - a fresh empty `revealed` set per Run, no re-boxing of the tools. `ToolRegistry::new(Vec<…>)` stays for tests and the single-Session case.
- **The overhead and the Deferred Tools section are sourced from a live per-session registry**, not the built-in-only `tools::specs()`/`tools::deferred_summary()` free fns. The Agent builds a per-session `with_shared` registry in `init_agent` and reads both figures off it. Because MCP tools are all deferred, `specs()` excludes them, so the overhead is unchanged and correct (exactly as if no server were attached); `deferred_summary()` now lists the `mcp__*` tools, so the model sees them in the "## Deferred Tools" section and can `tool_search` for them. The `tools.rs` free fns stay as the documented built-in floor.
- **`is_mcp` scoring is live.** `tool_search` reads `registry.is_mcp(name)` (a new registry accessor backed by a defaulted `Tool::is_mcp()`), so an MCP tool outranks an identical built-in - discovery is the only way the model reaches a deferred MCP tool. The scoring branch that was pinned by unit tests for this phase is now on the production path.

## Revision (P5, ADR-0062): F5 prompt-section composition is real

The "F5 will eventually own prompt-section composition" interim note in `init_agent` is now real for a second section. The Deferred Tools section (this ADR) and the managed-auto-memory suffix (ADR-0062) both compose at the same point in `init_agent`, each appended to the system prompt with the `\n\n---\n\n` join (qwen `appendManagedAutoMemoryToUserMemory`). The order is: base system prompt, then the "## Deferred Tools" section, then the memory suffix. The memory index is loaded ONCE here at Session start and never refreshed mid-Session (faithful cadence, ADR-0062).
