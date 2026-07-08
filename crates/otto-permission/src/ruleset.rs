//! Permission rulesets and evaluation — a port of the pure functions in
//! opencode `permission/index.ts` (`evaluate` ~28, `expand` ~178,
//! `fromConfig` ~186, `merge` ~200) together with the `Rule`/`Ruleset`
//! schemas from `packages/core/src/v1/permission.ts`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wildcard::wildcard_match;

/// A permission decision — port of `PermissionV1.Action`
/// (`v1/permission.ts`, `Schema.Literals(["allow", "deny", "ask"])`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Grant the request without prompting.
    Allow,
    /// Reject the request outright.
    Deny,
    /// Prompt the user to decide.
    Ask,
}

/// A single rule — port of `PermissionV1.Rule`
/// (`{ permission, pattern, action }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// The permission glob this rule governs (e.g. `edit`, `bash`, `*`).
    pub permission: String,
    /// The value glob this rule matches (e.g. a path or command, or `*`).
    pub pattern: String,
    /// The decision applied when both globs match.
    pub action: Action,
}

/// An ordered collection of [`Rule`]s — port of `PermissionV1.Ruleset`.
///
/// Precedence is *last-match-wins*: [`evaluate`] flattens rulesets in order
/// and keeps the last matching rule, so later rules (and later rulesets)
/// override earlier ones.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruleset(pub Vec<Rule>);

