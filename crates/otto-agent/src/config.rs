//! Config merge — port of the `for (const [key, value] of Object.entries(...))`
//! loop that overlays `cfg.agent` onto the built-in defaults (agent.ts:267-294).

use otto_permission::{merge, Ruleset};
use serde_json::Value;

use crate::agent::{AgentInfo, AgentMode, ModelRef};
use crate::builtins::{builtins, defaults};

/// Resolve the effective agent set — port of agent.ts:267-294.
///
/// Starts from the [`builtins`] defaults and overlays each entry of the
/// `config.agent` object (`config_agents`). For each `(key, value)`:
/// * a truthy `value.disable` removes the agent (agent.ts:268-271);
/// * an unknown key creates a fresh agent defaulting to `mode: "all"`,
///   `native: false`, permission `merge(defaults)` (agent.ts:272-280);
/// * scalar fields are overridden when present, `options` is deep-merged, and
///   `permission` is `merge(existing, fromConfig(value.permission))`
///   (agent.ts:281-293).
///
/// Iteration order of `config_agents` is preserved (serde_json's
/// `preserve_order` feature), matching JS object insertion order.
#[must_use]
pub fn resolve_agents(config_agents: &Value) -> Vec<AgentInfo> {
    let mut agents = builtins();

    let Some(entries) = config_agents.as_object() else {
        return agents;
    };

    for (key, value) in entries {
        // agent.ts:268-271 — `if (value.disable) { delete agents[key]; continue }`
        if value
            .get("disable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            agents.retain(|a| a.name != *key);
            continue;
        }

        // Locate or create the agent (agent.ts:272-280).
        let idx = match agents.iter().position(|a| a.name == *key) {
            Some(i) => i,
            None => {
                agents.push(AgentInfo {
                    name: key.clone(),
                    description: None,
                    mode: AgentMode::All,
                    native: false,
                    hidden: false,
                    top_p: None,
                    temperature: None,
                    color: None,
                    permission: merge(&[&defaults()]),
                    model: None,
                    variant: None,
                    prompt: None,
                    options: Value::Object(serde_json::Map::new()),
                    steps: None,
                });
                agents.len() - 1
            }
        };

        apply_overrides(&mut agents[idx], value);
    }

    agents
}

/// Apply one `config.agent[key]` entry's field overrides (agent.ts:281-293).
fn apply_overrides(item: &mut AgentInfo, value: &Value) {
    if let Some(spec) = value.get("model").and_then(Value::as_str) {
        item.model = Some(ModelRef::parse(spec));
    }
    if let Some(v) = value.get("variant").and_then(Value::as_str) {
        item.variant = Some(v.to_string());
    }
    if let Some(v) = value.get("prompt").and_then(Value::as_str) {
        item.prompt = Some(v.to_string());
    }
    if let Some(v) = value.get("description").and_then(Value::as_str) {
        item.description = Some(v.to_string());
    }
    if let Some(v) = value.get("temperature").and_then(Value::as_f64) {
        item.temperature = Some(v);
    }
    if let Some(v) = value.get("top_p").and_then(Value::as_f64) {
        item.top_p = Some(v);
    }
    if let Some(v) = value
        .get("mode")
        .and_then(|m| serde_json::from_value(m.clone()).ok())
    {
        item.mode = v;
    }
    if let Some(v) = value.get("color").and_then(Value::as_str) {
        item.color = Some(v.to_string());
    }
    if let Some(v) = value.get("hidden").and_then(Value::as_bool) {
        item.hidden = v;
    }
    if let Some(v) = value.get("name").and_then(Value::as_str) {
        item.name = v.to_string();
    }
    if let Some(v) = value.get("steps").and_then(Value::as_u64) {
        item.steps = Some(v as u32);
    }
    if let Some(v) = value.get("options") {
        merge_deep(&mut item.options, v);
    }
    if let Some(v) = value.get("permission") {
        item.permission = merge(&[&item.permission, &Ruleset::from_config(v)]);
    }
}

