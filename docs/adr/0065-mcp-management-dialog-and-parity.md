# ADR-0065: The MCP management dialog and full qwen parity

## Status

Accepted (2026-08-01). Supersedes the "Out of scope" clause of
[ADR-0056](0056-mcp-client-subsystem.md): the items that ADR deferred (OAuth,
live mid-session reconnect, MCP-call approval-gating via `trust`, a second
settings scope, the legacy HTTP+SSE transport) are brought into scope here - the
SSE transport is recorded in ADR-0056 itself as the now-third `McpTransport` arm.
The transport seam, fail-open attach, deferred-tool discovery, and result
collapse of ADR-0056 stand unchanged.

## Context

ADR-0056 gave Suspenders the MCP *core*: a config, a transport seam
(`McpConn`), a fail-open manager, a deferred-tool adapter, and result collapse.
It has no user-facing surface. qwen-code v0.16.0 ships a full `/mcp` management
dialog and the backend it stands on. The goal is a faithful port of qwen's MCP
dialogues, menus, and configuration - not approximated - onto Suspenders'
existing pure-core TEA architecture (ADR-0001, ADR-0019), the two-selection-
system UI (ADR-0051), and the config composition (ADR-0031), without degrading
quality.

qwen's `/mcp` is a **navigation-stack wizard** with six steps:

```
SERVER_LIST ─▶ SERVER_DETAIL ─┬─▶ TOOL_LIST ─▶ TOOL_DETAIL
                              ├─▶ DISABLE_SCOPE_SELECT
                              └─▶ AUTHENTICATE (OAuth)
```

Each step draws a bordered box: a **header** (title + count), **content**, and a
**footer** of key hints. The backend behind it: a per-server status registry
(`connected`/`connecting`/`disconnected`), tool annotations, an
`mcp.excluded` list across user + workspace settings scopes, live tool
re-discovery (reconnect / disable / enable), and an OAuth 2.0 provider with
PKCE + dynamic client registration + token storage.

## Decision

Port all six steps and their backend faithfully, in the Suspenders idiom, as
six independently-reviewable phases. The ubiquitous language: a **Managed MCP
Server** has a **Status**, a **Source** (which settings scope declared it), and
a set of discovered **MCP Tools** each carrying **Annotations** and a
**validity**. The dialog is the **MCP Dialog** - a Composer overlay in System A
(numbered dialog, ADR-0051), distinct from the `/` palette.

The dialog **views, manages, and authenticates** servers; it does not author
them. Server **authoring** (create / update / delete an entry) is a CLI surface,
`suspenders mcp add | remove | list` (Phase G), matching qwen exactly - qwen's
`/mcp` dialog is likewise read/manage/auth while `qwen mcp add/...` writes the
config. This split completes qwen parity: everything qwen's MCP surface does,
Suspenders now does.

### Phase A - Tool annotations + validity

Extend `McpToolInfo` (mcp/adapter.rs) with `annotations: McpToolAnnotations`
(`read_only`, `destructive`, `idempotent`, `open_world`, all `bool`, from the
MCP tool `annotations` block) and derive **validity** = has non-empty name AND
non-empty description; an invalid tool lists its missing fields
(`missing name`, `missing description`). The manager captures annotations off
the rmcp tool definition. A read model `McpServerView` / `McpToolView` (pure,
UI-free) exposes, per server, the discovered tools grouped for the dialog. This
is the only data the browse steps (TOOL_LIST/TOOL_DETAIL) need.

### Phase B - Settings scopes + `mcp.excluded`

Add a **Scope** to config composition: `User` (the existing
`~/.config/suspenders/config.json`) and `Workspace` (a project-local
`.suspenders/config.json` discovered from the cwd upward). `mcp_servers` merges
across scopes (workspace wins per key); the **Source** of each server is the
lowest scope that declares it. A new `mcp.excluded: [server-name, ...]` list
lives per scope and merges by concatenation (qwen `MergeStrategy.CONCAT`); a
server named in any scope's `excluded` is **disabled**. Scope-aware sparse
persist (`persist_excluded(scope, path, names)`) extends the existing
sticky-write pattern (ADR-0031). Env cannot express either (structure too
complex); both are file-only.

### Phase C - Server status + live manager

Introduce `McpServerStatus { Connecting, Connected, Disconnected }` and a
**status registry** owned by the Agent (`Arc<Mutex<…>>`), seeded at attach
(connected on success, disconnected on fail-open skip). The manager gains live
operations mirrored on qwen's tool-registry:

