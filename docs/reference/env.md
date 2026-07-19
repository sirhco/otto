# Environment Variables

Every environment variable otto reads, what it changes, and where the read happens in the source.

See also: [`./cli.md`](./cli.md) for flags, [`./config.md`](./config.md) for the config file.

## Config and paths

| Variable | Effect | Absent | Source |
| --- | --- | --- | --- |
| `OTTO_CONFIG_DIR` | Overrides the global config directory wholesale. | `dirs::config_dir()/otto` (macOS: `~/Library/Application Support/otto`); `./otto` if the platform dir can't be resolved. | `crates/otto-config/src/paths.rs:19,27` |
| `OTTO_CONFIG` | Path to one extra config file, merged above the global config and below project configs. | No extra file. | `crates/otto-config/src/loader.rs:47` |
| `OTTO_CONFIG_CONTENT` | Literal JSONC config text merged **last** — highest precedence of all config sources. | Nothing merged. | `crates/otto-config/src/loader.rs:48` |
| `OTTO_DISABLE_PROJECT_CONFIG` | Presence-only. Skips discovery of up-tree project configs (`otto.json` walk). Any value, including empty, disables. | Project configs are discovered and merged. | `crates/otto-config/src/loader.rs:49` |

Config merge order, low to high:

```text
1. global config      (config dir)
2. OTTO_CONFIG        (explicit file)
3. project configs    (up-tree discovery, unless OTTO_DISABLE_PROJECT_CONFIG)
4. OTTO_CONFIG_CONTENT (literal text — wins)
```

<!-- src: crates/otto-config/src/loader.rs:210-236 -->

Note that only `global_config_dir()` honors `OTTO_CONFIG_DIR`. The data dir (SQLite db, logs), cache dir, and state dir are derived from the platform dirs unconditionally.
<!-- src: crates/otto-config/src/paths.rs:33-46 -->

## Auth and provider keys

| Variable | Effect | Absent | Source |
| --- | --- | --- | --- |
| `OTTO_AUTH_CONTENT` | JSON credential store contents, read in place of `auth.json`. Read-only short-circuit: writes are not persisted back. Invalid JSON falls through to the file. | `auth.json` under the config dir. | `crates/otto-auth/src/store.rs:22,100` |
| `ANTHROPIC_API_KEY` | Anthropic API key. Sent as the `x-api-key` header. Used as a credential source for `otto providers login anthropic` and as the LLM route fallback when no key is configured or stored. | OAuth credential, then stored API key, then no auth. | `crates/otto-auth/src/providers/anthropic.rs:193`; `crates/otto-llm/src/providers/anthropic.rs:36,112` |
| `OPENAI_API_KEY` | OpenAI API key. Sent as `Authorization: Bearer`. | Stored credential, else no auth. | `crates/otto-llm/src/providers/openai.rs:25,72` |
| `GOOGLE_GENERATIVE_AI_API_KEY` | Google Gemini API key. Sent as the `x-goog-api-key` header; never as `authorization`. | Stored credential, else no auth. | `crates/otto-llm/src/providers/google.rs:26,83` |

An env-sourced key resolves to `None` when the variable is set but empty, so `ANTHROPIC_API_KEY=` behaves the same as unset.
<!-- src: crates/otto-llm/src/auth.rs:40 -->

A config-supplied `provider.<id>.options.apiKey` takes precedence over the env var for that provider, and is treated as a literal value.
<!-- src: crates/otto-app/src/route_factory.rs:165,244 -->

### Env-name indirection

`Secret::Env(name)` (`crates/otto-llm/src/auth.rs:19,32,40`) and `api_key::from_env(var)` (`crates/otto-auth/src/providers/api_key.rs:39`) both resolve a key by an env-var *name* supplied at the call site, so the plumbing for "read this provider's key from an arbitrary variable" exists.

In the current tree that indirection is not reachable from config. `Secret::config(..)` is only ever constructed with the three fixed constants above, and config's `apiKey` becomes `Secret::literal`, not `Secret::Env`. `api_key::from_env` has no production caller.
<!-- src: crates/otto-llm/src/providers/{anthropic,openai,google}.rs; crates/otto-app/src/route_factory.rs:165,244 -->

## Server and logging

| Variable | Effect | Absent | Source |
| --- | --- | --- | --- |
| `OTTO_SERVER_PASSWORD` | Basic-auth password gate for `otto serve`. Applied only when `--password` and `otto_SERVER_PASSWORD` are both unset. | No auth gate. | `crates/otto-cli/src/commands.rs:29,66` |
| `otto_SERVER_PASSWORD` | Same gate, read by clap as the `--password` fallback. Lowercase prefix. Wins over `OTTO_SERVER_PASSWORD`. | Falls through to `OTTO_SERVER_PASSWORD`. | `crates/otto-cli/src/cli.rs:102` |
| `OTTO_LOG` | Full `tracing` `EnvFilter` directive, e.g. `otto_session=debug,otto_llm=trace`. Beats `--log-level` and config `logLevel`. Invalid directives are ignored. | `--log-level`, then config `logLevel`, then `warn`. | `crates/otto-cli/src/logging.rs:28,64` |

Both password variables are live. Resolution order:

```text
--password  >  otto_SERVER_PASSWORD  >  OTTO_SERVER_PASSWORD
```

<!-- src: crates/otto-cli/src/cli.rs:102 (clap env), crates/otto-cli/src/commands.rs:66 (or_else fallback) -->

Log filter precedence:

```text
OTTO_LOG  >  --log-level  >  config logLevel  >  warn
```

<!-- src: crates/otto-cli/src/logging.rs:62-77 -->

## Models registry

