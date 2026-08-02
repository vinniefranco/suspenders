//! The MCP OAuth 2.0 subsystem (ADR-0065 Phase D): a faithful port of qwen's
//! `oauth-provider.ts` + `oauth-token-storage.ts` + `oauth-utils.ts`, in the
//! Suspenders idiom.
//!
//! Three pieces, all transport-free (the rmcp crate stays confined to
//! `manager.rs`):
//!
//! * [`OAuthToken`] / [`OAuthCredentials`] - the stored shapes (qwen's
//!   `token-storage/types.ts`), and [`McpOAuthTokenStorage`], the on-disk store
//!   at `~/.config/suspenders/mcp-oauth-tokens.json` (mode `0600`, atomic
//!   write-then-rename like `session.rs`). The pure JSON (de)serialization
//!   ([`parse_credentials`]/[`serialize_credentials`]) is split from the impure
//!   file IO so it unit-tests with literals. Lives in [`token`].
//! * [`McpOAuthProvider`] - the authorization-code-with-PKCE flow (qwen's
//!   `MCPOAuthProvider`): S256 PKCE ([`pkce`]), authorization-server metadata
//!   discovery from the MCP server URL's `/.well-known/*` endpoints
//!   ([`discovery`]), dynamic public-client registration, a hand-rolled localhost
//!   callback server ([`callback`], `std::net`, no web framework), a browser open
//!   (`std::process::Command`, no new dep), code -> token exchange, and refresh
//!   ([`http`]).
//! * [`OAuthProgress`] - the progress sink the AUTHENTICATE dialog step (Phase E)
//!   consumes, mirroring qwen's `OauthDisplayMessage` / `OauthAuthUrl` events
//!   ([`progress`]).
//!
//! The impure HTTP rides `reqwest` (already a dep via the LLM boundary + rmcp);
//! SHA-256 is `sha2`, the CSPRNG bytes are `rand`, and base64url is `base64`'s
//! `URL_SAFE_NO_PAD` engine.
//!
//! ## Deferred (Phase E / out of scope)
//!
//! The ratatui AUTHENTICATE dialog step consuming [`OAuthProgress`] is Phase E.
//! The encrypted-file / keychain hybrid store (qwen's `HybridTokenStorage`) is
//! NOT ported - Suspenders uses the plain mode-`0600` JSON file qwen falls back
//! to. Websocket transport and the 30s health loop stay out of scope (ADR-0065).

mod callback;
mod discovery;
mod http;
mod pkce;
mod progress;
mod token;

use crate::mcp::config::McpOAuthConfig;

use callback::wait_for_callback;
use discovery::{
    discover_authorization_server_metadata, discover_oauth_config, register_client,
    split_base_and_path,
};
use http::{
    AuthorizationUrlParams, TokenExchange, build_authorization_url, exchange_code_for_token,
    open_browser, refresh_access_token,
};
use pkce::PkceParams;
use token::now_ms;

// The public API surface, re-exported so callers keep their stable paths
// (`crate::mcp::oauth::...`) after the module was split into submodules.
pub use discovery::{WellKnownUrls, resource_parameter, well_known_urls};
pub use pkce::challenge_for;
pub use progress::{OAuthProgress, ProgressSink};
pub use token::{
    McpOAuthTokenStorage, OAuthCredentials, OAuthToken, parse_credentials, serialize_credentials,
};

/// qwen's `MCP_OAUTH_CLIENT_NAME`, renamed for Suspenders: the `client_name` a
/// dynamic client registration announces.
pub(super) const MCP_OAUTH_CLIENT_NAME: &str = "Suspenders MCP Client";

/// The localhost callback port the flow listens on (qwen `OAUTH_REDIRECT_PORT`,
/// 7777) and the path it answers (qwen `OAUTH_REDIRECT_PATH`).
pub(super) const OAUTH_REDIRECT_PORT: u16 = 7777;
pub(super) const OAUTH_REDIRECT_PATH: &str = "/oauth/callback";

/// How long the callback server waits for the browser redirect before giving up
/// (qwen's 5-minute timeout).
pub(super) const CALLBACK_TIMEOUT_SECS: u64 = 5 * 60;

// ---- The provider (qwen MCPOAuthProvider) ----------------------------------

