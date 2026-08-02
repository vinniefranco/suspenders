//! The impure token-endpoint legs (qwen's `buildAuthorizationUrl` /
//! `exchangeCodeForToken` / `refreshAccessToken`): the authorization URL
//! assembly, the code -> token exchange, refresh, and the shared
//! form-urlencoded / percent coding + browser open.

use crate::mcp::config::McpOAuthConfig;
use crate::mcp::oauth::discovery::resource_parameter;
use crate::mcp::oauth::pkce::PkceParams;
use crate::mcp::oauth::token::{OAuthToken, expires_at_from};

/// The authorization-URL inputs (qwen `buildAuthorizationUrl`'s argument list),
/// collapsed into one value so the builder takes a single parameter rather than
/// six positional strings.
pub(super) struct AuthorizationUrlParams<'a> {
    pub config: &'a McpOAuthConfig,
    pub client_id: &'a str,
    pub authorization_url: &'a str,
    pub redirect_uri: &'a str,
    pub pkce: &'a PkceParams,
    pub mcp_server_url: Option<&'a str>,
}

/// The code -> token exchange inputs (qwen `exchangeCodeForToken`'s argument
/// list), collapsed into one value so the exchange takes a single parameter
/// rather than seven positional strings.
pub(super) struct TokenExchange<'a> {
    pub config: &'a McpOAuthConfig,
    pub client_id: &'a str,
    pub token_url: &'a str,
    pub redirect_uri: &'a str,
    pub code: &'a str,
    pub code_verifier: &'a str,
    pub mcp_server_url: Option<&'a str>,
}

/// Builds the authorization URL with the PKCE + OAuth query parameters (qwen
/// `buildAuthorizationUrl`): `response_type=code`, the redirect URI, the state,
/// the S256 challenge, and the optional scope / audience / resource params,
/// appended to the authorization endpoint. Pure string assembly (no `url` crate):
/// each value is percent-encoded onto the query.
pub(super) fn build_authorization_url(params: &AuthorizationUrlParams<'_>) -> String {
    let mut query: Vec<(String, String)> = vec![
        ("client_id".into(), params.client_id.to_string()),
        ("response_type".into(), "code".into()),
        ("redirect_uri".into(), params.redirect_uri.to_string()),
        ("state".into(), params.pkce.state.clone()),
        ("code_challenge".into(), params.pkce.code_challenge.clone()),
        ("code_challenge_method".into(), "S256".into()),
    ];
    push_joined(&mut query, "scope", params.config.scopes.as_deref());
    push_joined(&mut query, "audience", params.config.audiences.as_deref());
    push_resource(&mut query, params.mcp_server_url);

    let encoded = encode_form(&query);
    // The endpoint may already carry a query (qwen appends onto whatever is
    // there); choose the join char accordingly.
    let sep = if params.authorization_url.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{}{sep}{encoded}", params.authorization_url)
}

/// Exchanges the authorization code for a token (qwen `exchangeCodeForToken`):
/// a form-urlencoded `authorization_code` grant POST to the token endpoint,
/// carrying the code, the redirect URI, the PKCE verifier, the client id (+ any
/// secret / audience / resource). Parses JSON, falling back to form-urlencoded.
pub(super) async fn exchange_code_for_token(
    exchange: &TokenExchange<'_>,
) -> Result<OAuthToken, String> {
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), exchange.code.to_string()),
        ("redirect_uri".into(), exchange.redirect_uri.to_string()),
        ("code_verifier".into(), exchange.code_verifier.to_string()),
        ("client_id".into(), exchange.client_id.to_string()),
    ];
    if let Some(secret) = &exchange.config.client_secret {
        form.push(("client_secret".into(), secret.clone()));
    }
    push_joined(&mut form, "audience", exchange.config.audiences.as_deref());
    push_resource(&mut form, exchange.mcp_server_url);
    post_token_request(exchange.token_url, &form, "Token exchange failed").await
}

/// Refreshes an access token (qwen `refreshAccessToken`): a form-urlencoded
/// `refresh_token` grant POST to the token endpoint.
pub(super) async fn refresh_access_token(
    config: &McpOAuthConfig,
    client_id: &str,
    refresh_token: &str,
    token_url: &str,
    mcp_server_url: Option<&str>,
) -> Result<OAuthToken, String> {
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), refresh_token.to_string()),
        ("client_id".into(), client_id.to_string()),
    ];
    if let Some(secret) = &config.client_secret {
        form.push(("client_secret".into(), secret.clone()));
    }
    push_joined(&mut form, "scope", config.scopes.as_deref());
    push_joined(&mut form, "audience", config.audiences.as_deref());
    push_resource(&mut form, mcp_server_url);
    post_token_request(token_url, &form, "Token refresh failed").await
}

/// Appends a space-joined `key=value` pair when the list is non-empty (qwen's
/// `scopes.join(" ")` / `audiences.join(" ")` guard). A no-op for an absent or
/// empty list, so the shared guard-then-push lives in one place.
fn push_joined(form: &mut Vec<(String, String)>, key: &str, values: Option<&[String]>) {
    if let Some(values) = values
        && !values.is_empty()
    {
        form.push((key.to_string(), values.join(" ")));
    }
}

/// Appends the RFC 8707 `resource` parameter for the MCP server URL, when one is
/// present and canonicalizes cleanly (qwen's optional resource param).
fn push_resource(form: &mut Vec<(String, String)>, mcp_server_url: Option<&str>) {
    if let Some(url) = mcp_server_url
        && let Ok(resource) = resource_parameter(url)
    {
        form.push(("resource".to_string(), resource));
    }
}

