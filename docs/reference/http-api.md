# HTTP API reference

`otto serve` exposes an opencode-compatible HTTP + SSE surface over the shared runtime; the otto TUI is a pure client of it, so this document is also the contract for third-party clients.

## Route index

<!-- src: crates/otto-server/src/lib.rs:182-217 -->

| Method | Path | Area | Summary |
| --- | --- | --- | --- |
| `GET` | `/app` | Meta | Instance info (version, directory) |
| `GET` | `/path` | Meta | Same handler as `/app` |
| `GET` | `/config` | Meta | Loaded config as JSON |
| `PATCH` | `/config` | Meta | Deep-merge a patch, return the merged result (not persisted) |
| `GET` | `/agent` | Meta | Resolved agent set |
| `GET` | `/provider` | Meta | Providers + models + default model |
| `GET` | `/lsp` | Meta | Status of spawned LSP clients |
| `GET` | `/doc` | Meta | Route map (**unauthenticated**) |
| `GET` | `/session` | Session | List sessions |
| `POST` | `/session` | Session | Create a session |
| `GET` | `/session/{id}` | Session | Fetch one session |
| `DELETE` | `/session/{id}` | Session | Delete a session (cascades messages/parts) |
| `GET` | `/session/{id}/message` | Messaging | Full message history with parts |
| `POST` | `/session/{id}/message` | Messaging | Run a turn, streaming `LLMEvent`s over SSE |
| `POST` | `/session/{id}/cancel` | Session | Interrupt the in-flight turn |
| `POST` | `/session/{id}/permission-mode` | Permission | Set the session's live permission mode |
| `GET` | `/event` | Events | Global SSE bus |
| `GET` | `/permission` | Permission | Pending permission asks |
| `POST` | `/permission/{request_id}/reply` | Permission | Resolve a permission ask |
| `GET` | `/question` | Permission | Pending question-tool asks |
| `POST` | `/question/{request_id}/reply` | Permission | Resolve a question-tool ask |
| `GET` | `/find` | Files | Substring content search |
| `GET` | `/find/file` | Files | Filename substring search |
| `GET` | `/file/content` | Files | Read one file |
| `GET` | `/file/list` | Files | Enumerate workspace files and directories |
| `GET` | `/experimental/worktree` | Worktree | List managed worktrees |
| `POST` | `/experimental/worktree` | Worktree | Create a worktree |
| `DELETE` | `/experimental/worktree` | Worktree | Remove a worktree |
| `POST` | `/experimental/worktree/reset` | Worktree | Hard-reset a worktree to origin |
| `POST` | `/workflow/{kind}` | Workflow | Start a `tdd`/`sdd`/`plan` run |
| `POST` | `/workflow/{session}/cancel` | Workflow | Cancel a running workflow |

## Starting the server

```bash
otto serve --port 4096 --hostname 127.0.0.1
otto serve --port 0                        # bind a random free port
otto serve --password hunter2 --cors       # basic auth + permissive CORS
```

<!-- src: crates/otto-cli/src/cli.rs:92-108 -->

| Flag | Default | Effect |
| --- | --- | --- |
| `--port <u16>` | `4096` | Port to bind; `0` selects a random free port |
| `--hostname <str>` | `127.0.0.1` | Interface to bind |
| `--password <str>` | none | Enables the HTTP Basic auth gate |
| `--cors` | off | Installs a permissive `tower-http` CORS layer |

The server prints `otto server listening on http://<addr>` on startup, plus `(basic-auth enabled)` when a password is in effect.

### Password environment variables

Two env vars are honored, from two different layers:

<!-- src: crates/otto-cli/src/cli.rs:101-103 (clap env), crates/otto-cli/src/commands.rs:29,68 (fallback) -->

- `otto_SERVER_PASSWORD` — read by clap's `env` attribute on `--password`. The lowercase-prefixed name is literal, not a rendering artifact of the help text.
- `OTTO_SERVER_PASSWORD` — read by `cmd_serve` as a fallback when `--password` resolved to `None`.

