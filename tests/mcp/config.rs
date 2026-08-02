use super::*;

fn stdio() -> McpServerConfig {
    McpServerConfig::new(McpTransport::Stdio {
        command: "my-server".into(),
        args: vec!["--flag".into()],
        env: BTreeMap::new(),
        cwd: None,
    })
}

#[test]
fn parses_a_command_only_entry_to_stdio() {
    let cfg: McpServerConfig =
        serde_json::from_str(r#"{"command":"my-server","args":["--flag"]}"#).unwrap();
    assert_eq!(
        cfg.transport,
        McpTransport::Stdio {
            command: "my-server".into(),
            args: vec!["--flag".into()],
            env: BTreeMap::new(),
            cwd: None,
        }
    );
}

#[test]
fn parses_an_http_url_only_entry_to_http() {
    let cfg: McpServerConfig = serde_json::from_str(
        r#"{"http_url":"https://example.test/mcp","headers":{"Authorization":"Bearer x"}}"#,
    )
    .unwrap();
    assert_eq!(
        cfg.transport,
        McpTransport::Http {
            url: "https://example.test/mcp".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer x".into())]),
        }
    );
}

#[test]
fn rejects_both_transports_at_parse_time() {
    let err =
        serde_json::from_str::<McpServerConfig>(r#"{"command":"cmd","http_url":"https://x.test"}"#)
            .unwrap_err();
    assert!(err.to_string().contains("both"));
}

#[test]
fn rejects_neither_transport_at_parse_time() {
    let err = serde_json::from_str::<McpServerConfig>(r#"{"trust":true}"#).unwrap_err();
    assert!(err.to_string().contains("neither"));
}

#[test]
fn rejects_an_unknown_key() {
    // deny_unknown_fields parity: a typo'd key stays a loud parse error.
    let err =
        serde_json::from_str::<McpServerConfig>(r#"{"command":"cmd","bogus":1}"#).unwrap_err();
    assert!(err.to_string().contains("bogus") || err.to_string().contains("unknown field"));
}

#[test]
fn rejects_a_duplicate_key() {
    let err =
        serde_json::from_str::<McpServerConfig>(r#"{"command":"a","command":"b"}"#).unwrap_err();
    assert!(err.to_string().contains("command"));
}

#[test]
fn rejects_stdio_only_keys_on_an_http_entry() {
    // `args`/`env`/`cwd` under an http_url would round-trip away; that is a
    // shape error, not a silent drop.
    let err = serde_json::from_str::<McpServerConfig>(
        r#"{"http_url":"https://x.test","args":["--flag"]}"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("args"));
}

#[test]
fn rejects_headers_on_a_stdio_entry() {
    let err = serde_json::from_str::<McpServerConfig>(r#"{"command":"cmd","headers":{"X":"y"}}"#)
        .unwrap_err();
    assert!(err.to_string().contains("headers"));
}

#[test]
fn round_trips_the_flat_stdio_wire_shape() {
    let raw = r#"{"command":"srv","args":["--x"],"env":{"LOG":"debug"},"cwd":"/tmp","timeout_ms":1000,"trust":true,"exclude_tools":["delete"]}"#;
    let cfg: McpServerConfig = serde_json::from_str(raw).unwrap();
    let back = serde_json::to_value(&cfg).unwrap();
    assert_eq!(
        back,
        serde_json::from_str::<serde_json::Value>(raw).unwrap()
    );
}

#[test]
fn round_trips_the_flat_http_wire_shape() {
    let raw = r#"{"http_url":"https://x.test/mcp","headers":{"Authorization":"Bearer x"},"include_tools":["keep"]}"#;
    let cfg: McpServerConfig = serde_json::from_str(raw).unwrap();
    let back = serde_json::to_value(&cfg).unwrap();
    assert_eq!(
        back,
        serde_json::from_str::<serde_json::Value>(raw).unwrap()
    );
}

#[test]
fn parses_a_nested_oauth_block_on_an_http_entry() {
    // ADR-0065 Phase D: the flat `oauth` key holds a nested snake_case object.
    let cfg: McpServerConfig = serde_json::from_str(
        r#"{"http_url":"https://x.test/mcp","oauth":{"enabled":true,"scopes":["read","write"]}}"#,
    )
    .unwrap();
    let oauth = cfg.oauth.expect("oauth parsed");
    assert_eq!(oauth.enabled, Some(true));
    assert_eq!(
        oauth.scopes,
        Some(vec!["read".to_string(), "write".to_string()])
    );
    // The unset fields stay None (a minimal `{"enabled": true}` is valid).
    assert!(oauth.client_id.is_none());
    assert!(oauth.authorization_url.is_none());
}

#[test]
fn rejects_an_unknown_key_inside_the_oauth_block() {
    // deny_unknown_fields parity carries INTO the nested block: a typo'd oauth
    // key is a loud error, not a silent drop.
    let err = serde_json::from_str::<McpServerConfig>(
        r#"{"http_url":"https://x.test","oauth":{"clientId":"x"}}"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("clientId") || err.to_string().contains("unknown field"));
}

#[test]
fn round_trips_the_flat_oauth_wire_shape() {
    let raw = r#"{"http_url":"https://x.test/mcp","oauth":{"enabled":true,"client_id":"abc","authorization_url":"https://auth.test/authorize","token_url":"https://auth.test/token","scopes":["read"]}}"#;
    let cfg: McpServerConfig = serde_json::from_str(raw).unwrap();
    let back = serde_json::to_value(&cfg).unwrap();
    assert_eq!(
        back,
        serde_json::from_str::<serde_json::Value>(raw).unwrap()
    );
}

#[test]
fn admits_everything_with_no_filters() {
    let cfg = stdio();
    assert!(cfg.admits("anything"));
}

#[test]
fn admits_treats_include_as_an_allowlist() {
    let mut cfg = stdio();
    cfg.include_tools = Some(vec!["keep".into()]);
    assert!(cfg.admits("keep"));
    assert!(!cfg.admits("drop"));
}

#[test]
fn admits_treats_a_paren_prefixed_include_entry_as_the_tool() {
    // qwen's paren form: `foo(...)` in the allowlist admits the tool `foo`.
    let mut cfg = stdio();
    cfg.include_tools = Some(vec!["keep(a, b)".into()]);
    assert!(cfg.admits("keep"));
    assert!(!cfg.admits("keeper")); // a longer name is NOT a paren match
    assert!(!cfg.admits("drop"));
}

#[test]
fn admits_always_removes_excluded_even_when_included() {
    let mut cfg = stdio();
    cfg.include_tools = Some(vec!["keep".into(), "banned".into()]);
    cfg.exclude_tools = vec!["banned".into()];
    assert!(cfg.admits("keep"));
    assert!(!cfg.admits("banned"));
}