/// POSTs a form-urlencoded token request and decodes the response into an
/// [`OAuthToken`] (the shared body of exchange + refresh, qwen's duplicated
/// token-endpoint handling): tries JSON first, then form-urlencoded, and maps a
/// non-2xx (or a missing access token) to a `{context}: ...` error.
async fn post_token_request(
    token_url: &str,
    form: &[(String, String)],
    error_context: &str,
) -> Result<OAuthToken, String> {
    let response = reqwest::Client::new()
        .post(token_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(
            reqwest::header::ACCEPT,
            "application/json, application/x-www-form-urlencoded",
        )
        .body(encode_form(form))
        .send()
        .await
        .map_err(|e| format!("{error_context}: {e}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("{error_context}: {e}"))?;

    if !status.is_success() {
        // Try to surface an `error` / `error_description` from a form-urlencoded
        // error body (qwen's fallback), else the raw text.
        if let Some(err) = form_field(&text, "error") {
            let desc = form_field(&text, "error_description").unwrap_or_default();
            return Err(format!("{error_context}: {err} - {desc}"));
        }
        return Err(format!("{error_context}: {} - {text}", status.as_u16()));
    }

    parse_token_response(&text)
        .ok_or_else(|| format!("{error_context}: no access token in response"))
}

/// The default `token_type` qwen assumes when the token endpoint omits one.
const DEFAULT_TOKEN_TYPE: &str = "Bearer";

/// The raw token fields either response shape yields (qwen's JSON / form parse):
/// the shared intermediate so the [`OAuthToken`] is assembled in one place rather
/// than duplicated per shape. `token_type` is already defaulted; `expires_in` is
/// still in seconds (turned into the absolute `expires_at` at build).
struct TokenFields {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
}

impl TokenFields {
    /// Assembles the stored [`OAuthToken`], turning `expires_in` seconds into the
    /// absolute epoch-millis `expires_at`.
    fn into_token(self) -> OAuthToken {
        OAuthToken {
            access_token: self.access_token,
            token_type: self.token_type,
            refresh_token: self.refresh_token,
            expires_at: self.expires_in.map(expires_at_from),
            scope: self.scope,
        }
    }
}

/// Parses a token endpoint response into an [`OAuthToken`] (qwen's JSON-then-
/// form-urlencoded parse): a JSON body maps directly; a form-urlencoded body maps
/// its fields, defaulting `token_type` to `Bearer`. Pure, so it unit-tests both
/// shapes. `None` when no access token is present.
fn parse_token_response(text: &str) -> Option<OAuthToken> {
    let fields = json_token_fields(text).or_else(|| form_token_fields(text))?;
    Some(fields.into_token())
}

/// The token fields off a JSON body (the common case), or `None` when the body is
/// not JSON or carries no `access_token`.
fn json_token_fields(text: &str) -> Option<TokenFields> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let access_token = value.get("access_token").and_then(|v| v.as_str())?;
    Some(TokenFields {
        access_token: access_token.to_string(),
        token_type: value
            .get("token_type")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_TOKEN_TYPE)
            .to_string(),
        refresh_token: value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        expires_in: value.get("expires_in").and_then(|v| v.as_u64()),
        scope: value
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// The token fields off a form-urlencoded body (the fallback), or `None` when it
/// carries no `access_token`.
fn form_token_fields(text: &str) -> Option<TokenFields> {
    Some(TokenFields {
        access_token: form_field(text, "access_token")?,
        token_type: form_field(text, "token_type")
            .unwrap_or_else(|| DEFAULT_TOKEN_TYPE.to_string()),
        refresh_token: form_field(text, "refresh_token"),
        expires_in: form_field(text, "expires_in").and_then(|s| s.parse::<u64>().ok()),
        scope: form_field(text, "scope"),
    })
}

/// Reads one field out of a form-urlencoded body (`a=1&b=2`), percent-decoding
/// the value. `None` when the key is absent. A minimal parser (no `url` crate):
/// enough for the token-endpoint fallback + error bodies qwen parses.
pub(super) fn form_field(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (percent_decode(k) == key).then(|| percent_decode(v))
    })
}

/// Encodes form pairs to an `application/x-www-form-urlencoded` body.
fn encode_form(form: &[(String, String)]) -> String {
    form.iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes a string for a query/form value (RFC 3986 unreserved kept,
/// space -> `%20`; a form body's `+`-for-space is NOT used, servers accept `%20`).
/// A small hand encoder so no `url`/`percent-encoding` dep is pulled.
pub(super) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Percent-decodes a query/form value (the inverse of [`percent_encode`], also
/// turning `+` into a space, since some servers form-encode that way).
pub(super) fn percent_decode(s: &str) -> String {
    /// A percent-escape is `%` plus this many hex digits.
    const HEX_DIGITS: usize = 2;
    /// The base a hex digit pair is combined in (`hi * 16 + lo`).
    const HEX_RADIX: u32 = 16;

    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + HEX_DIGITS < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(HEX_RADIX);
                let lo = (bytes[i + 2] as char).to_digit(HEX_RADIX);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * HEX_RADIX + lo) as u8);
                    i += 1 + HEX_DIGITS;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Opens the authorization URL in the platform browser (qwen's
/// `openBrowserSecurely`), best-effort: `xdg-open` on Linux, `open` on macOS,
/// `cmd /c start` on Windows, via `std::process::Command` (no new dep). A failure
/// is swallowed - the operator can still copy the URL from the progress sink.
pub(super) fn open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = std::process::Command::new("true");
        let _ = url;
        c
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
#[path = "../../../tests/mcp/oauth/http.rs"]
mod tests;