Precedence: `--password` flag, then `otto_SERVER_PASSWORD` (clap), then `OTTO_SERVER_PASSWORD` (fallback). Prefer the uppercase form; it is the one named as a constant in the code.

## Authentication

<!-- src: crates/otto-server/src/lib.rs:228-266 -->

When `ServeOptions.password` is `Some`, an axum middleware gates every route in the protected router. `/doc` is merged in outside that layer and is the only unauthenticated route.

The scheme is HTTP Basic. The username must be `otto` (the sole entry in `AUTH_USERS`); the password must match exactly. A missing header, a non-`Basic` scheme, undecodable base64, non-UTF-8 bytes, or a missing `:` all fail closed.

Failure response:

```http
HTTP/1.1 401 Unauthorized
WWW-Authenticate: Basic realm="otto"
Content-Type: application/json

{"error":{"message":"unauthorized"}}
```

```bash
curl -sS -u otto:hunter2 http://127.0.0.1:4096/session
# equivalently
curl -sS -H "Authorization: Basic $(printf 'otto:hunter2' | base64)" http://127.0.0.1:4096/session
```

With no password configured the gate is a pass-through and credentials are ignored.

## Error shape

<!-- src: crates/otto-server/src/lib.rs:271-323 -->

Every handler error renders as:

```json
{ "error": { "message": "session ses_x not found" } }
```

Status codes in use: `400` (bad request body/params), `404` (unknown session, permission, question, or file), `500` (storage, runtime, serde, or sqlx failure), `401` (auth). Storage/runtime/serde/sqlx errors all map to `500`.

Two prompt-endpoint validation failures deviate from the envelope and return a flat `{"error": "<message>"}` instead: attachment-count overflow and attachment resolution failure. <!-- src: crates/otto-server/src/lib.rs:615-665 -->

## Meta

### `GET /app`, `GET /path`

Both paths route to the same handler. <!-- src: crates/otto-server/src/lib.rs:183-184,332-339 -->

```json
{
  "version": "0.13.1",
  "directory": "/Users/me/project",
  "path": { "directory": "/Users/me/project", "cwd": "/Users/me/project", "root": "/Users/me/project" }
}
```

`200` always.

### `GET /config`

The loaded `otto-config` `Config`, serialized. See [`./config.md`](./config.md) for the field set. `200`, or `500` if serialization fails.

### `PATCH /config`

Request body: any JSON object. It is deep-merged over the loaded config (objects merge key-wise; any other value replaces wholesale) and the merged document is returned.

```bash
curl -sS -u otto:pw -X PATCH http://127.0.0.1:4096/config \
  -H 'content-type: application/json' \
  -d '{"model":"anthropic/claude-sonnet-4-5"}'
```

The runtime's live config is **not** mutated — this endpoint is a preview, not a write. <!-- src: crates/otto-server/src/lib.rs:353-363 -->

### `GET /agent`

Array of resolved agents. <!-- src: crates/otto-agent/src/agent.rs:63-103 -->

```json
[
  {
    "name": "build",
    "description": "Default coding agent",
    "mode": "primary",
    "native": true,
    "hidden": false,
    "permission": { "rules": [] },
    "options": {},
    "steps": 100
  }
]
```

Optional keys omitted when unset: `description`, `topP`, `temperature`, `color`, `model`, `variant`, `prompt`, `steps`.

### `GET /provider`

Providers are derived from the default model plus any agent-pinned models — this is not the full models.dev registry. <!-- src: crates/otto-server/src/lib.rs:372-404 -->

```json
{
  "providers": [
    { "id": "anthropic", "name": "anthropic", "models": [ { "id": "claude-sonnet-4-5", "name": "claude-sonnet-4-5" } ] }
  ],
  "default": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" }
}
```

`models` is an **array** of `{id, name}`, not a map.

### `GET /lsp`

