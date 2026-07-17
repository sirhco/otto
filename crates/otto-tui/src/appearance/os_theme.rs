//! Per-platform OS light/dark appearance detection. Every detector is
//! best-effort: process-spawn failure, non-UTF8 output, or an unparseable
//! response all fold to `None` rather than erroring — callers fall back to
//! the configured/default theme (see `lib.rs::run`).

use super::ThemeMode;

/// Detect the OS's current light/dark appearance for the running platform.
/// `None` means undetectable (unsupported platform/desktop environment,
/// missing binary, or a parse failure) — callers should fall back to
/// [`detect_os_theme_ssh`] (if applicable) or a default.
pub async fn detect_os_theme() -> Option<ThemeMode> {
    #[cfg(target_os = "macos")]
    {
        detect_macos().await
    }
    #[cfg(target_os = "linux")]
    {
        detect_linux().await
    }
    #[cfg(target_os = "windows")]
    {
        detect_windows().await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Whether this process looks like it's running over SSH.
#[must_use]
pub fn is_ssh_session() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

#[cfg(target_os = "macos")]
async fn detect_macos() -> Option<ThemeMode> {
    let output = tokio::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .await
        .ok()?;
    Some(parse_defaults_output(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
    ))
}

/// Pure core of the macOS detector: `defaults read` exits nonzero (key
/// unset) when the OS is in light mode — its normal, undocumented default
/// state — and prints `Dark\n` on stdout when in dark mode.
// Not cfg-gated (see module docs) so it always compiles and is unit-tested,
// but on non-macOS builds its only caller (`detect_macos`) is cfg'd out.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_defaults_output(success: bool, stdout: &str) -> ThemeMode {
    if success && stdout.trim() == "Dark" {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    }
}

#[cfg(target_os = "linux")]
async fn detect_linux() -> Option<ThemeMode> {
    let output = tokio::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_gsettings_output(&String::from_utf8_lossy(&output.stdout))
}

/// Pure core of the Linux detector. GNOME reports e.g. `'prefer-dark'\n` or
/// `'default'\n`. Any other desktop environment (KDE, XFCE, ...) or a GNOME
/// version predating `color-scheme` returns `None` — not supported by this
/// heuristic (GNOME-family only).
// Not cfg-gated (see module docs) so it always compiles and is unit-tested,
// but on non-Linux builds its only caller (`detect_linux`) is cfg'd out.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_gsettings_output(stdout: &str) -> Option<ThemeMode> {
    let value = stdout.trim();
    if value.contains("prefer-dark") {
        Some(ThemeMode::Dark)
    } else if value.contains("prefer-light") || value.contains("default") {
        Some(ThemeMode::Light)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
async fn detect_windows() -> Option<ThemeMode> {
    let output = tokio::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_reg_output(&String::from_utf8_lossy(&output.stdout))
}

/// Pure core of the Windows detector. `reg query` prints a line like
/// `    AppsUseLightTheme    REG_DWORD    0x1`; `0x1` = light, `0x0` = dark.
// Not cfg-gated (see module docs) so it always compiles and is unit-tested,
// but on non-Windows builds its only caller (`detect_windows`) is cfg'd out.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_reg_output(stdout: &str) -> Option<ThemeMode> {
    match stdout.split_whitespace().last()? {
        "0x0" => Some(ThemeMode::Dark),
        "0x1" => Some(ThemeMode::Light),
        _ => None,
    }
}

/// One-shot OSC 11 background-color query, for SSH sessions where the host
/// platform's own appearance can't be read directly. Writes `\x1b]11;?\x07`
/// to stdout and reads the terminal's reply from stdin in raw mode with a
/// short timeout. **Not repeatable** — the raw-mode read would race the live
/// `crossterm::EventStream` — callers must invoke this at most once, before
/// entering the main event loop.
pub async fn detect_os_theme_ssh() -> Option<ThemeMode> {
    use std::io::Write;
    use tokio::io::AsyncReadExt;

    let mut out = std::io::stdout();
    write!(out, "\x1b]11;?\x07").ok()?;
    out.flush().ok()?;

    crossterm::terminal::enable_raw_mode().ok()?;
    let mut stdin = tokio::io::stdin();
    let mut buf = [0u8; 64];
    let read = tokio::time::timeout(std::time::Duration::from_millis(200), stdin.read(&mut buf))
        .await
        .ok()
        .and_then(Result::ok);
    let _ = crossterm::terminal::disable_raw_mode();

    parse_osc11_reply(&buf[..read?])
}

/// Pure core of [`detect_os_theme_ssh`]: parses an OSC 11 reply of the form
/// `\x1b]11;rgb:RRRR/GGGG/BBBB` terminated by ST (`\x1b\\`) or BEL (`\x07`),
/// and classifies by perceptual luminance.
fn parse_osc11_reply(reply: &[u8]) -> Option<ThemeMode> {
    let text = std::str::from_utf8(reply).ok()?;
    let rgb = text.split("rgb:").nth(1)?;
    let mut channels = rgb.split(['/', '\x1b', '\x07']).filter(|s| !s.is_empty());
    let r_hex = channels.next()?;
    let g_hex = channels.next()?;
    let b_hex = channels.next()?;

    // Each channel may be a different digit-count per the X11 rgb: spec
    // (typically uniform in practice, but not guaranteed) — normalize each
    // by its own max value rather than assuming a fixed 16-bit width.
    let norm = |hex: &str| -> Option<f64> {
        let v = u32::from_str_radix(hex, 16).ok()?;
        let max = 16u32
            .checked_pow(u32::try_from(hex.len()).ok()?)?
            .checked_sub(1)?;
        if max == 0 {
            return None;
        }
        Some(f64::from(v) / f64::from(max) * 255.0)
    };
    let (r, g, b) = (norm(r_hex)?, norm(g_hex)?, norm(b_hex)?);
    let luminance = 0.299 * r + 0.587 * g + 0.114 * b;
    Some(if luminance < 128.0 {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults_dark() {
        assert_eq!(parse_defaults_output(true, "Dark\n"), ThemeMode::Dark);
    }

    #[test]
    fn parse_defaults_light_when_key_unset() {
        // Nonzero exit (key absent) is the default, undocumented light state.
        assert_eq!(parse_defaults_output(false, ""), ThemeMode::Light);
    }

    #[test]
    fn parse_defaults_light_on_unexpected_output() {
        assert_eq!(parse_defaults_output(true, "Light\n"), ThemeMode::Light);
    }

    #[test]
    fn parse_gsettings_dark() {
        assert_eq!(
            parse_gsettings_output("'prefer-dark'\n"),
            Some(ThemeMode::Dark)
        );
    }

    #[test]
    fn parse_gsettings_default_is_light() {
        assert_eq!(
            parse_gsettings_output("'default'\n"),
            Some(ThemeMode::Light)
        );
    }

    #[test]
    fn parse_gsettings_prefer_light_is_light() {
        assert_eq!(
            parse_gsettings_output("'prefer-light'\n"),
            Some(ThemeMode::Light)
        );
    }

    #[test]
    fn parse_gsettings_unparseable_is_none() {
        assert_eq!(parse_gsettings_output("garbage\n"), None);
    }

    #[test]
    fn parse_reg_dark() {
        assert_eq!(
            parse_reg_output("    AppsUseLightTheme    REG_DWORD    0x0\n"),
            Some(ThemeMode::Dark)
        );
    }

    #[test]
    fn parse_reg_light() {
        assert_eq!(
            parse_reg_output("    AppsUseLightTheme    REG_DWORD    0x1\n"),
            Some(ThemeMode::Light)
        );
    }

    #[test]
    fn parse_reg_garbage_is_none() {
        assert_eq!(parse_reg_output("ERROR: key not found\n"), None);
    }

    #[test]
    fn parse_osc11_reply_dark() {
        let reply = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
        assert_eq!(parse_osc11_reply(reply), Some(ThemeMode::Dark));
    }

    #[test]
    fn parse_osc11_reply_light() {
        let reply = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert_eq!(parse_osc11_reply(reply), Some(ThemeMode::Light));
    }

    #[test]
    fn parse_osc11_reply_garbage_is_none() {
        assert_eq!(parse_osc11_reply(b"not an osc11 reply"), None);
    }

    #[test]
    fn parse_osc11_reply_handles_8bit_hex_channels() {
        // 2-digit (8-bit) hex per channel, per the X11 rgb: spec — must not
        // be misread as a 16-bit value (that bug misclassified white as
        // Dark).
        let reply = b"\x1b]11;rgb:ff/ff/ff\x07";
        assert_eq!(parse_osc11_reply(reply), Some(ThemeMode::Light));
    }

    #[test]
    fn is_ssh_session_reflects_env() {
        // Exercises both branches without racing other tests' env vars: uses
        // vars this crate touches nowhere else.
        temp_env_var("SSH_TTY", Some("/dev/ttys001"), || {
            assert!(is_ssh_session());
        });
        temp_env_var("SSH_TTY", None, || {
            temp_env_var("SSH_CONNECTION", None, || {
                assert!(!is_ssh_session());
            });
        });
    }

    /// Set (or unset) an env var for the duration of `f`, restoring the prior
    /// value afterward. `std::env` tests are inherently process-global and
    /// therefore not run concurrently by default within one test binary
    /// invocation for THIS pattern to be safe — acceptable here since no
    /// other test in this crate reads `SSH_TTY`/`SSH_CONNECTION`.
    fn temp_env_var(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let prior = std::env::var(key).ok();
        // SAFETY: single-threaded test execution of this function; no other
        // thread reads/writes `key` concurrently (this crate's tests touch
        // no other SSH-related env var).
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        f();
        // SAFETY: see above.
        unsafe {
            match prior {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
