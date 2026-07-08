//! Provider construction units: assert each built-in provider wires the right
//! base URL, path, auth header, and (for Anthropic) the `anthropic-version`
//! header. Port-of-behaviour checks for
//! `packages/llm/src/providers/{anthropic,openai,openai-compatible}.ts`.

use std::sync::Arc;

use otto_llm::auth::AuthDef;
use otto_llm::providers::{Anthropic, Azure, Google, OpenAI, OpenAICompatible, Provider};
use otto_llm::transport::HttpTransport;
use otto_llm::Secret;

fn transport() -> Arc<HttpTransport> {
    Arc::new(HttpTransport::new())
}

#[test]
fn anthropic_uses_x_api_key_and_version_header() {
    let provider = Anthropic::new(Some(Secret::literal("sk-ant")), transport());

    let endpoint = provider.endpoint();
    assert_eq!(endpoint.base_url, "https://api.anthropic.com/v1");
    assert_eq!(endpoint.path, "/messages");
    assert_eq!(endpoint.url(), "https://api.anthropic.com/v1/messages");

    // Auth is x-api-key (NOT bearer), with an env fallback via or_else.
    match provider.auth() {
        AuthDef::OrElse(first, _) => match *first {
            AuthDef::Header { name, .. } => assert_eq!(name, "x-api-key"),
            other => panic!("expected x-api-key header, got {other:?}"),
        },
        other => panic!("expected or_else(header, env), got {other:?}"),
    }

    // The required anthropic-version header is stamped on every request.
    assert_eq!(
        provider
            .headers()
            .get("anthropic-version")
            .map(String::as_str),
        Some("2023-06-01")
    );

    // The route resolves to the anthropic protocol.
    assert_eq!(provider.route("claude-sonnet-4").id(), "anthropic");
    assert_eq!(provider.id(), "anthropic");
}

#[test]
fn anthropic_without_key_reads_env() {
    let provider = Anthropic::new(None, transport());
    match provider.auth() {
        AuthDef::Header { name, value } => {
            assert_eq!(name, "x-api-key");
            assert_eq!(value, Secret::config("ANTHROPIC_API_KEY"));
        }
        other => panic!("expected env-backed x-api-key header, got {other:?}"),
    }
}

#[test]
fn openai_uses_bearer_and_chat_completions() {
    let provider = OpenAI::new(Some(Secret::literal("sk-oai")), transport());

    let endpoint = provider.endpoint();
    assert_eq!(endpoint.base_url, "https://api.openai.com/v1");
    assert_eq!(endpoint.path, "/chat/completions");

    match provider.auth() {
        AuthDef::OrElse(first, _) => assert!(matches!(*first, AuthDef::Bearer(_))),
        other => panic!("expected or_else(bearer, env), got {other:?}"),
    }

    assert_eq!(provider.route("gpt-4o").id(), "openai-chat");
    assert_eq!(provider.id(), "openai");
}

#[test]
fn openai_compatible_profiles_have_right_base_urls() {
    let cases = [
        (
            OpenAICompatible::deepseek(None, transport()),
            "deepseek",
            "https://api.deepseek.com/v1",
        ),
        (
            OpenAICompatible::groq(None, transport()),
            "groq",
            "https://api.groq.com/openai/v1",
        ),
        (
            OpenAICompatible::togetherai(None, transport()),
            "togetherai",
            "https://api.together.xyz/v1",
        ),
        (
            OpenAICompatible::cerebras(None, transport()),
            "cerebras",
            "https://api.cerebras.ai/v1",
        ),
        (
            OpenAICompatible::fireworks(None, transport()),
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
        ),
        (
            OpenAICompatible::deepinfra(None, transport()),
            "deepinfra",
            "https://api.deepinfra.com/v1/openai",
        ),
        (
            OpenAICompatible::baseten(None, transport()),
            "baseten",
            "https://inference.baseten.co/v1",
        ),
        (
            OpenAICompatible::openrouter(None, transport()),
            "openrouter",
            "https://openrouter.ai/api/v1",
        ),
        (
            OpenAICompatible::xai(None, transport()),
            "xai",
            "https://api.x.ai/v1",
        ),
    ];
    for (provider, id, base_url) in cases {
        assert_eq!(provider.id(), id);
        assert_eq!(provider.endpoint().base_url, base_url);
        assert_eq!(provider.endpoint().path, "/chat/completions");
        assert_eq!(provider.route("m").id(), "openai-compatible-chat");
        // No key configured → no auth header.
        assert!(matches!(provider.auth(), AuthDef::None));
    }
}

#[test]
fn openai_compatible_bearer_when_key_present() {
    let provider = OpenAICompatible::deepseek(Some(Secret::literal("sk-ds")), transport());
    match provider.auth() {
        AuthDef::Bearer(secret) => assert_eq!(secret, Secret::literal("sk-ds")),
        other => panic!("expected bearer, got {other:?}"),
    }
}

#[test]
fn xai_profile_uses_x_ai_base_url_and_bearer_auth() {
    let provider = OpenAICompatible::xai(Some(Secret::literal("xk")), transport());

    let endpoint = provider.endpoint();
    assert_eq!(endpoint.base_url, "https://api.x.ai/v1");
    assert_eq!(endpoint.path, "/chat/completions");
    assert_eq!(
        endpoint.url(),
        "https://api.x.ai/v1/chat/completions".to_string()
    );
    assert!(endpoint
        .url()
        .starts_with("https://api.x.ai/v1/chat/completions"));

    let mut headers = std::collections::BTreeMap::new();
    provider.auth().apply(&mut headers).expect("apply auth");
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer xk")
    );
}

#[test]
fn azure_endpoint_and_auth() {
    let t = std::sync::Arc::new(HttpTransport::new());
    let p = Azure::new("myres".into(), Some(Secret::literal("ak")), t);
    let url = p.endpoint().url();
    assert!(url.starts_with("https://myres.openai.azure.com/openai/v1/chat/completions"));
    assert!(url.contains("api-version=v1"));
    let mut headers = std::collections::BTreeMap::new();
    p.auth().apply(&mut headers).unwrap();
    assert_eq!(headers.get("api-key").map(String::as_str), Some("ak"));
    assert!(!headers.contains_key("authorization"));
}

#[test]
fn google_endpoint_is_per_model_and_uses_x_goog_api_key() {
    let t = transport();
    let p = Google::new(Some(Secret::literal("gk")), t);

    let url = p.endpoint("gemini-2.0-flash").url();
    assert!(url.contains("/models/gemini-2.0-flash:streamGenerateContent"));
    assert!(url.contains("alt=sse"));

    let mut headers = std::collections::BTreeMap::new();
    p.auth().apply(&mut headers).expect("apply auth");
    assert_eq!(
        headers.get("x-goog-api-key").map(String::as_str),
        Some("gk")
    );
    assert!(!headers.contains_key("authorization"));

    assert_eq!(p.route("gemini-2.0-flash").id(), "gemini");
    assert_eq!(p.id(), "google");
}

#[test]
fn google_without_key_reads_env() {
    let p = Google::new(None, transport());
    match p.auth() {
        AuthDef::Header { name, value } => {
            assert_eq!(name, "x-goog-api-key");
            assert_eq!(value, Secret::config("GOOGLE_GENERATIVE_AI_API_KEY"));
        }
        other => panic!("expected env-backed x-goog-api-key header, got {other:?}"),
    }
}
