//! Store round-trip, mode 0600, slash normalisation, content override.

use std::collections::BTreeMap;

use otto_auth::{AuthStore, Credential};

fn temp_store() -> (tempfile::TempDir, AuthStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let store = AuthStore::with_path(path);
    (dir, store)
}

#[test]
fn set_get_remove_round_trip() {
    let (_dir, store) = temp_store();

    let cred = Credential::Api {
        key: "sk-test".to_string(),
        metadata: None,
    };
    store.set("anthropic", cred.clone()).unwrap();

    assert_eq!(store.get("anthropic").unwrap(), Some(cred));
    assert!(store.get("missing").unwrap().is_none());

    store.remove("anthropic").unwrap();
    assert!(store.get("anthropic").unwrap().is_none());
}

#[test]
fn all_returns_every_credential() {
    let (_dir, store) = temp_store();
    store
        .set(
            "a",
            Credential::Api {
                key: "1".into(),
                metadata: None,
            },
        )
        .unwrap();
    store
        .set(
            "b",
            Credential::WellKnown {
                key: "k".into(),
                token: "t".into(),
            },
        )
        .unwrap();
    let all = store.all().unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.contains_key("a"));
    assert!(all.contains_key("b"));
}

#[cfg(unix)]
#[test]
fn auth_file_is_mode_0600() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, store) = temp_store();
    store
        .set(
            "anthropic",
            Credential::Api {
                key: "sk".into(),
                metadata: None,
            },
        )
        .unwrap();

    let meta = std::fs::metadata(store.path().unwrap()).unwrap();
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "auth.json must be mode 0600, got {mode:o}");
}

#[test]
fn set_normalizes_trailing_slash() {
    let (_dir, store) = temp_store();
    store
        .set(
            "https://example.com/",
            Credential::WellKnown {
                key: "TOKEN".into(),
                token: "abc".into(),
            },
        )
        .unwrap();
    let data = store.all().unwrap();
    assert!(data.contains_key("https://example.com"));
    assert!(!data.contains_key("https://example.com/"));
}

#[test]
fn set_cleans_up_preexisting_trailing_slash_entry() {
    let (_dir, store) = temp_store();
    store
        .set(
            "https://example.com/",
            Credential::WellKnown {
                key: "TOKEN".into(),
                token: "old".into(),
            },
        )
        .unwrap();
    store
        .set(
            "https://example.com",
            Credential::WellKnown {
                key: "TOKEN".into(),
                token: "new".into(),
            },
        )
        .unwrap();
    let data = store.all().unwrap();
    let keys: Vec<_> = data
        .keys()
        .filter(|k| k.contains("example.com"))
        .cloned()
        .collect();
    assert_eq!(keys, vec!["https://example.com".to_string()]);
    match data.get("https://example.com").unwrap() {
        Credential::WellKnown { token, .. } => assert_eq!(token, "new"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn remove_deletes_both_slash_variants() {
    let (_dir, store) = temp_store();
    store
        .set(
            "https://example.com",
            Credential::WellKnown {
                key: "TOKEN".into(),
                token: "abc".into(),
            },
        )
        .unwrap();
    store.remove("https://example.com/").unwrap();
    let data = store.all().unwrap();
    assert!(!data.contains_key("https://example.com"));
    assert!(!data.contains_key("https://example.com/"));
}

#[test]
fn content_override_returns_parsed_creds() {
    // Programmatic equivalent of OTTO_AUTH_CONTENT (no env race).
    let content = r#"{
        "anthropic": { "type": "api", "key": "sk-override" },
        "openai": { "type": "oauth", "refresh": "r", "access": "a", "expires": 123 }
    }"#;
    let store = AuthStore::with_content(content);

    match store.get("anthropic").unwrap().unwrap() {
        Credential::Api { key, .. } => assert_eq!(key, "sk-override"),
        other => panic!("unexpected: {other:?}"),
    }
    match store.get("openai").unwrap().unwrap() {
        Credential::Oauth {
            refresh,
            access,
            expires,
            ..
        } => {
            assert_eq!(refresh, "r");
            assert_eq!(access, "a");
            assert_eq!(expires, 123);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn content_backed_store_is_read_only() {
    let store = AuthStore::with_content("{}");
    let err = store
        .set(
            "x",
            Credential::Api {
                key: "k".into(),
                metadata: None,
            },
        )
        .unwrap_err();
    assert!(matches!(err, otto_auth::AuthError::Io(_)));
}

#[test]
fn missing_file_is_empty_map() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::with_path(dir.path().join("does-not-exist.json"));
    assert!(store.all().unwrap().is_empty());
}

#[test]
fn api_metadata_round_trips() {
    let (_dir, store) = temp_store();
    let mut md = BTreeMap::new();
    md.insert("plan".to_string(), "max".to_string());
    let cred = Credential::Api {
        key: "sk".into(),
        metadata: Some(md.clone()),
    };
    store.set("prov", cred.clone()).unwrap();
    assert_eq!(store.get("prov").unwrap(), Some(cred));
}
