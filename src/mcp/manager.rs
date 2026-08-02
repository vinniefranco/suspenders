//! [`McpManager`] - the fail-open connect + discovery front for the MCP
//! subsystem, and [`RmcpConn`], the ONE place the `rmcp` wire crate is touched
//! (ADR-0056).
//!
//! [`McpManager::connect`] walks the Session's [`McpServerPlan`] map and
//! attaches each enabled server on its own. A server that will not resolve its
//! transport, will
//! not connect, or will not list its tools is recorded as a `(server, reason)`
//! failure and skipped - the Agent's built-in tools and its other MCP servers
//! carry on (fail-open, qwen's mcp-client-manager). A successful server's
//! admitted tools become [`crate::mcp::adapter::McpTool`]s over a shared
//! [`RmcpConn`], which owns the live rmcp service so the transport worker stays
//! alive for the Session.
//!
//! Everything rmcp lives below this line: the transport construction, the
//! `serve` handshake, `list_tools`, and the `CallToolResult` decode. Nothing
//! else in the crate imports rmcp - the rest of the subsystem works against the
//! [`McpConn`] seam.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::mcp::McpConn;
use crate::mcp::config::{McpOAuthConfig, McpServerConfig, McpTransport};
use crate::mcp::view::{McpServerStatus, McpServerView, McpSource, McpToolView};
use crate::tool::Tool;

mod connect;

use connect::{ServerAttach, build_adapters, connect_one};

/// The live registry of Managed MCP Servers (ADR-0065 Phase C): one
/// [`LiveServer`] per configured server, server-name-sorted, plus the per-server
/// connect failures. The Agent holds one for the Session's lifetime and drives
/// the live operations ([`reconnect`](McpManager::reconnect),
/// [`set_disabled`](McpManager::set_disabled)) through it; each server retains
/// exactly what is needed to re-attach it or rebuild its
/// [`McpTool`](crate::mcp::adapter::McpTool) boxes without a fresh connect
/// ([`adapters`](McpManager::adapters)). [`failures`](McpManager::failures) feeds
/// the per-server launch notices the Agent emits after connect.
///
/// The derived [`Default`] (no servers, no failures) is the same shape
/// [`McpManager::connect`] yields for an empty server map - a test that needs an
/// [`crate::agent`] state without attaching any MCP server reaches for it.
#[derive(Default)]
pub struct McpManager {
    /// The registry, keyed by server name so iteration is server-name-sorted -
    /// the same deterministic order the tool set, failure list, and views take.
    /// One entry per configured server (connected, failed, or disabled).
    servers: BTreeMap<String, LiveServer>,
    /// The MCP OAuth token-store path (ADR-0065 Phase D): where a per-server
    /// stored Bearer token is read from at connect (and refreshed to). `None` on a
    /// `Default` manager (the test/empty shape) - a server with no stored token
    /// connects unauthenticated exactly as before.
    oauth_tokens_path: Option<String>,
}

/// One Managed MCP Server's retained state (ADR-0065 Phase C): its attach plan
/// (config + source + disabled), the current attach outcome (the live conn + the
/// discovered tool views, or a failure reason), and the [`McpServerView`] the
/// dialog reads. Retaining the config lets a live op re-attach the server; the
/// conn + tool views + config timeout let [`McpManager::adapters`] rebuild the
/// server's [`McpTool`](crate::mcp::adapter::McpTool) boxes without a fresh
/// connect (the boxes were consumed into the session tool set at connect time).
struct LiveServer {
    /// The resolved config the live ops re-attach + rebuild adapters from.
    config: McpServerConfig,
    /// The scope that declared the server (drives the dialog grouping + the
    /// enable/disable scope choice).
    source: McpSource,
    /// Whether the server is disabled (excluded): shown, not attached.
    disabled: bool,
    /// The current attach state: the live conn + tool views on success, else the
    /// failure reason (or `Disabled`/never-attached).
    attach: Attach,
    /// The dialog read model, kept in sync with `attach` by every live op.
    view: McpServerView,
}