/// Deep-merge `src` into `dst` — port of remeda's `mergeDeep` for the
/// `options` bag (agent.ts:292). Objects merge key-by-key recursively; any
/// non-object value in `src` replaces the value in `dst`.
fn merge_deep(dst: &mut Value, src: &Value) {
    match (dst, src) {
        (Value::Object(dst_map), Value::Object(src_map)) => {
            for (k, v) in src_map {
                match dst_map.get_mut(k) {
                    Some(existing) => merge_deep(existing, v),
                    None => {
                        dst_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (dst, src) => *dst = src.clone(),
    }
}

/// Look up a single resolved agent by name — the `get` helper (agent.ts:312-314).
#[must_use]
pub fn get(config_agents: &Value, name: &str) -> Option<AgentInfo> {
    resolve_agents(config_agents)
        .into_iter()
        .find(|a| a.name == name)
}

/// List resolved agents sorted for presentation — port of `list`
/// (agent.ts:316-326): the default agent (`build`) first, then ascending by
/// name.
#[must_use]
pub fn list(config_agents: &Value) -> Vec<AgentInfo> {
    let mut agents = resolve_agents(config_agents);
    agents.sort_by(|a, b| {
        let a_default = a.name != "build";
        let b_default = b.name != "build";
        a_default.cmp(&b_default).then_with(|| a.name.cmp(&b.name))
    });
    agents
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_llm::model::{ModelId, ProviderId};
    use otto_permission::{evaluate, Action};
    use serde_json::json;

    #[test]
    fn no_config_returns_builtins() {
        let agents = resolve_agents(&json!({}));
        assert_eq!(agents.len(), 7);
        assert!(agents.iter().any(|a| a.name == "build"));
    }

    #[test]
    fn override_build_model_and_add_custom_agent() {
        let cfg = json!({
            "build": { "model": "anthropic/claude-sonnet-4" },
            "reviewer": { "description": "reviews code", "permission": { "edit": "deny" } }
        });
        let agents = resolve_agents(&cfg);

        let build = agents.iter().find(|a| a.name == "build").unwrap();
        let model = build.model.as_ref().unwrap();
        assert_eq!(model.provider, ProviderId::new("anthropic"));
        assert_eq!(model.model, ModelId::new("claude-sonnet-4"));

        // custom agent defaults to mode: all, native: false
        let reviewer = agents.iter().find(|a| a.name == "reviewer").unwrap();
        assert_eq!(reviewer.mode, AgentMode::All);
        assert!(!reviewer.native);
        assert_eq!(reviewer.description.as_deref(), Some("reviews code"));
        // permission merged over defaults: base allows edit, config denies it
        assert_eq!(
            evaluate(&[&reviewer.permission], "edit", "x").action,
            Action::Deny
        );
    }

    #[test]
    fn permission_merge_over_builtin() {
        // general already denies todowrite; a config deny of bash stacks on top.
        let cfg = json!({ "general": { "permission": { "bash": "deny" } } });
        let general = get(&cfg, "general").unwrap();
        assert_eq!(
            evaluate(&[&general.permission], "bash", "ls").action,
            Action::Deny
        );
        assert_eq!(
            evaluate(&[&general.permission], "todowrite", "*").action,
            Action::Deny
        );
    }

    #[test]
    fn disable_removes_agent() {
        let cfg = json!({ "plan": { "disable": true } });
        let agents = resolve_agents(&cfg);
        assert!(!agents.iter().any(|a| a.name == "plan"));
        assert!(agents.iter().any(|a| a.name == "build"));
    }

    #[test]
    fn options_deep_merge() {
        let cfg = json!({ "build": { "options": { "reasoning": { "effort": "high" } } } });
        let build = get(&cfg, "build").unwrap();
        assert_eq!(build.options["reasoning"]["effort"], json!("high"));
    }

    #[test]
    fn list_puts_build_first() {
        let agents = list(&json!({}));
        assert_eq!(agents[0].name, "build");
    }
}