/// The MCP OAuth provider (ADR-0065 Phase D, qwen's `MCPOAuthProvider`): runs the
/// authorization-code-with-PKCE flow for one server and stores the resulting
/// token. Holds the [`McpOAuthTokenStorage`] it saves through; a fresh one is
/// built per `/mcp` authenticate op (the store is stateless beyond the file).
pub struct McpOAuthProvider {
    storage: McpOAuthTokenStorage,
    /// An override for the redirect base + callback loopback, so the flow is
    /// testable without opening a real browser or binding the fixed port. `None`
    /// uses `http://localhost:7777/oauth/callback`.
    redirect_uri_override: Option<String>,
}

impl McpOAuthProvider {
    /// Builds a provider over the given token storage.
    pub fn new(storage: McpOAuthTokenStorage) -> McpOAuthProvider {
        McpOAuthProvider {
            storage,
            redirect_uri_override: None,
        }
    }

    /// The redirect URI the flow registers + expects the callback on (qwen's
    /// `config.redirectUri || http://localhost:<port><path>`).
    fn redirect_uri(&self, config: &McpOAuthConfig) -> String {
        if let Some(uri) = &config.redirect_uri {
            return uri.clone();
        }
        if let Some(uri) = &self.redirect_uri_override {
            return uri.clone();
        }
        format!("http://localhost:{OAUTH_REDIRECT_PORT}{OAUTH_REDIRECT_PATH}")
    }

    /// The full authorization-code-with-PKCE flow (qwen `authenticate`): discover
    /// the endpoints (when unset) from the MCP server URL, register a public
    /// client (when no client id), open the browser at the authorization URL,
    /// wait for the localhost callback, exchange the code for a token, store it,
    /// and return it. `progress` receives the copy-the-URL hints + the auth URL so
    /// the dialog (Phase E) can render them.
    ///
    /// NOTE: the network legs (discovery, registration, exchange) hit live
    /// endpoints, so the unit tests exercise only the pure pieces (PKCE, URL
    /// construction, storage); an end-to-end run is opt-in (a real MCP server).
    pub async fn authenticate(
        &self,
        server_name: &str,
        config: &McpOAuthConfig,
        mcp_server_url: Option<&str>,
        mut progress: ProgressSink<'_>,
    ) -> Result<OAuthToken, String> {
        let mut config = config.clone();

        // Discover the authorization + token endpoints from the MCP server's
        // well-known metadata when the config supplied none (qwen's discovery leg).
        if config.authorization_url.is_none()
            && let Some(url) = mcp_server_url
        {
            progress(OAuthProgress::Message(
                "No authorization URL; using OAuth discovery".to_string(),
            ));
            let discovered = discover_oauth_config(url).await.ok_or_else(|| {
                "Failed to discover OAuth configuration from MCP server".to_string()
            })?;
            // Merge discovered endpoints, PRESERVING any client credentials the
            // config already carried (qwen preserves clientId/clientSecret).
            config.authorization_url = discovered.authorization_url;
            config.token_url = discovered.token_url;
            config.scopes = discovered.scopes.or(config.scopes);
            config.registration_url = discovered
                .registration_url
                .or(config.registration_url.clone());
        }

        // Register a public client dynamically when no client id is configured
        // (qwen's registration leg): discover the registration endpoint from the
        // authorization server metadata when it was not already known.
        if config.client_id.is_none() {
            let registration_url = match &config.registration_url {
                Some(url) => Some(url.clone()),
                None => {
                    let authorization_url = config.authorization_url.as_ref().ok_or_else(|| {
                        "Cannot perform dynamic registration without authorization URL".to_string()
                    })?;
                    let (base, _) = split_base_and_path(authorization_url)?;
                    let metadata = discover_authorization_server_metadata(&base)
                        .await
                        .ok_or_else(|| {
                            "Failed to fetch authorization server metadata for client registration"
                                .to_string()
                        })?;
                    metadata.registration_endpoint()
                }
            };
            let registration_url = registration_url.ok_or_else(|| {
                "No client ID provided and dynamic registration not supported".to_string()
            })?;
            let registered =
                register_client(&registration_url, &config, &self.redirect_uri(&config)).await?;
            config.client_id = Some(registered.client_id);
            if registered.client_secret.is_some() {
                config.client_secret = registered.client_secret;
            }
        }

        // Validate the resolved config (qwen's post-discovery guard).
        let client_id = config.client_id.clone().ok_or_else(|| {
            "Missing required OAuth configuration after discovery and registration".to_string()
        })?;
        let authorization_url = config.authorization_url.clone().ok_or_else(|| {
            "Missing required OAuth configuration after discovery and registration".to_string()
        })?;
        let token_url = config.token_url.clone().ok_or_else(|| {
            "Missing required OAuth configuration after discovery and registration".to_string()
        })?;

        // PKCE + the authorization URL.
        let pkce = PkceParams::generate();
        let auth_url = build_authorization_url(&AuthorizationUrlParams {
            config: &config,
            client_id: &client_id,
            authorization_url: &authorization_url,
            redirect_uri: &self.redirect_uri(&config),
            pkce: &pkce,
            mcp_server_url,
        });

        progress(OAuthProgress::Message(
            "If the browser does not open, copy and paste this URL into your browser:".to_string(),
        ));
        progress(OAuthProgress::Message(
            "Make sure to copy the COMPLETE URL - it may wrap across multiple lines.".to_string(),
        ));
        progress(OAuthProgress::AuthUrl(auth_url.clone()));

        // Open the browser (best-effort, qwen warns-and-continues on failure) and
        // wait for the localhost callback carrying the code.
        open_browser(&auth_url);
        let code = wait_for_callback(&pkce.state).await?;

        progress(OAuthProgress::Message(
            "Authorization code received, exchanging for tokens...".to_string(),
        ));

        // Exchange the code for a token and store it.
        let token = exchange_code_for_token(&TokenExchange {
            config: &config,
            client_id: &client_id,
            token_url: &token_url,
            redirect_uri: &self.redirect_uri(&config),
            code: &code,
            code_verifier: &pkce.code_verifier,
            mcp_server_url,
        })
        .await?;

        self.storage.set(OAuthCredentials {
            server_name: server_name.to_string(),
            token: token.clone(),
            client_id: Some(client_id),
            token_url: Some(token_url),
            mcp_server_url: mcp_server_url.map(str::to_string),
            updated_at: now_ms(),
        })?;

        Ok(token)
    }

