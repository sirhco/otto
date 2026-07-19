//! Built-in provider definitions.
//!
//! Port of opencode `packages/llm/src/provider.ts` and
//! `packages/llm/src/providers/{anthropic,openai,openai-compatible}.ts`. A
//! [`Provider`] pairs deployment config (API key, base URL, transport) with a
//! wire [`Protocol`] and assembles a ready-to-run [`Route`] for a model. This
//! is the Rust analogue of opencode's `configure(options).model(id)` facades:
//! the deployment config is chosen at construction time, then a model is
//! selected.
//!
//! Every built-in provider exposes a single route, so the `model_id` passed to
//! [`Provider::route`] is advisory — the concrete model id is carried on the
//! [`LLMRequest`] (via [`crate::model::Model`]) and lowered into the request
//! body by the protocol. [`Provider::model`] bridges to the minimal
//! [`crate::registry`] so callers get real token limits / capabilities.
//!
//! [`Protocol`]: crate::protocol::Protocol

mod anthropic;
mod azure;
mod copilot;
mod google;
mod openai;
mod openai_compatible;
mod vertex;

use std::sync::Arc;

use crate::client::LLMClient;
use crate::model::Model;
use crate::route::Route;

pub use anthropic::Anthropic;
pub use azure::Azure;
pub use copilot::Copilot;
pub use google::Google;
pub use openai::OpenAI;
pub use openai_compatible::OpenAICompatible;
pub use vertex::Vertex;

/// A configured provider that can build routes and clients for its models.
///
/// Port of the `Provider.Definition` shape (`provider.ts`): a stable `id`
/// plus a model factory. Here the factory is split into [`Provider::route`]
/// (the transport-bound [`Route`]) and [`Provider::model`] (the model
/// metadata), with [`Provider::client`] as the common convenience.
pub trait Provider {
    /// The provider id, e.g. `anthropic` / `openai` / `deepseek`.
    fn id(&self) -> &str;

    /// Build the [`Route`] serving `model_id`.
    ///
    /// All built-in providers expose one route, so `model_id` is advisory (the
    /// model id travels on the request); it is accepted to match
    /// `provider.model(id)` and to leave room for multi-route providers.
    fn route(&self, model_id: &str) -> Box<dyn Route>;

    /// Resolve the [`Model`] metadata for `model_id` (registry lookup, with a
    /// default record for ids not embedded in the registry).
    fn model(&self, model_id: &str) -> Model;

    /// Build an [`LLMClient`] bound to `model_id`'s route.
    fn client(&self, model_id: &str) -> LLMClient {
        LLMClient::new(Arc::from(self.route(model_id)))
    }
}
