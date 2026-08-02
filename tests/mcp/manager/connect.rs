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
