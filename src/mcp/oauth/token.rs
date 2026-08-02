//! The stored OAuth shapes and the on-disk token store (qwen's
//! `token-storage/types.ts` + `oauth-token-storage.ts`). The pure JSON
//! (de)serialization is split from the impure file IO so it unit-tests with
//! literals.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The number of milliseconds in a second (qwen works in `Date.now()` millis, so
/// a server-reported `expires_in` in seconds is scaled by this).
const MILLIS_PER_SEC: u64 = 1000;

/// The clock-skew buffer applied to a token's expiry (qwen's 5-minute buffer): a
/// token within this window of expiring counts as expired, so a refresh happens
/// before the server would reject it.
const EXPIRY_BUFFER_MS: u64 = 5 * 60 * MILLIS_PER_SEC;

/// The mode a freshly created token file is opened at on unix (owner read/write
/// only), the permission qwen's `{ mode: 0o600 }` sets.
#[cfg(unix)]
const TOKEN_FILE_MODE: u32 = 0o600;

// ---- Stored shapes (qwen token-storage/types.ts) ---------------------------

/// A stored OAuth token (qwen's `OAuthToken`), snake_case on the wire. The
/// `expires_at` is epoch-millis (qwen's `Date.now()`-based `expiresAt`), so
/// [`OAuthToken::is_expired`] compares it against the current millis + the skew
/// buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    /// The bearer access token injected at connect.
    pub access_token: String,
    /// The token type (`Bearer` in practice); defaulted to `Bearer` when a
    /// form-urlencoded token endpoint omits it.
    pub token_type: String,
    /// The refresh token, when the server issued one (drives the 401-refresh
    /// retry and the pre-connect refresh of an expired token).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub refresh_token: Option<String>,
    /// Epoch-millis expiry (qwen `expiresAt`); absent means no expiry (always
    /// valid).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<u64>,
    /// The granted scope, when the server reported one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scope: Option<String>,
}

impl OAuthToken {
    /// Whether the token is expired (qwen `isTokenExpired`): a token with no
    /// `expires_at` is never expired; otherwise it is expired once the current
    /// time plus the [`EXPIRY_BUFFER_MS`] skew buffer reaches the expiry. `now_ms`
    /// is injected so the check is pure + unit-testable (qwen reads `Date.now()`).
    pub fn is_expired(&self, now_ms: u64) -> bool {
        match self.expires_at {
            None => false,
            Some(expires_at) => now_ms + EXPIRY_BUFFER_MS >= expires_at,
        }
    }
}

/// A stored MCP OAuth credential (qwen's `OAuthCredentials`): the token plus the
/// facts a later refresh needs (the `client_id`, the `token_url`, and the
/// `mcp_server_url` for the resource parameter), and when it was written. One
/// entry per server; the store is a JSON array of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthCredentials {
    /// The MCP server name (the `mcp_servers` map key) - the store's key.
    pub server_name: String,
    /// The stored token.
    pub token: OAuthToken,
    /// The client id this token was minted under (for a refresh); absent for a
    /// server whose client id was purely ambient.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_id: Option<String>,
    /// The token endpoint this token was minted at (a refresh POSTs here).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub token_url: Option<String>,
    /// The MCP server URL used as the resource parameter (carried through a
    /// refresh, MCP OAuth spec compliance).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mcp_server_url: Option<String>,
    /// Epoch-millis of the last write (qwen `updatedAt`).
    pub updated_at: u64,
}

// ---- Token storage (qwen oauth-token-storage.ts) ---------------------------

/// The on-disk MCP OAuth token store (ADR-0065 Phase D, qwen's
/// `MCPOAuthTokenStorage`): a JSON array of [`OAuthCredentials`] at a fixed path
/// (`~/.config/suspenders/mcp-oauth-tokens.json` in production), keyed by server
/// name. The impure surface is deliberately thin - [`get`](Self::get) /
/// [`set`](Self::set) / [`delete`](Self::delete) / [`get_all`](Self::get_all) -
/// and every (de)serialize routes through the pure [`parse_credentials`] /
/// [`serialize_credentials`] so the JSON logic tests with literals. Writes are
/// atomic (write-then-rename) and mode `0600`, mirroring `session.rs`'s persist.
pub struct McpOAuthTokenStorage {
    /// The token file path. In production it is
    /// [`default_mcp_oauth_tokens_path`](crate::session::default_mcp_oauth_tokens_path);
    /// tests point it at a tempdir.
    path: String,
}

impl McpOAuthTokenStorage {
    /// Builds a store over the given token-file path. The production caller passes
    /// [`default_mcp_oauth_tokens_path`](crate::session::default_mcp_oauth_tokens_path).
    pub fn new(path: impl Into<String>) -> McpOAuthTokenStorage {
        McpOAuthTokenStorage { path: path.into() }
    }