One entry per spawned LSP client; `[]` when none have started. <!-- src: crates/otto-lsp/src/service.rs:48-53 -->

```json
[{ "id": "rust", "name": "rust-analyzer", "root": "/Users/me/project", "status": "ready" }]
```

### `GET /doc`

Unauthenticated. Returns a hand-written OpenAPI-ish route map with `openapi`, `info.title`, `info.version`, and a `paths` object. <!-- src: crates/otto-server/src/lib.rs:1428-1464 -->

```bash
curl -sS http://127.0.0.1:4096/doc      # no credentials needed
```

Treat it as the live source of truth for which routes a given build exposes. It is hand-maintained and currently omits `/lsp`, `/question*`, `/session/{id}/cancel`, `/experimental/worktree*`, and `/workflow/*` — the router is authoritative when the two disagree.

## Session

### `GET /session`

Array of sessions, oldest-created first. Each row is the persisted `Session` plus two computed keys. <!-- src: crates/otto-server/src/lib.rs:420-443 -->

```json
[
  {
    "id": "ses_01H...",
    "project_id": "prj_01H...",
    "parent_id": null,
    "directory": "/Users/me/project",
    "title": "New Session",
    "version": "0.13.1",
    "cost": 0.0132,
    "tokens": { "input": 4210, "output": 388, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
    "metadata": null,
    "time_created": 1750000000000,
    "time_updated": 1750000042000,
    "busy": false,
    "kind": null
  }
]
```

- Session fields serialize flat **snake_case** (no `rename_all`). Clients that expect camelCase will silently read zeros. <!-- src: crates/otto-storage/src/store.rs:61-84 -->
- `busy` — `true` when the session has a registered prompt turn **or** a registered workflow run. A workflow root registers only in the workflow registry, so both must be checked.
- `kind` — lifted from `metadata.kind`: `"subagent"`, `"workflow_task"`, `"workflow_root"`, or `null`.

### `POST /session`

Body is optional; an absent or empty body creates a default session.

```json
{ "title": "Refactor the router", "agent": "build", "parentID": "ses_parent" }
```

| Field | Type | Default |
| --- | --- | --- |
| `title` | string | `"New Session"` |
| `agent` | string | runtime default agent (an unknown name silently falls back to the default) |
| `parentID` | string | none — sets the permission-inheritance parent |

Response `200`: the created `Session` object (same shape as a `GET /session` row, without `busy`/`kind`). Also fans a `session.created` frame onto `/event`. `500` if the session vanishes after creation.

### `GET /session/{id}`

`200` with the `Session`, or `404` `{"error":{"message":"session ses_x not found"}}`.

### `DELETE /session/{id}`

`200` with body `true`. Deleting cascades the session's messages and parts by FK. Deleting an unknown id still returns `true`. <!-- src: crates/otto-server/src/lib.rs:513-522 -->

### `POST /session/{id}/cancel`

No request body. Cancels the in-flight turn's `CancellationToken` without ending the session.

```json
{ "cancelled": true }
```

`false` when the session has no live turn (already finished, or unknown id). `200` in both cases. <!-- src: crates/otto-server/src/lib.rs:1235-1241 -->

## Messaging

### `GET /session/{id}/message`

Array of `WithParts` — a message plus its ordered parts. <!-- src: crates/otto-storage/src/model.rs:749-758,463-475,839-844 -->

```json
[
  {
    "info": {
      "id": "msg_01H...",
      "sessionID": "ses_01H...",
      "role": "user",
      "time": { "created": 1750000000000 },
      "agent": "build",
      "model": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" }
    },
    "parts": [
      { "id": "prt_01H...", "sessionID": "ses_01H...", "messageID": "msg_01H...", "type": "text", "text": "hello" }
    ]
  },
  {
    "info": {
      "id": "msg_01H...",
      "sessionID": "ses_01H...",
      "role": "assistant",
      "parentID": "msg_01H...",
      "modelID": "claude-sonnet-4-5",
      "providerID": "anthropic",
      "mode": "primary",
      "agent": "build",
      "path": { "cwd": "/Users/me/project", "root": "/Users/me/project" },
      "cost": 0.0021,
      "tokens": { "input": 812, "output": 40, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
      "time": { "created": 1750000001000, "completed": 1750000004000 },
      "finish": "stop"
    },
    "parts": [ { "id": "prt_...", "sessionID": "ses_...", "messageID": "msg_...", "type": "step-start" } ]
  }
]
```