/// One server's current attach state (ADR-0065 Phase C). A connected server holds
/// its shared [`McpConn`] (to keep the transport alive AND to rebuild adapters
/// over) and the discovered tool views; a failed server holds only its reason; a
/// disabled or never-attached server holds neither.
enum Attach {
    /// Connected: the shared conn its adapters call, plus the discovered tool
    /// views (name/description/schema) [`adapters`](McpManager::adapters) rebuilds
    /// the [`McpTool`](crate::mcp::adapter::McpTool) boxes from.
    Connected {
        conn: Arc<dyn McpConn>,
        tool_views: Vec<McpToolView>,
    },
    /// Failed attach, or disabled/never-attached: no conn, no tools.
    Down,
}

/// One server's attach plan (ADR-0065): its resolved config, the settings
/// [`source`](McpServerPlan::source) scope that declared it, and whether it is
/// [`disabled`](McpServerPlan::disabled) (named in an `mcp.excluded` list). The
/// Agent builds the plan map from the Session's merged servers + sources +
/// excluded set and hands it to [`McpManager::connect`]; a disabled server is
/// never attached, only shown.
pub struct McpServerPlan {
    /// The resolved per-server config.
    pub config: McpServerConfig,
    /// The scope that declared the server.
    pub source: McpSource,
    /// Whether the server is excluded (disabled): shown in the dialog but not
    /// attached.
    pub disabled: bool,
}

impl McpManager {
    /// Attaches every enabled MCP server, fail-open, and returns the manager plus
    /// the discovered [`McpTool`](crate::mcp::adapter::McpTool)s (already filtered
    /// by each server's include/exclude). A server that cannot resolve its
    /// transport, connect, or list its tools is recorded as a failure and
    /// skipped; a `disabled` server is skipped without attaching (it still gets a
    /// disabled view). An empty map yields an empty manager and no tools.
    pub async fn connect(
        plans: &BTreeMap<String, McpServerPlan>,
        oauth_tokens_path: Option<String>,
    ) -> (McpManager, Vec<Box<dyn Tool>>) {
        // Connect every ENABLED server CONCURRENTLY: N dead servers no longer
        // stack their timeouts (N x 30s serially would block the Agent actor
        // before it could serve a single message). Each `connect_one` is bounded
        // by its own timeout, so the wall-clock cost collapses from N timeouts to
        // ~1. Disabled servers are not attached at all.
        let enabled: Vec<&str> = plans
            .iter()
            .filter(|(_, plan)| !plan.disabled)
            .map(|(name, _)| name.as_str())
            .collect();
        let outcomes = futures_util::future::join_all(
            enabled
                .iter()
                .map(|name| connect_one(name, &plans[*name].config, oauth_tokens_path.as_deref())),
        )
        .await;
        // `join_all` preserves the enabled order, so zip re-keys each outcome by
        // name without depending on completion order.
        let mut outcome_by_name: BTreeMap<&str, Result<ServerAttach, String>> =
            enabled.into_iter().zip(outcomes).collect();

        // Fold one [`LiveServer`] per configured server into the registry
        // (server-name-sorted from the plan BTreeMap) and flatten the connected
        // servers' adapter boxes DETERMINISTICALLY: a disabled server takes no
        // outcome, an enabled one takes the outcome `connect_one` produced.
        let mut servers: BTreeMap<String, LiveServer> = BTreeMap::new();
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        for (name, plan) in plans {
            // A disabled server takes no outcome. An enabled one takes the outcome
            // `connect_one` produced, keyed back by name; a missing key (which the
            // zip above makes impossible) degrades to a recorded failure rather
            // than a panic, keeping connect fail-open end to end.
            let outcome = if plan.disabled {
                None
            } else {
                Some(
                    outcome_by_name
                        .remove(name.as_str())
                        .unwrap_or_else(|| Err("no attach outcome".to_string())),
                )
            };
            let (server, adapters) = LiveServer::build(name, plan, outcome);
            tools.extend(adapters);
            servers.insert(name.clone(), server);
        }

        (
            McpManager {
                servers,
                oauth_tokens_path,
            },
            tools,
        )
    }

