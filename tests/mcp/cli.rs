//! Unit tests for the pure `mcp add` config builder (and the `mcp list`
//! renderer). No filesystem: [`build_server_config`] turns parsed flags into an
//! [`McpServerConfig`] and every transport/positional/header/env/oauth rule is
//! asserted here. The final round-trip test locks wire fidelity: a built config
//! serializes and re-parses equal, so the builder never produces a shape the
//! parse-time invariants would reject.

use super::*;

/// A minimal `add` invocation the per-case tests tweak: stdio, no flags. Keeping
/// the base here (rather than deriving `Default`) leaves the arg struct a plain
/// clap type while the tests stay concise.
fn add(name: &str, command_or_url: &str) -> McpAddArgs {
    McpAddArgs {
        scope: McpScope::User,
        transport: McpTransportKind::Stdio,
        env: vec![],
        header: vec![],
        timeout: None,
        trust: false,
        include_tools: None,
        exclude_tools: None,
        oauth_client_id: None,
        oauth_client_secret: None,
        oauth_authorization_url: None,
        oauth_token_url: None,
        oauth_scopes: None,
        oauth_audiences: None,
        oauth_token_param_name: None,
        oauth_redirect_uri: None,
        name: name.into(),
        command_or_url: command_or_url.into(),
        args: vec![],
    }
}

#[test]
fn stdio_add_builds_command_args_env() {
    let mut a = add("pytools", "python");
    a.args = vec!["-m".into(), "my_server".into()];
    a.env = vec!["PORT=8080".into(), "HOST=localhost".into()];

    let cfg = build_server_config(&a).unwrap();
    match cfg.transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            assert_eq!(command, "python");
            assert_eq!(args, vec!["-m".to_string(), "my_server".to_string()]);
            assert_eq!(env.get("PORT").map(String::as_str), Some("8080"));
            assert_eq!(env.get("HOST").map(String::as_str), Some("localhost"));
            assert_eq!(cwd, None);
        }
        other => panic!("expected stdio, got {other:?}"),
    }
}

#[test]
fn http_add_builds_url_and_headers_rejecting_args() {
    let mut a = add("api", "https://example.com/mcp");
    a.transport = McpTransportKind::Http;
    a.header = vec!["Authorization: Bearer xyz".into(), "X-Env: prod".into()];

    let cfg = build_server_config(&a).unwrap();
    match cfg.transport {
        McpTransport::Http { url, headers } => {
            assert_eq!(url, "https://example.com/mcp");
            // The one conventional space after the colon is trimmed off the value.
            assert_eq!(
                headers.get("Authorization").map(String::as_str),
                Some("Bearer xyz")
            );
            assert_eq!(headers.get("X-Env").map(String::as_str), Some("prod"));
        }
        other => panic!("expected http, got {other:?}"),
    }
}

#[test]
fn sse_add_builds_url_accepting_headers() {
    let mut a = add("events", "https://example.com/sse");
    a.transport = McpTransportKind::Sse;
    a.header = vec!["X-Token: abc".into()];

    let cfg = build_server_config(&a).unwrap();
    match cfg.transport {
        McpTransport::Sse { url, headers } => {
            assert_eq!(url, "https://example.com/sse");
            assert_eq!(headers.get("X-Token").map(String::as_str), Some("abc"));
        }
        other => panic!("expected sse, got {other:?}"),
    }
}

#[test]
fn header_on_stdio_is_rejected() {
    let mut a = add("srv", "cmd");
    a.header = vec!["Authorization: Bearer xyz".into()];

    let err = build_server_config(&a).unwrap_err();
    assert!(err.contains("--header"), "got: {err}");
}

#[test]
fn args_on_http_are_rejected() {
    let mut a = add("api", "https://example.com/mcp");
    a.transport = McpTransportKind::Http;
    a.args = vec!["extra".into()];

    let err = build_server_config(&a).unwrap_err();
    assert!(err.contains("stdio"), "got: {err}");
}

#[test]
fn args_on_sse_are_rejected() {
    let mut a = add("events", "https://example.com/sse");
    a.transport = McpTransportKind::Sse;
    a.args = vec!["extra".into()];

    let err = build_server_config(&a).unwrap_err();
    assert!(err.contains("stdio"), "got: {err}");
}

#[test]
fn malformed_env_pair_is_rejected() {
    let mut a = add("srv", "cmd");
    a.env = vec!["NOTANEQUALS".into()];

    let err = build_server_config(&a).unwrap_err();
    assert!(err.contains("KEY=VALUE"), "got: {err}");
}

#[test]
fn include_exclude_tools_parsed_from_comma_lists() {
    let mut a = add("srv", "cmd");
    a.include_tools = Some("read, write ,  ".into());
    a.exclude_tools = Some("danger".into());

    let cfg = build_server_config(&a).unwrap();
    assert_eq!(
        cfg.include_tools,
        Some(vec!["read".to_string(), "write".to_string()])
    );
    assert_eq!(cfg.exclude_tools, vec!["danger".to_string()]);
}

#[test]
fn timeout_and_trust_land_on_the_config() {
    let mut a = add("srv", "cmd");
    a.timeout = Some(4500);
    a.trust = true;

    let cfg = build_server_config(&a).unwrap();
    assert_eq!(cfg.timeout_ms, Some(4500));
    assert_eq!(cfg.trust, Some(true));
}

