# Architecture

How otto is put together: the crate layering, the turn pipeline that spans most of it, and the invariants that keep the pipeline correct.

The repo root's [`CLAUDE.md`](../../CLAUDE.md) is the condensed version of this document, kept short because it is loaded into every agent's context. This page is the human-facing expansion — same facts, more explanation of *why*, plus worked traces. Where the two overlap, `CLAUDE.md` is the summary and this is the source.

## Workspace map

Twenty crates, all members of the root `Cargo.toml` workspace, on edition 2024.
<!-- src: Cargo.toml -->

Ordered by layer, lowest first. A crate may depend on crates above it in this table and not below it — that is the whole of the layering rule.

### Foundations

| Crate | Purpose |
| --- | --- |
| `otto-id` | Sortable, monotonic identifiers — byte-faithful port of opencode's `id/id.ts` (`<prefix>_<time+counter hex><base62 random>`). |
| `otto-events` | The `LLMEvent` union, `Usage` accounting, and the process-wide app event bus. |
| `otto-config` | The `Config` struct, the JSONC loader, and the merge order for global/project config. |

### Capabilities

| Crate | Purpose |
| --- | --- |
| `otto-llm` | Provider-agnostic LLM client. A *route* = Protocol (wire shape) + Endpoint + Auth + Transport (framing); feeding an `LLMRequest` through a route yields provider-neutral `LLMEvent`s. |
| `otto-auth` | Credential store (`auth.json`, mode `0600`) plus provider OAuth flows (Anthropic Pro/Max, OpenAI Codex, GitHub Copilot). |
| `otto-tools` | The `Tool` trait, the `ToolContext` execution seam, and the built-in tools (read/write/edit/bash/grep/glob/task/…). |
| `otto-storage` | Session message/part model and its SQLite persistence, including the `latest` / `filter_compacted` history derivations. |
| `otto-mcp` | MCP client: local/remote server config, connection lifecycle, and MCP tools projected into the `Tool` trait. |
| `otto-lsp` | Spawns language servers over stdio, collects diagnostics, surfaces errors-only `<diagnostics>` blocks after edits. |
| `otto-vcs` | Local git: process runner, repository-root resolution, worktree management. No network. |
| `otto-hooks` | Lifecycle hooks — user-configured external commands fired at points in the session/tool/compaction pipeline. otto extension, no opencode analog. |

### Policy and the loop

| Crate | Purpose |
| --- | --- |
| `otto-permission` | Permission ruleset plus the interactive ask/reply gate; owns mode resolution and the session/parent registry. |
| `otto-question` | The `question` tool's ask/reply gate — a simpler sibling of `otto-permission` with no ruleset/mode dimension: every ask reaches a human or auto-cancels. |
| `otto-agent` | Agent definitions, the built-in agents (build/plan/general/explore), and subagent-permission derivation. |
| `otto-session` | The agent loop. Ties `otto-llm` (streaming), `otto-tools` (execution), and `otto-storage` (persistence) into the while-loop that drives a turn. |
| `otto-workflow` | Deterministic drivers above `otto_session::run_loop`: the sdd, tdd, and plan engines over a shared `WfCtx`. |

### Assembly

| Crate | Purpose |
| --- | --- |
| `otto-app` | Shared runtime assembly — wires config + auth + providers + tools + MCP + permission + agents + storage into a `Runtime` consumed by both the CLI and the server. |

### Front ends

| Crate | Purpose |
| --- | --- |
| `otto-server` | HTTP + SSE server, an opencode-compatible route surface over `otto_app::Runtime`. |
| `otto-cli` | The `otto` binary: command tree, terminal renderer, the testable run flow, and the serve/models/providers/auth/agent/mcp commands. |
| `otto-tui` | A ratatui terminal client for `otto serve`. Talks HTTP/SSE only. |

What the layering buys: `otto-session` can be tested with a scripted route and an in-memory store because it never reaches for a provider or a config file itself — everything concrete is injected by `otto-app`. Both front ends share one `Runtime`, so `otto run`, `otto serve`, and the TUI cannot drift in behavior.

## The turn pipeline

One user prompt produces one *turn*, which is one or more *steps*. A step is a single provider request; the turn keeps stepping while the model keeps calling tools.