    /// The per-server connect failures (`(server, reason)`), server-name-sorted.
    /// The Agent surfaces one launch notice per entry after connect (see
    /// `init_agent`). Fail-open means a broken server is here, not a crash.
    /// Derived from the registry's failed views, so a later reconnect that clears
    /// a failure (or a disable that drops one) is reflected here too.
    pub fn failures(&self) -> Vec<(String, String)> {
        self.servers
            .values()
            .filter_map(|s| {
                s.view
                    .error
                    .clone()
                    .map(|reason| (s.view.name.clone(), reason))
            })
            .collect()
    }

    /// The `/mcp` dialog read model (ADR-0065): one [`McpServerView`] per
    /// configured server, connected or failed, in server-name-sorted order. Each
    /// view's `has_oauth_tokens` is filled from the token store (Phase D) so the
    /// dialog can gate the `Clear Authentication` / `Re-authenticate` actions.
    pub fn views(&self) -> Vec<McpServerView> {
        let stored = self.stored_oauth_servers();
        self.servers
            .values()
            .map(|s| {
                let mut view = s.view.clone();
                view.has_oauth_tokens = stored.contains(&view.name);
                view
            })
            .collect()
    }

    /// The set of server names with a stored OAuth token (ADR-0065 Phase D): read
    /// from the token store once per `views()` call so `has_oauth_tokens` reflects
    /// the current on-disk state (a just-authenticated server shows the token, a
    /// just-cleared one does not). An absent store / read error is an empty set
    /// (fail-soft: the dialog simply shows no stored tokens).
    fn stored_oauth_servers(&self) -> std::collections::BTreeSet<String> {
        let Some(path) = &self.oauth_tokens_path else {
            return std::collections::BTreeSet::new();
        };
        crate::mcp::oauth::McpOAuthTokenStorage::new(path)
            .get_all()
            .map(|all| all.into_keys().collect())
            .unwrap_or_default()
    }

    /// The MCP OAuth token-store path (ADR-0065 Phase D), so the Agent's
    /// `mcp_authenticate` / `mcp_clear_auth` ops write to the SAME store the
    /// connect-time injection reads from. `None` on a `Default` (empty) manager.
    pub fn oauth_tokens_path(&self) -> Option<&str> {
        self.oauth_tokens_path.as_deref()
    }

    /// The current MCP [`McpTool`](crate::mcp::adapter::McpTool) boxes for every
    /// CONNECTED server (ADR-0065 Phase C), server-name-sorted. The Agent rebuilds
    /// its Session tool set from these after a live op, so the next Run sees the
    /// current set (a reconnect's fresh tools, a disable's dropped ones). The boxes
    /// were consumed into the tool set at `connect`, so they are rebuilt here over
    /// each server's retained conn + tool views - no fresh connect.
    pub fn adapters(&self) -> Vec<Box<dyn Tool>> {
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();
        for (name, server) in &self.servers {
            if let Attach::Connected { conn, tool_views } = &server.attach {
                tools.extend(build_adapters(
                    name,
                    conn,
                    tool_views,
                    server.config.timeout_ms,
                ));
            }
        }
        tools
    }

    /// One server's OAuth config + its MCP server URL, for the Agent's
    /// `mcp_authenticate` op (ADR-0065 Phase D): the config the provider
    /// authenticates against and the HTTP URL that seeds discovery + the resource
    /// parameter (an HTTP server's `http_url`; `None` for a stdio server, which has
    /// no URL to discover from). `None` for an unknown server or one carrying no
    /// `oauth` block.
    pub fn oauth_target(&self, name: &str) -> Option<(McpOAuthConfig, Option<String>)> {
        let server = self.servers.get(name)?;
        let oauth = server.config.oauth.clone()?;
        let url = match &server.config.transport {
            McpTransport::Http { url, .. } => Some(url.clone()),
            McpTransport::Sse { url, .. } => Some(url.clone()),
            McpTransport::Stdio { .. } => None,
        };
        Some((oauth, url))
    }