All three are read by `LoadOptions::from_env`.
<!-- src: crates/otto-llm/src/models_dev.rs:213-218 -->

| Variable | Effect | Absent | Source |
| --- | --- | --- | --- |
| `OTTO_MODELS_PATH` | Overrides the on-disk cache path for the models.dev registry. | The caller-supplied cache path under the platform cache dir. | `crates/otto-llm/src/models_dev.rs:213` |
| `OTTO_MODELS_URL` | Base URL for the registry fetch. `/api.json` is appended (trailing slashes trimmed). | `https://models.dev` | `crates/otto-llm/src/models_dev.rs:217`, fetch at `:240` |
| `OTTO_DISABLE_MODELS_FETCH` | Presence-only. Disables the network fetch; only the cache and the embedded registry are used. Any value, including empty, disables. | Fetch enabled. | `crates/otto-llm/src/models_dev.rs:218` |

## Streaming and diagnostics

| Variable | Effect | Absent | Source |
| --- | --- | --- | --- |
| `otto_STREAM_IDLE_TIMEOUT_SECS` | Seconds a provider stream may go silent before it is torn down. Non-numeric, empty, negative, and zero-ish values fall back to the default. Also read independently by the TUI's HTTP/SSE client. | `120` | `crates/otto-llm/src/transport/mod.rs:68,92`; `crates/otto-tui/src/client.rs:100` |
| `otto_DEBUG_STREAM` | Presence-only. Dumps every raw SSE chunk verbatim to stderr, prefixed `[otto-stream]`, plus transport errors and termination markers. | Off; a single env lookup, no overhead. | `crates/otto-llm/src/transport/sse.rs:87` |

## Tools and TUI

| Variable | Effect | Absent | Source |
| --- | --- | --- | --- |
| `otto_SKILLS_DIR` | Extra global skill-discovery root, searched before `~/.claude/skills`. | Roots are the working directory, then `~/.claude/skills`. | `crates/otto-tools/src/tools/skill.rs:174` |
| `otto_NO_SPLASH` | Presence-only. Skips the TUI startup splash. Equivalent to `otto tui --no-splash`. | Splash shown when stdout is a TTY and the terminal is large enough. | `crates/otto-tui/src/lib.rs:246` |

## Known wart — inconsistent casing

Four variables use a **lowercase** `otto_` prefix while every other otto variable uses `OTTO_`:

| Working casing | Wrong casing (silently ignored) | Source |
| --- | --- | --- |
| `otto_STREAM_IDLE_TIMEOUT_SECS` | `OTTO_STREAM_IDLE_TIMEOUT_SECS` | `crates/otto-llm/src/transport/mod.rs:68` |
| `otto_DEBUG_STREAM` | `OTTO_DEBUG_STREAM` | `crates/otto-llm/src/transport/sse.rs:87` |
| `otto_SKILLS_DIR` | `OTTO_SKILLS_DIR` | `crates/otto-tools/src/tools/skill.rs:174` |
| `otto_NO_SPLASH` | `OTTO_NO_SPLASH` | `crates/otto-tui/src/lib.rs:246` |

Environment variables are case-sensitive on unix. Setting `OTTO_SKILLS_DIR` has no effect whatsoever — no warning, no fallback. Use the exact casing in the left column.

```bash
otto_DEBUG_STREAM=1 otto run "hello"          # works
OTTO_DEBUG_STREAM=1 otto run "hello"          # silently ignored
```

### The server password is the mirror case

`otto serve --password` documents its env fallback as `otto_SERVER_PASSWORD` in the CLI help, and clap really does read that lowercase name.
<!-- src: crates/otto-cli/src/cli.rs:101-103 -->

Separately, the dispatcher falls back to the uppercase `OTTO_SERVER_PASSWORD`.
<!-- src: crates/otto-cli/src/commands.rs:29,66 -->

Both work. The lowercase one is consumed first because clap populates the flag before the dispatcher's `or_else` runs, so if both are set the lowercase value is used.

## Ambient variables

Read but not otto-specific.

| Variable | Effect | Source |
| --- | --- | --- |
| `NO_COLOR` | Presence-only ([no-color.org](https://no-color.org)). Disables ANSI color in CLI output and forces the TUI's monochrome theme, overriding a configured theme preset. | `crates/otto-cli/src/render.rs:39`; `crates/otto-tui/src/lib.rs:82`; `crates/otto-tui/src/theme.rs:97` |
| `HOME` | Expands `~` in permission rule patterns, resolves `~/.claude/skills`, and marks paths "under home" for the bash tool's external-path check. Unset degrades gracefully (empty home; nothing is under home). | `crates/otto-permission/src/ruleset.rs:199`; `crates/otto-tools/src/tools/skill.rs:177`; `crates/otto-tools/src/tools/bash.rs:89` |
| `PATH` | Resolves LSP server binaries and detects whether `rtk` is installed for the rtk hook. Split on the platform separator; no `which` subprocess. | `crates/otto-lsp/src/registry.rs:73`; `crates/otto-tools/src/hooks/rtk.rs:92` |
| `COLORTERM` | `truecolor` or `24bit` (case-insensitive) selects the TUI's 24-bit color depth. | `crates/otto-tui/src/appearance/mod.rs:32` |
| `TERM` | Contains `256color` selects 256-color depth. Unrecognized or absent falls through to `Ansi16`. | `crates/otto-tui/src/appearance/mod.rs:33` |
| `SSH_TTY` | Presence marks the session as remote, which suppresses OS theme detection. | `crates/otto-tui/src/appearance/os_theme.rs:34` |
| `SSH_CONNECTION` | Same, checked as an alternative to `SSH_TTY`. | `crates/otto-tui/src/appearance/os_theme.rs:34` |
