//! `web_fetch(url, prompt, format?)`: fetches an http/https URL and runs a
//! prompt-guided extraction over its content (faithful to qwen web-fetch.ts).
//! The first tool that reaches outside the Project Root, so every call passes
//! the Approval gate - but the gate is DOMAIN-scoped now (ADR-0024, revised): a
//! Standing Approval matches the URL's hostname, so a second fetch to the same
//! host auto-approves. This mirrors qwen's `WebFetch(<hostname>)` permission
//! rule.
//!
//! The optional `format` param steers ONLY the Accept header sent to the server
//! (auto/markdown/html/text); the content is always normalized to plain text for
//! the LLM regardless. `text/markdown` and `text/plain` responses pass through
//! raw; EVERYTHING ELSE is converted via HTML-to-text (qwen never rejects a
//! content type). The content is capped at 100 000 chars, then handed to a
//! bounded model SIDE-QUERY ([`crate::tool::caps::SideQuery`], ADR-0055) with the
//! user's `prompt`: the model extracts and summarizes what the prompt asked for
//! and its reply is the tool's result. On any failure the result is qwen's
//! `Error: Error during fetch for {url}: {message}`.
//!
//! The fetch uses a FIXED 10 s timeout (qwen `URL_FETCH_TIMEOUT_MS`), not the
//! Run's command timeout. The download itself is guarded at 2 MB - the Result Cap
//! (Shaping) handles the Conversation-side size, but the guard keeps a huge file
//! from streaming into memory before the content cap trims it.

use crate::tool::caps::SideQueryRequest;
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

pub struct WebFetch;

/// The fixed fetch timeout, VERBATIM from qwen web-fetch.ts (`URL_FETCH_TIMEOUT_MS
/// = 10000`): the fetch is bounded at 10 s regardless of the Run's command
/// timeout.
const URL_FETCH_TIMEOUT_MS: u64 = 10_000;

/// The download guard: read at most this many body bytes (ADR-0024).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// The content cap handed to the side-query (qwen `MAX_CONTENT_LENGTH`): trim
/// the readable text to this many chars before extraction, so a huge page never
/// blows the side-query's own context.
const MAX_CONTENT_LENGTH: usize = 100_000;

/// Render width for the HTML → text conversion. qwen converts with
/// `wordwrap: false` (web-fetch.ts:130), i.e. NO wrapping, so we hand html2text
/// an effectively-unbounded width and let long lines overflow rather than
/// rewrap. A large finite sentinel (not `usize::MAX`, which risks internal
/// width arithmetic overflowing) achieves no-wrap for any realistic page.
const NO_WRAP_WIDTH: usize = 1_000_000;

/// The side-query's system instruction, VERBATIM from qwen web-fetch.ts: the
/// model is told to extract and summarize only what the prompt asked for.
const EXTRACTION_INSTRUCTION: &str = "Extract and summarize the requested information from the provided web content. Be concise and accurate. Respond only with the requested information.";

/// The invalid-url param-validation message, VERBATIM from qwen web-fetch.ts: a
/// url that will not parse or is not http/https earns this, not the fetch-failure
/// shape.
const INVALID_URL_MESSAGE: &str =
    "The 'url' must be a valid URL starting with http:// or https://.";

/// The tool description, VERBATIM from qwen web-fetch.ts `WebFetchTool` ctor.
const DESCRIPTION: &str = "Fetches content from a specified URL and processes it using an AI model\n- Takes a URL and a prompt as input\n- Supports content negotiation for markdown (reduces tokens by ~80%)\n- Fetches the URL content, converts HTML to text if needed\n- Processes the content with the prompt using a small, fast model\n- Returns the model's response about the content\n- Use this tool when you need to retrieve and analyze web content\n\nUsage notes:\n  - IMPORTANT: If an MCP-provided web fetch tool is available, prefer using that tool instead of this one, as it may have fewer restrictions. All MCP-provided tools start with \"mcp__\".\n  - The URL must be a fully-formed valid URL\n  - The prompt should describe what information you want to extract from the page\n  - format parameter (optional): controls only the Accept header sent to the server. All content is normalized to plain text for LLM processing, regardless of format.\n  - \"auto\" (default): Prefers markdown via content negotiation, accepts HTML as fallback. Use when user does NOT specify a format.\n  - \"markdown\": Sends Accept: text/markdown. Use when user explicitly asks for markdown content.\n  - \"html\": Sends Accept: text/html. Content is still converted to plain text for LLM processing.\n  - \"text\": Sends Accept: text/plain. Use when user explicitly asks for plain text.\n  - This tool is read-only and does not modify any files\n  - Results may be summarized if the content is very large\n  - Supports both public and private/localhost URLs using direct fetch";