    /// Re-attaches one server (ADR-0065 Phase C, qwen `discoverToolsForServer`):
    /// drop its current conn + tools + view and re-run the per-server attach,
    /// updating its state to the fresh outcome (connected on success, failed on
    /// error). A no-op for an unknown or disabled server (a disabled server is
    /// enabled via [`set_disabled`], not reconnected). The Agent rebuilds its tool
    /// set from [`adapters`](McpManager::adapters) afterwards.
    pub async fn reconnect(&mut self, name: &str) {
        let Some(server) = self.servers.get(name) else {
            return;
        };
        if server.disabled {
            return;
        }
        // Drop the old conn by re-attaching from the retained plan: the outcome
        // replaces the whole `LiveServer`, so the previous `Arc<dyn McpConn>` (and
        // its transport worker) drops when the last McpTool over it does.
        let plan = server.plan();
        let outcome = connect_one(name, &plan.config, self.oauth_tokens_path.as_deref()).await;
        let (rebuilt, _adapters) = LiveServer::build(name, &plan, Some(outcome));
        self.servers.insert(name.to_string(), rebuilt);
    }

    /// Disables or enables one server (ADR-0065 Phase C, qwen `disconnectServer` +
    /// exclude / re-`discoverToolsForServer`). Disabling drops the server's conn +
    /// tools and marks its view disabled (no attach); enabling re-attaches it from
    /// the retained plan. Updates the in-memory plan's `disabled` either way. A
    /// no-op for an unknown server. The Agent persists the `mcp.excluded` list to
    /// the right scope and rebuilds its tool set separately - this is only the
    /// in-memory half.
    pub async fn set_disabled(&mut self, name: &str, disabled: bool) {
        let Some(server) = self.servers.get(name) else {
            return;
        };
        let mut plan = server.plan();
        plan.disabled = disabled;
        // Disabling: rebuild as a disabled server (no attach, dropped conn/tools).
        // Enabling: re-attach from the retained plan, exactly like a reconnect.
        let outcome = if disabled {
            None
        } else {
            Some(connect_one(name, &plan.config, self.oauth_tokens_path.as_deref()).await)
        };
        let (rebuilt, _adapters) = LiveServer::build(name, &plan, outcome);
        self.servers.insert(name.to_string(), rebuilt);
    }

    /// Disconnects one server without disabling it (ADR-0065 Phase D, qwen
    /// `disconnectServer` used by `handleClearAuth`): drop its conn + tools and
    /// mark its view Disconnected, leaving the server ENABLED (no `mcp.excluded`
    /// write) so a later Authenticate/Reconnect re-attaches it. The counterpart to
    /// `set_disabled(true)` that keeps the enable state - clearing an OAuth token
    /// must drop the authenticated tools without disabling the server. A no-op for
    /// an unknown server. The Agent rebuilds its tool set from `adapters()`
    /// afterwards.
    pub fn disconnect(&mut self, name: &str) {
        let Some(server) = self.servers.get(name) else {
            return;
        };
        let plan = server.plan();
        // Rebuild with NO attach outcome for an enabled server: `build`'s `None`
        // arm marks a disabled server, so build the Down/enabled shape here by
        // routing an `Err` reason instead - it yields a Disconnected view with no
        // conn/tools while keeping `disabled == false`.
        let (rebuilt, _adapters) = LiveServer::build(
            name,
            &plan,
            Some(Err("disconnected (authentication cleared)".to_string())),
        );
        self.servers.insert(name.to_string(), rebuilt);
    }
}

