# Tools

The 14 built-in tools an otto agent can call, their parameters, the permission each one asks for, and how output is truncated.

All 14 are registered by `ToolRegistry::with_builtins` and the count is pinned by a test.
<!-- src: crates/otto-tools/src/registry.rs:46-63, :314 -->

| id | asks permission | pattern matched against | `always` grant |
|---|---|---|---|
| `apply_patch` | `edit` | each affected repo-relative path | `*` |
| `bash` | `bash` | the whole command string | `{command} *` |
| `edit` | `edit` | repo-relative file path | `*` |
| `glob` | — | (`external_directory` if the search dir escapes the project) | — |
| `grep` | — | (`external_directory` if the search dir escapes the project) | — |
| `invalid` | — | — | — |
| `question` | — | — | — |
| `read` | — | (`external_directory` if the path escapes the project) | — |
| `skill` | `skill` | skill name | the same skill name |
| `task` | — | — | — |
| `todowrite` | `todowrite` | `*` | `*` |
| `webfetch` | `webfetch` | the URL | `*` |
| `websearch` | `websearch` | the query string | `*` |
| `write` | `write` | repo-relative file path | `*` |

Every tool that resolves a filesystem path also calls `assert_external_directory`, which asks `external_directory` with pattern `<parent-dir>/*` when the target escapes the working directory.
<!-- src: crates/otto-tools/src/tools/mod.rs:63-88 -->

## apply_patch

**What it does.** Applies a stripped-down, file-oriented diff (`*** Begin Patch` … `*** End Patch`) that can add, delete, update, and move files in one call. Registered in place of `edit`/`write` for gpt-5-class models.
<!-- src: crates/otto-tools/src/registry.rs:102-115 -->

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `patchText` | string | yes | The full patch text describing all changes |