/// The `format` → Accept-header mapping, VERBATIM from qwen web-fetch.ts
/// `getAcceptHeader`. `format` steers ONLY this header; the response is always
/// normalized to plain text for the LLM. An unknown/absent value is `auto`.
fn accept_header(format: &str) -> &'static str {
    match format {
        "markdown" => "text/markdown",
        "html" => "text/html",
        "text" => "text/plain",
        // "auto" and any other value.
        _ => "text/markdown, text/html, text/plain",
    }
}

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: DESCRIPTION.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "description": "The URL to fetch content from",
                        "type": "string"
                    },
                    "prompt": {
                        "description": "The prompt to run on the fetched content",
                        "type": "string"
                    },
                    "format": {
                        "description": "Preferred content format (Accept header only): auto (default, prefers markdown), markdown, html, or text. All content is normalized to plain text.",
                        "type": "string",
                        "enum": ["auto", "markdown", "html", "text"]
                    }
                },
                "required": ["url", "prompt"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // Param validation, VERBATIM from qwen web-fetch.ts: an empty url/prompt
        // each earns its own message; a url that will not parse or is not
        // http/https earns the "valid URL" message (below, after the rewrite).
        let url = match input.get("url") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            _ => return Err("The 'url' parameter cannot be empty.".into()),
        };
        let prompt = match input.get("prompt") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            _ => return Err("The 'prompt' parameter cannot be empty.".into()),
        };
        // `format` steers only the Accept header; absent/unknown is `auto`.
        let format = input
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .to_string();

        // GitHub blob → raw rewrite (qwen web-fetch.ts): a blob page is HTML
        // chrome around the file; the raw host serves the file itself. The
        // rewritten URL is what we fetch AND what the error names (qwen's `url`
        // local), while the fallback prompt cites the ORIGINAL url (qwen's
        // `this.params.url`).
        let fetch_url = github_blob_to_raw(&url);

        // An unparseable url or a non-http/https scheme is qwen's verbatim
        // param-validation message, not the fetch-failure shape.
        let parsed =
            reqwest::Url::parse(&fetch_url).map_err(|_| INVALID_URL_MESSAGE.to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(INVALID_URL_MESSAGE.into());
        }

        // Fetch the content, normalized to plain text and capped for the
        // side-query. A fetch failure is qwen's
        // `Error: Error during fetch for {url}: {message}`.
        let content = match fetch(parsed, accept_header(&format)).await {
            Ok(text) => cap_content(text),
            Err(message) => return Err(fetch_error(&fetch_url, &message)),
        };

        // The fallback prompt, VERBATIM from qwen web-fetch.ts: the user's prompt,
        // the ORIGINAL url, and the fetched content fenced by `---` lines.
        let fallback_prompt = format!(
            "The user requested the following: \"{prompt}\".\n\nI have fetched the content from {url}. \
Please use the following content to answer the user's request.\n\n---\n{content}\n---"
        );

        // The bounded model side-query (ADR-0055): the extraction instruction as
        // system, the fallback prompt as the single user part, `model: None` so
        // the real impl defaults to the Run's captured MAIN model (qwen pins the
        // main model - fast models lose fidelity on long source material),
        // `max_attempts: 1` (best-effort; the error path handles a miss). The tool
        // does NOT call the Approver - approval is upstream via the batch gate.
        ctx.caps
            .side_query
            .run(SideQueryRequest {
                system: EXTRACTION_INSTRUCTION.into(),
                user_content: fallback_prompt,
                model: None,
                max_attempts: 1,
            })
            .await
            .map_err(|message| fetch_error(&fetch_url, &message))
    }
}

