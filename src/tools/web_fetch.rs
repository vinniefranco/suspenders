//! `web_fetch(url, prompt)`: fetches an http/https URL and runs a prompt-guided
//! extraction over its readable text (faithful to qwen web-fetch.ts). The first
//! tool that reaches outside the Project Root, so every call passes the Approval
//! gate - but the gate is DOMAIN-scoped now (ADR-0024, revised): a Standing
//! Approval matches the URL's hostname, so a second fetch to the same host
//! auto-approves. This mirrors qwen's `WebFetch(<hostname>)` permission rule.
//!
//! The tool fetches the URL (HTML converts to readable text via html2text;
//! other `text/*` and JSON pass through raw), caps the content at 100 000 chars,
//! then hands it to a bounded model SIDE-QUERY ([`crate::tool::caps::SideQuery`],
//! ADR-0055) with the user's `prompt`: the model extracts and summarizes what
//! the prompt asked for and its reply is the tool's result. On any failure the
//! result is qwen's `Error during fetch for {url}: {message}`.
//!
//! The download itself is guarded at 2 MB - the Result Cap (Shaping) handles the
//! Conversation-side size, but the guard keeps a huge file from streaming into
//! memory before the content cap trims it.

use crate::tool::caps::SideQueryRequest;
use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{Value, json};

pub struct WebFetch;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// The download guard: read at most this many body bytes (ADR-0024).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// The content cap handed to the side-query (qwen `MAX_CONTENT_LENGTH`): trim
/// the readable text to this many chars before extraction, so a huge page never
/// blows the side-query's own context.
const MAX_CONTENT_LENGTH: usize = 100_000;

/// Render width for the HTML → text conversion: wide, so code blocks and
/// tables survive without aggressive rewrapping.
const RENDER_WIDTH: usize = 100;

/// The side-query's system instruction, VERBATIM from qwen web-fetch.ts: the
/// model is told to extract and summarize only what the prompt asked for.
const EXTRACTION_INSTRUCTION: &str = "Extract and summarize the requested information from the provided web content. Be concise and accurate. Respond only with the requested information.";

/// The invalid-url param-validation message, VERBATIM from qwen web-fetch.ts: a
/// url that will not parse or is not http/https earns this, not the fetch-failure
/// shape.
const INVALID_URL_MESSAGE: &str =
    "The 'url' must be a valid URL starting with http:// or https://.";

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: "Fetch an http/https URL and process its content with a prompt using an \
                AI model. \
                Usage:\n\
                - Takes a URL and a prompt describing what to extract from the page.\n\
                - HTML is converted to plain text; other text/* and JSON are read raw.\n\
                - The fetched content is summarized by a model against your prompt - use this to \
                look up documentation for a library, API, or an error message, \
                e.g. url \"https://docs.rs/tokio\", prompt \"how do I spawn a task?\".\n\
                - Large pages are truncated automatically before processing.\n\
                - The user must approve the URL's domain before it is fetched."
                .into(),
            // Schema note: qwen carries an optional `format` enum that steers ONLY
            // the Accept header (auto/markdown/html/text); suspenders keeps its own
            // content negotiation (HTML→text, text/*+JSON raw) and does not expose
            // `format`, so the schema stays url + prompt required. If a host wants
            // Accept-header steering later, `format` rides here without touching the
            // extraction path.
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The full http/https URL to fetch, e.g. \
                            \"https://docs.rs/tokio\"."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "What to extract from the page, e.g. \
                            \"how do I spawn a task?\"."
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

        // GitHub blob → raw rewrite (qwen web-fetch.ts): a blob page is HTML
        // chrome around the file; the raw host serves the file itself. The
        // rewritten URL is what we fetch AND what the error names (qwen's `url`
        // local), while the fallback prompt cites the ORIGINAL url (qwen's
        // `this.params.url`).
        let fetch_url = github_blob_to_raw(&url);

        // An unparseable url or a non-http/https scheme is qwen's verbatim
        // param-validation message, not the fetch-failure shape.
        let parsed = reqwest::Url::parse(&fetch_url)
            .map_err(|_| INVALID_URL_MESSAGE.to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(INVALID_URL_MESSAGE.into());
        }

        let timeout = if ctx.command_timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            ctx.command_timeout_ms
        };

        // Fetch the readable text, capped for the side-query. A fetch failure is
        // already qwen's `Error during fetch for {url}: {message}`.
        let content = match fetch(parsed, timeout).await {
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

/// qwen's fetch-failure shape (web-fetch.ts): `Error during fetch for {url}:
/// {message}`. The `{url}` is the (rewritten) URL actually fetched.
fn fetch_error(url: &str, message: &str) -> String {
    format!("Error during fetch for {url}: {message}")
}

async fn fetch(url: reqwest::Url, timeout_ms: u64) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .map_err(|err| format!("could not build http client: {err}"))?;

    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|err| err.to_string())?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} fetching {url}"));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body = read_capped(response).await?;
    readable_text(&content_type, &body)
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

