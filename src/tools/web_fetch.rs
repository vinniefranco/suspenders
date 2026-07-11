//! `web_fetch(url)`: fetches an http/https URL and returns its readable text.
//! The first tool that reaches outside the Project Root, so every call passes
//! the Approval gate showing the full URL (ADR-0024); Standing Approval matches
//! the exact URL string, same no-widening rule as run_command (ADR-0005).
//!
//! HTML converts to readable text via html2text; other `text/*` and JSON pass
//! through raw; anything else is an error. The download itself is guarded at
//! 2 MB — the Result Cap (Shaping) handles the Conversation-side size, but the
//! guard keeps a huge file from streaming into memory first.

use crate::tool::{Tool, ToolCtx, ToolSpec};
use serde_json::{json, Value};

pub struct WebFetch;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// The download guard: read at most this many body bytes (ADR-0024).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Render width for the HTML → text conversion: wide, so code blocks and
/// tables survive without aggressive rewrapping.
const RENDER_WIDTH: usize = 100;

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: "Fetch a web page or documentation URL over http/https and return its \
                readable text (HTML is converted to plain text). Use this to look up \
                documentation for a library, API, or error message. Large pages are truncated. \
                The user must approve each URL before it is fetched."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The full http/https URL to fetch, e.g. \"https://docs.rs/tokio\"."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        let url = match input.get("url") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err("invalid input: web_fetch requires a non-empty string \"url\"".into()),
        };

        let parsed = reqwest::Url::parse(&url).map_err(|err| format!("invalid URL: {err}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("web_fetch only supports http/https URLs".into());
        }

        let timeout = if ctx.command_timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            ctx.command_timeout_ms
        };

        fetch(parsed, timeout).await
    }
}

async fn fetch(url: reqwest::Url, timeout_ms: u64) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .map_err(|err| format!("could not build http client: {err}"))?;

    let response = client.get(url.clone()).send().await.map_err(|err| err.to_string())?;

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

    let (body, cut) = read_capped(response).await?;
    let text = readable_text(&content_type, &body)?;

    if cut {
        Ok(format!("{text}\n[fetch cut at 2MB]"))
    } else {
        Ok(text)
    }
}

// The download guard: accumulate body chunks up to MAX_BODY_BYTES, then stop
// reading. Returns the (possibly cut) bytes and whether the guard was hit.
async fn read_capped(mut response: reqwest::Response) -> Result<(Vec<u8>, bool), String> {
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            body.extend_from_slice(&chunk[..MAX_BODY_BYTES - body.len()]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
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
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx(root: &std::path::Path) -> ToolCtx {
        ToolCtx {
            root: root.to_path_buf(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
            scout: None,
        }
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
    fn spec_requires_url() {
        let spec = WebFetch.spec();
        assert_eq!(spec.name, "web_fetch");
        assert_eq!(spec.input_schema["required"], serde_json::json!(["url"]));
        assert!(spec.input_schema["properties"]["url"].is_object());
    }

    #[tokio::test]
    async fn plain_text_passes_through_raw() {
        let tmp = TempDir::new().unwrap();
        let server = serve(
            ResponseTemplate::new(200)
                .set_body_raw("hello plain text", "text/plain; charset=utf-8"),
        )
        .await;

        let out = run(json!({"url": format!("{}/page", server.uri())}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(out, "hello plain text");
    }

    #[tokio::test]
    async fn json_passes_through_raw() {
        let tmp = TempDir::new().unwrap();
        let server = serve(
            ResponseTemplate::new(200).set_body_raw(r#"{"a": 1}"#, "application/json"),
        )
        .await;

        let out = run(json!({"url": format!("{}/page", server.uri())}), &ctx(tmp.path()))
            .await
            .unwrap();
        assert_eq!(out, r#"{"a": 1}"#);
    }

    #[tokio::test]
    async fn html_converts_to_readable_text() {
        let tmp = TempDir::new().unwrap();
        let html = "<html><body><h1>Tokio</h1>\
            <p>An <a href=\"https://docs.rs/tokio\">async runtime</a> for Rust.</p>\
            </body></html>";
        let server =
            serve(ResponseTemplate::new(200).set_body_raw(html, "text/html; charset=utf-8")).await;

        let out = run(json!({"url": format!("{}/page", server.uri())}), &ctx(tmp.path()))
            .await
            .unwrap();
        // Tags stripped, link text preserved.
        assert!(!out.contains("<h1>"));
        assert!(!out.contains("<a href"));
        assert!(out.contains("Tokio"));
        assert!(out.contains("async runtime"));
    }

    #[tokio::test]
    async fn non_2xx_status_is_an_error_naming_status_and_url() {
        let tmp = TempDir::new().unwrap();
        let server = serve(ResponseTemplate::new(404)).await;
        let url = format!("{}/page", server.uri());

        let err = run(json!({"url": url.clone()}), &ctx(tmp.path())).await.unwrap_err();
        assert!(err.contains("HTTP 404"));
        assert!(err.contains("/page"));
    }

    #[tokio::test]
    async fn connection_refused_is_an_error() {
        let tmp = TempDir::new().unwrap();
        // Bind then drop a listener: the port exists but nothing serves it.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };

        let err = run(
            json!({"url": format!("http://127.0.0.1:{port}/page")}),
            &ctx(tmp.path()),
        )
        .await
        .unwrap_err();
        assert!(!err.is_empty());
    }

    #[tokio::test]
    async fn non_http_schemes_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        for url in ["ftp://example.com/file", "file:///etc/passwd"] {
            assert_eq!(
                run(json!({"url": url}), &c).await,
                Err("web_fetch only supports http/https URLs".to_string())
            );
        }
    }

    #[tokio::test]
    async fn an_unparseable_url_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let err = run(json!({"url": "not a url"}), &ctx(tmp.path())).await.unwrap_err();
        assert!(err.contains("invalid URL"));
    }

    #[tokio::test]
    async fn missing_empty_or_non_string_url_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let c = ctx(tmp.path());
        assert!(run(json!({}), &c).await.is_err());
        assert!(run(json!({"url": ""}), &c).await.is_err());
        assert!(run(json!({"url": 42}), &c).await.is_err());
    }

    #[tokio::test]
    async fn unsupported_content_type_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let server = serve(
            ResponseTemplate::new(200).set_body_raw(vec![0u8, 1, 2], "application/octet-stream"),
        )
        .await;

        let err = run(json!({"url": format!("{}/page", server.uri())}), &ctx(tmp.path()))
            .await
            .unwrap_err();
        assert_eq!(err, "unsupported content type: application/octet-stream");
    }

    #[tokio::test]
    async fn the_download_guard_cuts_the_body_at_2mb_and_says_so() {
        let tmp = TempDir::new().unwrap();
        let big = "a".repeat(MAX_BODY_BYTES + 500_000);
        let server = serve(ResponseTemplate::new(200).set_body_raw(big, "text/plain")).await;

        let mut c = ctx(tmp.path());
        c.result_cap = usize::MAX; // the guard, not Shaping, is under test
        let out = run(json!({"url": format!("{}/page", server.uri())}), &c)
            .await
            .unwrap();
        assert!(out.ends_with("\n[fetch cut at 2MB]"));
        assert_eq!(out.len(), MAX_BODY_BYTES + "\n[fetch cut at 2MB]".len());
    }
}