```text
Route::stream (otto-llm)          provider SSE → decode_event → Protocol::step → LLMEvent stream
  → augment_with_tools (otto-session/runtime.rs)   executes tool calls as they stream; results appended at the stream TAIL
  → tap_events (otto-session/run.rs)               live event fan-out to CLI/TUI/server consumers
  → Processor::process (otto-session/processor.rs) folds LLMEvents into persisted messages/parts (SQLite)
  → run_loop (otto-session/run.rs)                 outer step loop; exits when the last assistant finished with
                                                   anything other than "tool-calls"
```

### `Route::stream`

A route is the composition of a protocol, an endpoint, auth, and a transport. The transport frames the provider's SSE bytes into events; the protocol's `decode_event` turns each frame into its own native event type, and `Protocol::step` folds that into the provider-neutral `LLMEvent` stream every downstream stage consumes.
<!-- src: crates/otto-llm/src/route.rs -->

Everything below this line is provider-neutral. Adding a provider means adding a protocol, not touching the session loop.

### `augment_with_tools`

Wraps the provider stream. As `ToolCall` events arrive it executes the corresponding tool via `ToolContext` — permission gate, question gate, abort token, subagent spawner, and the live event sender all hang off that context.
<!-- src: crates/otto-session/src/runtime.rs -->

The one structural fact worth internalizing: **tool results are appended at the tail of the stream**, not interleaved at the point of the call. That is what makes the drain invariant below necessary.

### `tap_events`

Optional. When `RunConfig.event_tx` is set, a dedicated pump forwards each event to the consumer as it arrives, decoupled from the processor's per-event persistence awaits. Without it, live UI updates would be paced by SQLite writes.
<!-- src: crates/otto-session/src/run.rs -->

### `Processor::process`

Folds the augmented stream into persisted rows: creates/updates the assistant message, appends text, reasoning, tool-call and tool-result parts, and records usage. It is single-use — every retry attempt builds a fresh one.
<!-- src: crates/otto-session/src/processor.rs -->

It returns a `ProcessOutcome`: `Continue`, `Stop`, or `Compact`.

### `run_loop`

The outer loop. Each iteration re-reads the full history from storage rather than carrying state forward, so compaction, salvage, and hook-synthesized turns all take effect simply by changing what is in the database.
<!-- src: crates/otto-session/src/run.rs -->

Per iteration, in order: fire `Stop` hooks and check the exit condition; run the auto-compaction pre-check; increment the step counter and decide whether this is the last step; create and persist a fresh assistant message; build the system prompt and converted messages; build tool definitions and the request; then stream/augment/process inside the retry loop.

The exit condition is: the latest assistant has a `finish` reason other than `"tool-calls"`, has no live tool-call parts, and post-dates the latest user message. Providers do sometimes return `"stop"` while tool calls are present, so the tool-call check is separate from the finish check and both must agree.

A hard cap of `MAX_ITERATIONS = 1000` bounds the loop; exceeding it is `RunError::IterationCap`.
<!-- src: crates/otto-session/src/run.rs -->

## Pipeline invariants

These are the rules whose violation recreates specific fixed bugs. The bug is the part a new contributor cannot infer from the code.

### A zero-event stream must surface `LLMError::EmptyStream`

If the processor reaches a clean EOF having seen no terminal `Finish`, it returns an error rather than `ProcessOutcome::Continue`: `NoTerminalFinish` if content was seen, `EmptyStream` if nothing was.
<!-- src: crates/otto-session/src/processor.rs -->

**Bug prevented:** some OpenAI-compatible gateways (litellm-style) answer with a stream containing nothing otto recognizes. Returning `Continue` makes `run_loop` immediately re-request with no backoff, burning through `MAX_ITERATIONS` in a tight spin against the provider. As an error it is classified retryable by `retry::retryable`, so the loop backs off instead.
<!-- src: crates/otto-session/src/retry.rs -->

The gate is suppressed for compaction, blocked, and already-errored turns so those are not misclassified, and it runs *after* processor cleanup so cleanup still happens on the error path.

### The processor drains past mid-stream errors

On a provider error the processor records the first error and keeps consuming the stream to completion, returning the stored error only after the drain finishes.
<!-- src: crates/otto-session/src/processor.rs -->

**Bug prevented:** because `augment_with_tools` appends tool results at the tail, a provider error mid-stream arrives *before* the results of tools that already ran. Bailing at the first error discards that completed work — and tools are not necessarily idempotent, so the retry re-runs writes, patches, and bash commands that already succeeded.

