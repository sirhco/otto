//! Serde port of opencode's `ConfigV1.Info`
//! (`packages/core/src/v1/config/config.ts:32-189`).
//!
//! Fidelity notes:
//! * Every field is optional and `#[serde(default)]`; unknown keys are tolerated
//!   (opencode does **not** set `additionalProperties: false`).
//! * Deeply-nested / fast-evolving sub-objects (`agent`, `provider`, `mcp`,
//!   `experimental`, `permission`, `formatter`, `lsp`, `skills`, `attachment`,
//!   `command`, `autoupdate`) are kept as raw [`serde_json::Value`] so this crate
//!   stays decoupled from those schemas (MCP typing is Phase 6, permission lives
//!   in `otto-permission`).
//! * Stable, load-bearing shapes get real types: [`LogLevel`], [`Share`],
//!   [`Compaction`], [`ToolOutput`], [`Watcher`], [`Enterprise`], plus `model` /
//!   `instructions` scalars & arrays.
//! * `skip_serializing_if = "Option::is_none"` is load-bearing for merge: a `None`
//!   field must not serialize to `null` and clobber a base value during deep merge
//!   (mirrors opencode `mergeDeep` operating on sparse objects).

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use otto_hooks::HooksConfig;

/// Default `$schema` otto injects on load — points at the JSON schema
/// generated from this module (`examples/gen_schema.rs`) and committed at
/// `schema/config.json` in the otto repo. Namespaced away from opencode's
/// `https://opencode.ai/config.json` (`packages/opencode/src/config/config.ts:232,254`),
/// which this schema was ported from but no longer shares hosting with.
pub const DEFAULT_SCHEMA: &str =
    "https://raw.githubusercontent.com/sirhco/otto/main/schema/config.json";

/// Log verbosity — `LogLevelRef` (`config.ts:27-30`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Session sharing behavior — `share` (`config.ts:57-60`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Share {
    Manual,
    Auto,
    Disabled,
}

/// `watcher` (`config.ts:51`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Watcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
}

/// `enterprise` (`config.ts:130-132`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Enterprise {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// `tool_output` truncation thresholds (`config.ts:133-145`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// `compaction` (`config.ts:146-165`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Compaction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_recent_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<u64>,
    /// Trailing tool-output token budget protected from post-turn pruning
    /// (otto extension; default 40000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune_protect_tokens: Option<u64>,
}

/// `retry` — provider retry/backoff knobs (otto extension, no opencode
/// analog). All fields optional; unset falls back to the session defaults
/// (`otto-session`'s `DEFAULT_MAX_RETRIES` / `DEFAULT_MAX_TOTAL_RETRIES`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Retry {
    /// Cap on retries of one step (one provider request). Default 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Cap on retries summed across all steps of one prompt. Default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_attempts: Option<u32>,
    /// Wall-clock cap on one prompt turn, in seconds. Unset = no timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_timeout_seconds: Option<u64>,
}