Message envelope keys are camelCase (`sessionID`, `messageID`, `parentID`, `modelID`, `providerID`), unlike the snake_case `Session` object. `role` discriminates the `info` body; `type` discriminates each part.

Part `type` values (kebab-case): `text`, `reasoning`, `file`, `tool`, `step-start`, `step-finish`, `snapshot`, `patch`, `agent`, `subtask`, `compaction`, `retry`. <!-- src: crates/otto-storage/src/model.rs:319-450 -->

Diagnostic note: an assistant message with `finish` absent and no `error` was killed mid-turn; `finish: "unknown"` means truncation was accepted after the retry budget ran out.

### `POST /session/{id}/message`

The streaming turn endpoint. Persists the user prompt, starts the run, and streams events until the run's join completes.

Request body:

```json
{
  "prompt": "add a test for the retry path",
  "agent": "build",
  "model": "anthropic/claude-sonnet-4-5",
  "files": [{ "path": "crates/otto-session/src/retry.rs" }]
}
```

| Field | Type | Notes |
| --- | --- | --- |
| `prompt` | string | Preferred form |
| `parts` | array | Alternative to `prompt`: `[{ "type": "text", "text": "…" }]`. Elements with `type: "file"` are dropped; remaining `text` values are concatenated with no separator |
| `agent` | string | Unknown names fall back to the runtime default |
| `model` | string | `provider/model`; split on the **first** slash only, so `github_copilot/claude-opus-4.8` keeps its slash in the model portion |
| `files` | array | `[{ "path": "…" }]`, resolved relative to the runtime directory; max **20** entries |

<!-- src: crates/otto-server/src/lib.rs:527-573,603-666 -->

`prompt` wins when non-empty; otherwise `parts` is flattened. An empty resulting prompt is `400 {"error":{"message":"empty prompt"}}`. An unknown session is `404`. Attachments resolve before any provider call, so a bad path costs no turn: over-limit is `400 {"error":"too many attachments (max 20)"}` and an unreadable/oversized/binary path is `400 {"error":"<detail>"}`.

Text attachments become synthetic text parts; image attachments become `file` parts carrying a data URL.

Response `200` with `content-type: text/event-stream`. Each frame is one serialized `LLMEvent`:

```text
data: {"type":"step-start","index":0}

data: {"type":"text-start","id":"blk_1"}

data: {"type":"text-delta","id":"blk_1","text":"Adding"}

data: {"type":"tool-input-start","id":"call_1","name":"read"}

data: {"type":"tool-result","id":"call_1","name":"read","result":{"type":"text","value":"..."}}

data: {"type":"step-finish","index":0,"reason":"tool-calls"}

data: {"type":"finish","reason":"stop","usage":{"inputTokens":812,"outputTokens":40,"totalTokens":852}}

```

Keep-alive comments are emitted during quiet stretches (tools running, provider mid-generation) so a client idle read-timeout does not kill the socket mid-turn. <!-- src: crates/otto-server/src/lib.rs:733-740 -->

Terminal-frame guarantee: if the run fails without emitting a `finish` (bad model, auth failure, provider error), the server synthesizes a final `{"type":"provider-error","message":"…"}` frame before closing. A client that waits on `finish` alone would otherwise hang forever. <!-- src: crates/otto-server/src/lib.rs:707-719 -->

Concurrency: registering a new turn on a session **cancels any still-running turn on that session** — two concurrent runs would race writes on the same rows. <!-- src: crates/otto-server/src/lib.rs:99-106,668-674 -->