// HTML converts to readable text; other text/* and JSON pass through raw;
// anything else is unsupported.
fn readable_text(content_type: &str, body: &[u8]) -> Result<String, String> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if ct.contains("text/html") {
        html2text::config::plain()
            .string_from_read(body, RENDER_WIDTH)
            .map_err(|err| format!("could not convert HTML to text: {err}"))
    } else if ct.starts_with("text/") || ct == "application/json" {
        Ok(String::from_utf8_lossy(body).into_owned())
    } else {
        Err(format!("unsupported content type: {content_type}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::caps::{Capabilities, SideQuery};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
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

    #[test]
    fn spec_requires_url_and_prompt() {
        let spec = WebFetch.spec();
        assert_eq!(spec.name, "web_fetch");
        assert_eq!(
            spec.input_schema["required"],
            serde_json::json!(["url", "prompt"])
        );
        assert!(spec.input_schema["properties"]["url"].is_object());
        assert!(spec.input_schema["properties"]["prompt"].is_object());
        // No `format` enum: suspenders keeps its own content negotiation.
        assert!(spec.input_schema["properties"]["format"].is_null());
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
        let url = format!("{}/page", server.uri());

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
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sq = Arc::new(FakeSideQuery {
            reply: "ok".into(),
            seen: Arc::clone(&seen),
        });
        let big = "a".repeat(MAX_CONTENT_LENGTH + 5_000);
        let server = serve(ResponseTemplate::new(200).set_body_raw(big, "text/plain")).await;

        let mut c = ctx_with_side_query(tmp.path(), sq);
        c.result_cap = usize::MAX;
        run(
            json!({"url": format!("{}/page", server.uri()), "prompt": "count the a's"}),
            &c,
        )
        .await
        .unwrap();

        // The fallback wrapper carries exactly MAX_CONTENT_LENGTH content chars.
        let seen = seen.lock().unwrap();
        let content_between = seen[0]
            .user_content
            .split("---\n")
            .nth(1)
            .unwrap()
            .trim_end_matches("\n---");
        assert_eq!(content_between.chars().count(), MAX_CONTENT_LENGTH);
    }

    // A non-2xx fetch is qwen's `Error during fetch for {url}: {message}`, naming
    // the fetched url.
    #[tokio::test]
    async fn a_fetch_failure_is_the_qwen_error_shape() {
        let tmp = TempDir::new().unwrap();
        let sq = Arc::new(FakeSideQuery {
            reply: String::new(),
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let server = serve(ResponseTemplate::new(404)).await;
        let url = format!("{}/page", server.uri());

        let err = run(
            json!({"url": url.clone(), "prompt": "anything"}),
            &ctx_with_side_query(tmp.path(), sq),
        )
        .await
        .unwrap_err();
        assert!(err.starts_with(&format!("Error during fetch for {url}: ")));
        assert!(err.contains("HTTP 404"));
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
        let server =
            serve(ResponseTemplate::new(200).set_body_raw("body", "text/plain")).await;
        let url = format!("{}/page", server.uri());

        let err = run(
            json!({"url": url.clone(), "prompt": "extract"}),
            &ctx_with_side_query(tmp.path(), Arc::new(FailingSideQuery)),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            format!("Error during fetch for {url}: model unavailable")
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
            json!({"url": format!("{}/page", server.uri()), "prompt": "what is it?"}),
            &ctx_with_side_query(tmp.path(), sq),
        )
        .await
        .unwrap();

        // The side-query saw stripped text, not markup.
        let seen = seen.lock().unwrap();
        assert!(!seen[0].user_content.contains("<h1>"));
        assert!(!seen[0].user_content.contains("<a href"));
        assert!(seen[0].user_content.contains("Tokio"));
        assert!(seen[0].user_content.contains("async runtime"));
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

    #[tokio::test]
    async fn unsupported_content_type_is_the_qwen_error_shape() {
        let tmp = TempDir::new().unwrap();
        let sq = Arc::new(FakeSideQuery {
            reply: String::new(),
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let server = serve(
            ResponseTemplate::new(200).set_body_raw(vec![0u8, 1, 2], "application/octet-stream"),
        )
        .await;
        let url = format!("{}/page", server.uri());

        let err = run(
            json!({"url": url.clone(), "prompt": "x"}),
            &ctx_with_side_query(tmp.path(), sq),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            format!(
                "Error during fetch for {url}: unsupported content type: application/octet-stream"
            )
        );
    }

    #[tokio::test]
    async fn the_download_guard_cuts_the_body_at_2mb() {
        let tmp = TempDir::new().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sq = Arc::new(FakeSideQuery {
            reply: "ok".into(),
            seen: Arc::clone(&seen),
        });
        // A body over both the 2 MB download guard and the content cap: the guard
        // trims to 2 MB, then the content cap trims to MAX_CONTENT_LENGTH.
        let big = "a".repeat(MAX_BODY_BYTES + 500_000);
        let server = serve(ResponseTemplate::new(200).set_body_raw(big, "text/plain")).await;

        let mut c = ctx_with_side_query(tmp.path(), sq);
        c.result_cap = usize::MAX;
        run(
            json!({"url": format!("{}/page", server.uri()), "prompt": "x"}),
            &c,
        )
        .await
        .unwrap();

        // The content the side-query saw is capped at MAX_CONTENT_LENGTH (well
        // under the 2 MB guard), proving both trims ran.
        let seen = seen.lock().unwrap();
        let content_between = seen[0]
            .user_content
            .split("---\n")
            .nth(1)
            .unwrap()
            .trim_end_matches("\n---");
        assert_eq!(content_between.chars().count(), MAX_CONTENT_LENGTH);
    }
}
