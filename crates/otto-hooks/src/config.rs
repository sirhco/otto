//! Typed config for otto's lifecycle hooks — deep-merged through the same
//! `otto.json`/`otto.jsonc` layering as the rest of otto-config, embedded via
//! `otto_config::schema::Config.hooks` (wired in a later plan).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One `HookMatcherGroup` list per lifecycle event otto supports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HooksConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_start: Vec<HookMatcherGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_prompt_submit: Vec<HookMatcherGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_tool_use: Vec<HookMatcherGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_tool_use: Vec<HookMatcherGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_compact: Vec<HookMatcherGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<HookMatcherGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_stop: Vec<HookMatcherGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notification: Vec<HookMatcherGroup>,
}

/// A tool-id matcher plus the commands to run when it matches (or, for
/// non-tool events, unconditionally).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HookMatcherGroup {
    /// Regex tested against the tool id. `None` matches every tool id, and
    /// the field is ignored entirely for non-tool events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
}

/// One external command to run via `sh -c`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct HookCommand {
    pub command: String,
    /// Overrides `otto_hooks::runner::DEFAULT_TIMEOUT_MS` for this command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_object_yields_default() {
        let cfg: HooksConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, HooksConfig::default());
    }

    #[test]
    fn deserializes_full_shape_from_json() {
        let json = r#"{
            "pre_tool_use": [
                { "matcher": "bash", "hooks": [ { "command": "check.sh", "timeout_ms": 5000 } ] }
            ]
        }"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.pre_tool_use.len(), 1);
        assert_eq!(cfg.pre_tool_use[0].matcher.as_deref(), Some("bash"));
        assert_eq!(cfg.pre_tool_use[0].hooks[0].command, "check.sh");
        assert_eq!(cfg.pre_tool_use[0].hooks[0].timeout_ms, Some(5000));
        assert!(cfg.session_start.is_empty());
    }

    #[test]
    fn matcher_and_timeout_default_to_none() {
        let json = r#"{ "stop": [ { "hooks": [ { "command": "notify.sh" } ] } ] }"#;
        let cfg: HooksConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.stop[0].matcher, None);
        assert_eq!(cfg.stop[0].hooks[0].timeout_ms, None);
    }
}