Relationship to `/event`: every frame yielded here is also fanned onto the global bus wrapped as `{"type":"message.part.updated","properties":<the event>}`, bracketed by `session.busy` before the run and `session.idle` after it settles (on every exit path, including cancellation). A client can therefore drive turns purely from `/event` and ignore this response body, or vice versa.

## Events

### `GET /event` (SSE)

The global bus. One connection carries permission asks, question asks, workflow progress, session lifecycle, and fanned-out run events across **all** sessions — there is no per-session filter, so clients filter on `sessionID`/`session` themselves. <!-- src: crates/otto-server/src/lib.rs:752-807 -->

Envelope, uniformly:

```json
{ "type": "<event-name>", "properties": { } }
```

The first frame on connect is always `server.connected`. Keep-alive comments hold the connection open between events. A lagging subscriber's dropped frames are skipped silently (broadcast channel capacity 1024), so a client that stalls can miss events — reconcile with `GET /session` and `GET /permission` after a stall.

| `type` | `properties` | Emitted by |
| --- | --- | --- |
| `server.connected` | `{}` | Connection open |
| `session.created` | `{ sessionID, title, parentID }` | `POST /session` |
| `session.busy` | `{ sessionID }` | Turn start |
| `session.idle` | `{ sessionID }` | Turn settled (success, error, or cancel) |
| `message.part.updated` | the raw `LLMEvent` object | Every frame of a running turn |
| `permission.asked` | `{ id, sessionID, permission, patterns, metadata }` | A tool call needing approval |
| `permission.mode_changed` | `{ sessionID, mode }` | `POST /session/{id}/permission-mode` |
| `question.asked` | `{ id, sessionID, questions[] }` | The question tool |
| `workflow.started` | `{ session, kind, arg }` | `POST /workflow/{kind}` |
| `workflow.progress` | `{ session, kind, task_index, status, notes }` | Live engine progress |
| `workflow.subagent` | `{ session, kind, task_index, verb, detail }` | Coalesced subagent activity |
| `workflow.done` | `{ session, kind, ok, summary, error }` | Run finished; `summary` or `error` is `null` |

Workflow envelopes key the session as `session`; session/permission envelopes key it as `sessionID`. This asymmetry is real, not a typo. <!-- src: crates/otto-server/src/lib.rs:1271-1347 -->

Each `questions[]` element is `{ question, header, options: [{ label, description }], multiple }`. <!-- src: crates/otto-server/src/lib.rs:811-821 -->

#### `LLMEvent` variants

Carried inside `message.part.updated.properties`, and the frame body of `POST /session/{id}/message`. Tagged by `type`, kebab-case. <!-- src: crates/otto-events/src/event.rs:96-381 -->

| `type` | Fields |
| --- | --- |
| `step-start` | `index` |
| `text-start` / `text-end` | `id`, `providerMetadata?` |
| `text-delta` | `id`, `text`, `providerMetadata?` |
| `reasoning-start` / `reasoning-end` | `id`, `providerMetadata?` |
| `reasoning-delta` | `id`, `text`, `providerMetadata?` |
| `tool-input-start` / `tool-input-end` | `id`, `name`, `providerMetadata?` |
| `tool-input-delta` | `id`, `name`, `text` (no `providerMetadata`) |
| `tool-call` | `id`, `name`, `input`, `providerExecuted?`, `providerMetadata?` |
| `tool-result` | `id`, `name`, `result`, `output?`, `providerExecuted?`, `providerMetadata?` |
| `tool-error` | `id`, `name`, `message`, `error?`, `providerMetadata?` |
| `step-finish` | `index`, `reason`, `usage?`, `providerMetadata?` |
| `finish` | `reason`, `usage?`, `providerMetadata?` |
| `provider-error` | `message`, `classification?`, `retryable?`, `providerMetadata?` |
| `retry` | `attempt`, `max`, `delay_ms`, `message`, `salvaged?` |
| `warning` | `message` |

