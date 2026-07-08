//! resolve: an expired Oauth triggers refresh (wiremock) and persists the new
//! tokens; a fresh credential is returned untouched.

use otto_auth::{AuthStore, Credential, Resolver};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn expired_anthropic_oauth_is_refreshed_and_persisted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "at-fresh",
            "refresh_token": "rt-fresh",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::with_path(dir.path().join("auth.json"));
    // Store an already-expired oauth credential.
    store
        .set(
            "anthropic",
            Credential::Oauth {
                refresh: "rt-old".into(),
                access: "at-old".into(),
                expires: now_ms() - 10_000,
                account_id: None,
                enterprise_url: None,
            },
        )
        .unwrap();

    let resolver = Resolver {
        anthropic_token_url: Some(format!("{}/v1/oauth/token", server.uri())),
        expiry_margin_ms: 60_000,
    };
    let resolved = resolver
        .resolve("anthropic", &store)
        .await
        .unwrap()
        .unwrap();

    assert!(resolved.refreshed);
    match &resolved.credential {
        Credential::Oauth {
            access, refresh, ..
        } => {
            assert_eq!(access, "at-fresh");
            assert_eq!(refresh, "rt-fresh");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // Persisted to the store.
    match store.get("anthropic").unwrap().unwrap() {
        Credential::Oauth { access, .. } => assert_eq!(access, "at-fresh"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn fresh_credential_is_returned_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::with_path(dir.path().join("auth.json"));
    store
        .set(
            "anthropic",
            Credential::Oauth {
                refresh: "rt".into(),
                access: "at".into(),
                expires: now_ms() + 3_600_000,
                account_id: None,
                enterprise_url: None,
            },
        )
        .unwrap();

    // No token url configured: if resolve tried to refresh it would fail to
    // connect. A fresh credential must not trigger a refresh.
    let resolver = Resolver {
        anthropic_token_url: Some("http://127.0.0.1:1/never".into()),
        expiry_margin_ms: 60_000,
    };
    let resolved = resolver
        .resolve("anthropic", &store)
        .await
        .unwrap()
        .unwrap();
    assert!(!resolved.refreshed);
    match resolved.credential {
        Credential::Oauth { access, .. } => assert_eq!(access, "at"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn missing_provider_resolves_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::with_path(dir.path().join("auth.json"));
    let resolver = Resolver::new();
    assert!(resolver.resolve("nope", &store).await.unwrap().is_none());
}

#[tokio::test]
async fn copilot_oauth_with_zero_expiry_is_not_refreshed() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::with_path(dir.path().join("auth.json"));
    // Copilot stores expires: 0 but has no refresh flow; must be returned as-is.
    store
        .set(
            "github-copilot",
            Credential::Oauth {
                refresh: "ghu".into(),
                access: "ghu".into(),
                expires: 0,
                account_id: None,
                enterprise_url: None,
            },
        )
        .unwrap();
    let resolver = Resolver::new();
    let resolved = resolver
        .resolve("github-copilot", &store)
        .await
        .unwrap()
        .unwrap();
    assert!(!resolved.refreshed);
    match resolved.credential {
        Credential::Oauth { access, .. } => assert_eq!(access, "ghu"),
        other => panic!("unexpected: {other:?}"),
    }
}