### Retry lives in two places

Classification and backoff are pure functions in `retry.rs`; the live attempt loop is inline in `run.rs`'s retry arm.
<!-- src: crates/otto-session/src/retry.rs, crates/otto-session/src/run.rs -->

Two budgets, deliberately different in scope:

| Budget | Field | Resets |
| --- | --- | --- |
| Per-step | `max_retries` | Each step |
| Whole-prompt | `max_total_retries` | Never, for the life of the turn |

An attempt is exhausted when either is hit. On exhaustion with a non-`NoTerminalFinish` error the failure is stamped onto the assistant message (`finalize_failed`) before propagating, so a turn never leaves an unfinalized message behind.

Two retry paths:

- **Salvage** — the failed attempt already completed tool calls. `salvage_completed_tools` finalizes the assistant as a `tool-calls` step and the outer loop continues from it. Without this, every retry throws away executed tools and the model re-runs the same reads and re-narrates.
- **Purge-and-replay** — the failed attempt has no completed tool parts. The next attempt begins by deleting the partial parts under the *same* assistant id, so a retry cannot duplicate content.

The purge runs at the top of the next attempt, not at the end of the failed one. That ordering is load-bearing for the truncation path below: the streamed parts are still intact when the budget runs out.

### `on_halt` must not fabricate a terminal `Finish`

A protocol whose stream started and then ended without a finish reason must not invent one. The processor's `NoTerminalFinish` gate is what handles truncation.
<!-- src: crates/otto-session/src/processor.rs -->

`run_loop` retries a truncated stream, and once the budget is spent it *accepts* it: logs a warning, calls `finalize_truncated`, emits an `LLMEvent::Warning` on the live tap, and returns `Continue`.
<!-- src: crates/otto-session/src/run.rs -->

**Bug prevented:** a provider that chronically omits `finish_reason` truncates identically on every attempt. Fabricating a finish makes truncation invisible; failing hard makes such a gateway unusable. Retry-then-accept-with-warning is the compromise, and it only works because the fabrication never happened.

Aborts also end a stream without a terminal finish, so the retry arm checks the abort token *first* and breaks gracefully into the interrupt path rather than treating the abort as a provider failure.

### `LLMEvent` is exhaustively matched in four places

Adding a variant to `otto_events::LLMEvent` means touching all of:

| Location | Role |
| --- | --- |
| `crates/otto-session/src/processor.rs` | Persistence fold |
| `crates/otto-tui/src/state.rs` (`fold_event`) | TUI state fold |
| `crates/otto-llm/src/protocols/anthropic_messages.rs` | Test type-tag helper |
| `crates/otto-llm/src/protocols/gemini.rs` | Test type-tag helper |

New fields on existing variants should be `#[serde(default)]` — events cross the HTTP/SSE boundary to the TUI, and a server and client at different versions must still interoperate.

### Adding a `RunConfig` field touches ~8 struct literals

`RunConfig` is constructed by hand in production and in every integration-test fixture. Expect to update `crates/otto-app/src/runtime.rs`, `crates/otto-session/src/subagent.rs`, and the fixtures in `crates/otto-session/tests/{run_loop,subagent,mcp_loop,compaction}.rs`. It is mechanical, but the compiler only finds them one at a time.

## Providers and routing

`otto-app/src/route_factory.rs` maps a provider id onto a wire protocol.
<!-- src: crates/otto-app/src/route_factory.rs -->

| Provider id | Route |
| --- | --- |
| `anthropic` | Native Anthropic Messages. An OAuth credential switches to Bearer auth rather than `x-api-key`. |
| `openai` | Native OpenAI. |
| `google`, `gemini` | Native Gemini. |
| `vertex` | Vertex AI. Requires `provider.vertex.options.project`; `location` defaults to `us-central1`. Uses a live GCP token source. |
| `github-copilot` | Copilot, with an enterprise domain picked up from the stored OAuth credential. |
| *anything else* | `OpenAICompatible` against `{baseURL}/chat/completions`. |

The fallthrough arm is the point of the design, not an afterthought: it is how litellm, ollama, vllm, and any other OpenAI-compatible endpoint work without code changes. `config.provider.<id>.options` supplies `baseURL` and `apiKey`; a stored credential wins over the config-supplied key.