**Permission asked.** `edit`, with one pattern per affected file (the patch's display paths), `always = ["*"]`. Each target path additionally goes through `assert_external_directory`.
<!-- src: crates/otto-tools/src/tools/apply_patch.rs:211-223, :120-187 -->

**Output limits.** Registry truncation (below). LSP diagnostics are appended after a successful apply when an `LspHandle` is injected.

## bash

**What it does.** Executes a shell command via `sh -c` from the working directory.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `command` | string | yes | The shell command to execute |
| `description` | string | no | Short (5-10 word) description shown to the user |
| `timeout` | number | no | Milliseconds; default 120000, capped at 600000 |
<!-- src: crates/otto-tools/src/tools/bash.rs:34-35, :292-302 -->

**Permission asked.** `bash`, with the pattern set to the **entire command string** — not the program name. `always = ["{command} *"]`, so an "Always" approval grants that exact command plus anything appended to it, and nothing else.
<!-- src: crates/otto-tools/src/tools/bash.rs:311-318 -->

After the `bash` ask, the command string is scanned for path-shaped tokens; any token resolving outside the working directory raises a separate `external_directory` ask (up to 5 candidates), with the derived directory clamped so it can never land above `$HOME`.
<!-- src: crates/otto-tools/src/tools/bash.rs:183, :236-239, :332 -->

## edit

**What it does.** Exact string replacement in a single file. An empty `oldString` means "create this file with `newString` as its contents" and errors if the file already exists.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `filePath` | string | yes | Absolute path to the file to modify |
| `oldString` | string | yes | Text to replace; empty means create-new-file |
| `newString` | string | yes | Replacement text |
| `replaceAll` | boolean | no | Replace every occurrence (default false) |

**Permission asked.** `edit`, pattern = the repo-relative path, `always = ["*"]`. There are two ask sites — the create-new-file branch and the replace branch — and both use the same request shape.
<!-- src: crates/otto-tools/src/tools/edit.rs:633-640, :676-683 -->

Because `always` is `["*"]`, approving "Always" on one file grants `edit` for **every** path for the rest of the session.

**Output limits.** Registry truncation. LSP diagnostics are appended when an `LspHandle` is present.

## glob

**What it does.** Fast filename pattern matching over the tree (`**/*.rs`, `src/**/*.ts`), returning matching file paths.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `pattern` | string | yes | Glob pattern to match paths against |
| `path` | string | no | Directory to search; defaults to the working directory |

**Permission asked.** None of its own. If `path` resolves outside the working directory, `assert_external_directory` asks `external_directory` with pattern `<dir>/*` (kind `directory`, so the glob is derived from the target itself).
<!-- src: crates/otto-tools/src/tools/glob.rs:83 -->

## grep

**What it does.** Regex content search across files, returning paths and line numbers with matching lines.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `pattern` | string | yes | Regex to search file contents for |
| `path` | string | no | Directory to search; defaults to the working directory |
| `include` | string | no | File filter, e.g. `*.js`, `*.{ts,tsx}` |

**Permission asked.** None of its own; `external_directory` when `path` escapes the project.
<!-- src: crates/otto-tools/src/tools/grep.rs:96 -->

## invalid

**What it does.** Internal repair sink, not a user-facing capability. When the model emits a malformed tool call the harness routes it here, and the tool echoes the decoded error back as `The arguments provided to the tool are invalid: {error}` so the model can correct itself. Its description string is literally `Do not use`.
<!-- src: crates/otto-tools/src/tools/invalid.rs:45-55, crates/otto-tools/descriptions/invalid.txt -->

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `tool` | string | yes | The tool whose call failed to decode (ignored in output) |
| `error` | string | yes | The decode error echoed back to the model |

**Permission asked.** None.

## question

**What it does.** Asks the user one or more multiple-choice questions mid-turn and returns the chosen labels as `"{header}: {labels}"` lines.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `questions` | array | yes | List of question objects |
| `questions[].question` | string | yes | The complete question text |
| `questions[].header` | string | yes | Very short label (max 30 chars) |
| `questions[].options` | array | yes | Choices, each `{label, description}` |
| `questions[].multiple` | boolean | no | Allow selecting more than one option |

**Permission asked.** None. The tool goes through `ctx.question.ask`, a separate seam from the permission gate. The `question` permission key exists only in agent rulesets (denied by default, allowed for `build` and `plan`) and is not consulted by the tool itself.
<!-- src: crates/otto-tools/src/tools/question.rs:74, crates/otto-agent/src/builtins.rs:46,87,116 -->

Errors when called with an empty list, when the answer count mismatches, when a selection is out of range, or when a non-`multiple` question gets more than one selection.

## read

**What it does.** Reads a file or directory from the local filesystem, returning up to 2000 lines from the start by default.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `filePath` | string | yes | Absolute path to the file or directory |
| `offset` | number | no | 1-indexed line to start from |
| `limit` | number | no | Max lines to read (default 2000) |

**Permission asked.** None of its own. Reading outside the project asks `external_directory` with pattern `<dir>/*`.
<!-- src: crates/otto-tools/src/tools/read.rs:127 -->

The agent-level `read` ruleset still applies: builtin defaults set `read` to `allow` except `*.env` / `*.env.*`, which are `ask` (see [agents](./agents.md)).

**Output limits.** Own caps before registry truncation: 2000 lines (`DEFAULT_READ_LIMIT`), 2000 chars per line (`MAX_LINE_LENGTH`), 50 KB total (`MAX_BYTES`).
<!-- src: crates/otto-tools/src/tools/read.rs:16-19 -->

## skill

**What it does.** Loads a named skill's instructions into the conversation, optionally only one `##`/`###` section of it, plus a sample of up to 10 sibling files from the skill directory.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `name` | string | yes | Skill name, must match one advertised in the system prompt |
| `section` | string | no | Load only this named heading's section |

**Permission asked.** `skill`, pattern = the skill name, `always` = the same skill name. This is the only tool whose `always` grant is narrow rather than `*`: approving "Always" grants that one skill, not all skills. Unknown skill names error before the ask.
<!-- src: crates/otto-tools/src/tools/skill.rs:260-273 -->

## task

**What it does.** Spawns a subagent session to handle a multi-step task and returns its final text. Requires a `SubagentSpawner` in the tool context; without one it errors.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `description` | string | yes | Short (3-5 word) task description |
| `prompt` | string | yes | The task for the subagent to perform |
| `subagent_type` | string | yes | Agent name, e.g. `general`, `explore` |
| `task_id` | string | no | Resume a previous task's subagent session |
| `command` | string | no | The command that triggered this task |
| `background` | boolean | no | Accepted for parity; ignored — the spawner always runs foreground |
<!-- src: crates/otto-tools/src/tools/task.rs:70-84 -->

**Permission asked.** **None.** The tool does not call the permission gate, and neither does `SessionSubagentSpawner::spawn`. The `task` permission key is used only in ruleset derivation: `derive_subagent_permission` adds `task: "*" -> deny` to a child session unless the subagent's own ruleset mentions `task`.
<!-- src: crates/otto-agent/src/subagent.rs:46-52, crates/otto-session/src/subagent.rs:169-195 -->

The child's tool lifecycle events (`ToolCall`/`ToolResult`/`ToolError`) are forwarded to the parent turn; child prose and finish events are dropped.
<!-- src: crates/otto-tools/src/tools/task.rs:91-110 -->

## todowrite

**What it does.** Creates and replaces the session's structured task list, echoing back a `{n} todos` title.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `todos` | array | yes | The full updated todo list |
| `todos[].id` | string | no | Unique identifier |
| `todos[].content` | string | yes | Brief task description |
| `todos[].status` | enum | yes | `pending` / `in_progress` / `completed` / `cancelled` |
| `todos[].priority` | enum | no | `high` / `medium` / `low` |

**Permission asked.** `todowrite`, pattern `*`, `always = ["*"]` — there is no per-list granularity.
<!-- src: crates/otto-tools/src/tools/todo.rs:98-105 -->

## webfetch

**What it does.** Fetches a URL and converts the response to text, markdown, or HTML.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `url` | string | yes | URL to fetch |
| `format` | enum | no | `text` / `markdown` / `html` (default `markdown`) |
| `timeout` | number | no | Seconds; default 30, capped at 120 |

**Permission asked.** `webfetch`, pattern = the URL, `always = ["*"]`. An "Always" approval therefore grants fetching **any** URL for the session, not just this host. Format is validated before the ask.
<!-- src: crates/otto-tools/src/tools/webfetch.rs:72-90 -->

**Output limits.** Response body capped at 5 MB, then registry truncation.
<!-- src: crates/otto-tools/src/tools/webfetch.rs:17-19 -->

## websearch

**What it does.** Runs a web search through the session's injected search provider. Without a provider the tool errors.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `query` | string | yes | The search query |
| `numResults` | number | no | Result count (default 8) |
| `livecrawl` | enum | no | `fallback` / `preferred` |
| `type` | enum | no | `auto` / `fast` / `deep` |
| `contextMaxCharacters` | number | no | Max chars of LLM-optimized context (default 10000) |

**Permission asked.** `websearch`, pattern = the query string, `always = ["*"]` — "Always" grants all future searches.
<!-- src: crates/otto-tools/src/tools/websearch.rs:111-118 -->

## write

**What it does.** Writes a file, overwriting any existing content and creating parent directories as needed.

**Parameters**

| name | type | required | meaning |
|---|---|---|---|
| `content` | string | yes | Content to write |
| `filePath` | string | yes | Absolute path to write to |

**Permission asked.** `write` — **not** `edit`. The tool asks under its own permission name with pattern = the repo-relative path and `always = ["*"]`.
<!-- src: crates/otto-tools/src/tools/write.rs:69-76 -->

This matters when writing config rules: `edit` rules do **not** gate the `write` tool, while `apply_patch` *does* ask under `edit`. Agent rulesets that deny writes broadly (`"*": "deny"` for `explore`) still cover it, because the deny is on the wildcard rather than on `edit`. The `plan` agent's ruleset denies only `edit`, so its write-blocking relies on the plan prompt and the `edit`/`apply_patch` gates rather than on a `write` rule.
<!-- src: crates/otto-agent/src/builtins.rs:119-122, :182-192 -->

**Output limits.** Registry truncation, plus an appended LSP diagnostics block for the written file and up to 5 other files carrying errors.
<!-- src: crates/otto-tools/src/tools/write.rs:84-90 -->

## Output truncation

Every tool result passes through `truncate_output` in the registry unless the tool already set a `truncated` key in its own metadata (the opt-out path).
<!-- src: crates/otto-tools/src/registry.rs:210-226 -->

| limit | value |
|---|---|
| `MAX_LINES` | 2000 |
| `MAX_BYTES` | 51200 (50 KB) |
| direction | head — the first N lines are kept |
<!-- src: crates/otto-tools/src/truncate.rs:14-17, :57-60 -->

Truncated output gains a `...N lines/bytes truncated...` marker and a hint; the hint text varies depending on whether the `task` tool is registered. `config.tool_output.max_lines` / `max_bytes` exist in the config schema but are **not wired to these constants** — nothing reads them today, so the limits are fixed.
<!-- src: crates/otto-config/src/schema.rs:68-75, :231-233 -->

## Gating

Which tools reach the model is decided in two independent places.

**Model gating.** `tools_for_model` swaps the edit surface by model id: gpt-5-class ids (containing `gpt-` but not `oss` or `gpt-4`) get `apply_patch` and lose `edit`/`write`; every other model gets `edit`/`write` and loses `apply_patch`.
<!-- src: crates/otto-tools/src/registry.rs:102-115 -->

**Config.** `config.tools` is a `{ "<tool-id>": bool }` map in the schema:

```json
{
  "tools": { "websearch": false }
}
```

It is parsed and validated, but no crate currently reads it — the map has no effect on the registry today.
<!-- src: crates/otto-config/src/schema.rs:219-221 -->

**Permissions.** The enforced restriction path is the ruleset layer, not `config.tools`. An agent's ruleset can deny a permission key outright (the `explore` agent's `"*": "deny"` plus re-allows), and a policy deny fails the tool call with a model-facing error rather than prompting a human. See [agents](./agents.md) and [permissions](../guide/permissions.md).

