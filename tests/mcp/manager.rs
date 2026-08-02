use super::*;
use crate::mcp::view::McpToolAnnotations;
use crate::mcp::{McpCallResult, McpError};
use serde_json::Value;

impl McpManager {
    /// The count of live connections, for tests + diagnostics.
    pub fn conn_count(&self) -> usize {
        self.servers
            .values()
            .filter(|s| matches!(s.attach, Attach::Connected { .. }))
            .count()
    }
}

#[tokio::test]
async fn an_empty_map_yields_an_empty_manager_and_no_tools() {
    let (manager, tools) = McpManager::connect(&BTreeMap::new(), None).await;
    assert_eq!(manager.conn_count(), 0);
    assert!(manager.failures().is_empty());
    assert!(tools.is_empty());
    assert!(manager.views().is_empty());
}

/// A tiny [`McpConn`] the assembly test can hand a real (if inert) conn, so
/// two `Ok` servers each contribute a conn + a tool without a live server.
struct StubConn;

#[async_trait::async_trait]
impl McpConn for StubConn {
    async fn call_tool(&self, _tool: &str, _arguments: Value) -> Result<McpCallResult, McpError> {
        Ok(McpCallResult {
            content: vec![],
            is_error: false,
        })
    }
}

/// A stub attach outcome for the build/live-op tests: a real (inert) conn plus
/// one tool view named `tool`, so a Connected `server` contributes a conn + a
/// tool without a live server behind it. The boxes are built from the view (as
/// `connect_one` and `adapters` do) under the given server name, so the stub
/// outcome mirrors a real attach's wire names.
fn ok_server(server: &str, tool: &str) -> Result<ServerAttach, String> {
    let conn: Arc<dyn McpConn> = Arc::new(StubConn);
    let view = McpToolView {
        name: tool.to_string(),
        description: "does a thing".to_string(),
        annotations: McpToolAnnotations::default(),
        input_schema: Value::Object(Default::default()),
    };
    let tools = build_adapters(server, &conn, std::slice::from_ref(&view), None);
    Ok((conn, tools, vec![view]))
}

/// A stdio config for the build tests (the transport shape drives only the
/// view's `transport_display`/`cwd`; the tool set comes from `ok_server`).
fn stdio_cfg(command: &str) -> McpServerConfig {
    McpServerConfig::new(McpTransport::Stdio {
        command: command.to_string(),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: None,
    })
}

/// A plan wrapping a stdio config, for the build tests.
fn plan(command: &str, source: McpSource, disabled: bool) -> McpServerPlan {
    McpServerPlan {
        config: stdio_cfg(command),
        source,
        disabled,
    }
}

/// A manager holding the given [`LiveServer`]s directly, so the live-op tests
/// stand up a Connected server over a stub conn without a real attach.
fn manager_of(servers: Vec<(&str, LiveServer)>) -> McpManager {
    McpManager {
        servers: servers
            .into_iter()
            .map(|(name, server)| (name.to_string(), server))
            .collect(),
        oauth_tokens_path: None,
    }
}

#[test]
fn build_makes_an_ok_server_connected_with_its_conn_tools_and_view() {
    let (server, tools) = LiveServer::build(
        "alpha",
        &plan("alpha", McpSource::User, false),
        Some(ok_server("alpha", "one")),
    );
    // The connected server contributes its tool box (wire-named) and a
    // Connected view carrying the tool view + the scope that declared it.
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].spec().name, "mcp__alpha__one");
    assert!(matches!(server.attach, Attach::Connected { .. }));
    assert_eq!(server.view.status, McpServerStatus::Connected);
    assert_eq!(server.view.source, McpSource::User);
    assert_eq!(server.view.transport_display, "alpha (stdio)");
    assert_eq!(server.view.tool_count(), 1);
    assert!(!server.disabled);
}