Named providers also honor `options.baseURL` and `options.apiKey`. That is how otto's *native* Anthropic protocol is pointed at a gateway — `provider.anthropic.options.baseURL` yields `{baseURL}/messages`, exactly the endpoint Anthropic-native clients use against litellm.

Two details that bite:

- **`ModelRef::parse` splits on the first slash only.** `ollama/gemma4:26b-mlx` and `github_copilot/claude-opus-4.8` both survive with the full remainder as the model id.
  <!-- src: crates/otto-agent/src/agent.rs -->
- **`config.provider.<id>.models.<model>.limits` overlays context/output windows** onto the resolved `Model`, for any provider. The embedded models.dev registry has never heard of local models, and the run loop's compaction pre-check reads `model.limits.context` — without the overlay, compaction never triggers and the provider silently truncates the prompt.

### Decoder tolerance

Five protocols live in `crates/otto-llm/src/protocols/`: `anthropic_messages`, `openai_chat`, `openai_responses`, `openai_compatible`, `gemini`. One shared HTTP transport sits under all of them.

Gateways vary, so decoding is deliberately forgiving:

- An error `code` may be a string or a number, and `error` itself may be a bare string rather than an object — both handled with `#[serde(untagged)]` enums.
  <!-- src: crates/otto-llm/src/protocols/openai_chat.rs -->
- An undecodable frame is skipped with a `tracing` warning rather than failing the stream. The stream only fails retryably when garbage dominates.
  <!-- src: crates/otto-llm/src/route.rs -->
- Reasoning arrives as `reasoning_content` (DeepSeek) or `reasoning` (OpenRouter, vLLM); both map to the same event.
  <!-- src: crates/otto-llm/src/protocols/openai_chat.rs -->

## Permissions

User-facing semantics — modes, rule syntax, what asks look like — are in [`../guide/permissions.md`](../guide/permissions.md). This section is the enforcement mechanism.

`Permission::ask` resolves in **two phases**, not one merged chain. Within each phase, `evaluate` is last-match-wins over the listed rulesets in order.
<!-- src: crates/otto-permission/src/permission.rs -->

| Phase | Layers, low → high | Question answered |
| --- | --- | --- |
| 1. Deny gate | agent session ruleset → user config ruleset → in-session `Always` approvals | Is this outright denied? |
| 2. Interactivity | mode overlay → user config ruleset → in-session `Always` approvals → danger rules | Does this need a human? |

The agent layer appears in phase 1 and is **deliberately absent from phase 2**. That asymmetry encodes two rules at once:

- An agent's deny holds in every mode — plan mode's edit-deny outside `.otto/plans/` survives full-auto, because full-auto only participates in phase 2.
- An agent's broad `* allow` defaults cannot bypass approve-each, and its `ask` rules (doom-loop, external-directory, `.env` reads) get answered by full-auto instead of raising a blocking prompt that reads to the user as a silent hang.

Anything the user stated explicitly — a config rule, an in-session `Always` — outranks the agent in both phases. Danger rules sit on top of phase 2 and prompt in every mode.

A phase-1 deny is a *policy* deny (`by_user: false`): the tool errors and the turn continues. It is not a human saying no.

### Two registration duties per session

Creating a session is not enough. Both of these must happen, in `Runtime::create_session` and in `SessionSubagentSpawner::spawn`:
<!-- src: crates/otto-app/src/runtime.rs, crates/otto-session/src/subagent.rs -->

| Call | Why |
| --- | --- |
| `link_parent(child, parent)` | Permission **mode** resolves live up the parent chain: a session with no explicit mode inherits its nearest ancestor's, re-resolved on every ask. This is how toggling full-auto in the TUI reaches a workflow's subagents mid-run. |
| `set_session_ruleset(id, ruleset)` | Installs the agent's ruleset for enforcement. Agent metadata alone is **not** enforced — an agent whose ruleset was never registered runs with no agent layer at all. |

**Any new session-creation path must do both.** Forgetting `set_session_ruleset` produces a plan-mode session that happily edits files; forgetting `link_parent` produces a subagent stuck in ask-mode while its parent is in full-auto.

### Every in-process driver needs a permission responder

Anything that runs a session or a workflow in-process must install a responder for permission asks. The reference implementation is `spawn_permission_pump`.
<!-- src: crates/otto-cli/src/run.rs -->