`reason` ∈ `stop`, `length`, `tool-calls`, `content-filter`, `error`, `unknown`. `classification` is currently only `context-overflow`.

`result` is tagged by its own `type`: `json`/`text`/`error` carry `value` (arbitrary JSON), `content` carries `value` as an array of content blocks.

`usage` uses camelCase keys, all optional: `inputTokens`, `outputTokens`, `nonCachedInputTokens`, `cacheReadInputTokens`, `cacheWriteInputTokens`, `reasoningTokens`, `totalTokens`, `providerMetadata`. Inclusive totals and a non-overlapping breakdown are both present: `nonCached + cacheRead + cacheWrite == inputTokens`, and `reasoningTokens <= outputTokens`. <!-- src: crates/otto-events/src/usage.rs:40-67 -->

Two client rules:

- **Treat `LLMEvent` as forward-compatible.** New fields are added with `#[serde(default)]`, and optional fields are omitted from the wire rather than sent as `null`. Ignore unknown fields; do not fail on a `type` you do not recognize.
- **Do not roll back the transcript on a salvaged retry.** `retry` with `salvaged: true` means the failed attempt's completed tool work was kept and the turn continues as a new step. Only `salvaged: false` implies the attempt is being replayed from scratch. Note that `salvaged` is omitted from the wire when `false`.

Example stream:

```text
data: {"type":"server.connected","properties":{}}

data: {"type":"session.busy","properties":{"sessionID":"ses_01H"}}

data: {"type":"message.part.updated","properties":{"type":"step-start","index":0}}

data: {"type":"message.part.updated","properties":{"type":"text-delta","id":"b1","text":"Reading "}}

data: {"type":"permission.asked","properties":{"id":"req_01H","sessionID":"ses_01H","permission":"bash","patterns":["cargo test"],"metadata":{"command":"cargo test"}}}

data: {"type":"message.part.updated","properties":{"type":"tool-result","id":"c1","name":"bash","result":{"type":"text","value":"ok"}}}

data: {"type":"message.part.updated","properties":{"type":"finish","reason":"stop","usage":{"inputTokens":812,"outputTokens":40}}}

data: {"type":"session.idle","properties":{"sessionID":"ses_01H"}}

```

## Permission and question

See [`../guide/permissions.md`](../guide/permissions.md) for how rules resolve.

### Permission flow

1. A tool call requires approval. The runtime blocks the turn and broadcasts `permission.asked` on `/event` with a `properties.id` (the request id).
2. A client that connected late, or reconnected, calls `GET /permission` to recover the pending set — the ask is not replayed on reconnect.
3. The client posts `POST /permission/{request_id}/reply` with a wire reply value.
4. The blocked tool call resumes; the turn's SSE stream continues emitting frames.

A blocked turn stays blocked until a reply arrives. Any in-process driver that runs sessions without a responder will hang silently on the first ask.

### `GET /permission`

<!-- src: crates/otto-server/src/lib.rs:826-843 -->

```json
[
  {
    "id": "req_01H...",
    "sessionID": "ses_01H...",
    "permission": "bash",
    "patterns": ["cargo test --workspace"],
    "metadata": { "command": "cargo test --workspace" }
  }
]
```

`metadata` is free-form (diff, filepath, command, …). `200` always; `[]` when nothing is pending.

### `POST /permission/{request_id}/reply`

<!-- src: crates/otto-server/src/lib.rs:846-875; otto-permission Reply enum -->

```json
{ "reply": "once" }
{ "reply": "always" }
{ "reply": "reject", "message": "not on main" }
```

| `reply` | Effect |
| --- | --- |
| `once` | Approve this call only |
| `always` | Approve and add an in-session Always rule (outranks the agent ruleset, loses to danger rules) |
| `reject` | Deny; the optional `message` is surfaced to the model |

`message` is only meaningful with `reject`. Response `200` body `true`. `400` for an unrecognized `reply` string, `404` if the request id is unknown or already resolved.