/// Port of opencode `ConfigV1.Info` (`config.ts:32-189`).
///
/// Extra / unknown keys parse without error but are dropped on re-serialize.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    /// `$schema` (`config.ts:33`).
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,

    /// `shell` (`config.ts:36`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,

    /// Named TUI color theme preset (e.g. "catppuccin", "gruvbox", "nord",
    /// "base16"). Unknown names fall back to the default dark theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,

    /// `logLevel` (`config.ts:37`).
    #[serde(rename = "logLevel", default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,

    /// `command` map (`config.ts:41-43`) — kept permissive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<HashMap<String, Value>>,

    /// `skills` (`config.ts:44`) — kept permissive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Value>,

    /// `watcher` (`config.ts:51`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watcher: Option<Watcher>,

    /// `plugin` specs (`config.ts:56`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<Vec<String>>,

    /// `share` (`config.ts:57`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<Share>,

    /// `autoupdate` — `bool | "notify"` (`config.ts:64`), kept permissive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoupdate: Option<Value>,

    /// `disabled_providers` (`config.ts:68`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_providers: Option<Vec<String>>,

    /// `enabled_providers` (`config.ts:71`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_providers: Option<Vec<String>>,

    /// `model` as `provider/model` (`config.ts:74`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// `small_model` (`config.ts:77`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,

    /// `default_agent` (`config.ts:80`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,

    /// `username` (`config.ts:84`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,

    /// `agent` map (`config.ts:93-106`) — permissive, decoupled from `ConfigAgentV1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Value>,

    /// `provider` map (`config.ts:107-109`) — permissive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Value>,

    /// `mcp` map (`config.ts:110-112`) — permissive (typed schema is Phase 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Value>,

    /// `formatter` — `bool | Record<..>` (`config.ts:113`), permissive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatter: Option<Value>,

    /// `lsp` — `bool | Record<..>` (`config.ts:117`), permissive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp: Option<Value>,

    /// `instructions` (`config.ts:121`) — merge concatenates + dedupes these
    /// (see `loader::merge`, `mergeConfigConcatArrays` config.ts:45-51).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Vec<String>>,

    /// `permission` (`config.ts:125`) — permissive; typing lives in `otto-permission`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<Value>,

    /// `permission_mode` — the starting per-session permission mode
    /// (`approve-each` | `accept-edits` | `full-auto`). Otto-only. Defaults to
    /// `approve-each` when absent. The mode can be cycled live in the TUI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,

    /// `tools` gate map (`config.ts:126`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<HashMap<String, bool>>,

    /// `attachment` (`config.ts:127`) — permissive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Value>,

    /// `enterprise` (`config.ts:130`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise: Option<Enterprise>,

    /// `tool_output` (`config.ts:133`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<ToolOutput>,

    /// `compaction` (`config.ts:146`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Compaction>,

    /// `retry` — provider retry/backoff knobs (otto extension).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,

    /// `experimental` (`config.ts:166`) — permissive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,

    /// `rtk` — optional RTK (Rust Token Killer) shell-command wrapping. Otto-only
    /// (no opencode analogue). When enabled and `rtk` is on `PATH`, the `bash`
    /// tool's commands are routed through `rtk` to compact their output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtk: Option<Rtk>,

    /// `tersemode` — optional terse-output mode. Otto-only (no opencode analogue).
    /// When enabled, a brevity directive is appended to the system prompt so the
    /// model emits fewer output tokens (drops filler/articles/hedging) while
    /// keeping code, paths, and error strings byte-exact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tersemode: Option<Tersemode>,

    /// `hooks` — otto-native lifecycle hooks (external, user-configured
    /// commands fired at points in the session/tool/compaction pipeline).
    /// No opencode analogue — otto extension inspired by Claude Code's hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksConfig>,
}

/// `rtk` config block. Off unless `enabled` is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Rtk {
    /// Route `bash` commands through the `rtk` proxy when it is available.
    #[serde(default)]
    pub enabled: bool,
}

/// `tersemode` config block. Off unless `enabled` is set; `level` defaults to
/// [`TersemodeLevel::Full`] when omitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Tersemode {
    /// Append the terse-output directive to the system prompt.
    #[serde(default)]
    pub enabled: bool,
    /// How aggressively to compress prose. Ignored when `enabled` is false.
    #[serde(default)]
    pub level: TersemodeLevel,
}

/// Tersemode intensity. Controls how far the brevity directive pushes the model.
/// Serialized lowercase (`"lite" | "full" | "ultra" | "wenyan"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TersemodeLevel {
    /// Drop filler/hedging, keep grammar + full sentences.
    Lite,
    /// Drop articles, fragments OK, short synonyms. The classic default.
    #[default]
    Full,
    /// Telegraphic: heavy abbreviation, arrows for causality, one word when one word does.
    Ultra,
    /// Classical-Chinese brevity register (文言文).
    Wenyan,
}

/// Per-provider `options` override (subset of opencode `ConfigV1.Info.options`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderOptions {
    #[serde(rename = "baseURL")]
    pub base_url: Option<String>,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
}

/// One `provider.<id>` config entry (extra opencode keys — name/npm/env — ignored).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderEntry {
    #[serde(default)]
    pub options: ProviderOptions,
    /// Per-model overrides keyed by model id (`provider.<id>.models.<model>`).
    #[serde(default)]
    pub models: HashMap<String, ModelEntry>,
}

/// One `provider.<id>.models.<model-id>` entry (extra opencode keys ignored).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelEntry {
    /// Declared context/output window for models the embedded registry doesn't
    /// know (e.g. local ollama models). Lets compaction trigger before the
    /// provider silently truncates the prompt.
    #[serde(default)]
    pub limits: Option<ModelLimitsEntry>,
}

/// `limits` block of a [`ModelEntry`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelLimitsEntry {
    /// Context window, in tokens.
    pub context: Option<u64>,
    /// Max output tokens.
    pub output: Option<u64>,
}

