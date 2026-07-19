# Configuration reference

Every key otto reads from `otto.json` / `otto.jsonc`, where those files are found, and how they merge.

## Where config lives

Config is JSONC: comments and trailing commas are accepted in every file, including ones named `.json`.
<!-- src: crates/otto-config/src/loader.rs:59-66 -->

### Filenames

Within any single directory, these basenames are searched in order — **later wins**:

| Order | Location | Note |
|---|---|---|
| 1 | `config.json` | legacy name, still honored |
| 2 | `otto.json` | |
| 3 | `otto.jsonc` | |
| 4 | `.otto/otto.json` | project dirs only |
| 5 | `.otto/otto.jsonc` | project dirs only |

<!-- src: crates/otto-config/src/loader.rs:21,25 -->

The global config directory only searches names 1–3; `.otto/` is swept for project directories.
<!-- src: crates/otto-config/src/loader.rs:194-205 -->

### Global directory

`$OTTO_CONFIG_DIR` when set, otherwise the platform config dir plus `otto`.
<!-- src: crates/otto-config/src/paths.rs:26-31 -->

| Platform | Config dir | Data dir (`otto.db`, logs) |
|---|---|---|
| macOS | `~/Library/Application Support/otto` | `~/Library/Application Support/otto` |
| Linux | `~/.config/otto` | `~/.local/share/otto` |
| Windows | `%APPDATA%\otto` | `%APPDATA%\otto` |

On macOS `dirs::config_dir()` and `dirs::data_dir()` both resolve to `~/Library/Application Support`, so the config dir and data dir are the same directory. On Linux they are separate.
<!-- src: crates/otto-config/src/paths.rs:26-37 -->

### Precedence

Low → high; each layer deep-merges over the one below.

1. Global directory files
2. `$OTTO_CONFIG` — an explicit extra file path
3. Project configs from the up-tree walk (skipped when `OTTO_DISABLE_PROJECT_CONFIG` is set)
4. `$OTTO_CONFIG_CONTENT` — raw JSON text, highest precedence

<!-- src: crates/otto-config/src/loader.rs:210-241 -->

`$schema` is injected with otto's default schema URL if no layer set one.
<!-- src: crates/otto-config/src/loader.rs:237-239 -->

### Project discovery walk

`discover()` collects directories from the cwd **upward**, stopping at (and including) the first ancestor containing a `.git` entry, or the filesystem root.
<!-- src: crates/otto-config/src/loader.rs:138-148 -->

Ordering has a trap. The walk does **not** emit all files per directory. It emits *all plain-name files across all directories* first, and only then *all `.otto/` files across all directories*:
<!-- src: crates/otto-config/src/loader.rs:150-169 -->

```text
<repo-root>/otto.json          (lowest)
<repo-root>/sub/otto.json
<cwd>/otto.json
<repo-root>/.otto/otto.json
<repo-root>/sub/.otto/otto.json
<cwd>/.otto/otto.json          (highest)
```

So `<repo-root>/.otto/otto.json` **outranks** `<cwd>/otto.json`, even though the repo root is further away. A `.otto/` config anywhere up the tree beats every plain-named config, including the one next to you.

### Merge semantics

Deep merge at the JSON level: objects merge key-by-key recursively; arrays and scalars are replaced wholesale by the higher layer.
<!-- src: crates/otto-config/src/loader.rs:72-86 -->

One exception: `instructions` is concatenated and deduped (base entries first, insertion order preserved) rather than replaced.
<!-- src: crates/otto-config/src/loader.rs:98-119 -->

A key set to `null` does not clobber a lower layer — absent fields are omitted at serialize time rather than written as `null`.
<!-- src: crates/otto-config/src/schema.rs:15-17 -->

## Top-level keys

Key names below are exactly as they appear in JSON. Only `logLevel` is renamed from its Rust field; everything else keeps snake_case.
<!-- src: schema/config.json properties -->

**Status** marks whether otto currently reads the key at runtime. Fields marked *dead* parse, validate, and merge, but no code outside `otto-config` consumes them — setting one has no effect today. They are retained for opencode schema fidelity.

