//! The PKCE parameters for one authorization flow (qwen's `generatePKCEParams`):
//! the S256 code challenge, the verifier, and the CSRF state.

use base64::Engine;
use sha2::{Digest, Sha256};

/// The number of CSPRNG bytes the PKCE verifier is built from (qwen's 32-byte
/// verifier); base64url'd (no padding) this is 43 characters.
const PKCE_VERIFIER_BYTES: usize = 32;

/// The number of CSPRNG bytes the CSRF state is built from (qwen's 16-byte
/// state); base64url'd (no padding) this is 22 characters.
const OAUTH_STATE_BYTES: usize = 16;

/// The base64url engine (RFC 4648 URL-safe, no padding) qwen's `base64url`
/// encoding maps to - used for the PKCE verifier, challenge, and state.
pub(super) const BASE64URL: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The PKCE parameters for one authorization flow (qwen's `PKCEParams`): the
/// `code_verifier` (kept for the token exchange), its `code_challenge` (sent on
/// the authorization request), and the `state` (CSRF guard, echoed back on the
/// callback).
#[derive(Debug, Clone)]
pub struct PkceParams {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
}

impl PkceParams {
    /// Generates fresh PKCE parameters (qwen `generatePKCEParams`): the verifier
    /// is base64url of 32 CSPRNG bytes, the challenge is base64url(sha256(verifier))
    /// (S256), and the state is base64url of 16 CSPRNG bytes.
    pub fn generate() -> PkceParams {
        use rand::RngCore;
        let mut rng = rand::thread_rng();

        let mut verifier_bytes = [0u8; PKCE_VERIFIER_BYTES];
        rng.fill_bytes(&mut verifier_bytes);
        let code_verifier = BASE64URL.encode(verifier_bytes);

        let code_challenge = challenge_for(&code_verifier);

        let mut state_bytes = [0u8; OAUTH_STATE_BYTES];
        rng.fill_bytes(&mut state_bytes);
        let state = BASE64URL.encode(state_bytes);

        PkceParams {
            code_verifier,
            code_challenge,
            state,
        }
    }
}

/// The S256 code challenge for a verifier (qwen's `sha256(verifier)` base64url):
/// base64url of the SHA-256 digest of the verifier's UTF-8 bytes. Pure, so the
/// PKCE relation is unit-tested against a literal verifier.
pub fn challenge_for(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    BASE64URL.encode(hasher.finalize())
}

#[cfg(test)]
#[path = "../../../tests/mcp/oauth/pkce.rs"]
mod tests;
