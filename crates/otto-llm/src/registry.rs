//! The process-global model registry.
//!
//! Port of the model metadata opencode reads from `models.dev`
//! (`packages/llm/src/schema`): each provider/model pair carries the id of
//! the [`Route`] that serves it, token limits, and capability flags. The
//! registry is loaded once per process — [`current`] lazily initialises it
//! to the embedded snapshot ([`crate::models_dev::Registry::embedded`]) on
//! first use, and a fetched-and-refreshed registry can later be swapped in
//! via [`install`] (e.g. by the Runtime after pulling a fresh models.dev
//! snapshot). [`parse_model`] and [`lookup`] cover the identity plumbing, and
//! [`model_or_default`] synthesises a sensible [`Model`] for anything not
//! in the installed registry.
//!
//! [`Route`]: crate::route::Route

use crate::model::{Model, ModelId, ProviderId};
use crate::models_dev::Registry;
use std::sync::{Arc, OnceLock, RwLock};

/// The process-global registry cell. Lazily initialised to
/// [`Registry::embedded`] on first access; swappable via [`install`].
static REGISTRY: OnceLock<RwLock<Arc<Registry>>> = OnceLock::new();

fn cell() -> &'static RwLock<Arc<Registry>> {
    REGISTRY.get_or_init(|| RwLock::new(Arc::new(Registry::embedded())))
}

/// Returns the currently installed registry, initialising it to
/// [`Registry::embedded`] on first call.
#[must_use]
pub fn current() -> Arc<Registry> {
    cell().read().expect("registry lock poisoned").clone()
}

/// Replaces the process-global registry (e.g. after the Runtime fetches a
/// fresh models.dev snapshot).
pub fn install(registry: Registry) {
    *cell().write().expect("registry lock poisoned") = Arc::new(registry);
}

/// Look a model up by `provider` + `model` id, returning its [`Model`] if
/// present in the currently installed registry.
///
/// Port of the models.dev lookup opencode performs when resolving a model id.
#[must_use]
pub fn lookup(provider: &str, model: &str) -> Option<Model> {
    current().lookup(provider, model)
}

/// Split a `"provider/model"` string into its parts on the **first** `/`.
///
/// Model ids may themselves contain `/` (e.g. OpenRouter's
/// `anthropic/claude-3.5-sonnet`), so only the leading segment is treated as
/// the provider. Returns `None` when there is no `/` at all, or when either
/// side is empty.
#[must_use]
pub fn parse_model(s: &str) -> Option<(ProviderId, ModelId)> {
    let (provider, model) = s.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((ProviderId::new(provider), ModelId::new(model)))
}

/// Resolve a model to a [`Model`], falling back to a default record when the
/// id is not embedded in the registry.
///
/// Unknown models get default (all-false / empty) capabilities and no limits,
/// with `route_id` set to `default_route` so a route can still be constructed.
/// This mirrors opencode tolerating models absent from its snapshot of
/// models.dev.
#[must_use]
pub fn model_or_default(provider: &str, model: &str, default_route: &str) -> Model {
    lookup(provider, model).unwrap_or_else(|| Model::new(provider, model, default_route))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelCapabilities, ModelLimits};

    #[test]
    fn parse_model_splits_on_first_slash() {
        let (p, m) = parse_model("anthropic/claude-opus-4-8").expect("parsed");
        assert_eq!(p, ProviderId::new("anthropic"));
        assert_eq!(m, ModelId::new("claude-opus-4-8"));
    }

    #[test]
    fn parse_model_keeps_slashes_in_model_id() {
        let (p, m) = parse_model("openrouter/anthropic/claude-3.5-sonnet").expect("parsed");
        assert_eq!(p, ProviderId::new("openrouter"));
        // Everything after the first `/` is the model id, slashes intact.
        assert_eq!(m, ModelId::new("anthropic/claude-3.5-sonnet"));
    }

    #[test]
    fn parse_model_requires_a_slash() {
        assert!(parse_model("claude-opus-4-8").is_none());
        assert!(parse_model("").is_none());
        assert!(parse_model("anthropic/").is_none());
        assert!(parse_model("/model").is_none());
    }

    /// Installs the fixture registry (`tests/fixtures/models_dev_sample.json`)
    /// as the process-global registry. Test-only: the global is process-wide
    /// and shared across all `#[test]`s in this crate, which run in
    /// parallel, so every install-dependent assertion is consolidated into
    /// one test (`lookup_reads_installed_registry`) rather than spread
    /// across several tests that would race each other's `install`/`current`
    /// calls.
    fn install_test_registry() {
        let json = include_str!("../tests/fixtures/models_dev_sample.json");
        install(crate::models_dev::Registry::from_json(json).unwrap());
    }

    #[test]
    fn lookup_reads_installed_registry() {
        install_test_registry();

        let opus = lookup("anthropic", "claude-opus-4-8").expect("known model");
        assert_eq!(opus.provider, ProviderId::new("anthropic"));
        assert_eq!(opus.route_id, "anthropic");
        assert_eq!(opus.limits.context, Some(200_000));
        assert_eq!(opus.limits.output, Some(32_000));
        assert!(opus.capabilities.reasoning);
        assert!(opus.capabilities.tool_call);
        assert!(opus.capabilities.attachment);

        let o3 = lookup("openai", "o3").expect("known model");
        assert_eq!(o3.route_id, "openai-chat");
        assert_eq!(o3.limits.context, Some(200_000));
        assert!(o3.capabilities.reasoning);
        assert!(!o3.capabilities.temperature);

        let gpt = lookup("openai", "gpt-4o").expect("known model");
        assert_eq!(gpt.route_id, "openai-chat");
        assert_eq!(gpt.limits.context, Some(128_000));
        assert!(!gpt.capabilities.reasoning);

        // Misses are None.
        assert!(lookup("anthropic", "nope").is_none());
        assert!(lookup("bogus", "gpt-4o").is_none());

        // model_or_default synthesises unknown ids with default caps/limits...
        let unknown = model_or_default("anthropic", "claude-future-99", "anthropic");
        assert_eq!(unknown.id, ModelId::new("claude-future-99"));
        assert_eq!(unknown.provider, ProviderId::new("anthropic"));
        assert_eq!(unknown.route_id, "anthropic");
        assert_eq!(unknown.limits, ModelLimits::default());
        assert_eq!(unknown.capabilities, ModelCapabilities::default());

        // ...and still resolves known ids from the installed registry.
        let known = model_or_default("openai", "o3", "openai-chat");
        assert!(known.capabilities.reasoning);
    }
}