| Key | Type | Default | Status | Meaning |
|---|---|---|---|---|
| `$schema` | string | otto's schema URL | live (metadata) | Editor completion source; injected if unset. |
| `shell` | string | — | **dead** | Intended shell override. The `bash` tool hardcodes `sh`. <!-- src: crates/otto-tools/src/tools/bash.rs:335 --> |
| `theme` | string | dark preset | live | TUI color preset (`catppuccin`, `gruvbox`, `nord`, `base16`, `light`, `auto`). `auto` follows OS appearance. `NO_COLOR` wins over any value. <!-- src: crates/otto-tui/src/lib.rs:81-95 --> |
| `logLevel` | `"DEBUG"`\|`"INFO"`\|`"WARN"`\|`"ERROR"` | `WARN` | live | Tracing level. Overridden by `--log-level` and `OTTO_LOG`. <!-- src: crates/otto-cli/src/logging.rs:70-88 --> |
| `command` | object | — | **dead** | Custom command definitions; no registry consumes them. |
| `skills` | any | — | **dead** | See [Instructions and skills](#instructions-and-skills-what-actually-works). |
| `watcher` | object | — | **dead** | `{ "ignore": [...] }`; no file watcher reads it. |
| `plugin` | string[] | — | **dead** | No plugin loader exists. |
| `share` | `"manual"`\|`"auto"`\|`"disabled"` | — | **dead** | Session sharing; unimplemented. |
| `autoupdate` | bool \| `"notify"` | — | **dead** | No updater. |
| `disabled_providers` | string[] | — | **dead** | Route factory does not filter on it. |
| `enabled_providers` | string[] | — | **dead** | As above. |
| `model` | string | `anthropic/claude-sonnet-4-5` | live | Default model as `provider/model`. <!-- src: crates/otto-app/src/route_factory.rs:55-62 --> |
| `small_model` | string | — | **dead** | No cheap-model call site (titles/summaries use the main model). |
| `default_agent` | string | `build` | live | Starting agent; falls back to `build`, then the first resolved agent. <!-- src: crates/otto-app/src/runtime.rs:352-360 --> |
| `username` | string | — | **dead** | Never surfaced. |
| `agent` | object | — | live | Per-agent overrides. See [`agent`](#agent). |
| `provider` | object | — | live | Per-provider endpoints, keys, model limits. See [`provider`](#provider). |
| `mcp` | object | — | live | MCP servers to connect at startup. See [`mcp`](#mcp). |
| `formatter` | bool \| object | — | **dead** | No format-on-write path. |
| `lsp` | bool \| object | enabled | live | LSP server enablement and overrides. See [`lsp`](#lsp). |
| `instructions` | string[] | — | **dead** | See [Instructions and skills](#instructions-and-skills-what-actually-works). |
| `permission` | string \| object | empty ruleset | live | Permission rules. See [`permission`](#permission). |
| `permission_mode` | `"approve-each"`\|`"accept-edits"`\|`"full-auto"` | `approve-each` | live | Starting mode; cyclable live in the TUI. Unknown values fall back to the default. <!-- src: crates/otto-app/src/runtime.rs:836-843 --> |
| `tools` | object of bool | — | **dead** | `ToolRegistry::with_builtins` never consults this map. |
| `attachment` | any | — | **dead** | Unimplemented. |
| `enterprise` | object | — | **dead** | `{ "url": "..." }`; nothing fetches it. |
| `tool_output` | object | — | **dead** | Truncation is hardcoded at 2000 chars. <!-- src: crates/otto-session/src/compaction.rs:57 --> |
| `compaction` | object | see below | live | Context compaction and pruning. See [`compaction`](#compaction). |
| `retry` | object | see below | live | Provider retry budgets and turn timeout. See [`retry`](#retry). |
| `experimental` | any | — | **dead** | Unimplemented. |
| `rtk` | object | off | live | Route `bash` commands through `rtk`. See [`rtk`](#rtk). |
| `tersemode` | object | off | live | Append a brevity directive to the system prompt. See [`tersemode`](#tersemode). |
| `hooks` | object | — | live | Lifecycle hook commands. See [`hooks`](#hooks). |

Unknown keys parse without error but are dropped on re-serialize (`GET`/`PATCH /config` will not round-trip them).
<!-- src: crates/otto-config/src/schema.rs:113-115 -->

## Instructions and skills: what actually works

Both `instructions` and `skills` are declared in the schema and merged correctly, but nothing reads them. Do not rely on either. What otto does instead:

### Instruction files

otto walks from the cwd up to the **filesystem root** — no `.git` stop, unlike config discovery — reading `AGENTS.md` then `CLAUDE.md` in each directory. Nearest directory first. Empty or whitespace-only files are skipped.
<!-- src: crates/otto-session/src/system.rs:108-123 -->

### Skills

Skills are discovered by a recursive walk for files named `SKILL.md` under these roots, in order:

1. the current working directory
2. `$otto_SKILLS_DIR` (note the lowercase `otto` prefix — this is the literal env var name)
3. `$HOME/.claude/skills`

The first root to define a given skill name wins. Only a name + description index is injected into the system prompt; bodies load on demand via the `skill` tool.
<!-- src: crates/otto-tools/src/tools/skill.rs:151-190 -->

`.otto/` is a config-file location only. It is not scanned for skills or instruction files.

## Sub-objects

### `provider`

Per-provider endpoint, credential, and model-window overrides.

```json
{
  "provider": {
    "anthropic": {
      "options": { "baseURL": "https://litellm.internal/v1" }
    },
    "ollama": {
      "options": { "baseURL": "http://localhost:11434/v1" },
      "models": {
        "qwen3:32b": { "limits": { "context": 32768, "output": 8192 } }
      }
    },
    "vertex": {
      "options": { "project": "my-gcp-project", "location": "europe-west1" }
    }
  }
}
```

| Field | Type | Meaning |
|---|---|---|
| `options.baseURL` | string | Endpoint override. Required for provider ids with no native route. |
| `options.apiKey` | string | Fallback key when no credential is stored for the provider. |
| `options.project` | string | GCP project id. `vertex` only; ignored elsewhere. |
| `options.location` | string | GCP region, default `us-central1`. `vertex` only. |
| `models.<id>.limits.context` | integer | Context window in tokens. |
| `models.<id>.limits.output` | integer | Max output tokens. |

<!-- src: crates/otto-config/src/schema.rs:303-344 -->

Extra opencode keys (`name`, `npm`, `env`) parse and are ignored. A malformed `provider` value yields an empty override map rather than failing the config.
<!-- src: crates/otto-config/src/schema.rs:346-356 -->

`anthropic`, `openai`, `google`, `gemini`, `vertex`, and `github-copilot` get native protocol routes. Any other id falls through to the OpenAI-compatible protocol at `{baseURL}/chat/completions`.

### `permission`

Rules are `permission-id → action`, or `permission-id → { glob: action }`. A bare string at the top level means `* → action`. Actions are `allow`, `ask`, `deny`.

```json
{
  "permission": {
    "bash": { "git push *": "ask", "*": "allow" },
    "edit": { "*.lock": "deny", "*": "allow" },
    "webfetch": "ask"
  }
}
```

<!-- src: crates/otto-permission/src/ruleset.rs:70-110 -->

Evaluation is last-match-wins across layers: mode overlay < agent session ruleset < user config ruleset < in-session Always approvals < danger rules. See [../guide/permissions.md](../guide/permissions.md).

### `compaction`

```json
{
  "compaction": {
    "auto": true,
    "prune": true,
    "tail_turns": 2,
    "preserve_recent_tokens": 20000,
    "reserved": 20000,
    "prune_protect_tokens": 40000
  }
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `auto` | bool | `true` | Auto-compact when the context window fills. |
| `prune` | bool | — | Post-turn pruning of old tool outputs. |
| `tail_turns` | integer | `2` | Turns kept verbatim at the tail. |
| `preserve_recent_tokens` | integer | `20000` | Recent-token budget kept out of the summary. |
| `reserved` | integer | `20000` | Headroom reserved below the context limit. |
| `prune_protect_tokens` | integer | `40000` | Trailing tool-output budget protected from pruning. |

<!-- src: crates/otto-session/src/run.rs:44,47; crates/otto-session/src/compaction.rs:44,54 -->
<!-- src: crates/otto-app/src/runtime.rs:586-600 -->

### `retry`

```json
{
  "retry": {
    "max_attempts": 5,
    "max_total_attempts": 20,
    "turn_timeout_seconds": 900
  }
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `max_attempts` | integer | `5` | Retries of a single step. Budget resets each step. |
| `max_total_attempts` | integer | `20` | Retries summed across all steps of one prompt. Does not reset. |
| `turn_timeout_seconds` | integer | unset (no timeout) | Wall-clock cap on one turn. At the deadline the run's abort token fires; partial work is persisted. |

<!-- src: crates/otto-config/src/schema.rs:100-110; crates/otto-app/src/runtime.rs:602-620 -->

### `tool_output`

Declared, **not read** — truncation uses a hardcoded 2000-char limit.
<!-- src: crates/otto-session/src/compaction.rs:57 -->

```json
{ "tool_output": { "max_lines": 2000, "max_bytes": 200000 } }
```

### `watcher`

Declared, **not read**.

```json
{ "watcher": { "ignore": ["target/**", "node_modules/**"] } }
```

### `enterprise`

Declared, **not read**.

```json
{ "enterprise": { "url": "https://otto.internal" } }
```

### `rtk`

```json
{ "rtk": { "enabled": true } }
```

Off unless `enabled` is `true`. When on and `rtk` is present on `PATH`, `bash` tool commands are routed through the `rtk` proxy to compact their output.
<!-- src: crates/otto-app/src/runtime.rs:775-779 -->

### `tersemode`

```json
{ "tersemode": { "enabled": true, "level": "full" } }
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | Append the brevity directive to the system prompt. |
| `level` | `"lite"`\|`"full"`\|`"ultra"`\|`"wenyan"` | `"full"` | Compression intensity. Ignored when disabled. |

Every level carries a byte-exact-preservation clause: code blocks, paths, commands, identifiers, URLs, and error strings are never compressed.
<!-- src: crates/otto-app/src/runtime.rs:782-836 -->

### `hooks`

External commands fired at lifecycle points, run via `sh -c`.

```json
{
  "hooks": {
    "pre_tool_use": [
      {
        "matcher": "^(edit|write)$",
        "hooks": [{ "command": "./scripts/guard.sh", "timeout_ms": 5000 }]
      }
    ],
    "session_start": [
      { "hooks": [{ "command": "echo started >> /tmp/otto.log" }] }
    ]
  }
}
```

Event keys: `session_start`, `user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `pre_compact`, `stop`, `subagent_stop`, `notification`.

| Field | Type | Meaning |
|---|---|---|
| `matcher` | string | Regex tested against the tool id. Absent matches every tool; ignored for non-tool events. An invalid regex never matches. |
| `hooks[].command` | string | Command to run. |
| `hooks[].timeout_ms` | integer | Per-command timeout override. |

<!-- src: crates/otto-hooks/src/config.rs:12-57 -->

### `agent`

Overrides on builtin agents, or definitions of new ones. An unknown key creates a new agent.

```json
{
  "agent": {
    "build": { "model": "anthropic/claude-opus-4-8", "steps": 200 },
    "plan": { "disable": true },
    "reviewer": {
      "description": "Reviews diffs",
      "prompt": "You review diffs and report findings only.",
      "temperature": 0.2,
      "permission": { "edit": "deny" }
    }
  }
}
```

| Field | Type | Meaning |
|---|---|---|
| `disable` | bool | Remove the agent entirely. |
| `model` | string | `provider/model` override. |
| `variant` | string | Model variant. |
| `prompt` | string | System prompt override. |
| `description` | string | Shown in agent pickers and to the `task` tool. |
| `temperature` | number | Sampling temperature. |
| `top_p` | number | Nucleus sampling. |
| `mode` | string | Agent mode. |
| `color` | string | TUI accent color. |
| `hidden` | bool | Hide from pickers. |
| `name` | string | Rename. |
| `steps` | integer | Step cap for the agent. |
| `options` | object | Deep-merged provider option bag. |
| `permission` | string \| object | Ruleset merged over the agent's existing rules. |

<!-- src: crates/otto-agent/src/config.rs:24-114 -->

### `mcp`

Servers are keyed by name and discriminated on `type`. Connection failures are non-fatal — a server that fails to connect is skipped.
<!-- src: crates/otto-app/src/runtime.rs:205-220 -->

```json
{
  "mcp": {
    "fs": {
      "type": "local",
      "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."],
      "environment": { "LOG": "debug" },
      "enabled": true,
      "timeout": 5000
    },
    "docs": {
      "type": "remote",
      "url": "https://mcp.internal/sse",
      "headers": { "Authorization": "Bearer ..." }
    }
  }
}
```

| Field | Applies to | Meaning |
|---|---|---|
| `type` | both | `"local"` or `"remote"`. |
| `command` | local | Argv; element 0 is the executable. |
| `cwd` | local | Working directory; relative paths resolve from the workspace. |
| `environment` | local | Env vars for the child process. |
| `url` | remote | Server URL (streamable HTTP, SSE fallback). |
| `headers` | remote | Extra headers on every request. |
| `oauth` | remote | OAuth config, or `false` to disable auto-detection. |
| `enabled` | both | Default enabled. |
| `timeout` | both | Per-request timeout in ms, default `5000`. |

<!-- src: crates/otto-mcp/src/config.rs:20-67 -->

### `lsp`

`false` disables LSP entirely; `true` or absent enables the built-in defaults; an object supplies per-server overrides.

```json
{
  "lsp": {
    "rust-analyzer": { "command": ["rust-analyzer"], "extensions": [".rs"] },
    "gopls": { "disabled": true }
  }
}
```

An override without a non-empty `command` array is discarded.
<!-- src: crates/otto-lsp/src/config.rs:10-40 -->

### `formatter`

Declared, **not read** — there is no format-on-write path.

## Worked examples

### Minimal Anthropic

```json
{ "model": "anthropic/claude-opus-4-8" }
```

Credentials come from `otto auth` or provider env vars. See [./env.md](./env.md).

### Local model via ollama

```json
{
  "model": "ollama/qwen3:32b",
  "provider": {
    "ollama": {
      "options": { "baseURL": "http://localhost:11434/v1" },
      "models": {
        "qwen3:32b": { "limits": { "context": 32768, "output": 8192 } }
      }
    }
  }
}
```

`ollama` is not a native provider id, so it falls through to the OpenAI-compatible protocol at `{baseURL}/chat/completions`.

**`limits` is not optional in practice.** otto sizes the context window from an embedded models.dev registry, which has no entry for local models. Without a `limits` block the resolved window is unknown, compaction never triggers, and the provider silently truncates the prompt — the run degrades with no error. Set `context` to the window you actually served the model with.
<!-- src: crates/otto-app/src/route_factory.rs:28-33 -->

### Native Anthropic protocol against a litellm gateway

```json
{
  "model": "anthropic/claude-opus-4-8",
  "provider": {
    "anthropic": {
      "options": {
        "baseURL": "https://litellm.internal/v1",
        "apiKey": "sk-litellm-..."
      }
    }
  }
}
```

Keeping the provider id `anthropic` keeps otto's native Anthropic protocol (`/v1/messages`) rather than falling through to OpenAI-compatible. `ModelRef::parse` splits on the first slash only, so gateway-style model ids like `github_copilot/claude-opus-4.8` survive intact as the model portion.

### Project-local overrides with a JSONC file

```jsonc
// <repo-root>/.otto/otto.jsonc — outranks every plain-named config up the tree
{
  "permission_mode": "accept-edits",
  "permission": {
    "bash": { "git push *": "ask", "*": "allow" },
  },
  "retry": { "turn_timeout_seconds": 900 },
}
```

## Regenerating the schema

`schema/config.json` is generated from the Rust `Config` structs and is committed. Regenerate it after any change to those structs:

```bash
cargo run -q -p otto-config --example gen_schema > schema/config.json
```

## See also

- [./env.md](./env.md) — environment variables, including `OTTO_CONFIG`, `OTTO_CONFIG_DIR`, `OTTO_CONFIG_CONTENT`, `OTTO_DISABLE_PROJECT_CONFIG`, `OTTO_LOG`
- [./cli.md](./cli.md) — command-line flags that override config
- [../guide/permissions.md](../guide/permissions.md) — permission rule evaluation
- [../getting-started.md](../getting-started.md)
