//! Typed config for otto's lifecycle hooks — deep-merged through the same
//! `otto.json`/`otto.jsonc` layering as the rest of otto-config, embedded via
//! `otto_config::schema::Config.hooks` (wired in a later plan).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event::HookKind;

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

impl HookMatcherGroup {
    /// Whether this group's hooks should run for `tool_id`. `None` (either
    /// the group's `matcher` is absent, or `tool_id` is `None` because the
    /// firing event isn't tool-scoped) always matches. An invalid regex
    /// never matches, rather than panicking or matching everything.
    #[must_use]
    pub fn matches(&self, tool_id: Option<&str>) -> bool {
        let (Some(pattern), Some(id)) = (&self.matcher, tool_id) else {
            return true;
        };
        regex::Regex::new(pattern).is_ok_and(|re| re.is_match(id))
    }
}

impl HooksConfig {
    /// The configured `HookMatcherGroup`s for `kind`.
    #[must_use]
    pub fn groups_for(&self, kind: HookKind) -> &[HookMatcherGroup] {
        match kind {
            HookKind::SessionStart => &self.session_start,
            HookKind::UserPromptSubmit => &self.user_prompt_submit,
            HookKind::PreToolUse => &self.pre_tool_use,
            HookKind::PostToolUse => &self.post_tool_use,
            HookKind::PreCompact => &self.pre_compact,
            HookKind::Stop => &self.stop,
            HookKind::SubagentStop => &self.subagent_stop,
            HookKind::Notification => &self.notification,
        }
    }
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

    #[test]
    fn matches_every_tool_id_when_matcher_absent() {
        let group = HookMatcherGroup {
            matcher: None,
            hooks: vec![],
        };
        assert!(group.matches(Some("bash")));
        assert!(group.matches(Some("edit")));
    }

    #[test]
    fn matches_non_tool_events_regardless_of_matcher() {
        let group = HookMatcherGroup {
            matcher: Some("^edit$".to_string()),
            hooks: vec![],
        };
        assert!(
            group.matches(None),
            "non-tool events ignore the matcher entirely"
        );
    }

    #[test]
    fn matcher_regex_matches_anywhere_in_tool_id() {
        let group = HookMatcherGroup {
            matcher: Some("_search".to_string()),
            hooks: vec![],
        };
        assert!(group.matches(Some("github_search")));
        assert!(!group.matches(Some("bash")));
    }

    #[test]
    fn matcher_anchors_are_respected_when_present() {
        let group = HookMatcherGroup {
            matcher: Some("^(edit|write)$".to_string()),
            hooks: vec![],
        };
        assert!(group.matches(Some("edit")));
        assert!(group.matches(Some("write")));
        assert!(!group.matches(Some("apply_patch")));
    }

    #[test]
    fn invalid_regex_never_matches() {
        let group = HookMatcherGroup {
            matcher: Some("(unterminated".to_string()),
            hooks: vec![],
        };
        assert!(!group.matches(Some("bash")));
    }

    #[test]
    fn groups_for_returns_the_right_field() {
        let cfg = HooksConfig {
            stop: vec![HookMatcherGroup {
                matcher: None,
                hooks: vec![HookCommand {
                    command: "notify.sh".to_string(),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        };
        assert_eq!(cfg.groups_for(crate::event::HookKind::Stop).len(), 1);
        assert!(
            cfg.groups_for(crate::event::HookKind::SessionStart)
                .is_empty()
        );
    }
}
