//! Copilot: device-start + poll against wiremock (canned device + token).

use otto_auth::Credential;
use otto_auth::providers::copilot::{CopilotOAuth, DevicePoll};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn start_device_parses_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "device_code": "dc-123",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "interval": 5
        })))
        .mount(&server)
        .await;

    let copilot = CopilotOAuth::with_base_url(server.uri());
    let start = copilot.start_device().await.unwrap();
    assert_eq!(start.device_code, "dc-123");
    assert_eq!(start.user_code, "ABCD-1234");
    assert_eq!(start.verification_uri, "https://github.com/login/device");
    assert_eq!(start.interval, 5);
}

#[tokio::test]
async fn poll_complete_returns_oauth_credential() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ghu_abc123",
            "token_type": "bearer",
            "scope": "read:user"
        })))
        .mount(&server)
        .await;

    let copilot = CopilotOAuth::with_base_url(server.uri());
    match copilot.poll("dc-123").await.unwrap() {
        DevicePoll::Complete(cred) => match *cred {
            Credential::Oauth {
                access,
                refresh,
                expires,
                ..
            } => {
                assert_eq!(access, "ghu_abc123");
                assert_eq!(refresh, "ghu_abc123");
                assert_eq!(expires, 0);
            }
            other => panic!("unexpected: {other:?}"),
        },
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[tokio::test]
async fn poll_pending_and_slow_down() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "error": "authorization_pending"
        })))
        .mount(&server)
        .await;

    let copilot = CopilotOAuth::with_base_url(server.uri());
    assert_eq!(copilot.poll("dc").await.unwrap(), DevicePoll::Pending);
}

#[tokio::test]
async fn poll_terminal_error_is_oauth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "error": "access_denied"
        })))
        .mount(&server)
        .await;

    let copilot = CopilotOAuth::with_base_url(server.uri());
    let err = copilot.poll("dc").await.unwrap_err();
    assert!(matches!(err, otto_auth::AuthError::Oauth(_)));
}

#[tokio::test]
async fn enterprise_domain_is_stamped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ghu_ent"
        })))
        .mount(&server)
        .await;

    let copilot =
        CopilotOAuth::with_base_url(server.uri()).with_enterprise_domain("company.ghe.com");
    match copilot.poll("dc").await.unwrap() {
        DevicePoll::Complete(cred) => match *cred {
            Credential::Oauth { enterprise_url, .. } => {
                assert_eq!(enterprise_url.as_deref(), Some("company.ghe.com"));
            }
            other => panic!("unexpected: {other:?}"),
        },
        other => panic!("expected Complete, got {other:?}"),
    }
}