impl LiveServer {
    /// Builds one server's [`LiveServer`] from its plan and its attach outcome,
    /// returning it alongside the [`McpTool`](crate::mcp::adapter::McpTool) boxes
    /// its connected tools contribute (empty for a disabled or failed server). The
    /// one place a plan + outcome fold into retained state: `connect`, `reconnect`,
    /// and `set_disabled` all route through it, so the view, failure, and adapters
    /// stay consistent regardless of which op produced the outcome.
    ///
    /// `outcome` is `None` for a disabled server (never attached, disabled view)
    /// and `Some(result)` for an enabled one: `Ok` yields a Connected server with
    /// its conn + tool views + adapter boxes, `Err` a Disconnected server carrying
    /// the reason (fail-open).
    fn build(
        name: &str,
        plan: &McpServerPlan,
        outcome: Option<Result<ServerAttach, String>>,
    ) -> (LiveServer, Vec<Box<dyn Tool>>) {
        // Each arm differs only in three things: whether the server is disabled,
        // its attach state (+ the tool views its view draws), and the adapter
        // boxes it contributes. Compute those, then assemble the one `LiveServer`
        // shape - the config/source/view plumbing is written once.
        // The Disconnected/enabled/no-tools base; each arm overrides only the
        // fields that actually differ from it via struct-update.
        let base = ServerViewParts {
            status: McpServerStatus::Disconnected,
            source: plan.source,
            is_disabled: false,
            tools: Vec::new(),
            error: None,
        };
        let (disabled, attach, parts, tools) = match outcome {
            None => (
                true,
                Attach::Down,
                ServerViewParts {
                    is_disabled: true,
                    ..base
                },
                Vec::new(),
            ),
            Some(Ok((conn, tools, tool_views))) => (
                false,
                Attach::Connected {
                    conn,
                    tool_views: tool_views.clone(),
                },
                ServerViewParts {
                    status: McpServerStatus::Connected,
                    tools: tool_views,
                    ..base
                },
                tools,
            ),
            Some(Err(reason)) => (
                false,
                Attach::Down,
                ServerViewParts {
                    error: Some(reason),
                    ..base
                },
                Vec::new(),
            ),
        };
        let server = LiveServer {
            config: plan.config.clone(),
            source: plan.source,
            disabled,
            attach,
            view: server_view(name, &plan.config, parts),
        };
        (server, tools)
    }

    /// Reconstructs the attach plan from the retained state, so a live op can
    /// re-attach the server without the Session handing the plan map back in.
    fn plan(&self) -> McpServerPlan {
        McpServerPlan {
            config: self.config.clone(),
            source: self.source,
            disabled: self.disabled,
        }
    }
}

/// The attach-outcome half of a server's [`McpServerView`]: the resolved status,
/// the source scope that declared it, whether it is disabled, its admitted tool
/// views, and any failure reason. The name + config half rides alongside as the
/// [`server_view`] arguments, since those come from the plan rather than the
/// outcome.
struct ServerViewParts {
    status: McpServerStatus,
    source: McpSource,
    is_disabled: bool,
    tools: Vec<McpToolView>,
    error: Option<String>,
}

/// Builds one server's [`McpServerView`] from its name, config, and the
/// attach-outcome [`ServerViewParts`] (status, source, disabled, tools, error).
/// OAuth (`has_oauth_tokens`) is left `false` here and filled by
/// [`McpManager::views`] from the token store per read (ADR-0065 Phase D), so a
/// just-authenticated / just-cleared server reflects the current on-disk state
/// without rebuilding the view.
fn server_view(name: &str, cfg: &McpServerConfig, parts: ServerViewParts) -> McpServerView {
    let cwd = match &cfg.transport {
        McpTransport::Stdio { cwd, .. } => cwd.clone(),
        McpTransport::Http { .. } => None,
        McpTransport::Sse { .. } => None,
    };
    McpServerView {
        name: name.to_string(),
        status: parts.status,
        source: parts.source,
        transport_display: crate::mcp::view::format_transport(&cfg.transport),
        cwd,
        trust: cfg.trust.unwrap_or(false),
        tools: parts.tools,
        is_disabled: parts.is_disabled,
        has_oauth_tokens: false,
        error: parts.error,
    }
}

#[cfg(test)]
#[path = "../../tests/mcp/manager.rs"]
mod tests;
