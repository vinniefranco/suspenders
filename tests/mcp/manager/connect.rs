use super::*;

#[test]
fn bearer_header_and_sse_query_injection_match_the_transport_shapes() {
    // The HTTP shape: an `Authorization: Bearer <token>` header value builds
    // clean (the same construction `serve` performs).
    let header = http::HeaderValue::from_str(&format!("Bearer {}", "tok-abc123")).unwrap();
    assert_eq!(header.to_str().unwrap(), "Bearer tok-abc123");

    // The SSE shape: the token rides as the configured query parameter, `?`
    // vs `&` chosen by whether the URL already has a query.
    assert_eq!(
        append_query_param("https://mcp.test/sse", "access_token", "tok"),
        "https://mcp.test/sse?access_token=tok"
    );
    assert_eq!(
        append_query_param("https://mcp.test/sse?v=1", "access_token", "tok"),
        "https://mcp.test/sse?v=1&access_token=tok"
    );
}

#[test]
fn resolve_url_passes_absolute_endpoints_through_unchanged() {
    // An endpoint carrying its own scheme is authoritative; the base is ignored.
    assert_eq!(
        resolve_url("https://mcp.test/sse", "https://other.test/rpc?sessionId=x"),
        "https://other.test/rpc?sessionId=x"
    );
}

#[test]
fn resolve_url_re_roots_a_root_relative_endpoint_onto_the_origin() {
    // A leading `/` replaces the base's path (query included) with the endpoint,
    // keeping only the base's scheme + authority.
    assert_eq!(
        resolve_url("https://mcp.test/sse", "/messages?sessionId=x"),
        "https://mcp.test/messages?sessionId=x"
    );
}

#[test]
fn resolve_url_joins_a_bare_endpoint_onto_the_sse_directory() {
    // A dir-relative value joins onto everything up to the base path's last `/`.
    assert_eq!(
        resolve_url("https://mcp.test/sse", "messages"),
        "https://mcp.test/messages"
    );
}