#[test]
fn trust_absent_leaves_the_flag_off_the_wire() {
    let cfg = build_server_config(&add("srv", "cmd")).unwrap();
    assert_eq!(cfg.trust, None);
}

#[test]
fn oauth_flags_populate_the_block_enabled() {
    let mut a = add("api", "https://example.com/mcp");
    a.transport = McpTransportKind::Http;
    a.oauth_client_id = Some("client-123".into());
    a.oauth_authorization_url = Some("https://auth.example.com/authorize".into());
    a.oauth_token_url = Some("https://auth.example.com/token".into());
    a.oauth_scopes = Some("read,write".into());
    a.oauth_redirect_uri = Some("http://localhost:7777/callback".into());

    let cfg = build_server_config(&a).unwrap();
    let oauth = cfg.oauth.expect("oauth block present");
    assert_eq!(oauth.enabled, Some(true));
    assert_eq!(oauth.client_id.as_deref(), Some("client-123"));
    assert_eq!(
        oauth.authorization_url.as_deref(),
        Some("https://auth.example.com/authorize")
    );
    assert_eq!(
        oauth.token_url.as_deref(),
        Some("https://auth.example.com/token")
    );
    assert_eq!(
        oauth.scopes,
        Some(vec!["read".to_string(), "write".to_string()])
    );
    assert_eq!(
        oauth.redirect_uri.as_deref(),
        Some("http://localhost:7777/callback")
    );
}

#[test]
fn oauth_token_param_name_lands_and_round_trips() {
    let mut a = add("events", "https://example.com/sse");
    a.transport = McpTransportKind::Sse;
    a.oauth_token_param_name = Some("access_token".into());

    let cfg = build_server_config(&a).unwrap();
    let oauth = cfg.oauth.clone().expect("oauth block present");
    assert_eq!(oauth.enabled, Some(true));
    assert_eq!(oauth.token_param_name.as_deref(), Some("access_token"));

    // The SSE query-param-token shape survives the wire round-trip.
    let json = serde_json::to_string(&cfg).unwrap();
    let reparsed: McpServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(cfg, reparsed);
}

#[test]
fn oauth_audiences_parsed_from_comma_list() {
    let mut a = add("api", "https://example.com/mcp");
    a.transport = McpTransportKind::Http;
    a.oauth_audiences = Some("https://api.a, https://api.b ,  ".into());

    let cfg = build_server_config(&a).unwrap();
    let oauth = cfg.oauth.expect("oauth block present");
    assert_eq!(oauth.enabled, Some(true));
    assert_eq!(
        oauth.audiences,
        Some(vec![
            "https://api.a".to_string(),
            "https://api.b".to_string()
        ])
    );
}

#[test]
fn no_oauth_flags_leaves_the_block_absent() {
    let cfg = build_server_config(&add("srv", "cmd")).unwrap();
    assert_eq!(cfg.oauth, None);
}

#[test]
fn built_config_round_trips_through_serde() {
    let mut a = add("pytools", "python");
    a.args = vec!["-m".into(), "my_server".into()];
    a.env = vec!["PORT=8080".into()];
    a.timeout = Some(3000);
    a.trust = true;
    a.include_tools = Some("read,write".into());
    a.exclude_tools = Some("danger".into());

    let cfg = build_server_config(&a).unwrap();
    let json = serde_json::to_string(&cfg).unwrap();
    let reparsed: McpServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(cfg, reparsed);
}

#[test]
fn http_with_oauth_round_trips_through_serde() {
    let mut a = add("api", "https://example.com/mcp");
    a.transport = McpTransportKind::Http;
    a.header = vec!["Authorization: Bearer xyz".into()];
    a.oauth_client_id = Some("client-123".into());
    a.oauth_scopes = Some("read".into());

    let cfg = build_server_config(&a).unwrap();
    let json = serde_json::to_string(&cfg).unwrap();
    let reparsed: McpServerConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(cfg, reparsed);
}

#[test]
fn list_renders_empty_state_when_no_servers() {
    let cfg = crate::session::SessionConfig::base();
    let mut lines = Vec::new();
    print_list(&cfg, &mut |l| lines.push(l.to_string()));
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("no MCP servers"), "got: {:?}", lines);
}

#[test]
fn list_renders_each_server_with_transport_and_scope() {
    let mut cfg = crate::session::SessionConfig::base();
    cfg.mcp_servers.insert(
        "pytools".into(),
        build_server_config(&{
            let mut a = add("pytools", "python");
            a.args = vec!["-m".into(), "srv".into()];
            a
        })
        .unwrap(),
    );
    cfg.mcp_sources
        .insert("pytools".into(), crate::mcp::McpSource::Workspace);

    let mut lines = Vec::new();
    print_list(&cfg, &mut |l| lines.push(l.to_string()));
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("pytools"), "got: {:?}", lines);
    assert!(
        lines[0].contains("python -m srv (stdio)"),
        "got: {:?}",
        lines
    );
    assert!(lines[0].contains("[project]"), "got: {:?}", lines);
}