    /// A valid access token for a server, refreshing an expired one if possible
    /// (qwen `getValidToken`): returns the stored token when unexpired; otherwise
    /// refreshes via the stored refresh token + token URL, stores the new token,
    /// and returns it. On a refresh failure the stored credential is deleted (it
    /// is no good). `None` when there is no credential, no refresh path, or the
    /// refresh failed. `now_ms` is injected for the expiry check (test seam).
    pub async fn valid_token(
        &self,
        server_name: &str,
        config: &McpOAuthConfig,
        now_ms: u64,
    ) -> Result<Option<OAuthToken>, String> {
        let Some(credentials) = self.storage.get(server_name)? else {
            return Ok(None);
        };
        if !credentials.token.is_expired(now_ms) {
            return Ok(Some(credentials.token));
        }
        // Expired: refresh when we have a refresh token + a token URL + a client id.
        let (Some(refresh_token), Some(token_url), Some(client_id)) = (
            credentials.token.refresh_token.as_ref(),
            credentials.token_url.as_ref(),
            credentials.client_id.as_ref(),
        ) else {
            return Ok(None);
        };
        match refresh_access_token(
            config,
            client_id,
            refresh_token,
            token_url,
            credentials.mcp_server_url.as_deref(),
        )
        .await
        {
            Ok(mut new_token) => {
                // Carry the old refresh token / scope when the server omitted them
                // (qwen keeps the prior values).
                if new_token.refresh_token.is_none() {
                    new_token.refresh_token = credentials.token.refresh_token.clone();
                }
                if new_token.scope.is_none() {
                    new_token.scope = credentials.token.scope.clone();
                }
                self.storage.set(OAuthCredentials {
                    server_name: server_name.to_string(),
                    token: new_token.clone(),
                    client_id: Some(client_id.clone()),
                    token_url: Some(token_url.clone()),
                    mcp_server_url: credentials.mcp_server_url.clone(),
                    updated_at: self_now(now_ms),
                })?;
                Ok(Some(new_token))
            }
            Err(_) => {
                // The refresh failed; the stored token is no good, drop it.
                let _ = self.storage.delete(server_name);
                Ok(None)
            }
        }
    }
}

/// The `updated_at` stamp for a refresh-driven store: the injected `now_ms` when
/// non-zero (the tests pass a fixed clock), else the live clock. Keeps
/// `valid_token` deterministic under a test clock while stamping real writes with
/// the wall clock.
fn self_now(injected: u64) -> u64 {
    if injected == 0 { now_ms() } else { injected }
}
