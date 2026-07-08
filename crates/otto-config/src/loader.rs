//! Config parsing, merging, discovery and loading — synchronous port of
//! opencode `packages/opencode/src/config/config.ts` and `config/paths.ts`.
//!
//! Out of scope for now (left as TODOs): remote WellKnown / `remote_config`
//! fetch (`config.ts:355-395`), managed / MDM preferences (`config.ts:515-533`),
//! auto-discovered command/agent/plugin loading, and `$schema` write-back.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::paths;
use crate::schema::{Config, DEFAULT_SCHEMA};

/// Config file basenames, low → high precedence within a single directory.
///
/// Mirrors opencode's global order `config.json → opencode.json → opencode.jsonc`
/// (`config.ts:258-260`, later wins) extended with the `otto.*` aliases.
const FILE_NAMES: &[&str] = &[
    "config.json", // legacy
    "opencode.json",
    "opencode.jsonc",
    "otto.json",
    "otto.jsonc",
];

/// `.opencode/` directory config files, low → high precedence
/// (`config.ts:425` — `["opencode.json", "opencode.jsonc"]`).
const DOT_DIR_NAMES: &[&str] = &["opencode.json", "opencode.jsonc"];

/// Env-driven overrides, passed explicitly so callers (and tests) never have to
/// mutate process env. Populate from the environment with [`EnvOverrides::from_env`].
#[derive(Debug, Clone, Default)]
pub struct EnvOverrides {
    /// `OPENCODE_CONFIG` — explicit extra config file merged above global
    /// (`config.ts:400-403`).
    pub config: Option<PathBuf>,
    /// `OPENCODE_CONFIG_CONTENT` — raw JSON merged last / highest
    /// (`config.ts:467-475`).
    pub config_content: Option<String>,
    /// `OPENCODE_DISABLE_PROJECT_CONFIG` — skip up-tree project configs
    /// (`config.ts:405`).
    pub disable_project_config: bool,
}

impl EnvOverrides {
    /// Read the three override env vars from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            config: std::env::var_os("OPENCODE_CONFIG").map(PathBuf::from),
            config_content: std::env::var("OPENCODE_CONFIG_CONTENT").ok(),
            disable_project_config: std::env::var_os("OPENCODE_DISABLE_PROJECT_CONFIG").is_some(),
        }
    }
}

/// Parse JSONC text (comments + trailing commas) into a [`Config`].
///
/// Ports opencode `ConfigParse.jsonc` + schema decode (`config.ts:226-227`):
/// strip comments to a `serde_json::Value`, then deserialize. Empty / whitespace
/// input yields the default config (opencode `loadFile` returns `{}`, config.ts:242).
pub fn parse(text: &str) -> Result<Config> {
    let value = jsonc_parser::parse_to_serde_value(text, &Default::default())
        .map_err(|e| Error::Jsonc(e.to_string()))?;
    match value {
        None => Ok(Config::default()),
        Some(v) => serde_json::from_value(v).map_err(Error::Deserialize),
    }
}

/// Deep-merge `over` into `base` at the `serde_json::Value` level.
///
/// Objects merge recursively; arrays and scalars are replaced by `over`
/// (remeda `mergeDeep`, `config.ts:41-43`).
fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

/// Deep-merge two configs, `over` winning on conflicts.
///
/// Port of `mergeConfigConcatArrays` (`config.ts:45-51`): a plain deep merge,
/// **except** `instructions`, which is concatenated and deduped (insertion order
/// preserved, base before override) instead of replaced — matching
/// `Array.from(new Set([...target, ...source]))`.
///
/// Infallible: both inputs are valid [`Config`] values, so the round-trip through
/// `serde_json::Value` cannot fail.
#[must_use]
pub fn merge(base: Config, over: Config) -> Config {
    let base_instr = base.instructions.clone();
    let over_instr = over.instructions.clone();

    let mut base_v = serde_json::to_value(&base).expect("Config serializes");
    let over_v = serde_json::to_value(&over).expect("Config serializes");
    deep_merge(&mut base_v, over_v);
    let mut merged: Config = serde_json::from_value(base_v).expect("merged Config deserializes");

    if let (Some(a), Some(b)) = (base_instr, over_instr) {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for item in a.into_iter().chain(b) {
            if seen.insert(item.clone()) {
                out.push(item);
            }
        }
        merged.instructions = Some(out);
    }

    merged
}

