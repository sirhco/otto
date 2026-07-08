//! Warm-boot cache: memoizes the assembled system prompt so repeat child
//! spawns of the same `(provider, model, agent, directory)` skip the
//! `instructions(cwd)` fs-walk + reassembly that `build_system` performs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use otto_llm::Model;

use crate::system::build_system;

/// The memoized system prompt (`build_system`'s output) behind an `Arc`.
///
/// v1 caches the system prompt only. The design doc's `tool_defs` field is
/// deferred: tool-def serialization lives in the route/LLM layer (not wired
/// here), and an unconsumed field would trip `clippy -D warnings`.
#[derive(Clone)]
pub struct WarmCache {
    pub system: Arc<Vec<String>>,
}

/// Cache key. Keyed on the `ProviderId`/`ModelId` *strings* (`Model` itself is
/// not `Hash`/`Eq` — its `cost` holds `f64`). `is_git` is included so a repo
/// that gains a `.git` mid-process keys to a fresh entry.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct WarmKey {
    pub provider: String,
    pub model_id: String,
    pub agent: String,
    pub directory: PathBuf,
    pub is_git: bool,
}

/// Return the cached `WarmCache` for `(model, agent, directory)`, building it
/// once on a miss. Concurrent-safe via the `Mutex`; the build itself runs
/// outside the lock is NOT required for correctness here (build is idempotent
/// and cheap relative to a network turn), so we keep it simple and hold the
/// lock across build.
pub(crate) fn compute_warm(
    map: &Mutex<HashMap<WarmKey, Arc<WarmCache>>>,
    directory: &Path,
    model: &Model,
    agent: &str,
    agent_prompt: Option<&str>,
) -> Arc<WarmCache> {
    let is_git = directory.join(".git").exists();
    let key = WarmKey {
        provider: model.provider.0.clone(),
        model_id: model.id.0.clone(),
        agent: agent.to_string(),
        directory: directory.to_path_buf(),
        is_git,
    };
    let mut m = map.lock().unwrap();
    if let Some(c) = m.get(&key) {
        return c.clone();
    }
    let system = build_system(
        &model.provider,
        &model.id,
        agent_prompt,
        directory,
        is_git,
        std::env::consts::OS,
        "",
        None,
        None,
        None,
    );
    let c = Arc::new(WarmCache {
        system: Arc::new(system),
    });
    m.insert(key, c.clone());
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_llm::model::{ModelId, ProviderId};

    fn test_model() -> Model {
        Model {
            id: ModelId("test-model".into()),
            provider: ProviderId("test-provider".into()),
            route_id: String::new(),
            limits: Default::default(),
            capabilities: Default::default(),
            cost: None,
        }
    }

    #[test]
    fn compute_warm_memoizes_per_key() {
        let map = Mutex::new(HashMap::new());
        let dir = tempfile::tempdir().unwrap();
        let model = test_model();

        let a = compute_warm(&map, dir.path(), &model, "build", None);
        let b = compute_warm(&map, dir.path(), &model, "build", None);
        // Second lookup returns the SAME Arc — a cache hit, not a rebuild.
        assert!(Arc::ptr_eq(&a, &b));

        let c = compute_warm(&map, dir.path(), &model, "plan", None);
        // Different agent -> distinct entry.
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(map.lock().unwrap().len(), 2);
    }

    #[test]
    fn compute_warm_populates_system() {
        let map = Mutex::new(HashMap::new());
        let dir = tempfile::tempdir().unwrap();
        let model = test_model();
        let c = compute_warm(&map, dir.path(), &model, "build", None);
        // The cached system prompt is the non-empty build output.
        assert!(!c.system.is_empty());
    }
}
