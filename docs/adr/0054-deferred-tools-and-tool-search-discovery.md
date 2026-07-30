# Deferred tools and on-demand discovery via tool_search

The wire tool list used to be a static `tools::specs()` - every tool's spec rode every request. That is fine at nine tools, but the port adds low-frequency built-ins, subagents, skills, and (later) an open-ended set of MCP tools. Sending every schema on every request would bloat the system prompt for a small local model that mostly needs a handful of core tools.

We adopt qwen-code's deferral model. Each tool carries three flags on the `Tool` trait (`should_defer`, `always_load`, `search_hint`, all defaulting off). A tool that is `should_defer && !always_load` is withheld from the wire list; its name and a one-line description appear instead in a "## Deferred Tools" section of the system prompt. The model reaches a deferred tool by calling `tool_search` (verbatim port), which matches by `select:<name>` or keyword score, reveals the matched tools, and returns their schemas in an informational `<functions>` block. A revealed tool joins the wire list on the very next request and stays for the rest of the Run.

Reveal state lives on a Run-scoped `ToolRegistry` (`src/tool_registry.rs`) that owns the tool set plus a `revealed` set. The registry is built once per Run and carried as an `Arc` on `ToolCtx`, so `tool_search` (which runs with a `ToolCtx` like any tool) reveals into the same registry the request builder reads from. Because the wire list is recomputed from the registry on every request, a reveal is picked up synchronously with no separate declaration cache to sync - so reveal is infallible. This is deliberately simpler than qwen's `setTools()`/client round-trip and its rollback path, which guard a gap that does not exist here. Reveals reset per Run (a fresh registry), matching qwen's clear-on-session-reset.

The registry handle is the first capability carried on `ToolCtx`; F1 (the capability-context refactor) adds the rest (approval, questioning, side queries, subagent spawning) to the same seat.

Consequences:

- The Agent's one-time tool-spec overhead estimate uses the base `tools::specs()` (nothing revealed yet). Reveals add token cost on demand that the estimate does not pre-count - the same property qwen has, and the reason the reveal-aware list is read only in the request builder.
- The `<function>` blocks `tool_search` returns serialize the native Anthropic `ToolSpec` (`input_schema`, ADR-0003), not qwen's Gemini `parametersJsonSchema` shape.
- In this phase no built-in is deferred, so the "## Deferred Tools" section is empty and the machinery is inert. It exists for the phases that flip `should_defer` (subagents, skills, MCP).
