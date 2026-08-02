# The MCP client subsystem: attach servers, discover tools, fail open

Suspenders is a coding agent for small local models; its built-in tool set is small on purpose. To reach the wider world (a filesystem server, a GitHub server, a browser-automation server) the user attaches Model Context Protocol servers - external processes or HTTP endpoints that expose their own tools. This ADR is the MCP client subsystem (F8): how a configured server becomes a set of Suspenders tools the model can call, and what stays out.

## The wire crate is confined behind one seam

We use the `rmcp` crate (v3.0.1) for the MCP wire protocol, with three transports: `TokioChildProcess` (stdio - spawn a command with args/env/cwd), the streamable-HTTP client (a URL under the `http_url` key plus static headers), and legacy MCP HTTP+SSE (a URL under the `url` key plus static headers - qwen's `url`, distinct from streamable-HTTP's `http_url`). The transport is modelled as a three-way sum type `McpTransport::{Stdio, Http, Sse}` so "more than one transport key" and "neither" are unrepresentable, decided once at parse time. Every `rmcp` import lives in ONE file, `src/mcp/manager/connect.rs`: the transport construction, the `().serve(transport).await` handshake, `list_tools`, and the `CallToolResult` decode. Nothing else in the crate touches `rmcp`. rmcp 3.0.1 dropped its standalone SSE client transport (only `TokioChildProcess` + `StreamableHttpClientTransport` ship in its `transport` module), so the legacy HTTP+SSE protocol is a small hand-rolled `Transport<RoleClient>` (`SseClientTransport`) living inside that same rmcp-facing file - reqwest + the `sse-stream` crate do the framing, and the handshake GETs the `url` as a `text/event-stream`, reads the leading `event: endpoint` frame for the POST endpoint, POSTs JSON-RPC there, and receives responses as `event: message` frames. Confining it to `connect.rs` keeps the wire crate behind the one seam even though the transport is ours.

The boundary the rest of the subsystem works against is `McpConn`:

```rust
#[async_trait]
pub trait McpConn: Send + Sync {
    async fn call_tool(&self, tool: &str, arguments: Value) -> Result<McpCallResult, McpError>;
}
```

The one production impl, `RmcpConn`, owns a live rmcp service (so the transport worker stays alive for the Session) and decodes rmcp's `CallToolResult` into a transport-free `McpCallResult` (`Vec<McpBlock>` + `is_error`). Everything above the seam - the config, the result collapse, the `McpTool` adapter - works against `McpConn`, so a `FakeConn` drives the unit tests and no test needs a live server. (An `http` type-path note: static HTTP headers are built from `http::{HeaderName, HeaderValue}` and set on rmcp's `StreamableHttpClientTransportConfig::custom_headers`, and the SSE transport carries them on both its GET and POST; `http` is a direct dep used only in `manager/connect.rs`.)

## Attach once, fail open per server

The Agent owns the Session's single MCP connect. In its async init (`init_agent`, before the actor's recv loop), `McpManager::connect(&session.mcp_servers).await` attaches every configured server on its own: resolve the transport, connect (bounded by the server's `timeout_ms`, default 30s stdio / 5s HTTP), list tools (paginated - the discovery loops over `next_cursor` so a large server's later tool pages are not dropped), and build one `McpTool` per admitted tool over a shared `RmcpConn`. The servers connect CONCURRENTLY (a `join_all` over the per-server connects), so N dead servers collapse from N stacked timeouts to ~1; the outcomes reassemble in server-name-sorted order, so the tool set + failure list are deterministic across runs regardless of which handshake finished first. Any per-server await error - a malformed transport, a failed handshake, a failed discovery - is recorded as a `(server, reason)` failure and the server is skipped. A broken server never crashes startup; the Agent's built-in tools and its other servers carry on, and the Agent emits one launch notice per failure (the fail-open report line an Extension crash also takes). This mirrors qwen's mcp-client-manager: a bad server is a recorded skip, not a launch failure. (A malformed *config* - more than one of `command`/`http_url`/`url`, or none of them - is the loud exception: it fails at parse time, distinct from the fail-open connect.)

The per-server `timeout_ms` bounds BOTH the connect handshake (the `serve` + `list_all_tools` at attach time, `manager/connect.rs`) AND every per-call execution: an `McpTool` whose config set a timeout wraps each `call_tool` in that same duration (`adapter.rs`), so a hung tool call fails into an `is_error` Tool Result rather than blocking the Run. A stdio server's child stderr is set to `Stdio::null()` at spawn, so a server that logs to stderr does not write onto the ratatui screen (stdin/stdout stay piped for the transport).

## The Session-stable tool set, shared across Runs

The discovered `McpTool`s join the built-ins to form one `Arc<[Box<dyn Tool>]>` the Agent builds once. Each Run's `ToolRegistry` is built with `ToolRegistry::with_shared` over that `Arc` (a refcount bump, no re-boxing) with its own fresh `revealed` set. The set threads Agent -> `AgentDeps` -> `Capture.tools` -> the Run's registry. This is the `Arc`-share refactor to `ToolRegistry` (ADR-0054's revision): `new(Vec<…>)` stays for tests and the single-Session case; `with_shared(Arc<[…]>)` is the multi-Run path.

## Naming, all-deferred, is_mcp scoring

A discovered tool takes the wire name `mcp__<server>__<tool>` (qwen's convention), sanitized by `valid_name`: any character outside `[a-zA-Z0-9_.-]` becomes `_`, and a name over 63 chars keeps the first 28 and last 32 joined by `___`. MCP tools are ALL deferred (`should_defer` true, `always_load` false): they never ride the wire list the model sees at Run start, and are surfaced on demand through `tool_search`, which reads `registry.is_mcp(name)` to weigh them slightly higher - discovery is the only way the model reaches a deferred MCP tool. The Agent's Deferred Tools system-prompt section and its tool-spec overhead are sourced from a live per-session registry over the shared set (ADR-0054 revision), so the section lists the `mcp__*` tools while the overhead stays unchanged (all-deferred tools are excluded from `specs()`).

## Results collapse to canonical text

Suspenders' Tool Results stay canonical text (ADR-0039): no rich display channel, no inline media. An MCP result's content blocks fold to one string (`mcp::result::render`): text verbatim; image/audio and blob resources become a placeholder line that names the tool by its wire name (`[Tool '<tool_name>' provided the following <kind> data with mime-type: <mime>]`, matching qwen's `mcp-tool.ts` so the model can tell which tool returned which media); a text resource yields its text; a resource link becomes `Resource Link: <label> at <uri>`; parts join on newlines. An `is_error` result comes back `Err(joined)` so the Tools dispatch marks it `is_error` - the same shape every other tool's failure takes (an `is_error` result with no content joins to a non-empty fallback, so the model never sees a blank error). A dropped server surfaces the same way: an `McpError` from the conn becomes an `is_error` Tool Result, never a crash.

## Out of scope (deferred by design)

- **MCP prompts + resources** - only tools are discovered and called.
- **Websocket transport** - stdio, streamable-HTTP, and legacy HTTP+SSE only.
- **OAuth** - static `headers` only (a bearer token the user writes by hand); no token is ever persisted by the tool.
- **MCP-call approval-gating** - the per-server `trust` flag is parsed and stored for parity, but gates nothing in this phase.
- **Inline multimodal tool-result data** - media collapses to a placeholder line; the bytes never reach the model.
- **Live mid-session reconnect / unreveal-on-disconnect** - a dropped server surfaces as an `is_error` Tool Result, not a re-attach; the reveal state is not rolled back.
