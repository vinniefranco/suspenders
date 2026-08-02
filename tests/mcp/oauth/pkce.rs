use super::*;

#[test]
fn pkce_challenge_is_base64url_sha256_of_the_verifier() {
    // A known-answer vector: SHA-256 of "abc123" base64url'd (no padding).
    let challenge = challenge_for("abc123");
    let mut hasher = Sha256::new();
    hasher.update(b"abc123");
    let expected = BASE64URL.encode(hasher.finalize());
    assert_eq!(challenge, expected);
    // base64url never carries `+`, `/`, or `=` padding.
    assert!(!challenge.contains('+'));
    assert!(!challenge.contains('/'));
    assert!(!challenge.contains('='));
}

#[test]
fn generate_makes_the_expected_shapes_and_holds_the_pkce_relation() {
    let pkce = PkceParams::generate();
    // 32 random bytes base64url'd (no padding) is 43 chars; 16 bytes is 22.
    assert_eq!(pkce.code_verifier.len(), 43);
    assert_eq!(pkce.state.len(), 22);
    // The challenge is the S256 of the freshly generated verifier.
    assert_eq!(pkce.code_challenge, challenge_for(&pkce.code_verifier));
    // Two generations differ (the CSPRNG is live).
    assert_ne!(pkce.code_verifier, PkceParams::generate().code_verifier);
}
