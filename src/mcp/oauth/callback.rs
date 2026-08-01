//! The hand-rolled localhost callback server (qwen's `startCallbackServer`),
//! off the async runtime on a blocking thread: bind the redirect port, accept one
//! connection, parse the `code`/`state`/`error`, validate the CSRF state, answer a
//! tiny HTML page, and return the code. Built on `std::net` (no web framework).

use std::io::{Read, Write};
use std::net::TcpListener;

use crate::mcp::oauth::http::percent_decode;
use crate::mcp::oauth::{CALLBACK_TIMEOUT_SECS, OAUTH_REDIRECT_PATH, OAUTH_REDIRECT_PORT};

/// How long a single accepted callback socket is given to deliver its request
/// before the read gives up (the browser redirect arrives immediately once the
/// socket connects).
const CALLBACK_READ_TIMEOUT_SECS: u64 = 5;

/// How long the accept loop sleeps between non-blocking `accept` polls while it
/// waits for the browser redirect (a coarse busy-wait bounded by the deadline).
const CALLBACK_POLL_MS: u64 = 50;

/// The read buffer for one callback HTTP request line (the redirect is a short
/// `GET`, so a single 4 KiB read captures the request line + headers).
const CALLBACK_READ_BUF: usize = 4096;

/// The HTTP status codes the callback page answers with (no framework, so the
/// status line is assembled by hand).
const HTTP_OK: u16 = 200;
const HTTP_BAD_REQUEST: u16 = 400;
const HTTP_NOT_FOUND: u16 = 404;

/// Runs the localhost callback server for one flow (qwen `startCallbackServer`),
/// off the async runtime on a blocking thread: bind the redirect port, accept one
/// connection, parse the `code`/`state`/`error` from its request line, validate
/// the state (CSRF guard), answer a tiny HTML page, and return the code. Times
/// out after [`CALLBACK_TIMEOUT_SECS`]. Hand-rolled on `std::net` (no web
/// framework), matching the "no new dep" constraint.
pub(super) async fn wait_for_callback(expected_state: &str) -> Result<String, String> {
    let expected_state = expected_state.to_string();
    tokio::task::spawn_blocking(move || callback_blocking(&expected_state))
        .await
        .map_err(|e| format!("OAuth callback task failed: {e}"))?
}

/// The blocking half of [`wait_for_callback`]: bind, accept, parse, respond.
/// Separated so the async wrapper is a thin `spawn_blocking`.
fn callback_blocking(expected_state: &str) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", OAUTH_REDIRECT_PORT)).map_err(|e| {
        format!("OAuth callback server could not bind port {OAUTH_REDIRECT_PORT}: {e}")
    })?;
    listener
        .set_nonblocking(false)
        .map_err(|e| format!("OAuth callback server setup failed: {e}"))?;

    // A crude accept timeout: block up to the deadline on one connection. The
    // std listener has no accept-timeout, so a per-accept read timeout on the
    // accepted stream bounds the whole wait closely enough (the browser redirect
    // arrives in seconds; a stalled flow times out at CALLBACK_TIMEOUT_SECS).
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(CALLBACK_TIMEOUT_SECS);
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("OAuth callback server setup failed: {e}"))?;

    loop {
        if std::time::Instant::now() >= deadline {
            return Err("OAuth callback timeout".to_string());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(
                        CALLBACK_READ_TIMEOUT_SECS,
                    )))
                    .ok();
                return handle_callback_stream(&mut stream, expected_state);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(CALLBACK_POLL_MS));
            }
            Err(e) => return Err(format!("OAuth callback accept failed: {e}")),
        }
    }
}

/// Parses one HTTP request off the callback socket and answers it (qwen's request
/// handler): read the request line, pull `code`/`state`/`error` from the query,
/// validate the path + state, write the success/error HTML, and yield the code.
/// Split out so the request handling is pure-ish (only the socket is impure) and
/// the callback-query parsing ([`parse_callback_query`]) is unit-tested apart.
fn handle_callback_stream(
    stream: &mut std::net::TcpStream,
    expected_state: &str,
) -> Result<String, String> {
    let mut buf = [0u8; CALLBACK_READ_BUF];
    let n = stream
        .read(&mut buf)
        .map_err(|e| format!("OAuth callback read failed: {e}"))?;
    let request = String::from_utf8_lossy(&buf[..n]);
    // The request line: `GET /oauth/callback?code=...&state=... HTTP/1.1`.
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != OAUTH_REDIRECT_PATH {
        write_http(stream, HTTP_NOT_FOUND, "text/plain", "Not found");
        return Err("OAuth callback hit an unexpected path".to_string());
    }

    let parsed = parse_callback_query(query);
    if let Some(error) = parsed.error {
        write_http(
            stream,
            HTTP_OK,
            "text/html",
            &callback_html("Authentication Failed", &format!("Error: {error}")),
        );
        return Err(format!("OAuth error: {error}"));
    }
    let (Some(code), Some(state)) = (parsed.code, parsed.state) else {
        write_http(
            stream,
            HTTP_BAD_REQUEST,
            "text/plain",
            "Missing code or state parameter",
        );
        return Err("OAuth callback missing code or state".to_string());
    };
    if state != expected_state {
        write_http(
            stream,
            HTTP_BAD_REQUEST,
            "text/plain",
            "Invalid state parameter",
        );
        return Err("State mismatch - possible CSRF attack".to_string());
    }

    write_http(
        stream,
        HTTP_OK,
        "text/html",
        &callback_html(
            "Authentication Successful!",
            "You can close this window and return to Suspenders.",
        ),
    );
    Ok(code)
}

/// The parsed callback query (qwen reads `code`/`state`/`error` off the redirect).
#[derive(Debug, Default, PartialEq, Eq)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Pulls `code`/`state`/`error` out of the callback query string (percent-decoded).
/// Pure, so the redirect parsing is unit-tested without a socket.
fn parse_callback_query(query: &str) -> CallbackQuery {
    let mut parsed = CallbackQuery::default();
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_decode(v);
        match percent_decode(k).as_str() {
            "code" => parsed.code = Some(value),
            "state" => parsed.state = Some(value),
            "error" => parsed.error = Some(value),
            _ => {}
        }
    }
    parsed
}

/// The tiny HTML page the callback answers with (qwen's success/failure page):
/// a heading + one line, HTML-escaped.
fn callback_html(heading: &str, body: &str) -> String {
    format!(
        "<html><body><h1>{}</h1><p>{}</p></body></html>",
        escape_html(heading),
        escape_html(body)
    )
}

/// Minimal HTML escaping for the callback page's interpolated text (qwen escapes
/// the error into the page).
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Writes a bare HTTP/1.1 response to the callback socket (no framework): status
/// line, content-type, close, and body.
fn write_http(stream: &mut std::net::TcpStream, status: u16, content_type: &str, body: &str) {
    let reason = match status {
        HTTP_OK => "OK",
        HTTP_BAD_REQUEST => "Bad Request",
        HTTP_NOT_FOUND => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

#[cfg(test)]
#[path = "../../../tests/mcp/oauth/callback.rs"]
mod tests;
