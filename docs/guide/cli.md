# CLI Guide

How to drive otto from the terminal — `otto run`, `otto tui`, `otto serve`, `otto workflow`, `otto worktree`, and the introspection commands.

For the terse per-flag tables, see [the CLI reference](../reference/cli.md). This page is about usage patterns.

## Global flags

These are `global = true` in clap, so they may appear before or after the subcommand.

| Flag | Default | Effect |
| --- | --- | --- |
| `--cwd <PATH>` | `.` | Working directory the runtime is rooted at (config discovery, session `directory`, git root). |
| `--log-level <LEVEL>` | — | `debug` \| `info` \| `warn` \| `error`. Anything that doesn't parse as a level degrades to `warn`. <!-- src: crates/otto-cli/src/logging.rs:92 --> |
| `--print-logs` | off | Write logs to stderr instead of the rolling log file. |

```bash
otto --cwd ~/code/myapp run "summarize the test suite"
otto run --print-logs --log-level debug "why did the build break?"
```

Filter precedence is `OTTO_LOG` > `--log-level` > config `logLevel` > `warn`. <!-- src: crates/otto-cli/src/logging.rs:62-97 -->

`OTTO_LOG` takes full `EnvFilter` directive syntax, not just a bare level, and beats both `--log-level` and the config file:

```bash
OTTO_LOG=otto_session=debug,otto_llm=trace otto run "..."
```

An unparseable `OTTO_LOG` value is ignored and the next source in the chain applies.

## `otto run`

One non-interactive agent turn, streamed to stdout.

```bash
# One-shot: message words are joined with spaces.
otto run "add a test for ModelRef::parse"

# Piped stdin: used when no message args are given and stdin is not a TTY.
git diff | otto run
cat spec.md | otto run
```

An empty prompt (no args, TTY stdin) is an error — pass a message or pipe one. <!-- src: crates/otto-cli/src/run.rs:386-398 -->

### Continuing a session

```bash
# Resume the newest session rooted in this directory (falls back to the
# newest session overall if this directory has none).
otto run --continue "now wire it into the CLI"

# Resume an explicit session id (validated to exist; errors if not found).
otto run --session ses_01J... "keep going"
```

<!-- src: crates/otto-cli/src/run.rs:403-432 -->

### Model and agent

```bash
otto run --model anthropic/claude-opus-4-8 "..."
otto run --agent plan "draft an approach for the retry rework"
```

`--model` is `provider/model`, split on the **first** slash only, so gateway ids like `github_copilot/claude-opus-4.8` survive intact as the model portion. `--agent` must name a resolved agent (see `otto agent list`); an unknown name is an error. Both fall back to the runtime defaults when omitted. <!-- src: crates/otto-cli/src/run.rs:322-337 -->

### `--yes` and permissions

`otto run` installs a TTY permission responder:

- **stdin is a TTY** — every permission ask prompts on stderr with `[y]es / [n]o / [a]lways`. `--yes` has no effect here; you are still asked.
- **stdin is not a TTY** (piped, CI) — no prompt is possible, so a fixed policy applies: with `--yes` every ask is answered *allow-once*; without it every ask is rejected with `permission auto-rejected (non-interactive; pass --yes to allow)`.

<!-- src: crates/otto-cli/src/run.rs:216-252 -->

> [!WARNING]
> The built-in danger ruleset is the highest-precedence permission layer and always produces an *ask*, even in full-auto mode. <!-- src: crates/otto-permission/src/permission.rs:298 --> That ask is still routed to the responder — so under `--yes` with non-interactive stdin, dangerous operations are auto-approved along with everything else. Use `--yes` only where that is acceptable.

See [permissions](./permissions.md) for the full layering rules.

The question tool has no `--yes` analog: non-interactive stdin always cancels a question rather than guessing an answer. <!-- src: crates/otto-cli/src/run.rs:267-270 -->

Ctrl-C cancels the run. <!-- src: crates/otto-cli/src/run.rs:349-355 -->

## `otto tui`

