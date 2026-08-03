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
    // Read-only (qwen web-fetch.ts:752 `Kind.Fetch`): ALLOWED in plan mode, but
    // still gated (its confirmation is qwen's `type: 'info'`, so plan mode does
    // not block it - it falls through to the normal domain-scoped Ask).
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Fetch
    }

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
#[path = "../../tests/tools/web_fetch.rs"]
mod tests;
