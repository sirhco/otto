//! Serde wire-schema for the models.dev `api.json` registry format, plus the
//! [`Registry`] it maps into and the [`load`]/[`refresh`] fetch-or-embed
//! loader.
//!
//! [`registry`](crate::registry) backs its process-global model registry
//! with [`Registry::embedded`], so this module is load-bearing outside its
//! own tests, and is now public so the CLI/runtime can drive [`load`] and
//! [`registry::install`](crate::registry::install) directly.

use serde::Deserialize;
use std::collections::HashMap;

pub type RawApi = HashMap<String, RawProvider>;

#[derive(Deserialize, Clone, Debug)]
pub struct RawProvider {
    #[serde(default)]
    pub api: Option<String>,
    pub name: String,
    #[serde(default)]
    pub env: Vec<String>,
    pub id: String,
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub models: HashMap<String, RawModel>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct RawModel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub attachment: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub temperature: bool,
    #[serde(default)]
    pub tool_call: bool,
    #[serde(default)]
    pub cost: Option<RawCost>,
    #[serde(default)]
    pub limit: RawLimit,
    #[serde(default)]
    pub modalities: Option<RawModalities>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct RawCost {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct RawLimit {
    #[serde(default)]
    pub context: Option<u64>,
    #[serde(default)]
    pub input: Option<u64>,
    #[serde(default)]
    pub output: Option<u64>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct RawModalities {
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
}

use crate::model::{Model, ModelCapabilities, ModelCost, ModelLimits};

pub fn route_id_for(provider_id: &str) -> &'static str {
    match provider_id {
        "anthropic" => "anthropic",
        "openai" => "openai-chat",
        "azure" => "azure-openai-chat",
        "google" => "gemini",
        "amazon-bedrock" => "bedrock-converse",
        _ => "openai-compatible-chat",
    }
}

pub fn to_model(provider_id: &str, raw: &RawModel) -> Model {
    let mut model = Model::new(provider_id, &raw.id, route_id_for(provider_id));
    model.limits = ModelLimits {
        context: raw.limit.context,
        input: raw.limit.input,
        output: raw.limit.output,
    };
    model.capabilities = ModelCapabilities {
        temperature: raw.temperature,
        reasoning: raw.reasoning,
        attachment: raw.attachment,
        tool_call: raw.tool_call,
        interleaved: false,
        input_modalities: raw
            .modalities
            .as_ref()
            .map(|m| m.input.clone())
            .unwrap_or_default(),
        output_modalities: raw
            .modalities
            .as_ref()
            .map(|m| m.output.clone())
            .unwrap_or_default(),
    };
    model.cost = raw.cost.as_ref().map(|c| ModelCost {
        input: c.input,
        output: c.output,
        cache_read: c.cache_read,
        cache_write: c.cache_write,
    });
    model
}

use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ProviderMeta {
    pub id: String,
    pub name: String,
    pub env: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Registry {
    models: BTreeMap<(String, String), Model>,
    providers: Vec<ProviderMeta>,
}

impl Registry {
    pub fn from_json(json: &str) -> Result<Registry, serde_json::Error> {
        let api: RawApi = serde_json::from_str(json)?;
        let mut models = BTreeMap::new();
        let mut providers = Vec::new();
        for (pid, prov) in &api {
            providers.push(ProviderMeta {
                id: prov.id.clone(),
                name: prov.name.clone(),
                env: prov.env.clone(),
            });
            for (mid, raw) in &prov.models {
                models.insert((pid.clone(), mid.clone()), to_model(pid, raw));
            }
        }
        providers.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Registry { models, providers })
    }

    pub fn embedded() -> Registry {
        Self::from_json(include_str!("../assets/models.json"))
            .expect("embedded models.json must be valid")
    }

    pub fn lookup(&self, provider: &str, model: &str) -> Option<Model> {
        self.models
            .get(&(provider.to_string(), model.to_string()))
            .cloned()
    }

    pub fn all_models(&self) -> impl Iterator<Item = &Model> {
        self.models.values()
    }

    pub fn providers(&self) -> &[ProviderMeta] {
        &self.providers
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Freshness window for the on-disk `api.json` cache: a cache file younger
/// than this is used as-is without hitting the network.
pub const CACHE_TTL: Duration = Duration::from_secs(300);

/// Options controlling [`load`]/[`refresh`]'s fetch-or-embed resolution.
///
/// Env names mirror opencode's `flag.ts` for parity with its
/// `OPENCODE_MODELS_URL` / `OPENCODE_MODELS_PATH` /
/// `OPENCODE_DISABLE_MODELS_FETCH` variables.
#[derive(Clone, Debug)]
pub struct LoadOptions {
    pub cache_path: PathBuf,
    pub source_url: String,
    pub fetch: bool,
}

impl LoadOptions {
    /// Builds options from the environment, falling back to `cache_path`
    /// unless `OPENCODE_MODELS_PATH` overrides it.
    #[must_use]
    pub fn from_env(cache_path: PathBuf) -> Self {
        let cache_path = std::env::var("OPENCODE_MODELS_PATH")
            .map(PathBuf::from)
            .unwrap_or(cache_path);
        let source_url =
            std::env::var("OPENCODE_MODELS_URL").unwrap_or_else(|_| "https://models.dev".into());
        let fetch = std::env::var("OPENCODE_DISABLE_MODELS_FETCH").is_err();
        Self {
            cache_path,
            source_url,
            fetch,
        }
    }
}

fn read_fresh_cache(path: &std::path::Path) -> Option<Registry> {
    let meta = std::fs::metadata(path).ok()?;
    let age = SystemTime::now()
        .duration_since(meta.modified().ok()?)
        .ok()?;
    if age > CACHE_TTL {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    Registry::from_json(&body).ok()
}

async fn fetch_and_cache(opts: &LoadOptions) -> Option<Registry> {
    let url = format!("{}/api.json", opts.source_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let body = client
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    let reg = Registry::from_json(&body).ok()?;
    // Atomic write: write to a sibling temp file, then rename into place so
    // concurrent readers never observe a partially-written cache. This is
    // single-writer safe only — no cross-process flock for MVP.
    if let Some(parent) = opts.cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = opts.cache_path.with_extension("json.tmp");
    if std::fs::write(&tmp, &body).is_ok() {
        let _ = std::fs::rename(&tmp, &opts.cache_path);
    }
    Some(reg)
}

/// Best-effort models.dev resolution: fresh disk cache, then fetch, then
/// embedded snapshot. Never panics and never hangs (the fetch has a 10s
/// timeout) — always returns a usable [`Registry`].
pub async fn load(opts: &LoadOptions) -> Registry {
    if let Some(reg) = read_fresh_cache(&opts.cache_path) {
        return reg;
    }
    if opts.fetch
        && let Some(reg) = fetch_and_cache(opts).await
    {
        return reg;
    }
    // A stale cache is still better than falling back to embedded.
    if let Ok(body) = std::fs::read_to_string(&opts.cache_path)
        && let Ok(reg) = Registry::from_json(&body)
    {
        return reg;
    }
    Registry::embedded()
}

/// Like [`load`], but ignores cache freshness and forces a fetch when
/// `opts.fetch` is enabled (used by `models --refresh`).
pub async fn refresh(opts: &LoadOptions) -> Registry {
    if opts.fetch
        && let Some(reg) = fetch_and_cache(opts).await
    {
        return reg;
    }
    load(opts).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/models_dev_sample.json");

    #[test]
    fn maps_entry_to_otto_model() {
        let api: RawApi = serde_json::from_str(SAMPLE).unwrap();
        let opus = to_model("anthropic", &api["anthropic"].models["claude-opus-4-8"]);
        assert_eq!(opus.provider.0, "anthropic");
        assert_eq!(opus.id.0, "claude-opus-4-8");
        assert_eq!(opus.route_id, "anthropic");
        assert_eq!(opus.limits.context, Some(200000));
        assert_eq!(opus.limits.output, Some(32000));
        assert!(opus.capabilities.temperature && opus.capabilities.reasoning);
        assert_eq!(
            opus.capabilities.input_modalities,
            vec!["text", "image", "pdf"]
        );
        assert_eq!(opus.cost.as_ref().unwrap().output, Some(75.0));

        let o3 = to_model("openai", &api["openai"].models["o3"]);
        assert_eq!(o3.route_id, "openai-chat");
        assert!(!o3.capabilities.temperature);

        let ds = to_model("deepseek", &api["openai"].models["gpt-4o"]); // route mapping for "other"
        assert_eq!(ds.route_id, "openai-compatible-chat");
        assert!(ds.cost.is_none());
    }

    #[test]
    fn parses_sample_api_json() {
        let api: RawApi = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(api.len(), 2);
        let anthropic = &api["anthropic"];
        assert_eq!(anthropic.name, "Anthropic");
        assert_eq!(anthropic.env, vec!["ANTHROPIC_API_KEY"]);
        let opus = &anthropic.models["claude-opus-4-8"];
        assert!(opus.reasoning && opus.temperature && opus.tool_call);
        assert_eq!(opus.limit.context, Some(200000));
        assert_eq!(opus.cost.as_ref().unwrap().output, Some(75.0));
        assert_eq!(
            opus.modalities.as_ref().unwrap().input,
            vec!["text", "image", "pdf"]
        );
        // o3: temperature=false; gpt-4o: cost missing, modalities missing
        assert!(!api["openai"].models["o3"].temperature);
        assert!(api["openai"].models["gpt-4o"].cost.is_none());
    }

    #[test]
    fn registry_from_json_indexes_models() {
        let reg = Registry::from_json(SAMPLE).unwrap();
        assert_eq!(reg.len(), 3); // opus, o3, gpt-4o
        assert!(reg.lookup("anthropic", "claude-opus-4-8").is_some());
        assert!(reg.lookup("nope", "nope").is_none());
        assert!(reg.providers().iter().any(|p| p.id == "openai"));
        // deterministic ordering
        let ids: Vec<_> = reg
            .all_models()
            .map(|m| format!("{}/{}", m.provider.0, m.id.0))
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn embedded_snapshot_parses() {
        let reg = Registry::embedded();
        assert!(!reg.is_empty());
    }

    /// A model entry missing the `limit` key entirely must still parse (with
    /// empty limits) instead of failing the whole registry — a single odd
    /// entry from a future models.dev payload shouldn't discard ~5000 other
    /// models. See `RawModel.limit`'s `#[serde(default)]`.
    #[test]
    fn model_without_limit_key_parses_with_none_limits() {
        let json = r#"{
            "id": "no-limit-model",
            "name": "No Limit Model"
        }"#;
        let raw: RawModel = serde_json::from_str(json).unwrap();
        assert_eq!(raw.limit.context, None);
        assert_eq!(raw.limit.input, None);
        assert_eq!(raw.limit.output, None);

        let model = to_model("anthropic", &raw);
        assert_eq!(model.limits.context, None);
        assert_eq!(model.limits.input, None);
        assert_eq!(model.limits.output, None);
    }
}
