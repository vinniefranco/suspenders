//! Authorization-server + protected-resource metadata discovery (qwen's
//! `oauth-utils.ts`): the well-known URL construction (pure, unit-tested) and the
//! impure fetch legs, plus dynamic public-client registration.

use serde::Deserialize;

use crate::mcp::config::McpOAuthConfig;
use crate::mcp::oauth::MCP_OAUTH_CLIENT_NAME;

/// The well-known metadata URLs to try for an MCP server / authorization server
/// (qwen `OAuthUtils.buildWellKnownUrls` + `discoverAuthorizationServerMetadata`).
/// Pure URL construction split from the impure fetch so the endpoint set is
/// unit-tested. `base` is `scheme://host` of the input URL; the path suffix (when
/// the URL carries one) is inserted per RFC 8414 / OpenID discovery.
pub fn well_known_urls(server_url: &str) -> Result<WellKnownUrls, String> {
    let (base, path) = split_base_and_path(server_url)?;
    let path_suffix = path.trim_end_matches('/');
    let has_path = !path_suffix.is_empty();

    // Root-based endpoints (always tried).
    let mut authorization_server = vec![format!("{base}/.well-known/oauth-authorization-server")];
    let mut openid = vec![format!("{base}/.well-known/openid-configuration")];
    let protected_resource = format!("{base}/.well-known/oauth-protected-resource");
    let protected_resource_path = if has_path {
        Some(format!(
            "{base}/.well-known/oauth-protected-resource{path_suffix}"
        ))
    } else {
        None
    };

    if has_path {
        // Path-based discovery, tried FIRST (qwen inserts the path into the
        // well-known endpoints for issuer URLs with a path component).
        authorization_server.insert(
            0,
            format!("{base}/.well-known/oauth-authorization-server{path_suffix}"),
        );
        openid.insert(
            0,
            format!("{base}/.well-known/openid-configuration{path_suffix}"),
        );
        // qwen also tries path-appended OpenID (`{path}/.well-known/...`).
        openid.insert(
            1,
            format!("{base}{path_suffix}/.well-known/openid-configuration"),
        );
    }

    Ok(WellKnownUrls {
        protected_resource,
        protected_resource_path,
        authorization_server,
        openid,
    })
}

/// The well-known discovery endpoints for a server (qwen `buildWellKnownUrls` +
/// `discoverAuthorizationServerMetadata`). `authorization_server` and `openid`
/// are tried in order until one resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WellKnownUrls {
    /// The root protected-resource metadata URL (RFC 9728).
    pub protected_resource: String,
    /// The path-suffixed protected-resource metadata URL, when the input URL had
    /// a path (tried when the root one misses).
    pub protected_resource_path: Option<String>,
    /// The authorization-server metadata URLs (RFC 8414), path-suffixed first.
    pub authorization_server: Vec<String>,
    /// The OpenID Connect discovery URLs, tried after the RFC 8414 ones.
    pub openid: Vec<String>,
}

/// Splits a URL into `(scheme://host, path)` (qwen's `${protocol}//${host}` +
/// `pathname`). A minimal hand parser (no `url` crate dep): find the scheme, the
/// authority, and the path. An input without a scheme or host is an error.
pub(super) fn split_base_and_path(url: &str) -> Result<(String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("not an absolute URL: {url:?}"))?;
    if scheme.is_empty() {
        return Err(format!("URL missing a scheme: {url:?}"));
    }
    // The authority runs up to the first `/`, `?`, or `#`; the path is what
    // follows (query/fragment stripped, they never belong to a well-known path).
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..auth_end];
    if host.is_empty() {
        return Err(format!("URL missing a host: {url:?}"));
    }
    let after = &rest[auth_end..];
    let path = after.split(['?', '#']).next().unwrap_or("").to_string();
    Ok((format!("{scheme}://{host}"), path))
}

/// The canonical resource parameter for an MCP server URL (qwen
/// `buildResourceParameter`, RFC 8707): `scheme://host` plus the path (no query,
/// no fragment), with a trailing slash trimmed from a non-root path. Pure.
pub fn resource_parameter(endpoint_url: &str) -> Result<String, String> {
    let (base, path) = split_base_and_path(endpoint_url)?;
    let path = if path == "/" { "" } else { path.as_str() };
    let mut canonical = format!("{base}{path}");
    if canonical.ends_with('/') && !path.is_empty() {
        canonical.pop();
    }
    Ok(canonical)
}

/// The authorization-server metadata subset the flow reads (qwen's
/// `OAuthAuthorizationServerMetadata`): the endpoints it needs. Extra fields are
/// ignored (serde default), so a richer server response still parses.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct AuthorizationServerMetadata {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
}

/// The protected-resource metadata subset (qwen's `OAuthProtectedResourceMetadata`):
/// the authorization servers to defer to and any resource scopes.
#[derive(Debug, Clone, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Option<Vec<String>>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
}