```bash
curl -sS -u otto:pw -X POST http://127.0.0.1:4096/permission/req_01H/reply \
  -H 'content-type: application/json' -d '{"reply":"once"}'
```

### `GET /question`

Same shape as `/permission` but carrying prompts:

```json
[
  {
    "id": "qst_01H...",
    "sessionID": "ses_01H...",
    "questions": [
      {
        "question": "Which database?",
        "header": "Setup",
        "options": [{ "label": "sqlite", "description": "embedded" }, { "label": "postgres", "description": null }],
        "multiple": false
      }
    ]
  }
]
```

### `POST /question/{request_id}/reply`

<!-- src: crates/otto-server/src/lib.rs:897-924 -->

```json
{ "reply": "answered", "answers": [[0], [1, 2]] }
{ "reply": "cancelled" }
```

`answers` is an array-per-question of selected zero-based option indices; an inner array may hold multiple entries only when that question's `multiple` is `true`. It defaults to `[]` when omitted. `200` body `true`; `400` for an unknown `reply`; `404` for an unknown request id.

### `POST /session/{id}/permission-mode`

<!-- src: crates/otto-server/src/lib.rs:941-968; crates/otto-permission/src/mode.rs:42-49 -->

```json
{ "mode": "full-auto" }
```

`mode` ∈ `approve-each`, `accept-edits`, `full-auto`. `200` body `true`, and a `permission.mode_changed` frame is fanned onto `/event`. `400` for any other string.

Mode resolves live up the parent chain, so setting it on a chat session also applies to workflow sessions and subagents parented to it.

## Files and search

Both search routes walk the instance directory directly (no ripgrep), skipping `.git`, `target`, `node_modules`, `.svn`, `.hg`, descending at most 12 levels, and capping results at 200. <!-- src: crates/otto-server/src/find.rs:13-46 -->

### `GET /find?pattern=<substring>`

Plain substring match against file lines — not a regex.

```json
[{ "path": "/Users/me/project/src/lib.rs", "line": 42, "text": "fn main() {" }]
```

An empty `pattern` returns `[]`. `200` always.

### `GET /find/file?query=<substring>`

Substring match against the file **name** only. Returns an array of absolute path strings. An empty `query` matches everything (still capped at 200). `200` always.

### `GET /file/content?path=<path>`

`path` may be absolute or relative to the instance directory.

```json
{ "path": "/Users/me/project/src/lib.rs", "content": "..." }
```

`404` when the path cannot be read. Non-UTF-8 files fail to read. `path` is required; omitting it is a `400` from query extraction.

### `GET /file/list?limit=<n>`

<!-- src: crates/otto-server/src/lib.rs:1039-1047 -->

```json
{ "files": ["src/main.rs", "Cargo.toml"], "dirs": ["src", "crates/otto-tui"], "truncated": false }
```

Paths are repo-relative. `limit` defaults to `1000` and is clamped to `1..=5000`. `dirs` entries carry **no** trailing slash — the TUI appends one client-side to mark directories. `truncated` is `true` when either list hit the limit.

## Workflow

See [`../guide/workflows.md`](../guide/workflows.md) for what each engine does.

### `POST /workflow/{kind}`

`kind` ∈ `tdd`, `sdd`, `plan`. Anything else is `400 unknown workflow kind: <kind>`.

```json
{ "arg": ".otto/plans/refactor.md", "parent": "ses_01H..." }
```

| Field | Type | Notes |
| --- | --- | --- |
| `arg` | string, required | Plan file path for `sdd`/`plan`; a feature description for `tdd` |
| `parent` | string, optional | Parent session id |

`parent` links the new workflow session into the permission service's chain, so the workflow **and every subagent under it** inherit the caller's permission mode live. Launching from a TUI session in full-auto with `parent` set means the whole run is full-auto; omitting `parent` means it is not. <!-- src: crates/otto-server/src/lib.rs:1129-1138,1155-1164 -->

