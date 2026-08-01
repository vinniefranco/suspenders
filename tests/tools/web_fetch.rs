
use super::*;
use crate::tool::caps::{Capabilities, SideQuery};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use wiremock::matchers::{headers, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A fake [`SideQuery`] that records the request it saw and returns a fixed
/// reply - the seam web_fetch's extraction rides. Its capture lets the tests
/// assert the verbatim fallback wrapper and system instruction reached it.
struct FakeSideQuery {
    reply: String,
    seen: Arc<Mutex<Vec<SideQueryRequest>>>,
}

#[async_trait::async_trait]
impl SideQuery for FakeSideQuery {
    async fn run(&self, request: SideQueryRequest) -> Result<String, String> {
        self.seen.lock().unwrap().push(request);
        Ok(self.reply.clone())
    }
}

/// A ctx whose SideQuery is the given fake, so the extraction is scripted and
/// inspectable (no live Llm).
fn ctx_with_side_query(root: &std::path::Path, sq: Arc<dyn SideQuery>) -> ToolCtx {
    let mut ctx = ToolCtx::for_test(root.to_path_buf(), 10_000);
    ctx.caps = Capabilities::for_test_with_side_query(sq);
    ctx
}

async fn run(input: Value, ctx: &ToolCtx) -> Result<String, String> {
    WebFetch.run(&input, ctx).await
}

async fn serve(response: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

/// The `/page` URL of a mock server - the shape every fetch test targets.
fn page_url(server: &MockServer) -> String {
    format!("{}/page", server.uri())
}

/// The content the side-query saw, pulled out of the verbatim fallback
/// wrapper (the text fenced between the `---` lines).
fn fenced_content(seen: &[SideQueryRequest]) -> String {
    seen[0]
        .user_content
        .split("---\n")
        .nth(1)
        .unwrap()
        .trim_end_matches("\n---")
        .to_string()
}

/// Serve a `body`-sized text page through an UNCAPPED ctx (so only the tool's
/// own guards trim), run the fetch, and return the content the side-query
/// saw. The shared body of the content-cap and download-guard tests.
async fn content_seen_for_body(root: &std::path::Path, body: String) -> String {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sq = Arc::new(FakeSideQuery {
        reply: "ok".into(),
        seen: Arc::clone(&seen),
    });
    let server = serve(ResponseTemplate::new(200).set_body_raw(body, "text/plain")).await;

    let mut c = ctx_with_side_query(root, sq);
    c.result_cap = usize::MAX;
    run(json!({"url": page_url(&server), "prompt": "x"}), &c)
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    fenced_content(&seen)
}

#[test]
fn spec_carries_url_prompt_and_optional_format() {
    let spec = WebFetch.spec();
    assert_eq!(spec.name, "web_fetch");
    // Only url + prompt are required; format is optional.
    assert_eq!(
        spec.input_schema["required"],
        serde_json::json!(["url", "prompt"])
    );
    // Property descriptions are the qwen verbatim strings.
    assert_eq!(
        spec.input_schema["properties"]["url"]["description"],
        "The URL to fetch content from"
    );
    assert_eq!(
        spec.input_schema["properties"]["prompt"]["description"],
        "The prompt to run on the fetched content"
    );
    assert_eq!(
        spec.input_schema["properties"]["format"]["description"],
        "Preferred content format (Accept header only): auto (default, prefers markdown), \
             markdown, html, or text. All content is normalized to plain text."
    );
    // The format enum carries exactly qwen's four values.
    assert_eq!(
        spec.input_schema["properties"]["format"]["enum"],
        serde_json::json!(["auto", "markdown", "html", "text"])
    );
}

// The `format` → Accept-header mapping, VERBATIM from qwen `getAcceptHeader`.
#[test]
fn format_maps_to_the_qwen_accept_header() {
    assert_eq!(accept_header("markdown"), "text/markdown");
    assert_eq!(accept_header("html"), "text/html");
    assert_eq!(accept_header("text"), "text/plain");
    assert_eq!(
        accept_header("auto"),
        "text/markdown, text/html, text/plain"
    );
    // An absent/unknown value defaults to auto.
    assert_eq!(
        accept_header("something-else"),
        "text/markdown, text/html, text/plain"
    );
}

// The Accept header sent on the wire is the one the `format` param selects;
// the default (no `format`) is auto.
#[tokio::test]
async fn the_selected_accept_header_is_sent_on_the_wire() {
    let tmp = TempDir::new().unwrap();
    for (format, accept) in [
        (Some("markdown"), "text/markdown"),
        (Some("html"), "text/html"),
        (Some("text"), "text/plain"),
        (Some("auto"), "text/markdown, text/html, text/plain"),
        (None, "text/markdown, text/html, text/plain"),
    ] {
        let sq = Arc::new(FakeSideQuery {
            reply: "ok".into(),
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        // wiremock's header matcher splits the request value on commas, so
        // pass the expected Accept as its comma-split parts (one part for
        // the single-value formats, three for auto).
        let expected: Vec<&str> = accept.split(", ").collect();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .and(headers("accept", expected))
            .respond_with(ResponseTemplate::new(200).set_body_raw("body", "text/plain"))
            .mount(&server)
            .await;

        let mut input = json!({"url": page_url(&server), "prompt": "x"});
        if let Some(f) = format {
            input["format"] = json!(f);
        }
        // A 200 only comes back when the Accept header matched the mock.
        let out = run(input, &ctx_with_side_query(tmp.path(), sq)).await;
        assert_eq!(out, Ok("ok".to_string()), "format {format:?}");
    }
}

// The extraction result IS the side-query's reply, and the side-query saw the
// verbatim fallback wrapper (with the ORIGINAL url) and the verbatim system
// instruction.
#[tokio::test]
async fn fetch_feeds_the_side_query_and_returns_its_extraction() {
    let tmp = TempDir::new().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sq = Arc::new(FakeSideQuery {
        reply: "spawn with tokio::spawn".into(),
        seen: Arc::clone(&seen),
    });
    let server =
        serve(ResponseTemplate::new(200).set_body_raw("Use tokio::spawn to spawn.", "text/plain"))
            .await;
    let url = page_url(&server);

    let out = run(
        json!({"url": url.clone(), "prompt": "how do I spawn?"}),
        &ctx_with_side_query(tmp.path(), sq),
    )
    .await
    .unwrap();
    assert_eq!(out, "spawn with tokio::spawn");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let req = &seen[0];
    // Verbatim system instruction.
    assert_eq!(
        req.system,
        "Extract and summarize the requested information from the provided web content. \
             Be concise and accurate. Respond only with the requested information."
    );
    // Verbatim fallback wrapper: the prompt, the original url, and the
    // content fenced by `---` lines.
    let expected = format!(
        "The user requested the following: \"how do I spawn?\".\n\nI have fetched the content \
from {url}. Please use the following content to answer the user's request.\n\n---\n\
Use tokio::spawn to spawn.\n---"
    );
    assert_eq!(req.user_content, expected);
    // web_fetch pins the main model by passing None; best-effort single attempt.
    assert!(req.model.is_none());
    assert_eq!(req.max_attempts, 1);
}

// The GitHub blob → raw rewrite (qwen web-fetch.ts): both markers present
// swaps host AND collapses `/blob/`; a URL missing either marker is untouched.
#[test]
fn github_blob_url_is_rewritten_to_raw() {
    assert_eq!(
        github_blob_to_raw("https://github.com/owner/repo/blob/main/src/lib.rs"),
        "https://raw.githubusercontent.com/owner/repo/main/src/lib.rs"
    );
    // A URL missing either marker is untouched (needs both `github.com` and
    // `/blob/`).
    assert_eq!(
        github_blob_to_raw("https://github.com/owner/repo"),
        "https://github.com/owner/repo"
    );
    assert_eq!(
        github_blob_to_raw("https://docs.rs/tokio/blob/x"),
        "https://docs.rs/tokio/blob/x"
    );
}

// The URL actually fetched is the rewritten one - proven at the fetch layer:
// a mock serves the `/blob/`-collapsed path, a URL carrying the `/blob/`
// marker lands there (the rewrite collapsed it), and the fallback prompt still
// cites the ORIGINAL url.
#[tokio::test]
async fn a_blob_marked_url_is_fetched_at_its_rewritten_path() {
    let tmp = TempDir::new().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sq = Arc::new(FakeSideQuery {
        reply: "ok".into(),
        seen: Arc::clone(&seen),
    });
    let mock = MockServer::start().await;
    // The mock host has no literal `github.com`, so we put BOTH markers in the
    // PATH: `.../github.com/.../blob/...`. The rewrite swaps `github.com` →
    // `raw.githubusercontent.com` and collapses `/blob/` → `/`, so the fetched
    // path becomes `.../raw.githubusercontent.com/owner/repo/main/lib.rs`. Only
    // that post-rewrite path serves 200; the pre-rewrite path 404s, so a 200
    // proves the rewrite ran before the fetch.
    Mock::given(method("GET"))
        .and(path("/github.com/owner/repo/blob/main/lib.rs"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/raw.githubusercontent.com/owner/repo/main/lib.rs"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("file body", "text/plain"))
        .mount(&mock)
        .await;
    let blob = format!("{}/github.com/owner/repo/blob/main/lib.rs", mock.uri());
    let out = run(
        json!({"url": blob.clone(), "prompt": "summarize"}),
        &ctx_with_side_query(tmp.path(), sq),
    )
    .await
    .unwrap();
    assert_eq!(out, "ok");
    // The fallback prompt cites the ORIGINAL blob url, not the rewritten one.
    let seen = seen.lock().unwrap();
    assert!(seen[0].user_content.contains(&blob));
}

// Content is capped at MAX_CONTENT_LENGTH before the side-query sees it.
#[tokio::test]
async fn content_is_capped_at_100000_chars() {
    let tmp = TempDir::new().unwrap();
    let big = "a".repeat(MAX_CONTENT_LENGTH + 5_000);
    // The fallback wrapper carries exactly MAX_CONTENT_LENGTH content chars.
    let content = content_seen_for_body(tmp.path(), big).await;
    assert_eq!(content.chars().count(), MAX_CONTENT_LENGTH);
}

// A non-2xx fetch is qwen's `Error: Error during fetch for {url}: {message}`,
// naming the fetched url and carrying the leading `Error: ` prefix.
#[tokio::test]
async fn a_fetch_failure_is_the_qwen_error_shape() {
    let tmp = TempDir::new().unwrap();
    let sq = Arc::new(FakeSideQuery {
        reply: String::new(),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let server = serve(ResponseTemplate::new(404)).await;
    let url = page_url(&server);

    let err = run(
        json!({"url": url.clone(), "prompt": "anything"}),
        &ctx_with_side_query(tmp.path(), sq),
    )
    .await
    .unwrap_err();
    // The full qwen shape: the `Error: ` wrapper, the fetched url, then
    // qwen's verbatim non-ok message `Request failed with status code
    // 404 Not Found` (no repeated url).
    assert_eq!(
        err,
        format!(
            "Error: Error during fetch for {url}: Request failed with status code 404 Not Found"
        )
    );
}

// A side-query failure is also folded into the qwen error shape.
#[tokio::test]
async fn a_side_query_failure_is_the_qwen_error_shape() {
    struct FailingSideQuery;
    #[async_trait::async_trait]
    impl SideQuery for FailingSideQuery {
        async fn run(&self, _request: SideQueryRequest) -> Result<String, String> {
            Err("model unavailable".into())
        }
    }

    let tmp = TempDir::new().unwrap();
    let server = serve(ResponseTemplate::new(200).set_body_raw("body", "text/plain")).await;
    let url = page_url(&server);

    let err = run(
        json!({"url": url.clone(), "prompt": "extract"}),
        &ctx_with_side_query(tmp.path(), Arc::new(FailingSideQuery)),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        format!("Error: Error during fetch for {url}: model unavailable")
    );
}

#[tokio::test]
async fn html_converts_to_readable_text_before_extraction() {
    let tmp = TempDir::new().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sq = Arc::new(FakeSideQuery {
        reply: "ok".into(),
        seen: Arc::clone(&seen),
    });
    let html = "<html><body><h1>Tokio</h1>\
            <p>An <a href=\"https://docs.rs/tokio\">async runtime</a> for Rust.</p>\
            </body></html>";
    let server =
        serve(ResponseTemplate::new(200).set_body_raw(html, "text/html; charset=utf-8")).await;

    run(
        json!({"url": page_url(&server), "prompt": "what is it?"}),
        &ctx_with_side_query(tmp.path(), sq),
    )
    .await
    .unwrap();

    // The side-query saw stripped text, not markup.
    let seen = seen.lock().unwrap();
    let content = &seen[0].user_content;
    assert!(!content.contains("<h1>"));
    assert!(!content.contains("<a href"));
    assert!(content.contains("Tokio"));
    assert!(content.contains("async runtime"));
    // qwen drops link targets (`ignoreHref: true`): the href URL must NOT
    // appear, neither inline nor as a trailing footnote.
    assert!(
        !content.contains("https://docs.rs/tokio"),
        "link target leaked into converted text: {content:?}"
    );
}

#[tokio::test]
async fn non_http_schemes_are_rejected() {
    let tmp = TempDir::new().unwrap();
    let sq = Arc::new(FakeSideQuery {
        reply: String::new(),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let c = ctx_with_side_query(tmp.path(), sq);
    for url in ["ftp://example.com/file", "file:///etc/passwd"] {
        assert_eq!(
            run(json!({"url": url, "prompt": "x"}), &c).await,
            Err("The 'url' must be a valid URL starting with http:// or https://.".to_string())
        );
    }
}

#[tokio::test]
async fn an_unparseable_url_is_the_qwen_invalid_url_message() {
    let tmp = TempDir::new().unwrap();
    let sq = Arc::new(FakeSideQuery {
        reply: String::new(),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let err = run(
        json!({"url": "not a url", "prompt": "x"}),
        &ctx_with_side_query(tmp.path(), sq),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err,
        "The 'url' must be a valid URL starting with http:// or https://."
    );
}

#[tokio::test]
async fn missing_empty_url_or_prompt_is_the_qwen_verbatim_message() {
    let tmp = TempDir::new().unwrap();
    let sq = Arc::new(FakeSideQuery {
        reply: String::new(),
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let c = ctx_with_side_query(tmp.path(), sq);
    // Missing/empty/wrong-typed url is the verbatim empty-url message.
    for input in [
        json!({"prompt": "x"}),
        json!({"url": "", "prompt": "x"}),
        json!({"url": 42, "prompt": "x"}),
    ] {
        assert_eq!(
            run(input, &c).await,
            Err("The 'url' parameter cannot be empty.".to_string())
        );
    }
    // Missing/empty prompt is the verbatim empty-prompt message.
    for input in [
        json!({"url": "https://example.com"}),
        json!({"url": "https://example.com", "prompt": ""}),
    ] {
        assert_eq!(
            run(input, &c).await,
            Err("The 'prompt' parameter cannot be empty.".to_string())
        );
    }
}

// qwen NEVER rejects a content type: anything that is not text/markdown or
// text/plain is run through HTML-to-text and handed to the side-query. An
// arbitrary content type (here `application/octet-stream`) succeeds, not
// errors.
#[tokio::test]
async fn an_arbitrary_content_type_is_converted_never_rejected() {
    let tmp = TempDir::new().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sq = Arc::new(FakeSideQuery {
        reply: "ok".into(),
        seen: Arc::clone(&seen),
    });
    let server = serve(
        ResponseTemplate::new(200).set_body_raw("<p>hello world</p>", "application/octet-stream"),
    )
    .await;

    let out = run(
        json!({"url": page_url(&server), "prompt": "x"}),
        &ctx_with_side_query(tmp.path(), sq),
    )
    .await;
    // No error, and the converted (markup-stripped) text reached the
    // side-query.
    assert_eq!(out, Ok("ok".to_string()));
    let seen = seen.lock().unwrap();
    assert!(seen[0].user_content.contains("hello world"));
    assert!(!seen[0].user_content.contains("<p>"));
}

// text/markdown passes through raw (not HTML-converted).
#[tokio::test]
async fn markdown_content_passes_through_raw() {
    let tmp = TempDir::new().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sq = Arc::new(FakeSideQuery {
        reply: "ok".into(),
        seen: Arc::clone(&seen),
    });
    let md = "# Title\n\n- one\n- two";
    let server = serve(ResponseTemplate::new(200).set_body_raw(md, "text/markdown")).await;

    run(
        json!({"url": page_url(&server), "prompt": "x"}),
        &ctx_with_side_query(tmp.path(), sq),
    )
    .await
    .unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(fenced_content(&seen), md);
}

#[tokio::test]
async fn the_download_guard_cuts_the_body_at_2mb() {
    let tmp = TempDir::new().unwrap();
    // A body over both the 2 MB download guard and the content cap: the guard
    // trims to 2 MB, then the content cap trims to MAX_CONTENT_LENGTH.
    let big = "a".repeat(MAX_BODY_BYTES + 500_000);
    // The content the side-query saw is capped at MAX_CONTENT_LENGTH (well
    // under the 2 MB guard), proving both trims ran.
    let content = content_seen_for_body(tmp.path(), big).await;
    assert_eq!(content.chars().count(), MAX_CONTENT_LENGTH);
}