- `discover_tools_for_server(name)` - drop the server's tools + reveal state,
  re-attach, re-register (used by Reconnect and post-Authenticate).
- `disconnect_server(name)` - drop tools, close the peer, no scope write.
- `disable_server(name)` - `disconnect_server` + add to `mcp.excluded` + drop
  the status entry.
- `enable_server(name)` - remove from `mcp.excluded` (all scopes) + re-discover.

Because Runs hold the session tool set as an `Arc<[Box<dyn Tool>]>`, live
mutation swaps the Agent's shared set behind a lock and bumps a generation the
next Run's registry reads (the in-flight Run finishes on its captured set, as
with a model switch). The dialog drives these through Agent query/command
methods, never touching the manager directly.

### Phase D - OAuth subsystem

A faithful OAuth 2.0 port confined to a new `mcp::oauth` module:

- `McpOAuthConfig` on the server config (`enabled`, `client_id`,
  `client_secret`, `authorization_url`, `token_url`, `scopes`, `audiences`,
  `redirect_uri`, `token_param_name`, `registration_url`).
- **Token storage** at `~/.config/suspenders/mcp-oauth-tokens.json`, mode
  `0600`, a JSON array of `{server_name, token, client_id, token_url,
  mcp_server_url, updated_at}`; `get`/`set`/`delete`/`get_all`.
- **Provider**: PKCE (S256), authorization-server metadata discovery from the
  MCP server's `/.well-known/*` endpoints, dynamic client registration (public
  client, `token_endpoint_auth_method: none`), a localhost callback server on
  the redirect port, secure browser open, code→token exchange, refresh.
- **Wire-in**: an authenticated stdio/HTTP server injects `Authorization:
  Bearer <token>` (or the SSE `token_param_name` query) at connect; a 401
  triggers refresh-then-retry.