#[test]
fn build_records_an_err_server_as_a_disconnected_view_with_no_tools() {
    let (server, tools) = LiveServer::build(
        "beta",
        &plan("beta", McpSource::User, false),
        Some(Err("boom".to_string())),
    );
    assert!(tools.is_empty());
    assert!(matches!(server.attach, Attach::Down));
    assert_eq!(server.view.status, McpServerStatus::Disconnected);
    assert_eq!(server.view.error.as_deref(), Some("boom"));
    assert_eq!(server.view.tool_count(), 0);
}

#[test]
fn build_shows_a_disabled_server_without_attaching_it() {
    // A disabled server takes no outcome: no conn, no tools, disabled view.
    let (server, tools) = LiveServer::build("srv", &plan("srv", McpSource::Workspace, true), None);
    assert!(tools.is_empty());
    assert!(matches!(server.attach, Attach::Down));
    assert!(server.disabled);
    assert!(server.view.is_disabled);
    assert_eq!(server.view.source, McpSource::Workspace);
    assert_eq!(server.view.tool_count(), 0);
    assert!(server.view.error.is_none());
}

#[test]
fn failures_and_views_derive_from_the_registry_in_server_name_order() {
    // alpha Ok, beta failed, gamma Ok - the BTreeMap keeps them sorted.
    let (alpha, _) = LiveServer::build(
        "alpha",
        &plan("alpha", McpSource::User, false),
        Some(ok_server("alpha", "one")),
    );
    let (beta, _) = LiveServer::build(
        "beta",
        &plan("beta", McpSource::User, false),
        Some(Err("boom".to_string())),
    );
    let (gamma, _) = LiveServer::build(
        "gamma",
        &plan("gamma", McpSource::User, false),
        Some(ok_server("gamma", "three")),
    );
    let manager = manager_of(vec![("alpha", alpha), ("beta", beta), ("gamma", gamma)]);
    assert_eq!(manager.conn_count(), 2);
    assert_eq!(
        manager.failures(),
        vec![("beta".to_string(), "boom".to_string())]
    );
    let views = manager.views();
    assert_eq!(views.len(), 3);
    assert_eq!(views[0].name, "alpha");
    assert_eq!(views[1].name, "beta");
    assert_eq!(views[2].name, "gamma");
}

#[test]
fn adapters_regenerate_the_current_connected_servers_tools() {
    let (alpha, _) = LiveServer::build(
        "alpha",
        &plan("alpha", McpSource::User, false),
        Some(ok_server("alpha", "one")),
    );
    let (beta, _) = LiveServer::build(
        "beta",
        &plan("beta", McpSource::User, false),
        Some(ok_server("beta", "two")),
    );
    let manager = manager_of(vec![("alpha", alpha), ("beta", beta)]);
    // adapters() rebuilds the boxes over the retained conns + views, in
    // server-name order and byte-identical to the originals.
    let adapters = manager.adapters();
    assert_eq!(adapters.len(), 2);
    assert_eq!(adapters[0].spec().name, "mcp__alpha__one");
    assert_eq!(adapters[1].spec().name, "mcp__beta__two");
}

#[tokio::test]
async fn set_disabled_true_drops_a_servers_tools_and_marks_its_view_disabled() {
    let (alpha, _) = LiveServer::build(
        "alpha",
        &plan("alpha", McpSource::User, false),
        Some(ok_server("alpha", "one")),
    );
    let mut manager = manager_of(vec![("alpha", alpha)]);
    assert_eq!(manager.adapters().len(), 1);

    manager.set_disabled("alpha", true).await;
    // Disabling drops the conn + tools and marks the view disabled; the plan's
    // disabled flag flips so a later reconnect is a no-op until re-enabled.
    assert_eq!(manager.conn_count(), 0);
    assert!(manager.adapters().is_empty());
    let views = manager.views();
    assert!(views[0].is_disabled);
    assert_eq!(views[0].status, McpServerStatus::Disconnected);
    assert_eq!(views[0].tool_count(), 0);
}

