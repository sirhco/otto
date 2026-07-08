//! The Amazon Bedrock provider.
//!
//! Wires the SigV4-signed [`BedrockTransport`] and the
//! [`protocols::bedrock_converse::BedrockConverse`] protocol into a
//! [`Provider`]. Unlike the other built-in providers, `Bedrock` is NOT
//! generic over [`Transport`](crate::transport::Transport) — signing is done
//! by [`BedrockTransport`] itself (it owns the [`AwsCredentials`]), so
//! [`Bedrock::route`] always passes [`AuthDef::None`]; there is no separate
//! header-based auth strategy to layer on top.
//!
//! Serves `POST {baseURL}/model/{model_id}/converse-stream`, where the model
//! id — unlike Anthropic/OpenAI's static path — is embedded per model
//! (mirroring [`super::google::Google`]'s per-model endpoint).

use std::sync::Arc;

use crate::auth::AuthDef;
use crate::model::Model;
use crate::protocols::bedrock_converse::BedrockConverse;
use crate::protocols::utils::sigv4::AwsCredentials;
use crate::registry;
use crate::route::{Endpoint, GenericRoute, Route};
use crate::transport::bedrock::BedrockTransport;

use super::Provider;

/// The provider id (`ProviderID.make("amazon-bedrock")`), matching the
/// models.dev provider key.
const PROVIDER_ID: &str = "amazon-bedrock";
/// The route id served by this provider (`BedrockConverse::id`).
const ROUTE_ID: &str = "bedrock-converse";

/// The native Amazon Bedrock provider.
///
/// Owns a [`BedrockTransport`] built from [`AwsCredentials`] at construction
/// time (see the module docs for why this isn't generic over `Transport`).
pub struct Bedrock {
    base_url: String,
    transport: Arc<BedrockTransport>,
}

impl Bedrock {
    /// Configure the provider from AWS credentials. The default base URL is
    /// derived from `creds.region`
    /// (`https://bedrock-runtime.{region}.amazonaws.com`), and the transport
    /// is a fresh real-clock [`BedrockTransport`] signing with `creds`.
    #[must_use]
    pub fn new(creds: AwsCredentials) -> Self {
        let base_url = format!("https://bedrock-runtime.{}.amazonaws.com", creds.region);
        Bedrock {
            base_url,
            transport: Arc::new(BedrockTransport::new(creds)),
        }
    }

    /// Override the base URL (e.g. a mock server in tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the transport (e.g. a [`BedrockTransport::with_fixed_clock`]
    /// in tests, so a pinned clock reaches the route).
    #[must_use]
    pub fn with_transport(mut self, transport: BedrockTransport) -> Self {
        self.transport = Arc::new(transport);
        self
    }

    /// The resolved endpoint for `model_id`
    /// (`{baseURL}/model/{model_id}/converse-stream`). Like Gemini, the model
    /// id is embedded in the path, so the endpoint must be rebuilt per model.
    /// `model_id` is percent-encoded (unreserved-set + `/` preserved) to
    /// match the encoding [`crate::protocols::utils::sigv4::sign`]
    /// independently derives for the canonical request.
    #[must_use]
    pub fn endpoint(&self, model_id: &str) -> Endpoint {
        Endpoint::new(
            self.base_url.clone(),
            format!(
                "/model/{}/converse-stream",
                percent_encode_model_id(model_id)
            ),
        )
    }
}

/// Percent-encode a Bedrock model id for use in a URL path: every byte
/// outside the RFC 3986 unreserved set (`A-Z a-z 0-9 - _ . ~`) is
/// percent-encoded, while `/` separators are preserved (cross-region
/// inference profile ids and colon-suffixed model ids like
/// `anthropic.claude-3-5-sonnet-20241022-v2:0` need this).
fn percent_encode_model_id(model_id: &str) -> String {
    let mut out = String::with_capacity(model_id.len());
    for byte in model_id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

impl Provider for Bedrock {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn route(&self, model_id: &str) -> Box<dyn Route> {
        Box::new(GenericRoute::new(
            Arc::new(BedrockConverse),
            self.endpoint(model_id),
            AuthDef::None,
            self.transport.clone(),
        ))
    }

    fn model(&self, model_id: &str) -> Model {
        registry::model_or_default(PROVIDER_ID, model_id, ROUTE_ID)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_creds() -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            region: "us-east-1".into(),
        }
    }

    #[test]
    fn endpoint_embeds_model_id_and_default_region_host() {
        let endpoint = Bedrock::new(test_creds()).endpoint("anthropic.claude-3");
        let url = endpoint.url();
        assert!(
            url.contains("/model/anthropic.claude-3/converse-stream"),
            "unexpected url: {url}"
        );
        assert!(
            url.contains("bedrock-runtime.us-east-1.amazonaws.com"),
            "unexpected url: {url}"
        );
    }

    #[test]
    fn endpoint_uses_region_from_credentials() {
        let mut creds = test_creds();
        creds.region = "eu-west-1".into();
        let endpoint = Bedrock::new(creds).endpoint("anthropic.claude-3");
        assert!(
            endpoint
                .url()
                .contains("bedrock-runtime.eu-west-1.amazonaws.com")
        );
    }

    #[test]
    fn endpoint_percent_encodes_colon_in_model_id() {
        let endpoint =
            Bedrock::new(test_creds()).endpoint("anthropic.claude-3-5-sonnet-20241022-v2:0");
        assert!(
            endpoint
                .url()
                .contains("/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse-stream")
        );
    }

    #[test]
    fn with_base_url_overrides_default() {
        let endpoint = Bedrock::new(test_creds())
            .with_base_url("http://127.0.0.1:9999")
            .endpoint("m");
        assert_eq!(
            endpoint.url(),
            "http://127.0.0.1:9999/model/m/converse-stream"
        );
    }

    #[test]
    fn id_and_model_use_provider_id() {
        let provider = Bedrock::new(test_creds());
        assert_eq!(provider.id(), "amazon-bedrock");
        let model = provider.model("anthropic.claude-3");
        assert_eq!(model.provider.0, "amazon-bedrock");
        assert_eq!(model.route_id, "bedrock-converse");
    }
}