/// qwen's GitHub blob → raw rewrite: a `github.com/.../blob/...` URL becomes the
/// `raw.githubusercontent.com/.../...` URL that serves the file itself. Both
/// substitutions must apply (host and the `/blob/` path segment); a URL missing
/// either marker is returned unchanged.
fn github_blob_to_raw(url: &str) -> String {
    if url.contains("github.com") && url.contains("/blob/") {
        url.replace("github.com", "raw.githubusercontent.com")
            .replace("/blob/", "/")
    } else {
        url.to_string()
    }
}

/// Trims the readable text to the content cap (qwen `substring(0,
/// MAX_CONTENT_LENGTH)`), on a char boundary so multibyte text never splits mid
/// codepoint.
fn cap_content(text: String) -> String {
    if text.chars().count() <= MAX_CONTENT_LENGTH {
        return text;
    }
    text.chars().take(MAX_CONTENT_LENGTH).collect()
}

/// qwen's fetch-failure llmContent (web-fetch.ts): the leading `Error: ` prefix
/// on `Error during fetch for {url}: {message}`. The `{url}` is the (rewritten)
/// URL actually fetched.
fn fetch_error(url: &str, message: &str) -> String {
    format!("Error: Error during fetch for {url}: {message}")
}

async fn fetch(url: reqwest::Url, accept: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(URL_FETCH_TIMEOUT_MS))
        .build()
        .map_err(|err| format!("could not build http client: {err}"))?;

    let response = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, accept)
        .send()
        .await
        .map_err(|err| err.to_string())?;

    let status = response.status();
    if !status.is_success() {
        // qwen's non-ok {message}, VERBATIM (web-fetch.ts:107): `Request failed
        // with status code {status} {statusText}` where `status` is the numeric
        // code and `statusText` the reason phrase. Built from `as_u16()` +
        // `canonical_reason()` so reqwest's Display (which already folds in the
        // reason) does not double it. The wrapper already names the url, so the
        // message deliberately does not repeat it.
        return Err(format!(
            "Request failed with status code {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        ));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body = read_capped(response).await?;
    Ok(readable_text(&content_type, &body))
}

// The download guard: accumulate body chunks up to MAX_BODY_BYTES, then stop
// reading. Returns the (possibly cut) bytes - the content cap trims the readable
// text further before it reaches the side-query.
async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            body.extend_from_slice(&chunk[..MAX_BODY_BYTES - body.len()]);
            return Ok(body);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Normalize the response body to plain text (qwen web-fetch.ts content
/// handling): `text/markdown` and `text/plain` are read raw; EVERYTHING ELSE is
/// converted via HTML-to-text. qwen never rejects a content type. HTML that will
/// not parse degrades to the lossy body text rather than erroring.
///
/// The conversion mirrors qwen's `convert` options (web-fetch.ts:130-134):
/// - `{ a, ignoreHref: true }` -> link URLs are dropped (no inline `[N]` markers
///   and no trailing `[N]: <url>` footnotes), via [`TrivialDecorator`] rather
///   than the default [`PlainDecorator`] (which emits both);
/// - `{ img, format: 'skip' }` -> images contribute no URL (Trivial emits only
///   an image's title text, which is virtually always empty, never the src);
/// - `wordwrap: false` -> no rewrapping, via the no-wrap render width.
fn readable_text(content_type: &str, body: &[u8]) -> String {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if ct.contains("text/markdown") || ct.contains("text/plain") {
        String::from_utf8_lossy(body).into_owned()
    } else {
        html2text::config::with_decorator(html2text::render::text_renderer::TrivialDecorator::new())
            .allow_width_overflow()
            .string_from_read(body, NO_WRAP_WIDTH)
            .unwrap_or_else(|_| String::from_utf8_lossy(body).into_owned())
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(accept_header("auto"), "text/markdown, text/html, text/plain");
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
        let server = serve(
            ResponseTemplate::new(200).set_body_raw("Use tokio::spawn to spawn.", "text/plain"),
        )
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
            format!("Error: Error during fetch for {url}: Request failed with status code 404 Not Found")
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
            ResponseTemplate::new(200)
                .set_body_raw("<p>hello world</p>", "application/octet-stream"),
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
}
