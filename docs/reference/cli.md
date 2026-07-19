# CLI Reference

Every `otto` command and flag, derived from the clap tree in `crates/otto-cli/src/cli.rs`.

For prose and worked examples see [`../guide/cli.md`](../guide/cli.md). For environment variables see [`./env.md`](./env.md).

## Global flags

Available on every subcommand (`global = true`).
<!-- src: crates/otto-cli/src/cli.rs:20-36 -->

| Flag | Type | Default | Help |
| --- | --- | --- | --- |
| `--cwd <CWD>` | path | `.` | "Working directory the runtime is rooted at." |
| `--log-level <LOG_LEVEL>` | string | none | "Log verbosity (e.g. `debug`, `info`, `warn`, `error`)." |
| `--print-logs` | flag | `false` | "Print logs to stderr instead of a log file." |

`OTTO_LOG` overrides `--log-level`. See [`./env.md`](./env.md#server-and-logging).

## `otto run`

"Run a single agent turn against a prompt."
<!-- src: crates/otto-cli/src/cli.rs:63-88 -->

| Arg / flag | Type | Default | Help |
| --- | --- | --- | --- |
| `[MESSAGE]...` | positional, variadic | read from stdin | "The prompt. Multiple words are joined; if omitted, read from stdin." |
| `--agent <AGENT>` | string | configured default agent | "Run as this agent (defaults to the configured default agent)." |
| `--model <MODEL>` | string | configured model | "Generate with this `provider/model` (defaults to the configured model)." |
| `--session <SESSION>` | string | new session | "Continue the session with this id." |
| `--continue` | flag | `false` | "Continue the most recent session in this directory." |
| `--yes` | flag | `false` | "Auto-allow permission requests when running non-interactively." |

The struct field is `continue_`; the flag on the command line is `--continue`.
<!-- src: crates/otto-cli/src/cli.rs:82-83 -->

## `otto serve`

"Serve the HTTP + SSE API."
<!-- src: crates/otto-cli/src/cli.rs:90-108 -->

| Flag | Type | Default | Help |
| --- | --- | --- | --- |
| `--port <PORT>` | u16 | `4096` | "Port to bind (0 selects a random free port)." |
| `--hostname <HOSTNAME>` | string | `127.0.0.1` | "Hostname / interface to bind." |
| `--password <PASSWORD>` | string | none | "Basic-auth password gate (also read from `otto_SERVER_PASSWORD`)." |
| `--cors` | flag | `false` | "Enable permissive CORS." |

`--password` has a clap env fallback of `otto_SERVER_PASSWORD` (lowercase); the dispatcher additionally falls back to `OTTO_SERVER_PASSWORD`. Both work — see [`./env.md`](./env.md#known-wart--inconsistent-casing).
<!-- src: crates/otto-cli/src/cli.rs:102; crates/otto-cli/src/commands.rs:29,66 -->

## `otto models`

"List available models."
<!-- src: crates/otto-cli/src/cli.rs:110-119 -->

| Arg / flag | Type | Default | Help |
| --- | --- | --- | --- |
| `[PROVIDER]` | positional | all providers | "Only list models from this provider." |
| `--refresh` | flag | `false` | "Force a fresh fetch of the models.dev registry before listing." |

## `otto providers`

"Manage providers and their credentials." Requires a subcommand.
<!-- src: crates/otto-cli/src/cli.rs:121-144 -->

| Subcommand | Positional | Required | Help |
| --- | --- | --- | --- |
| `list` | — | — | "List configured providers and whether credentials are present." |
| `login <PROVIDER>` | `provider: String` | yes | "Log in to a provider (API key or, for anthropic, OAuth)." Arg: "The provider id (e.g. `anthropic`, `openai`)." |
| `logout <PROVIDER>` | `provider: String` | yes | "Remove a provider's stored credentials." Arg: "The provider id." |

`login` also takes `--enterprise <DOMAIN>` — "GitHub Enterprise domain for `github-copilot` (e.g. `acme.ghe.com`). Routes API calls to `https://copilot-api.<domain>` instead of the public host. Ignored by other providers." See [../guide/providers.md](../guide/providers.md#enterprise).

## `otto auth`

"Manage authentication credentials (alias of `providers`)." Requires a subcommand.
<!-- src: crates/otto-cli/src/cli.rs:146-169 -->

| Subcommand | Positional | Required | Help |
| --- | --- | --- | --- |
| `list` | — | — | "List stored credentials (redacted)." |
| `login <PROVIDER>` | `provider: String` | yes | "Log in to a provider." Arg: "The provider id." Also takes `--enterprise <DOMAIN>`. |
| `logout <PROVIDER>` | `provider: String` | yes | "Remove a provider's stored credentials." Arg: "The provider id." |

`AuthCommand` is a distinct enum from `ProvidersCommand` with its own help strings — `auth list` documents itself as listing stored credentials, `providers list` as listing configured providers.
<!-- src: crates/otto-cli/src/cli.rs:131-133,156-158 -->

## `otto agent`

"Inspect agents." Requires a subcommand.
<!-- src: crates/otto-cli/src/cli.rs:171-184 -->

| Subcommand | Flags | Help |
| --- | --- | --- |
| `list` | none | "List resolved agents." |

## `otto mcp`

"Inspect MCP servers." Requires a subcommand.
<!-- src: crates/otto-cli/src/cli.rs:186-199 -->

| Subcommand | Flags | Help |
| --- | --- | --- |
| `list` | none | "List configured MCP servers and their connection status." |

## `otto worktree`

"Manage git worktrees (isolated agent workspaces)." Requires a subcommand.
<!-- src: crates/otto-cli/src/cli.rs:201-207,223-244 -->

| Subcommand | Arg / flag | Type | Required | Help |
| --- | --- | --- | --- | --- |
| `list` | — | — | — | "List managed worktrees." |
| `create` | `--name <NAME>` | string | no | "Create a new worktree on a `otto/<name>` branch." Flag: "A name for the worktree (slugified). Defaults to `workspace`." |
| `remove` | `<DIRECTORY>` | positional | yes | "Remove a worktree and delete its branch." Arg: "Absolute path of the worktree directory." |
| `reset` | `<DIRECTORY>` | positional | yes | "Hard-reset a worktree to the origin default branch." Arg: "Absolute path of the worktree directory." |

`--name` on `create` is a named flag, not a positional.
<!-- src: crates/otto-cli/src/cli.rs:230-232 -->

## `otto tui`

"Launch the terminal UI." Auto-spawns a server unless `--server` is given.
<!-- src: crates/otto-cli/src/cli.rs:209-221 -->

| Flag | Type | Default | Help |
| --- | --- | --- | --- |
| `--server <SERVER>` | string | auto-spawn | "Attach to an already-running server instead of auto-spawning one." |
| `--password <PASSWORD>` | string | none | "Basic-auth password for `--server`." |
| `--no-splash` | flag | `false` | "Skip the startup splash screen." |

`otto tui --password` has **no** env fallback, unlike `otto serve --password`. `otto_NO_SPLASH` is an env equivalent of `--no-splash`.
<!-- src: crates/otto-cli/src/cli.rs:216-217; crates/otto-tui/src/lib.rs:246 -->

## `otto workflow`

"Run a native dev-loop workflow (TDD / SDD / plan execution)." Requires a subcommand.
<!-- src: crates/otto-cli/src/cli.rs:246-299 -->

All three subcommands share `--dry-run` and `--auto`/`-y`; they differ only in the required target flag.

### `otto workflow tdd`

"Native test-driven-development cycle (Phase 3)."

| Flag | Type | Required | Help |
| --- | --- | --- | --- |
| `--feature <FEATURE>` | string | **yes** | "The feature to drive a TDD cycle for." |
| `--dry-run` | flag | no | "Preview only: print what would run, dispatch no subagents, leave the working tree untouched." |
| `--auto`, `-y` | flag | no | "Full-auto permission mode: subagent tool calls run without prompting (dangerous patterns still ask)." |

### `otto workflow sdd`

"Native subagent-driven-development orchestration (Phase 4)."

| Flag | Type | Required | Help |
| --- | --- | --- | --- |
| `--plan <PLAN>` | string | **yes** | "Path to the plan file whose `### Task N` sections drive the run." |
| `--dry-run` | flag | no | "Preview only: parse the plan + print the task list, dispatch no subagents, leave the working tree untouched." |
| `--auto`, `-y` | flag | no | "Full-auto permission mode: subagent tool calls run without prompting (dangerous patterns still ask)." |

### `otto workflow plan`

"Plan-execution + verification gate (Phase 5)."

| Flag | Type | Required | Help |
| --- | --- | --- | --- |
| `--plan <PLAN>` | string | **yes** | "Path to the plan file whose `### Task N` sections are executed in order." |
| `--dry-run` | flag | no | "Preview only: parse the plan + print the task list and the verification commands, dispatch no subagents, leave the tree untouched." |
| `--auto`, `-y` | flag | no | "Full-auto permission mode: subagent tool calls run without prompting (dangerous patterns still ask)." |

`--plan` and `--feature` are `String`, not `PathBuf` — clap performs no path validation.
<!-- src: crates/otto-cli/src/cli.rs:260,275,289 -->