```bash
# Auto-spawns a local `otto serve` and attaches to it.
otto tui

# Attach to an already-running server instead.
otto tui --server http://127.0.0.1:4096
otto tui --server https://box.internal:4096 --password hunter2

# Skip the startup splash.
otto tui --no-splash
```

When `--server` is absent, the TUI spawns a local server process and uses its base URL; with `--server` it attaches to that URL and spawns nothing. `--password` supplies basic-auth credentials for the attached server. <!-- src: crates/otto-tui/src/lib.rs:69-76 -->

See [the TUI guide](./tui.md) for the layout, the complete keymap, and the overlays.

## `otto serve`

The opencode-compatible HTTP + SSE API.

```bash
otto serve                                   # http://127.0.0.1:4096
otto serve --port 0                          # bind a random free port
otto serve --hostname 0.0.0.0 --port 8080 --password hunter2 --cors
```

| Flag | Default | Notes |
| --- | --- | --- |
| `--port` | `4096` | `0` selects a random free port. |
| `--hostname` | `127.0.0.1` | Resolved with `hostname:port`; a name that does not resolve is an error. |
| `--password` | — | Basic-auth gate. Also read from the environment (see below). |
| `--cors` | off | Permissive CORS. |

The password falls back to the environment when `--password` is omitted. Two names are honoured: clap reads `otto_SERVER_PASSWORD` and the handler additionally falls back to `OTTO_SERVER_PASSWORD`. <!-- src: crates/otto-cli/src/cli.rs:102, crates/otto-cli/src/commands.rs:29,66 -->

The bind URL is printed on startup, and `(basic-auth enabled)` when a password is set.

See [the HTTP API reference](../reference/http-api.md) for endpoints.

## `otto workflow`

Native dev-loop engines. See [workflows](./workflows.md) for what each one actually does.

| Subcommand | Required arg | Purpose |
| --- | --- | --- |
| `otto workflow tdd` | `--feature <TEXT>` | Drive a TDD cycle for a feature. |
| `otto workflow sdd` | `--plan <FILE>` | Subagent-driven development over a plan file's `### Task N:` sections. |
| `otto workflow plan` | `--plan <FILE>` | Execute a plan file in order, then run the verification gate. |

All three take `--dry-run` and `--auto` (short `-y`).

Preview first, then commit to a real run:

```bash
# 1. Dry run: parse the plan, print the task list (and, for `plan`, the
#    verification commands). No subagents dispatched, working tree untouched.
otto workflow sdd --plan docs/plans/retry.md --dry-run

# 2. Real run, full-auto so subagent tool calls don't stop for each prompt.
otto workflow sdd --plan docs/plans/retry.md --auto
```

`--auto` puts the workflow session into full-auto permission mode, which children inherit through the parent chain; danger patterns still ask. Without `--auto`, a TTY prompts for each ask and a non-TTY stdin rejects them with a hint to pass `--auto`. <!-- src: crates/otto-cli/src/workflow.rs:79-104 -->

Ctrl-C cancels an in-flight workflow — the same token is handed to the engine, so it stops dispatching rather than merely tearing down the permission pump. <!-- src: crates/otto-cli/src/workflow.rs:91-96 -->

## `otto worktree`

Isolated git worktrees for agent work, created under `{data_dir}/otto/worktree/<project-slug>/`. Requires the current directory to be inside a git repository.

```bash
otto worktree list
otto worktree create --name retry-rework
otto worktree remove /Users/me/Library/Application\ Support/otto/worktree/myapp/retry-rework
otto worktree reset  /Users/me/Library/Application\ Support/otto/worktree/myapp/retry-rework
```

Details that bite:

- **`create --name` is a flag, not a positional.** `otto worktree create retry-rework` will not parse. The name is slugified and defaults to `workspace`. <!-- src: crates/otto-cli/src/cli.rs:229-233 -->
- **Branch naming is `otto/<slug>`.** If the directory or branch is taken, otto appends `-2`, `-3`, … up to 50 before giving up. <!-- src: crates/otto-vcs/src/worktree.rs:134-164 -->
- **`remove` and `reset` take a directory, not a name**, and the listing prints the absolute path you should pass. `remove` force-removes the worktree and best-effort deletes its `otto/<slug>` branch.
- **`reset` refuses unmanaged directories.** It hard-resets to `origin/<default-branch>` and runs `git clean -ffdx`, so it checks the directory is a worktree otto manages *before* touching git at all; anything else fails with `not a managed worktree: <dir>`. <!-- src: crates/otto-vcs/src/worktree.rs:268-288 -->
- `list` excludes the primary repository checkout.

## Introspection

```bash
otto models                    # every model in the installed registry
otto models anthropic          # filter by provider id (positional)
otto models --refresh          # fetch models.dev first, then list

otto providers list            # provider ids + credential status
otto auth list                 # identical output — `auth` shares the implementation

otto agent list                # name, mode (primary/subagent/all), description
otto mcp list                  # configured MCP servers, each probed for status
```

`otto models` prints `provider/model  context=<N>k  [caps]  $in/$out`, where caps are drawn from `tools`, `reasoning`, `vision` and the cost hint is per-Mtok and omitted for models with no published pricing. Unknown context windows render `context=?`. <!-- src: crates/otto-cli/src/commands.rs:90-131 -->

`--refresh` re-fetches the models.dev registry into the cache and installs it before listing; without it the currently installed (embedded or cached) snapshot is used. <!-- src: crates/otto-cli/src/lib.rs:43-50 -->

`otto mcp list` actually connects to each configured server to report status; a failure is reported inline as `Failed(<err>)` and never aborts the listing.

## Auth

```bash
otto auth login anthropic       # choose [1] API key or [2] OAuth (Claude Pro/Max)
otto auth login github-copilot  # device-code flow, polled to completion
otto auth login openai          # plain API key prompt
otto auth logout openai
```

`otto providers login|logout` are the same commands under a different name.

Login **requires an interactive terminal** — it bails with `login requires an interactive terminal` when stdin is not a TTY, so it cannot be scripted. Set provider credentials via config or environment for non-interactive use. <!-- src: crates/otto-cli/src/commands.rs:214-217 -->

See [providers](./providers.md) for per-provider setup, gateways, and OpenAI-compatible endpoints.

## Debugging a run

### Logs

The default sink is a daily-rolling file under the global data directory:

```text
{data_dir}/otto/logs/otto.log.YYYY-MM-DD
```

On macOS that is `~/Library/Application Support/otto/logs/`. <!-- src: crates/otto-cli/src/logging.rs:45-49 -->

The file sink (rather than stderr) is the default because the TUI's alternate screen would garble stderr output. Pass `--print-logs` to redirect to stderr for `otto run` / `otto serve`, and raise the level with `OTTO_LOG`:

```bash
OTTO_LOG=otto_session=debug,otto_llm=debug otto run --print-logs "..."
```

Retries, salvage decisions, exhaustion, accepted truncation, and skipped stream frames are all logged.

Logging never blocks a command: an unwritable log directory or a double init degrades to no logging rather than failing.

### Ground truth in SQLite

The session store is `{data_dir}/otto/otto.db`. The last assistant message's `error` and `finish` fields say what happened to a stopped turn:

```bash
sqlite3 ~/Library/Application\ Support/otto/otto.db \
  "select json_extract(data,'\$.error'), json_extract(data,'\$.finish') from message order by id desc limit 5;"
```

| Reading | Meaning |
| --- | --- |
| `error` non-null | Provider failure text — the turn failed with that error. |
| `finish = "unknown"` | Accepted truncation: the stream ended with no finish reason and the retry budget was exhausted, so otto accepted it with a warning. |
| `finish` missing **and** `error` null | The turn was killed mid-stream (process died, Ctrl-C, host went away). |
| `finish = "tool-calls"` | Normal intermediate step — the run loop continues. |

In the TUI, fatal errors stay in the scrollback as red lines and warnings as dim `⚠` lines; the header status alone is transient.