- Progress is surfaced through a channel the AUTHENTICATE step renders (mirrors
  qwen's `OauthDisplayMessage`/`OauthAuthUrl` events).

This is the largest phase and stays behind the `McpConn` seam - the rmcp crate
is still touched only in the manager.

### Phase E - The MCP Dialog

A new `ui/mcp_command.rs` (pure row/step logic) plus an `McpDialog` overlay
state on the Composer, opened by a `/mcp` Slash Command (ADR-0032 registry
entry). The dialog is a **distinct System-A overlay** (ADR-0051): a
`Option<McpDialog>` on the Composer, mutually exclusive with the flat
`CommandSelector`, NOT an overload of it - the flat selector is one pickable
list, the dialog is a navigation stack of heterogeneous steps. Keys route
first-refusal (Approval -> McpDialog -> CommandSelector/draft -> Screen);
Enter/Escape push/pop the stack, Escape at the root closes. Data loads async
exactly like `/model`: opening emits an effect that calls `Agent::mcp_views()`
off-loop and posts an `Event::McpDialogReady { generation, servers }` the
Composer folds (generation-tagged so a stale fetch is dropped). Each step is a
pure `(view-model) -> rows + header + footer` builder over the Phase A/C read
model, reusing `SelectionList` + `SelectorRow`.

Steps: SERVER_LIST -> SERVER_DETAIL -> {TOOL_LIST -> TOOL_DETAIL | AUTHENTICATE}.
Actions (View tools, Reconnect, Enable/Disable, Authenticate, Clear
Authentication) are the qwen `ServerDetailStep` action list, shown conditionally
exactly as qwen does, dispatching to the Agent methods (`mcp_reconnect`,
`mcp_set_enabled`, `mcp_authenticate`, `mcp_clear_auth`) and re-fetching views on
completion. All strings, icons (`✓`/`…`/`✗`/`❯`), grouping (User/Workspace/
Extension), the tool annotation WORDS (`read-only`, `destructive`, `idempotent`,
`open-world`) and their colours, column widths, scroll indicator, and empty
states are ported verbatim. The dialog stays ratatui/crossterm-free; only the
Agent calls and OAuth progress cross the impure seam.

Two deliberate divergences from a literal port of qwen's step set, to avoid dead
code: qwen defines a `DISABLE_SCOPE_SELECT` step but its `handleDisable`
auto-resolves the scope by the server's Source and never navigates to it, so the
step is unreachable in v0.16.0 - Suspenders' Disable/Enable likewise
auto-resolves scope (Phase C's `mcp_set_enabled` writes the server's own scope
config) and the unreachable scope-select step is omitted. Everything the runtime
actually reaches is ported faithfully.

### Phase F - MCP health pill

A footer pill (ADR-0053 flat footer) that reads the status registry and shows
`N MCP{s} offline` when `disconnected > 0`, hidden otherwise; `connecting` is
suppressed to avoid boot flicker (qwen's rule).

### Phase G - The `mcp add/remove/list` CLI

The server-authoring surface (`src/mcp/cli.rs`), a faithful port of qwen's
`qwen mcp add/remove/list` in Suspenders' snake_case idiom. `main` gains an
optional `#[command(subcommand)]`; an absent subcommand leaves the default run
path (bare `suspenders`, headless, `--write-config`) unchanged. The module
splits on the pure/impure line the config seam keeps: `build_server_config` is a
PURE flags -> `McpServerConfig` builder doing ALL the validation (transport,
positional `<commandOrUrl>`/`[args...]`, header/env, OAuth rules) with clear
error strings and no filesystem, so wire fidelity is unit-tested with literals;
`dispatch` is the impure edge that resolves the scope path, calls the persist/
remove/compose seams, and prints. The flags mirror qwen: `-s/--scope`
(`user`|`project`), `-t/--transport` (`stdio`|`http`|`sse`, the three transports
of ADR-0056), `-e/--env`, `-H/--header`, `--timeout`, `--trust`,
`--include-tools`, `--exclude-tools`, and `--oauth-*` (any of which arms
`oauth.enabled`).

**Scope semantics** mirror qwen. `add`/`remove` target one scope: `--scope
project` writes the workspace `.suspenders/config.json` under the cwd,
`--scope user` (the default) writes the XDG user config. `list` composes BOTH
scopes and takes `--root` (not `--scope`), annotating each server with its
Source. **Persistence** is a sparse nested read-modify-write into the
`mcp_servers` map (`SessionConfig::persist_mcp_server` / `remove_mcp_server`,
with `servers_with_source` composing the listing), reusing the atomic
write-then-rename and sticky-write pattern of ADR-0031/ADR-0033: only the one
server key is set or dropped, every other key the user wrote is preserved, and
`token` is still never written by the tool.

## Consequences

- Suspenders gains a second settings scope - the first project-local config.
  This is a general capability (workspace config) that MCP is the first
  consumer of; it is designed as such, not bolted onto MCP.
- Suspenders grows its first CLI subcommand tree (`mcp add/remove/list`). The
  optional subcommand leaves the default run path untouched, so a bare
  `suspenders` still launches the TUI; authoring a server no longer requires
  hand-editing `config.json`. Servers can now be authored from the CLI and
  viewed/managed/authenticated from the dialog - the split qwen uses.
- OAuth adds a real subsystem (browser flow, token store on disk). It is
  confined behind the transport seam and the `mcp::oauth` module.
- The session tool set becomes live-mutable behind a generation, a small
  extension of the existing "next Run captures fresh state" rule.
- The `trust` flag, parsed-but-inert since ADR-0056, now gates MCP-call
  approval where the dialog and capability context meet (folded into Phase C's
  status/detail work where the detail view surfaces trust).
- Faithfulness is measured against qwen v0.16.0's component strings and layout;
  divergence is limited to snake_case config keys (ADR-0031) and Rust idiom.
- OAuth callback binds the loopback redirect port (7777) for one flow at a time;
  a second concurrent authenticate fails cleanly on "address in use" rather than
  coordinating a shared callback server (qwen keeps module-level state). The
  dialog drives authentication sequentially, so this is a deliberate
  simplification, not a limitation in practice. Token expiry is computed with
  saturating arithmetic so a hostile `expires_in` cannot wrap into a spuriously
  expired token.

## Alternatives considered

- **Browse-only dialog** (read surface + reconnect, no OAuth/scopes): smaller,
  cleaner today, but not the faithful copy the goal requires. Rejected by the
  product owner in favour of full parity.
- **Reuse the flat single-level Selector** for the dialog: the wizard needs a
  navigation stack and heterogeneous step views (key/value detail, radio
  actions, scrolling tool list). Modelled as a distinct `McpDialog` overlay
  rather than overloading `CommandSelector`, keeping System A's numbered dialog
  semantics intact.