Returns immediately — the engine runs detached:

```json
{ "session": "ses_01H..." }
```

The new session is tagged `metadata.kind = "workflow_root"` (surfacing as `kind` on `GET /session`). Progress arrives only on `/event` as `workflow.started` → `workflow.progress`* / `workflow.subagent`* → `workflow.done`.

Failure modes: `500` if the subagent spawner cannot be built or the working directory is not a git repository. For `sdd`/`plan`, an unreadable plan file or a plan with no `### Task N:` headings is not a `400` — it surfaces asynchronously as `workflow.done` with `ok: false`. <!-- src: crates/otto-server/src/lib.rs:1414-1423 -->

### `POST /workflow/{session}/cancel`

Path segment is the workflow's **session id**, not the kind. No request body.

```json
{ "cancelled": true }
```

`false` when no run is registered for that session. `200` in both cases.

## Worktree

All four routes require the instance directory to be a git repository; otherwise `400 worktree requires a git repository`. Other git failures are `500`. Worktrees are created under `<global data dir>/worktree/<project-slug>`. <!-- src: crates/otto-server/src/lib.rs:1055-1118 -->

### `GET /experimental/worktree`

Array of absolute directory path strings.

```json
["/Users/me/Library/Application Support/otto/worktree/project/feature-x"]
```

### `POST /experimental/worktree`

Body optional.

```json
{ "name": "feature x" }
```

`name` is slugified for both the branch and the directory; absent or empty-after-slugify defaults to `workspace`. Response:

```json
{ "name": "feature-x", "branch": "feature-x", "directory": "/Users/.../worktree/project/feature-x" }
```

`branch` is `null` for a detached HEAD.

### `DELETE /experimental/worktree`

Body required:

```json
{ "directory": "/Users/.../worktree/project/feature-x" }
```

Response `200` body `true`/`false`.

### `POST /experimental/worktree/reset`

Body required, same `{ "directory": "…" }` shape. Hard-resets the worktree to origin. Response `200` body `true`/`false`.

## Worked walkthrough

```bash
# 1. Start the server.
export OTTO_SERVER_PASSWORD=hunter2
otto serve --port 4096 --hostname 127.0.0.1 &
# → otto server listening on http://127.0.0.1:4096
# → (basic-auth enabled)

# 2. Confirm it is up (no credentials needed for /doc).
curl -sS http://127.0.0.1:4096/doc | head -c 120

# 3. Open the global event stream in a second terminal, before prompting.
curl -sS -N -u otto:hunter2 http://127.0.0.1:4096/event

# 4. Create a session and capture its id.
SES=$(curl -sS -u otto:hunter2 -X POST http://127.0.0.1:4096/session \
  -H 'content-type: application/json' \
  -d '{"title":"api walkthrough"}' | jq -r .id)
echo "$SES"

# 5. Send a message; the response body is the SSE turn stream.
curl -sS -N -u otto:hunter2 -X POST "http://127.0.0.1:4096/session/$SES/message" \
  -H 'content-type: application/json' \
  -d '{"prompt":"list the crates in this workspace","agent":"build"}'

# 6. If the turn blocks on a permission ask, approve it from a third terminal.
REQ=$(curl -sS -u otto:hunter2 http://127.0.0.1:4096/permission | jq -r '.[0].id')
curl -sS -u otto:hunter2 -X POST "http://127.0.0.1:4096/permission/$REQ/reply" \
  -H 'content-type: application/json' -d '{"reply":"once"}'

# 7. Interrupt a long turn without ending the session.
curl -sS -u otto:hunter2 -X POST "http://127.0.0.1:4096/session/$SES/cancel"

# 8. Read the persisted transcript once the turn settles.
curl -sS -u otto:hunter2 "http://127.0.0.1:4096/session/$SES/message" | jq '.[].info.role'
```

Step 3 before step 5 matters: `permission.asked` is broadcast, not replayed. A client that connects after the ask must recover it from `GET /permission`.