/// Discover project config files by walking **up** from `cwd`.
///
/// Ports `ConfigPaths.files` + the `.opencode` directory sweep
/// (`paths.ts:10-41`, `config.ts:406-433`). The walk stops at (and includes) the
/// `worktree` dir when given, otherwise at the first ancestor containing `.git`,
/// otherwise at the filesystem root.
///
/// Return order is opencode's application order — lowest precedence first:
/// ancestor dirs before `cwd` (so files closer to `cwd` are merged last and win),
/// and within that, plain project files before `.opencode/` files. Returns empty
/// when `disable_project` is set (`config.ts:405`).
#[must_use]
pub fn discover(cwd: &Path, worktree: Option<&Path>, disable_project: bool) -> Vec<PathBuf> {
    if disable_project {
        return Vec::new();
    }

    // Collect dirs from cwd upward, stopping at worktree / git root / fs root.
    let mut dirs = Vec::new();
    let mut cur = Some(cwd);
    while let Some(d) = cur {
        dirs.push(d.to_path_buf());
        if worktree == Some(d) || d.join(".git").exists() {
            break;
        }
        cur = d.parent();
    }
    dirs.reverse(); // rootmost ancestor first, cwd last

    let mut out = Vec::new();
    // Plain project config files, ancestor → cwd.
    for dir in &dirs {
        for name in FILE_NAMES {
            let p = dir.join(name);
            if p.is_file() {
                out.push(p);
            }
        }
    }
    // `.opencode/` directory config files, ancestor → cwd.
    for dir in &dirs {
        for name in DOT_DIR_NAMES {
            let p = dir.join(".opencode").join(name);
            if p.is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// Read + parse a config file. Returns `Ok(None)` when the file is absent or
/// empty (`loadFile`, `config.ts:239-244`).
fn load_file(path: &Path) -> Result<Option<Config>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(parse(&text)?))
}

/// Load and merge the global config files in `config_dir`.
///
/// Port of `loadGlobal` (`config.ts:246-279`): merge
/// `config.json → opencode.json → opencode.jsonc` (later wins). If none exist the
/// default is `{ "$schema": DEFAULT_SCHEMA }`; `$schema` is injected either way
/// (`config.ts:231-235,254`).
pub fn load_global(config_dir: &Path, _env: &EnvOverrides) -> Result<Config> {
    let mut cfg = Config::default();
    for name in ["config.json", "opencode.json", "opencode.jsonc"] {
        if let Some(next) = load_file(&config_dir.join(name))? {
            cfg = merge(cfg, next);
        }
    }
    if cfg.schema.is_none() {
        cfg.schema = Some(DEFAULT_SCHEMA.to_string());
    }
    Ok(cfg)
}

/// Load the fully-merged config for `cwd`, using `config_dir` as the global
/// config directory and the explicit `env` overrides.
///
/// Precedence (low → high), porting `loadInstanceState` (`config.ts:397-475`):
/// 1. global config (`load_global`)
/// 2. `OPENCODE_CONFIG` explicit file (`env.config`)
/// 3. up-tree project configs (`discover`), unless `disable_project_config`
/// 4. `OPENCODE_CONFIG_CONTENT` (`env.config_content`) — highest
///
/// This is the testable seam: pass a tempdir as `config_dir` and a constructed
/// [`EnvOverrides`] so no process env or real global dir is touched.
pub fn load_with(cwd: &Path, config_dir: &Path, env: &EnvOverrides) -> Result<Config> {
    let mut cfg = load_global(config_dir, env)?;

    if let Some(path) = &env.config
        && let Some(next) = load_file(path)?
    {
        cfg = merge(cfg, next);
    }

    for path in discover(cwd, None, env.disable_project_config) {
        if let Some(next) = load_file(&path)? {
            cfg = merge(cfg, next);
        }
    }

    if let Some(content) = &env.config_content {
        cfg = merge(cfg, parse(content)?);
    }

    if cfg.schema.is_none() {
        cfg.schema = Some(DEFAULT_SCHEMA.to_string());
    }
    Ok(cfg)
}

/// Load the fully-merged config for `cwd` using the real global config dir and
/// process-env overrides. Convenience wrapper over [`load_with`].
pub fn load(cwd: &Path) -> Result<Config> {
    let env = EnvOverrides::from_env();
    let config_dir = paths::global_config_dir();
    load_with(cwd, &config_dir, &env)
}
