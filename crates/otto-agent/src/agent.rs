//! The [`AgentInfo`] value type — port of the `Info` schema in opencode
//! `agent/agent.ts` (`Info = Schema.Struct({...})`, agent.ts:35-56).

use otto_llm::model::{ModelId, ProviderId};
use otto_permission::Ruleset;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How an agent may be invoked — port of the `mode` literal
/// (`Schema.Literals(["subagent", "primary", "all"])`, agent.ts:38).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Only invocable as a child of another agent (via the `task` tool).
    Subagent,
    /// A top-level agent the user can select.
    Primary,
    /// Invocable both as a primary and as a subagent. This is the default for
    /// unknown agents merged in from config (agent.ts:276).
    All,
}

/// A resolved model reference — port of the optional `model` struct
/// (`{ modelID, providerID }`, agent.ts:45-50).
///
/// Field order follows the otto seam (`ProviderId`, `ModelId`); the serde
/// names mirror opencode's `providerID` / `modelID` keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    /// The owning provider id.
    #[serde(rename = "providerID")]
    pub provider: ProviderId,
    /// The model id.
    #[serde(rename = "modelID")]
    pub model: ModelId,
}

impl ModelRef {
    /// Parse a `"provider/model"` string — port of `Provider.parseModel`
    /// (used at agent.ts:281). Splits on the **first** `/`; everything before
    /// is the provider, everything after is the model. A string with no `/`
    /// is treated as a bare model id with an empty provider.
    #[must_use]
    pub fn parse(spec: &str) -> Self {
        match spec.split_once('/') {
            Some((provider, model)) => ModelRef {
                provider: ProviderId::new(provider),
                model: ModelId::new(model),
            },
            None => ModelRef {
                provider: ProviderId::new(""),
                model: ModelId::new(spec),
            },
        }
    }
}

/// A fully-resolved agent definition — port of `Info` (agent.ts:35-56).
///
/// Built-in agents are produced by [`crate::builtins`]; user `config.agent`
/// entries are deep-merged over them by [`crate::config::resolve_agents`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// The agent's unique name (agent.ts:36).
    pub name: String,
    /// Human-readable description shown in agent pickers (agent.ts:37).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How the agent may be invoked (agent.ts:38).
    pub mode: AgentMode,
    /// Whether this is a built-in (non-user) agent (agent.ts:39).
    #[serde(default)]
    pub native: bool,
    /// Whether the agent is hidden from selection UIs (agent.ts:40).
    #[serde(default)]
    pub hidden: bool,
    /// Nucleus-sampling `top_p` override (agent.ts:41).
    #[serde(rename = "topP", default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Sampling temperature override (agent.ts:42).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// UI accent color (agent.ts:43).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The agent's permission ruleset (agent.ts:44).
    pub permission: Ruleset,
    /// Optional pinned model (agent.ts:45-50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Optional model variant selector (agent.ts:51).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// The agent's system prompt, if it overrides the default (agent.ts:52).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Provider-specific options bag (agent.ts:53). Defaults to `{}`.
    #[serde(default)]
    pub options: Value,
    /// Optional cap on agentic steps (agent.ts:54).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
}

impl AgentInfo {
    /// Does the ruleset contain any rule for the given `permission` name?
    ///
    /// Mirrors the `permission.some((rule) => rule.permission === name)`
    /// checks used by the subagent derivation (subagent-permissions.ts:18-19).
    #[must_use]
    pub fn permits_key(&self, permission: &str) -> bool {
        self.permission
            .rules()
            .iter()
            .any(|rule| rule.permission == permission)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_mode_serde_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&AgentMode::Subagent).unwrap(),
            "\"subagent\""
        );
        assert_eq!(
            serde_json::from_str::<AgentMode>("\"all\"").unwrap(),
            AgentMode::All
        );
    }

    #[test]
    fn model_ref_parse_splits_on_first_slash() {
        let m = ModelRef::parse("anthropic/claude-sonnet-4/preview");
        assert_eq!(m.provider, ProviderId::new("anthropic"));
        assert_eq!(m.model, ModelId::new("claude-sonnet-4/preview"));
    }

    #[test]
    fn model_ref_parse_without_slash() {
        let m = ModelRef::parse("gpt-4o");
        assert_eq!(m.provider, ProviderId::new(""));
        assert_eq!(m.model, ModelId::new("gpt-4o"));
    }
}