## Known warts

- The `explore` agent's ruleset allows a `list` permission, but **no `list` tool exists** in the registry. The entry is inherited from the upstream opencode agent definition and is inert.
  <!-- src: crates/otto-agent/src/builtins.rs:186, crates/otto-tools/src/registry.rs:296-314 -->
- `invalid` is registered like a normal tool and is therefore advertised to the model, despite being an internal error-repair sink whose description is `Do not use`.
  <!-- src: crates/otto-tools/src/registry.rs:60 -->
- `write` asks the `write` permission while `apply_patch` asks `edit`, so the two write paths are **not gated by the same key**. Any ruleset that means to block file mutation must deny both. This previously let the `plan` agent's `edit` deny be bypassed via the `write` tool; `plan()` now carries both keys, but the asymmetry remains a trap for custom agents, since `defaults()` is `"*": "allow"` and an unlisted permission falls through to allow.
- The `task` permission is never asked, so a ruleset rule like `"task": { "general": "deny" }` does not block the `task` tool; it only flips the subagent-derivation defaults.

## Adjacent machinery

| file | role |
|---|---|
| `crates/otto-tools/src/hooks/rtk.rs` | Rewrites `bash` commands to run through the `rtk` proxy. Only wraps simple single commands — any shell operator (`\|`, `&&`, redirection, subshell) leaves the command untouched. Inert when `rtk` is not on `PATH`. |
| `crates/otto-tools/src/lsp.rs` | The `LspHandle` trait. When injected, `edit`/`write`/`apply_patch` append a `<diagnostics>` block after a successful write, plus up to 5 other files with errors. |
| `crates/otto-tools/src/subagent.rs` | The `SubagentSpawner` / `SubagentRequest` contract the `task` tool depends on; the real implementation is `SessionSubagentSpawner` in `otto-session`. |
| `crates/otto-tools/src/hook_escalation.rs` | Turns a lifecycle-hook `Decision::Ask` verdict into a real interactive permission ask under the `hook` permission, instead of treating it as a deny. |
| `crates/otto-tools/src/registry.rs` | Fires `PreToolUse` / `PostToolUse` lifecycle hooks around every call; a `PostToolUse` system message lands in the result's `hookMessage` metadata. |
