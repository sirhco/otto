//! Provider-agnostic LLM client — port of opencode `packages/llm`.
//!
//! A **route** = **Protocol** (wire shape) + **Endpoint** + **Auth** +
//! **Transport** (framing). Feeding an [`request::LLMRequest`] through a route
//! yields a stream of provider-neutral [`otto_events::LLMEvent`]s, which
//! [`client::LLMClient::generate`] folds into an [`response::LLMResponse`].
//!
//! This crate is the *core seam*: the [`protocol::Protocol`] trait, the
//! [`route`] pipeline, and the shared lifecycle / tool-stream state machines
//! ([`protocols::utils`]). Concrete protocols (anthropic/openai) and the
//! provider registry are filled in by later phases.
//!
//! Source of truth: opencode `packages/llm/src/`.

#![forbid(unsafe_code)]

pub mod auth;
pub mod client;
pub mod error;
pub mod message;
pub mod model;
pub mod models_dev;
pub mod protocol;
pub mod protocols;
pub mod providers;
pub mod registry;
pub mod request;
pub mod response;
pub mod route;
pub mod transport;

// Re-export the streaming event vocabulary from otto-events so downstream
// crates get it from the LLM seam too, without redefining it.
pub use otto_events::{
    FinishReason, Json, LLMEvent, ProviderMetadata, ToolOutput, ToolResultValue, Usage,
};

pub use auth::{AuthDef, Secret};
pub use client::LLMClient;
pub use error::LLMError;
pub use message::{ContentPart, Message, Role, SystemPart, ToolChoice, ToolDefinition};
pub use model::{Model, ModelCapabilities, ModelCost, ModelId, ModelLimits, ProviderId};
pub use protocol::Protocol;
pub use providers::{
    Anthropic, Azure, Bedrock, Copilot, Google, OpenAI, OpenAICompatible, Provider,
};
pub use registry::{lookup, model_or_default, parse_model};
pub use request::{
    CacheHint, CacheKind, CachePolicy, CachePolicyObject, CachePreset, GenerationOptions,
    HttpOptions, LLMRequest, MessagesPolicy, MessagesPolicyNamed, ProviderOptions,
};
pub use response::LLMResponse;
pub use route::{run_stream, Endpoint, GenericRoute, Route};
pub use transport::bedrock::BedrockTransport;
pub use transport::{sse, HttpTransport, PreparedHttp, Transport};

/// The shared lifecycle state machine ([`protocols::utils::lifecycle`]).
pub use protocols::utils::lifecycle;
/// AWS SigV4 credentials, used by [`Bedrock`] ([`protocols::utils::sigv4`]).
pub use protocols::utils::sigv4::AwsCredentials;
/// The shared tool-stream accumulator ([`protocols::utils::tool_stream`]).
pub use protocols::utils::tool_stream;
