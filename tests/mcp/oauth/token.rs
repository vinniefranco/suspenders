use super::*;

// ---- Token expiry ----

#[test]
fn a_token_with_no_expiry_is_never_expired() {
    let token = OAuthToken {
        access_token: "t".into(),
        token_type: "Bearer".into(),
        refresh_token: None,
        expires_at: None,
        scope: None,
    };
    assert!(!token.is_expired(u64::MAX / 2));
}

#[test]
fn a_token_expires_within_the_skew_buffer() {
    let token = OAuthToken {
        access_token: "t".into(),
        token_type: "Bearer".into(),
        refresh_token: None,
        expires_at: Some(10_000_000),
        scope: None,
    };
    // Well before expiry (minus the buffer): valid.
    assert!(!token.is_expired(10_000_000 - EXPIRY_BUFFER_MS - 1));
    // Inside the buffer window: treated as expired.
    assert!(token.is_expired(10_000_000 - EXPIRY_BUFFER_MS));
    assert!(token.is_expired(10_000_000));
}

#[test]
fn expires_at_from_saturates_on_an_absurd_expires_in() {
    // A hostile/huge `expires_in` must not wrap the *1000 or the +now into a
    // small "already expired" value: it pins at u64::MAX (never expires).
    assert_eq!(expires_at_from(u64::MAX), u64::MAX);
    assert_eq!(expires_at_from(u64::MAX / 2), u64::MAX);
    // A normal value still lands in the future (now + secs*1000, both > 0).
    assert!(expires_at_from(3600) >= 3_600_000);
}

// ---- Storage: pure JSON split ----

fn cred(server: &str, access: &str) -> OAuthCredentials {
    OAuthCredentials {
        server_name: server.to_string(),
        token: OAuthToken {
            access_token: access.to_string(),
            token_type: "Bearer".into(),
            refresh_token: Some("ref".into()),
            expires_at: Some(123),
            scope: Some("read".into()),
        },
        client_id: Some("cid".into()),
        token_url: Some("https://auth.test/token".into()),
        mcp_server_url: Some("https://mcp.test/mcp".into()),
        updated_at: 42,
    }
}

#[test]
fn parse_and_serialize_credentials_round_trip_via_literals() {
    let mut all = BTreeMap::new();
    all.insert("gh".to_string(), cred("gh", "tok-gh"));
    all.insert("gl".to_string(), cred("gl", "tok-gl"));
    let json = serialize_credentials(&all).unwrap();
    let back = parse_credentials(&json).unwrap();
    assert_eq!(back, all);
    // The on-disk shape is a JSON array in server-name order.
    assert!(json.trim_start().starts_with('['));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value[0]["server_name"], "gh");
    assert_eq!(value[1]["server_name"], "gl");
}

#[test]
fn parse_credentials_lets_a_later_duplicate_win() {
    let raw = r#"[
            {"server_name":"gh","token":{"access_token":"old","token_type":"Bearer"},"updated_at":1},
            {"server_name":"gh","token":{"access_token":"new","token_type":"Bearer"},"updated_at":2}
        ]"#;
    let all = parse_credentials(raw).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all["gh"].token.access_token, "new");
}

// ---- Storage: file IO round-trip ----

fn temp_store() -> (tempfile::TempDir, McpOAuthTokenStorage) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp-oauth-tokens.json");
    let storage = McpOAuthTokenStorage::new(path.to_string_lossy().into_owned());
    (dir, storage)
}

#[test]
fn get_on_an_absent_file_is_an_empty_map() {
    let (_dir, storage) = temp_store();
    assert!(storage.get_all().unwrap().is_empty());
    assert!(storage.get("gh").unwrap().is_none());
}

#[test]
fn set_get_delete_get_all_round_trip() {
    let (_dir, storage) = temp_store();
    storage.set(cred("gh", "tok-gh")).unwrap();
    storage.set(cred("gl", "tok-gl")).unwrap();

    assert_eq!(
        storage.get("gh").unwrap().unwrap().token.access_token,
        "tok-gh"
    );
    let all = storage.get_all().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all["gl"].token.access_token, "tok-gl");

    // Replacing a server upserts by name (still two entries).
    storage.set(cred("gh", "tok-gh-2")).unwrap();
    assert_eq!(
        storage.get("gh").unwrap().unwrap().token.access_token,
        "tok-gh-2"
    );
    assert_eq!(storage.get_all().unwrap().len(), 2);

    // Deleting one leaves the other.
    storage.delete("gh").unwrap();
    assert!(storage.get("gh").unwrap().is_none());
    assert_eq!(storage.get_all().unwrap().len(), 1);

    // Deleting an absent server is a no-op.
    storage.delete("ghost").unwrap();
    assert_eq!(storage.get_all().unwrap().len(), 1);
}

#[test]
fn deleting_the_last_credential_removes_the_file() {
    let (_dir, storage) = temp_store();
    storage.set(cred("gh", "tok")).unwrap();
    storage.delete("gh").unwrap();
    // The file is unlinked (qwen removes it rather than leaving `[]`), so a
    // fresh read is an empty map with no leftover file.
    assert!(storage.get_all().unwrap().is_empty());
    assert!(!std::path::Path::new(&storage.path).exists());
}

#[cfg(unix)]
#[test]
fn the_token_file_is_written_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, storage) = temp_store();
    storage.set(cred("gh", "tok")).unwrap();
    let mode = std::fs::metadata(&storage.path)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn get_all_errors_on_a_malformed_file() {
    let (_dir, storage) = temp_store();
    std::fs::write(&storage.path, "not json").unwrap();
    assert!(storage.get_all().is_err());
}
