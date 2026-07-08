//! XDG-style global directories — ported from opencode
//! `packages/core/src/global.ts:10-29`, namespaced under otto's own
//! directories (no longer shared with an opencode install).
//!
//! Uses the `dirs` crate, which resolves the platform-native equivalents
//! (XDG on Linux, the macOS `~/Library/...` locations, `%APPDATA%` on
//! Windows). Each lives under an `otto` subdirectory.
//!
//! The `OTTO_CONFIG_DIR` env var overrides the config dir (port of
//! `global.ts:64`'s `Flag.OPENCODE_CONFIG_DIR ?? Path.config`, namespaced to
//! otto's own env var).

use std::path::PathBuf;

/// Application subdirectory name (`global.ts:10`).
const APP: &str = "otto";

/// Env var overriding the global config dir (`global.ts:64`).
const CONFIG_DIR_ENV: &str = "OTTO_CONFIG_DIR";

/// Global config dir — `xdgConfig/otto` (port of `global.ts:13`), or the
/// `OTTO_CONFIG_DIR` override when set.
///
/// Falls back to `./otto` only if the platform config dir can't be resolved.
#[must_use]
pub fn global_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        return PathBuf::from(dir);
    }
    dirs::config_dir().unwrap_or_default().join(APP)
}

/// Global data dir — `xdgData/opencode` (`global.ts:11`).
#[must_use]
pub fn global_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_default().join(APP)
}

/// Global cache dir — `xdgCache/opencode` (`global.ts:12`).
#[must_use]
pub fn global_cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_default().join(APP)
}

/// Global state dir — `xdgState/opencode` (`global.ts:14`).
///
/// `dirs::state_dir()` is Linux-only; on platforms without a state dir we fall
/// back to the data dir (matching how those platforms collapse XDG state).
#[must_use]
pub fn global_state_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_default()
        .join(APP)
}

/// Binary dir — `cache/opencode/bin` (`global.ts:22`, `bin: path.join(cache, "bin")`).
#[must_use]
pub fn bin_dir() -> PathBuf {
    global_cache_dir().join("bin")
}
