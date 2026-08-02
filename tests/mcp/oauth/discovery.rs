use super::*;

#[test]
fn well_known_urls_for_a_root_server_are_the_root_endpoints() {
    let urls = well_known_urls("https://mcp.test").unwrap();
    assert_eq!(
        urls.protected_resource,
        "https://mcp.test/.well-known/oauth-protected-resource"
    );
    assert!(urls.protected_resource_path.is_none());
    assert_eq!(
        urls.authorization_server,
        vec!["https://mcp.test/.well-known/oauth-authorization-server".to_string()]
    );
    assert_eq!(
        urls.openid,
        vec!["https://mcp.test/.well-known/openid-configuration".to_string()]
    );
}

#[test]
fn well_known_urls_for_a_path_server_try_path_suffixed_first() {
    let urls = well_known_urls("https://mcp.test/api/mcp").unwrap();
    // Path-suffixed authorization-server metadata is tried FIRST (qwen order).
    assert_eq!(
        urls.authorization_server[0],
        "https://mcp.test/.well-known/oauth-authorization-server/api/mcp"
    );
    // The root endpoint is still present as the fallback.
    assert!(
        urls.authorization_server
            .contains(&"https://mcp.test/.well-known/oauth-authorization-server".to_string())
    );
    // The path-suffixed protected-resource URL is offered.
    assert_eq!(
        urls.protected_resource_path.as_deref(),
        Some("https://mcp.test/.well-known/oauth-protected-resource/api/mcp")
    );
    // Query + fragment are stripped from the well-known path.
    let with_query = well_known_urls("https://mcp.test/api/mcp?x=1#frag").unwrap();
    assert_eq!(
        with_query.authorization_server[0],
        "https://mcp.test/.well-known/oauth-authorization-server/api/mcp"
    );
}

#[test]
fn well_known_urls_rejects_a_relative_url() {
    assert!(well_known_urls("not-a-url").is_err());
    assert!(well_known_urls("https://").is_err());
}

#[test]
fn resource_parameter_is_the_canonical_uri_without_query_or_trailing_slash() {
    assert_eq!(
        resource_parameter("https://mcp.test/api/mcp?x=1").unwrap(),
        "https://mcp.test/api/mcp"
    );
    // A trailing slash on a non-root path is trimmed.
    assert_eq!(
        resource_parameter("https://mcp.test/api/").unwrap(),
        "https://mcp.test/api"
    );
    // A root path collapses to just scheme://host.
    assert_eq!(
        resource_parameter("https://mcp.test/").unwrap(),
        "https://mcp.test"
    );
}