impl Ruleset {
    /// An empty ruleset.
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Borrow the underlying rules.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.0
    }

    /// Build a ruleset from a config value — port of `fromConfig`
    /// (`index.ts:186`).
    ///
    /// The value is a map of `<permission> -> <rule>` where each `<rule>` is
    /// either a string action (`"allow" | "deny" | "ask"`, applied with
    /// pattern `"*"`) or an object of `<glob> -> action` (one [`Rule`] per
    /// glob). A bare top-level string is normalized to `{ "*": <string> }`
    /// exactly as `ConfigPermissionV1` does. Glob keys are run through
    /// [`expand`] so `~`, `~/…`, and `$HOME…` resolve to the home directory.
    ///
    /// Rule precedence follows the iteration order of the input `Value`; enable
    /// serde_json's `preserve_order` feature to retain the user's key order.
    #[must_use]
    pub fn from_config(value: &Value) -> Self {
        let mut rules = Vec::new();

        // normalizeInput: a bare string becomes { "*": <string> }.
        if let Some(action) = value.as_str().and_then(parse_action) {
            rules.push(Rule {
                permission: "*".to_string(),
                pattern: "*".to_string(),
                action,
            });
            return Ruleset(rules);
        }

        let Some(map) = value.as_object() else {
            return Ruleset(rules);
        };

        for (permission, rule) in map {
            match rule {
                Value::String(s) => {
                    if let Some(action) = parse_action(s) {
                        rules.push(Rule {
                            permission: permission.clone(),
                            pattern: "*".to_string(),
                            action,
                        });
                    }
                }
                Value::Object(globs) => {
                    for (glob, action_value) in globs {
                        if let Some(action) = action_value.as_str().and_then(parse_action) {
                            rules.push(Rule {
                                permission: permission.clone(),
                                pattern: expand(glob),
                                action,
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        Ruleset(rules)
    }

    /// Push all rules from `other` after this ruleset's rules.
    pub fn extend(&mut self, other: &Ruleset) {
        self.0.extend(other.0.iter().cloned());
    }
}

/// The outcome of [`evaluate`] — the winning [`Action`] and the `pattern` of
/// the rule that produced it (or the default `"*"`). Mirrors the subset of
/// `PermissionV1.Rule` that callers consume from `evaluate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRule {
    /// The decided action.
    pub action: Action,
    /// The pattern of the matched rule, or `"*"` when nothing matched.
    pub pattern: String,
}

/// Evaluate `permission`/`pattern` against `rulesets` — port of `evaluate`
/// (`index.ts:28`).
///
/// Rulesets are flattened in order and the **last** rule whose `permission`
/// glob matches `permission` *and* whose `pattern` glob matches `pattern`
/// wins (last-match-wins). When nothing matches the default is
/// `{ action: Ask, pattern: "*" }`.
#[must_use]
pub fn evaluate(rulesets: &[&Ruleset], permission: &str, pattern: &str) -> ResolvedRule {
    let mut winner: Option<&Rule> = None;
    for ruleset in rulesets {
        for rule in &ruleset.0 {
            if wildcard_match(permission, &rule.permission)
                && wildcard_match(pattern, &rule.pattern)
            {
                winner = Some(rule);
            }
        }
    }
    match winner {
        Some(rule) => ResolvedRule {
            action: rule.action,
            pattern: rule.pattern.clone(),
        },
        None => ResolvedRule {
            action: Action::Ask,
            pattern: "*".to_string(),
        },
    }
}

/// Concatenate rulesets into one — port of `merge` (`index.ts:200`).
///
/// This is pure concatenation; precedence still resolves via last-match-wins
/// in [`evaluate`], so rulesets passed later override earlier ones.
#[must_use]
pub fn merge(rulesets: &[&Ruleset]) -> Ruleset {
    let mut out = Vec::new();
    for ruleset in rulesets {
        out.extend(ruleset.0.iter().cloned());
    }
    Ruleset(out)
}

/// Expand a pattern's leading home shorthands — port of `expand`
/// (`index.ts:178`). `~`, `~/…`, `$HOME`, and `$HOME/…` resolve to the home
/// directory; everything else is returned unchanged.
#[must_use]
pub fn expand(pattern: &str) -> String {
    let home = home_dir();
    if let Some(rest) = pattern.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    if pattern == "~" {
        return home;
    }
    if let Some(rest) = pattern.strip_prefix("$HOME") {
        // Matches opencode's `home + pattern.slice(5)` for both "$HOME/…" and
        // a bare "$HOME"; `rest` is whatever follows the literal "$HOME".
        return format!("{home}{rest}");
    }
    pattern.to_string()
}

/// The home directory, equivalent to `os.homedir()` on unix (`$HOME`).
fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Parse a config action string; unknown strings yield `None`.
fn parse_action(s: &str) -> Option<Action> {
    match s {
        "allow" => Some(Action::Allow),
        "deny" => Some(Action::Deny),
        "ask" => Some(Action::Ask),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(permission: &str, pattern: &str, action: Action) -> Rule {
        Rule {
            permission: permission.to_string(),
            pattern: pattern.to_string(),
            action,
        }
    }

    #[test]
    fn evaluate_defaults_to_ask() {
        let rs = Ruleset::new();
        let got = evaluate(&[&rs], "edit", "src/main.rs");
        assert_eq!(got.action, Action::Ask);
        assert_eq!(got.pattern, "*");
    }

    #[test]
    fn evaluate_last_match_wins_within_ruleset() {
        let rs = Ruleset(vec![
            rule("edit", "*", Action::Allow),
            rule("edit", "*.rs", Action::Deny),
        ]);
        assert_eq!(evaluate(&[&rs], "edit", "a.rs").action, Action::Deny);
        assert_eq!(evaluate(&[&rs], "edit", "a.ts").action, Action::Allow);
    }

    #[test]
    fn evaluate_last_match_wins_across_rulesets() {
        let base = Ruleset(vec![rule("edit", "*", Action::Deny)]);
        let over = Ruleset(vec![rule("edit", "*", Action::Allow)]);
        // later ruleset overrides
        assert_eq!(evaluate(&[&base, &over], "edit", "x").action, Action::Allow);
        assert_eq!(evaluate(&[&over, &base], "edit", "x").action, Action::Deny);
    }

    #[test]
    fn evaluate_wildcard_permission() {
        let rs = Ruleset(vec![rule("*", "*", Action::Allow)]);
        assert_eq!(evaluate(&[&rs], "bash", "anything").action, Action::Allow);
    }

    #[test]
    fn merge_concatenates() {
        let a = Ruleset(vec![rule("edit", "*", Action::Allow)]);
        let b = Ruleset(vec![rule("bash", "*", Action::Deny)]);
        let merged = merge(&[&a, &b]);
        assert_eq!(merged.0.len(), 2);
    }

    #[test]
    fn from_config_string_form() {
        let cfg = json!({ "edit": "deny", "bash": "allow" });
        let rs = Ruleset::from_config(&cfg);
        assert!(rs.0.contains(&rule("edit", "*", Action::Deny)));
        assert!(rs.0.contains(&rule("bash", "*", Action::Allow)));
    }

    #[test]
    fn from_config_object_form() {
        let cfg = json!({ "edit": { "*.rs": "allow", "*.lock": "deny" } });
        let rs = Ruleset::from_config(&cfg);
        assert!(rs.0.contains(&rule("edit", "*.rs", Action::Allow)));
        assert!(rs.0.contains(&rule("edit", "*.lock", Action::Deny)));
    }

    #[test]
    fn from_config_top_level_string() {
        let cfg = json!("allow");
        let rs = Ruleset::from_config(&cfg);
        assert_eq!(rs.0, vec![rule("*", "*", Action::Allow)]);
    }

    #[test]
    fn from_config_expands_home() {
        let home = std::env::var("HOME").unwrap_or_default();
        let cfg = json!({ "read": { "~/secrets/*": "deny", "$HOME/keys": "deny" } });
        let rs = Ruleset::from_config(&cfg);
        assert!(rs.0.contains(&rule("read", &format!("{home}/secrets/*"), Action::Deny)));
        assert!(rs.0.contains(&rule("read", &format!("{home}/keys"), Action::Deny)));
    }

    #[test]
    fn expand_variants() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(expand("~"), home);
        assert_eq!(expand("~/a/b"), format!("{home}/a/b"));
        assert_eq!(expand("$HOME"), home);
        assert_eq!(expand("$HOME/a"), format!("{home}/a"));
        assert_eq!(expand("relative/path"), "relative/path");
    }
}