#[tokio::test]
async fn reconnect_and_set_disabled_are_no_ops_for_an_unknown_server() {
    let mut manager = manager_of(vec![]);
    // No panic, no phantom entry: an unknown name is simply ignored.
    manager.reconnect("ghost").await;
    manager.set_disabled("ghost", true).await;
    assert!(manager.views().is_empty());
}

#[test]
fn views_flag_has_oauth_tokens_from_the_store() {
    // ADR-0065 Phase D: a server with a stored token shows `has_oauth_tokens`,
    // one without does not - filled per `views()` from the token store.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp-oauth-tokens.json");
    let storage = crate::mcp::oauth::McpOAuthTokenStorage::new(path.to_string_lossy().into_owned());
    storage
        .set(crate::mcp::oauth::OAuthCredentials {
            server_name: "auth".to_string(),
            token: crate::mcp::oauth::OAuthToken {
                access_token: "tok".into(),
                token_type: "Bearer".into(),
                refresh_token: None,
                expires_at: None,
                scope: None,
            },
            client_id: None,
            token_url: None,
            mcp_server_url: None,
            updated_at: 0,
        })
        .unwrap();

    let (auth, _) = LiveServer::build(
        "auth",
        &plan("auth", McpSource::User, false),
        Some(ok_server("auth", "one")),
    );
    let (plain, _) = LiveServer::build(
        "plain",
        &plan("plain", McpSource::User, false),
        Some(ok_server("plain", "two")),
    );
    let mut manager = manager_of(vec![("auth", auth), ("plain", plain)]);
    manager.oauth_tokens_path = Some(path.to_string_lossy().into_owned());

    let views = manager.views();
    let auth_view = views.iter().find(|v| v.name == "auth").unwrap();
    let plain_view = views.iter().find(|v| v.name == "plain").unwrap();
    assert!(auth_view.has_oauth_tokens);
    assert!(!plain_view.has_oauth_tokens);
}

#[test]
fn disconnect_drops_a_servers_tools_but_leaves_it_enabled() {
    // Clear-auth's disconnect (ADR-0065 Phase D): the conn + tools go, the view
    // is Disconnected, but the server stays enabled (no exclude, so a later
    // reconnect re-attaches it) - unlike `set_disabled(true)`.
    let (alpha, _) = LiveServer::build(
        "alpha",
        &plan("alpha", McpSource::User, false),
        Some(ok_server("alpha", "one")),
    );
    let mut manager = manager_of(vec![("alpha", alpha)]);
    assert_eq!(manager.adapters().len(), 1);

    manager.disconnect("alpha");
    assert_eq!(manager.conn_count(), 0);
    assert!(manager.adapters().is_empty());
    let views = manager.views();
    assert_eq!(views[0].status, McpServerStatus::Disconnected);
    // NOT disabled: the server can still be re-authenticated / reconnected.
    assert!(!views[0].is_disabled);
}

#[test]
fn oauth_target_returns_the_config_and_http_url_for_an_oauth_server() {
    // An HTTP server carrying an oauth block yields its config + its URL (the
    // discovery + resource-parameter seed); a plain server yields None.
    let mut cfg = McpServerConfig::new(McpTransport::Http {
        url: "https://mcp.test/mcp".into(),
        headers: BTreeMap::new(),
    });
    cfg.oauth = Some(crate::mcp::config::McpOAuthConfig {
        enabled: Some(true),
        ..Default::default()
    });
    let (server, _) = LiveServer::build(
        "auth",
        &McpServerPlan {
            config: cfg,
            source: McpSource::User,
            disabled: false,
        },
        Some(Err("not connected".to_string())),
    );
    let manager = manager_of(vec![("auth", server)]);
    let (oauth, url) = manager.oauth_target("auth").expect("oauth target");
    assert_eq!(oauth.enabled, Some(true));
    assert_eq!(url.as_deref(), Some("https://mcp.test/mcp"));
    // A plain (non-oauth) server has no target.
    assert!(manager.oauth_target("ghost").is_none());
}