    /// Loads every stored credential (qwen `getAllCredentials`), keyed by server
    /// name. An absent file is an empty map (never an error - a fresh install has
    /// no tokens); a malformed file is a [`String`] error naming the reason, the
    /// same shape `session.rs` takes.
    pub fn get_all(&self) -> Result<BTreeMap<String, OAuthCredentials>, String> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => {
                return Err(format!(
                    "failed to read MCP OAuth tokens at {}: {e}",
                    self.path
                ));
            }
        };
        parse_credentials(&raw)
            .map_err(|e| format!("malformed MCP OAuth tokens at {}: {e}", self.path))
    }

    /// One server's stored credential, or `None` (qwen `getCredentials`).
    pub fn get(&self, server_name: &str) -> Result<Option<OAuthCredentials>, String> {
        Ok(self.get_all()?.remove(server_name))
    }

    /// Stores (or replaces) a server's credential (qwen `setCredentials` /
    /// `saveToken`): load the current set, upsert by server name, and write the
    /// whole array back atomically at mode `0600`.
    pub fn set(&self, credential: OAuthCredentials) -> Result<(), String> {
        let mut all = self.get_all()?;
        all.insert(credential.server_name.clone(), credential);
        self.write_all(&all)
    }

    /// Removes a server's credential (qwen `deleteCredentials`): load, drop by
    /// name, and write back - or unlink the file entirely when nothing remains
    /// (qwen removes the file rather than leaving an empty array). A no-op for an
    /// absent server.
    pub fn delete(&self, server_name: &str) -> Result<(), String> {
        let mut all = self.get_all()?;
        if all.remove(server_name).is_none() {
            return Ok(());
        }
        if all.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!(
                    "failed to remove MCP OAuth tokens at {}: {e}",
                    self.path
                )),
            };
        }
        self.write_all(&all)
    }

    /// Serializes the map to the JSON array and writes it atomically at mode
    /// `0600`. Write-then-rename, never in place (a crash mid-write must not tear
    /// the token file), the SAME shape as `session.rs`'s persist; the parent dir
    /// is scaffolded first (qwen's `ensureConfigDir`).
    fn write_all(&self, all: &BTreeMap<String, OAuthCredentials>) -> Result<(), String> {
        let json = serialize_credentials(all)?;

        if let Some(parent) = std::path::Path::new(&self.path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create token directory {parent:?}: {e}"))?;
        }

        // Write-then-rename in the SAME directory (a same-dir rename is atomic on
        // POSIX). The temp file is created with mode 0600 up front on unix so the
        // token bytes are never briefly world-readable.
        let tmp = format!("{}.tmp", self.path);
        write_private(&tmp, json.as_bytes())
            .map_err(|e| format!("failed to write MCP OAuth tokens to {tmp}: {e}"))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("failed to write MCP OAuth tokens to {}: {e}", self.path))
    }
}

/// Writes `bytes` to `path` at mode `0600` on unix (owner read/write only), the
/// permission qwen's `{ mode: 0o600 }` sets. On non-unix the mode is not
/// expressible, so it is a plain write (the platform's default ACL applies).
fn write_private(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(TOKEN_FILE_MODE)
            .open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// Parses the token file's JSON array into a server-name-keyed map (qwen's
/// `getAllCredentials` loop). Pure + path-free, so it unit-tests with literals,
/// the same split as `session.rs`'s `merge_json_key`. A later duplicate
/// `server_name` wins (last write), matching qwen's `Map.set`.
pub fn parse_credentials(raw: &str) -> Result<BTreeMap<String, OAuthCredentials>, String> {
    let creds: Vec<OAuthCredentials> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    Ok(creds
        .into_iter()
        .map(|c| (c.server_name.clone(), c))
        .collect())
}

/// Serializes a server-name-keyed map back to the token file's JSON array (qwen
/// writes `Array.from(tokens.values())` pretty-printed). The array is in
/// server-name order (the map is a `BTreeMap`), so a load -> save round-trip is
/// stable on disk. Pure, the mirror of [`parse_credentials`]; the error is a
/// [`String`] so the store's error contract stays one type throughout.
pub fn serialize_credentials(all: &BTreeMap<String, OAuthCredentials>) -> Result<String, String> {
    let array: Vec<&OAuthCredentials> = all.values().collect();
    serde_json::to_string_pretty(&array).map_err(|e| e.to_string())
}

/// The current time in epoch-millis (qwen's `Date.now()`), for a token's
/// `updated_at` + `expires_at`. A clock before the epoch reads as 0.
pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The absolute expiry (epoch-millis) for a server-reported `expires_in` seconds.
/// Saturating throughout: a hostile or absurd `expires_in` (near `u64::MAX`)
/// cannot wrap the multiply or the add into a small "already expired" value - it
/// pins at `u64::MAX` (effectively never expires, and the server still validates
/// the token on use).
pub(super) fn expires_at_from(secs: u64) -> u64 {
    now_ms().saturating_add(secs.saturating_mul(MILLIS_PER_SEC))
}

#[cfg(test)]
#[path = "../../../tests/mcp/oauth/token.rs"]
mod tests;
