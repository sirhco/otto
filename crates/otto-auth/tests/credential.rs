//! Credential serde: each variant round-trips; a literal opencode auth.json
//! blob deserializes.

use std::collections::BTreeMap;

use otto_auth::Credential;

#[test]
fn oauth_round_trips() {
    let cred = Credential::Oauth {
        refresh: "r".into(),
        access: "a".into(),
        expires: 1_700_000_000_000,
        account_id: Some("acct_1".into()),
        enterprise_url: None,
    };
    let json = serde_json::to_string(&cred).unwrap();
    assert!(json.contains("\"type\":\"oauth\""));
    assert!(json.contains("\"accountId\":\"acct_1\""));
    // enterprise_url is None -> skipped.
    assert!(!json.contains("enterpriseUrl"));
    let back: Credential = serde_json::from_str(&json).unwrap();
    assert_eq!(cred, back);
}

#[test]
fn api_round_trips() {
    let mut md = BTreeMap::new();
    md.insert("k".to_string(), "v".to_string());
    let cred = Credential::Api {
        key: "sk-abc".into(),
        metadata: Some(md),
    };
    let json = serde_json::to_string(&cred).unwrap();
    assert!(json.contains("\"type\":\"api\""));
    let back: Credential = serde_json::from_str(&json).unwrap();
    assert_eq!(cred, back);
}

#[test]
fn wellknown_round_trips() {
    let cred = Credential::WellKnown {
        key: "KEY".into(),
        token: "TOK".into(),
    };
    let json = serde_json::to_string(&cred).unwrap();
    assert!(json.contains("\"type\":\"wellknown\""));
    let back: Credential = serde_json::from_str(&json).unwrap();
    assert_eq!(cred, back);
}

#[test]
fn literal_opencode_auth_json_deserializes() {
    // A blob shaped exactly like opencode's auth.json.
    let blob = r#"{
        "anthropic": {
            "type": "oauth",
            "refresh": "rt-anthropic",
            "access": "at-anthropic",
            "expires": 1750000000000
        },
        "openai": {
            "type": "oauth",
            "refresh": "rt-openai",
            "access": "at-openai",
            "expires": 1750000000000,
            "accountId": "acct_openai"
        },
        "github-copilot": {
            "type": "oauth",
            "refresh": "ghu_token",
            "access": "ghu_token",
            "expires": 0,
            "enterpriseUrl": "company.ghe.com"
        },
        "openrouter": { "type": "api", "key": "sk-or-123" },
        "some-wellknown": { "type": "wellknown", "key": "WK", "token": "wk-token" }
    }"#;
    let map: BTreeMap<String, Credential> = serde_json::from_str(blob).unwrap();
    assert_eq!(map.len(), 5);

    match &map["github-copilot"] {
        Credential::Oauth {
            expires,
            enterprise_url,
            ..
        } => {
            assert_eq!(*expires, 0);
            assert_eq!(enterprise_url.as_deref(), Some("company.ghe.com"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    match &map["openrouter"] {
        Credential::Api { key, .. } => assert_eq!(key, "sk-or-123"),
        other => panic!("unexpected: {other:?}"),
    }
    match &map["some-wellknown"] {
        Credential::WellKnown { key, token } => {
            assert_eq!(key, "WK");
            assert_eq!(token, "wk-token");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn is_expired_only_applies_to_oauth() {
    let oauth = Credential::Oauth {
        refresh: "r".into(),
        access: "a".into(),
        expires: 1000,
        account_id: None,
        enterprise_url: None,
    };
    assert!(oauth.is_expired(1000, 0));
    assert!(oauth.is_expired(500, 600)); // within margin
    assert!(!oauth.is_expired(500, 400)); // not yet

    let api = Credential::Api {
        key: "k".into(),
        metadata: None,
    };
    assert!(!api.is_expired(i64::MAX, i64::MAX));
}
