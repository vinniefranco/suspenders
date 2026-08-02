use super::*;

#[test]
fn build_authorization_url_carries_the_pkce_and_oauth_params() {
    let config = McpOAuthConfig {
        scopes: Some(vec!["read".into(), "write".into()]),
        audiences: Some(vec!["aud1".into()]),
        ..Default::default()
    };
    let pkce = PkceParams {
        code_verifier: "verifier".into(),
        code_challenge: "chal".into(),
        state: "st8".into(),
    };
    let url = build_authorization_url(&AuthorizationUrlParams {
        config: &config,
        client_id: "client-1",
        authorization_url: "https://auth.test/authorize",
        redirect_uri: "http://localhost:7777/oauth/callback",
        pkce: &pkce,
        mcp_server_url: Some("https://mcp.test/mcp"),
    });
    assert!(url.starts_with("https://auth.test/authorize?"));
    assert!(url.contains("client_id=client-1"));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("code_challenge=chal"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state=st8"));
    // Space-joined scope + audience are percent-encoded (%20 between).
    assert!(url.contains("scope=read%20write"));
    assert!(url.contains("audience=aud1"));
    // The resource parameter is the canonical MCP server URI, percent-encoded.
    assert!(url.contains("resource=https%3A%2F%2Fmcp.test%2Fmcp"));
    // The redirect URI is percent-encoded onto the query.
    assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A7777%2Foauth%2Fcallback"));
}

#[test]
fn build_authorization_url_appends_onto_an_endpoint_that_already_has_a_query() {
    let pkce = PkceParams {
        code_verifier: "v".into(),
        code_challenge: "c".into(),
        state: "s".into(),
    };
    let url = build_authorization_url(&AuthorizationUrlParams {
        config: &McpOAuthConfig::default(),
        client_id: "cid",
        authorization_url: "https://auth.test/authorize?prompt=login",
        redirect_uri: "http://localhost/cb",
        pkce: &pkce,
        mcp_server_url: None,
    });
    assert!(url.starts_with("https://auth.test/authorize?prompt=login&"));
}

#[test]
fn percent_encode_and_decode_round_trip() {
    for raw in ["hello world", "a/b?c=d&e", "unreserved-_.~", "spaced value"] {
        assert_eq!(percent_decode(&percent_encode(raw)), raw);
    }
    // A `+` decodes to a space (some servers form-encode that way).
    assert_eq!(percent_decode("a+b"), "a b");
}

#[test]
fn form_field_reads_a_named_value() {
    let body = "access_token=abc123&token_type=Bearer&expires_in=3600";
    assert_eq!(form_field(body, "access_token").as_deref(), Some("abc123"));
    assert_eq!(form_field(body, "token_type").as_deref(), Some("Bearer"));
    assert!(form_field(body, "missing").is_none());
}

#[test]
fn parse_token_response_reads_a_json_body() {
    let token = parse_token_response(
        r#"{"access_token":"tok","token_type":"Bearer","refresh_token":"ref","scope":"read"}"#,
    )
    .unwrap();
    assert_eq!(token.access_token, "tok");
    assert_eq!(token.token_type, "Bearer");
    assert_eq!(token.refresh_token.as_deref(), Some("ref"));
    assert_eq!(token.scope.as_deref(), Some("read"));
}

#[test]
fn parse_token_response_falls_back_to_form_urlencoded() {
    let token = parse_token_response("access_token=tok&refresh_token=ref").unwrap();
    assert_eq!(token.access_token, "tok");
    // token_type defaults to Bearer when the form omits it (qwen's default).
    assert_eq!(token.token_type, "Bearer");
    assert_eq!(token.refresh_token.as_deref(), Some("ref"));
}

#[test]
fn parse_token_response_is_none_without_an_access_token() {
    assert!(parse_token_response(r#"{"error":"invalid_grant"}"#).is_none());
    assert!(parse_token_response("error=invalid_grant").is_none());
}