impl Config {
    /// Parse the permissive `provider` map into typed per-id overrides.
    /// Absent or unparseable → empty map (never errors the whole config).
    #[must_use]
    pub fn provider_overrides(&self) -> HashMap<String, ProviderEntry> {
        self.provider
            .as_ref()
            .and_then(|v| serde_json::from_value::<HashMap<String, ProviderEntry>>(v.clone()).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_overrides_parses_per_model_limits() {
        // `provider.<id>.models.<model-id>.limits` declares the context/output
        // window for models the embedded registry doesn't know (local ollama
        // models), so compaction can trigger before the provider silently
        // truncates the prompt.
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider": {
                "ollama": {
                    "options": { "baseURL": "http://localhost:11434/v1" },
                    "models": {
                        "gemma4:26b-mlx": { "limits": { "context": 32768, "output": 8192 } },
                        "ornith:9b": { "name": "extra keys ignored" }
                    }
                }
            }
        }))
        .unwrap();
        let ov = cfg.provider_overrides();
        let gemma = &ov["ollama"].models["gemma4:26b-mlx"];
        let limits = gemma.limits.as_ref().expect("limits parsed");
        assert_eq!(limits.context, Some(32_768));
        assert_eq!(limits.output, Some(8192));
        assert!(
            ov["ollama"].models["ornith:9b"].limits.is_none(),
            "model entry without limits parses as None"
        );
    }

    #[test]
    fn provider_overrides_parses_baseurl_and_apikey() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider": {
                "ollama": { "options": { "baseURL": "http://localhost:11434/v1" } },
                "myco":   { "name": "Custom", "npm": "x", "options": { "baseURL": "https://api.co/v1", "apiKey": "sk-1" }, "models": {} }
            }
        })).unwrap();
        let ov = cfg.provider_overrides();
        assert_eq!(
            ov["ollama"].options.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(ov["ollama"].options.api_key, None);
        assert_eq!(
            ov["myco"].options.base_url.as_deref(),
            Some("https://api.co/v1")
        );
        assert_eq!(ov["myco"].options.api_key.as_deref(), Some("sk-1"));
    }

    #[test]
    fn provider_overrides_tolerates_missing_and_malformed() {
        // no provider key at all
        let cfg: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(cfg.provider_overrides().is_empty());
        // provider is not an object-of-objects
        let cfg2: Config =
            serde_json::from_value(serde_json::json!({ "provider": "nonsense" })).unwrap();
        assert!(
            cfg2.provider_overrides().is_empty(),
            "malformed provider -> empty, no panic"
        );
    }

    #[test]
    fn parses_theme_key() {
        let cfg: Config = serde_json::from_str(r#"{ "theme": "nord" }"#).unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("nord"));
    }

    #[test]
    fn theme_absent_is_none() {
        let cfg: Config = serde_json::from_str(r#"{ "shell": "/bin/zsh" }"#).unwrap();
        assert_eq!(cfg.theme, None);
    }

    #[test]
    fn parses_rtk_enabled() {
        let cfg: Config = serde_json::from_str(r#"{ "rtk": { "enabled": true } }"#).unwrap();
        assert_eq!(cfg.rtk.map(|r| r.enabled), Some(true));
    }

    #[test]
    fn rtk_absent_is_none() {
        let cfg: Config = serde_json::from_str(r#"{ "shell": "/bin/zsh" }"#).unwrap();
        assert_eq!(cfg.rtk, None);
    }

    #[test]
    fn parses_tersemode_enabled_and_level() {
        let cfg: Config =
            serde_json::from_str(r#"{ "tersemode": { "enabled": true, "level": "ultra" } }"#)
                .unwrap();
        let c = cfg.tersemode.unwrap();
        assert!(c.enabled);
        assert_eq!(c.level, TersemodeLevel::Ultra);
    }

    #[test]
    fn tersemode_level_defaults_to_full() {
        let cfg: Config = serde_json::from_str(r#"{ "tersemode": { "enabled": true } }"#).unwrap();
        let c = cfg.tersemode.unwrap();
        assert!(c.enabled);
        assert_eq!(c.level, TersemodeLevel::Full);
    }

    #[test]
    fn tersemode_absent_is_none() {
        let cfg: Config = serde_json::from_str(r#"{ "shell": "/bin/zsh" }"#).unwrap();
        assert_eq!(cfg.tersemode, None);
    }

    #[test]
    fn parses_permission_mode() {
        let cfg: Config = serde_json::from_str(r#"{ "permission_mode": "full-auto" }"#).unwrap();
        assert_eq!(cfg.permission_mode.as_deref(), Some("full-auto"));
    }

    #[test]
    fn permission_mode_absent_is_none() {
        let cfg: Config = serde_json::from_str(r#"{ "shell": "/bin/zsh" }"#).unwrap();
        assert_eq!(cfg.permission_mode, None);
    }
}