/// The dynamic client registration response subset (qwen's
/// `OAuthClientRegistrationResponse`): the minted client id (+ secret, for a
/// confidential client).
#[derive(Debug, Clone, Deserialize)]
pub(super) struct ClientRegistration {
    pub(super) client_id: String,
    #[serde(default)]
    pub(super) client_secret: Option<String>,
}

/// Discovers the OAuth config from an MCP server URL's well-known metadata (qwen
/// `OAuthUtils.discoverOAuthConfig`): try the protected-resource metadata (root,
/// then path-suffixed) to find the authorization server, else fall back to the
/// server's own authorization-server metadata. Returns the endpoints as an
/// [`McpOAuthConfig`] (only the discovered fields set), or `None`.
pub(super) async fn discover_oauth_config(server_url: &str) -> Option<McpOAuthConfig> {
    let urls = well_known_urls(server_url).ok()?;
    let client = reqwest::Client::new();

    // Protected-resource metadata (RFC 9728): root, then path-suffixed.
    let mut resource =
        fetch_json::<ProtectedResourceMetadata>(&client, &urls.protected_resource).await;
    if resource.is_none()
        && let Some(path_url) = &urls.protected_resource_path
    {
        resource = fetch_json::<ProtectedResourceMetadata>(&client, path_url).await;
    }

    if let Some(resource) = &resource
        && let Some(servers) = &resource.authorization_servers
        && let Some(auth_server_url) = servers.first()
        && let Some(metadata) = discover_authorization_server_metadata(auth_server_url).await
    {
        let mut config = metadata_to_config(metadata);
        // Protected-resource scopes take precedence (RFC 9728).
        if let Some(scopes) = &resource.scopes_supported
            && !scopes.is_empty()
        {
            config.scopes = Some(scopes.clone());
        }
        return Some(config);
    }

    // Fallback: the server's own authorization-server metadata.
    let metadata = discover_authorization_server_metadata(server_url).await?;
    Some(metadata_to_config(metadata))
}

/// Discovers authorization-server metadata by trying the RFC 8414 then OpenID
/// well-known endpoints in order (qwen `discoverAuthorizationServerMetadata`).
pub(super) async fn discover_authorization_server_metadata(
    auth_server_url: &str,
) -> Option<AuthorizationServerMetadata> {
    let urls = well_known_urls(auth_server_url).ok()?;
    let client = reqwest::Client::new();
    for endpoint in urls.authorization_server.iter().chain(urls.openid.iter()) {
        if let Some(metadata) = fetch_json::<AuthorizationServerMetadata>(&client, endpoint).await {
            return Some(metadata);
        }
    }
    None
}

impl AuthorizationServerMetadata {
    /// The registration endpoint the metadata advertised, when any (drives the
    /// dynamic-registration leg's endpoint lookup).
    pub(super) fn registration_endpoint(self) -> Option<String> {
        self.registration_endpoint
    }
}

/// Folds authorization-server metadata into an [`McpOAuthConfig`] (qwen
/// `metadataToOAuthConfig`): only the discovered endpoints/scopes are set. The
/// metadata is consumed so the discovered fields move in rather than clone.
fn metadata_to_config(metadata: AuthorizationServerMetadata) -> McpOAuthConfig {
    McpOAuthConfig {
        authorization_url: metadata.authorization_endpoint,
        token_url: metadata.token_endpoint,
        scopes: metadata.scopes_supported,
        registration_url: metadata.registration_endpoint,
        ..Default::default()
    }
}

/// GETs a JSON document, returning `None` on any transport / non-2xx / decode
/// failure (qwen's fetch helpers swallow errors into `null`). Discovery must
/// stay fail-soft: a missing well-known endpoint is not an error, just a miss.
async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Option<T> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<T>().await.ok()
}

/// POSTs a dynamic client registration (qwen `registerClient`): a public client
/// (`token_endpoint_auth_method: none`), authorization_code + refresh_token
/// grants, `code` response type, S256. Returns the minted client id (+ secret).
pub(super) async fn register_client(
    registration_url: &str,
    config: &McpOAuthConfig,
    redirect_uri: &str,
) -> Result<ClientRegistration, String> {
    let body = serde_json::json!({
        "client_name": MCP_OAUTH_CLIENT_NAME,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "code_challenge_method": ["S256"],
        "scope": config.scopes.as_ref().map(|s| s.join(" ")).unwrap_or_default(),
    });
    let response = reqwest::Client::new()
        .post(registration_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Client registration failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "Client registration failed: {} - {text}",
            status.as_u16()
        ));
    }
    response
        .json::<ClientRegistration>()
        .await
        .map_err(|e| format!("Client registration returned bad JSON: {e}"))
}

#[cfg(test)]
#[path = "../../../tests/mcp/oauth/discovery.rs"]
mod tests;
