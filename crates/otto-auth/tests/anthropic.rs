//! Anthropic: authorize_url shape; exchange/refresh against a wiremock server.

use otto_auth::providers::anthropic::AnthropicOAuth;
use otto_auth::{Credential, Pkce};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn authorize_url_contains_pkce_and_client_id() {
    let pkce = Pkce::generate();
    let oauth = AnthropicOAuth::new();
    let (url, verifier) = oauth.authorize_url(&pkce);

    assert_eq!(verifier, pkce.verifier);
    assert!(url.contains("code_challenge="));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("client_id="));
    // The challenge value should be url-encoded present in the string.
    assert!(url.contains(&pkce.challenge) || url.contains(&urlencode(&pkce.challenge)));
}

fn urlencode(s: &str) -> String {
    // base64url uses only unreserved chars, so this is effectively identity,
    // but guard anyway.
    s.replace('+', "%2B").replace('/', "%2F")
}

#[tokio::test]
async fn exchange_produces_oauth_credential() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "at-new",
            "refresh_token": "rt-new",
            "expires_in": 3600
        })))
        .mount(&server)
        .await;

    let oauth = AnthropicOAuth::with_token_url(format!("{}/v1/oauth/token", server.uri()));
    let cred = oauth
        .exchange("the-code#the-state", "the-verifier")
        .await
        .unwrap();

    match cred {
        Credential::Oauth {
            access,
            refresh,
            expires,
            ..
        } => {
            assert_eq!(access, "at-new");
            assert_eq!(refresh, "rt-new");
            assert!(expires > 0);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn refresh_produces_oauth_credential() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "at-refreshed",
            "refresh_token": "rt-refreshed",
            "expires_in": 7200
        })))
        .mount(&server)
        .await;

    let oauth = AnthropicOAuth::with_token_url(format!("{}/v1/oauth/token", server.uri()));
    let cred = oauth.refresh("old-refresh").await.unwrap();

    match cred {
        Credential::Oauth {
            access, refresh, ..
        } => {
            assert_eq!(access, "at-refreshed");
            assert_eq!(refresh, "rt-refreshed");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn non_2xx_is_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let oauth = AnthropicOAuth::with_token_url(format!("{}/v1/oauth/token", server.uri()));
    let err = oauth.refresh("x").await.unwrap_err();
    match err {
        otto_auth::AuthError::Http { status, .. } => assert_eq!(status, 400),
        other => panic!("unexpected: {other:?}"),
    }
}