Without one, the first `Ask`-mode tool call blocks on a oneshot that nobody will ever answer. There is no timeout and no error — the run simply hangs, silently. This is the single most common way a new driver appears to "work" until the first non-trivial prompt.

## Server, TUI, workflows

### Server

`otto-server` exposes an opencode-compatible surface over the shared `Runtime`: `/session/*` CRUD plus the prompt endpoint, a global `/event` SSE stream, and the workflow routes.
<!-- src: crates/otto-server/src/lib.rs -->

### TUI

`otto-tui` is a pure HTTP/SSE client of `otto serve`, which `otto tui` auto-spawns. `POST /session/{id}/message` streams one turn; `/event` carries the global bus — workflow progress, permission asks, session lifecycle.

State is an Elm-style `App` in `crates/otto-tui/src/state.rs`: messages in, state mutated, view rendered from state. Nothing in the TUI reaches into the session loop directly.

Transcript rendering is memoized in a `LineCache`, keyed by `App.render_gen` plus the render width; either changing rebuilds it. `render_gen` is bumped by any mutation affecting the transcript.
<!-- src: crates/otto-tui/src/state.rs -->

The geometry is in **wrapped rows, not logical lines**, everywhere. `line_wrap_starts` maps each logical line to its starting viewport row, and `line_wrap_starts.len() == lines.len()`. Measure with `Paragraph::new(..).wrap(..)` — anything that assumes one logical line is one row breaks the scroll math the moment a message is longer than the pane is wide.

### Workflows

`otto-workflow` ships three engines over a shared `WfCtx`: `SddWorkflow`, `TddWorkflow`, `PlanWorkflow`.
<!-- src: crates/otto-workflow/src/lib.rs -->

Driven two ways:

| Entry point | Mechanism |
| --- | --- |
| `POST /workflow/{kind}` | Server-side. Parented to the caller's session via the `parent` body field. `POST /workflow/{session}/cancel` cancels via a registered `CancellationToken`. |
| `otto workflow` | CLI, in-process `Runtime` plus a `WorkflowHarness` supplying the permission responder and progress reporting. |

Plan files are parsed from `### Task N:` headings (`parse_plan_tasks`). Sdd implementers report a trailing `{"status": ...}` JSON line, read by `parse_status`.

## Compaction and agents

Built-in agents carry step caps. `build`, `plan`, and `general` allow 200 steps; `explore` allows 100; the smallest built-in allows 4.
<!-- src: crates/otto-agent/src/builtins.rs -->

On the final step, `run_loop` appends `MAX_STEPS_PROMPT` as an assistant message: tools are disabled, the model must respond with text only, summarizing what was done and what remains.
<!-- src: crates/otto-session/src/run.rs -->

Post-turn pruning erases old completed tool outputs to reclaim context.
<!-- src: crates/otto-session/src/compaction.rs -->

The rule that matters: pruning walks newest-first and **keeps the newest `read` output per distinct file path**, bounded at `PRUNE_READ_PATH_EXEMPT_MAX = 30` paths. Exempt reads do not consume the protect budget. Without the exemption, a long session re-reads the same files after every prune — a read/prune/re-read loop that burns tokens and never converges. With an unbounded exemption, a 400-file sweep would pin its entire history.

Auto-compaction is separate and runs *before* generating: if the last finished assistant's recorded tokens have reached the usable context slice, `run_loop` summarizes and re-reads the compacted history on the next iteration. The summary assistant is itself excluded from the check, so compaction cannot loop.

## Relationship to opencode

otto is a port of [opencode](https://github.com/sst/opencode). Upstream `opencode/packages/` is the **behavioral source of truth**.

Ported code cites its origin inline as `file.ts:line` — for example `prompt.ts:1186-1201` above the assistant-message creation in `run_loop`, or `compaction.ts:243-287` above `prune`. These are not decorative: when a ported behavior looks wrong, the citation is how you check it against upstream before "fixing" a faithful port.

Deliberate divergences are marked in doc comments as **"otto extension"** or **"no opencode analog"**. `otto-hooks` and `otto-vcs` are whole crates with no upstream counterpart; smaller extensions are noted at the item that diverges.

When changing ported behavior, keep the citation and update it, or replace it with an explicit divergence note. Silently editing ported code without touching the citation leaves the next reader with a comment that lies.
