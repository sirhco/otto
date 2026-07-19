# Changelog

All notable changes to otto are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/); versions follow
[SemVer](https://semver.org/) (pre-1.0: minor bumps may break).

## [Unreleased]

### Fixed

- **Plan mode's edit restriction was bypassable via the `write` tool.** The
  `plan` agent's ruleset denied only the `edit` permission, but `WriteTool`
  asks under its own `write` permission — and the base ruleset is `"*":
  "allow"`, so `write` fell through to allow. A plan-mode session could
  therefore create or overwrite arbitrary files despite the agent being
  documented as read-only. `plan()` now carries a `write` deny mirroring the
  `edit` one, with the same `.otto/plans/*.md` exception. `apply_patch` was
  never affected — it asks under `edit`. Added a regression test; the existing
  `plan_denies_edits_except_plan_files` test carried a comment asserting that
  `write` mapped to `edit` at gate time, which was never true.

## [0.13.0] - 2026-07-19

### Removed

- **Bedrock, Azure, and nine unofficial OpenAI-compatible provider presets
  (xai, deepseek, groq, togetherai, cerebras, fireworks, deepinfra, baseten,
  openrouter) are no longer natively supported.** otto now natively supports
  Anthropic, OpenAI, Gemini (Google + Vertex), and GitHub Copilot; every other
  provider id — including the ones removed here — routes through the generic
  `OpenAICompatible` catch-all, which is also how local/gateway tooling
  (litellm, ollama, vllm) has always worked. Migration: replace a named
  preset or the Azure config with `config.provider.<id>.options.baseURL` (and
  `apiKey` if needed) pointing at the same endpoint the preset used — for
  example, `provider.xai.options.baseURL = "https://api.x.ai/v1"`. Bedrock
  has no generic equivalent (its AWS SigV4/event-stream wire format isn't
  OpenAI-compatible) and is dropped without a replacement path.

## [0.12.0] - 2026-07-19

### Added

- **`otto tui`'s prompt editor gains cursor navigation and undo/redo.**
  Left/Right/Up/Down/Home/End now move the cursor (Up/Down are
  wrap-aware, crossing soft-wrapped and logical lines with a sticky
  preferred column) — previously arrow keys did nothing once the input
  had text. `ctrl+_`/`ctrl+shift+_` undo/redo edits, batching runs of
  typing into a single step.

### Fixed

- **Anthropic OAuth credentials (`otto auth login anthropic`) now
  authenticate correctly.** The Claude Pro/Max access token was being
  sent through the same `x-api-key` header as a plain API key, 401ing
  every inference call; it now sends `Authorization: Bearer <token>`
  plus the `anthropic-beta: oauth-2025-04-20` header the grant requires.

## [0.11.0] - 2026-07-18

### Added

- **`otto tui` gains a multi-agent dashboard.** Open it from the command
  palette (`ctrl+k` → "Dashboard…") to see your other sessions at a
  glance — busy, idle, or awaiting a permission/question ask — without
  leaving the session you're attached to. Select a row to peek its
  latest message or a pending ask's options, and answer permission
  (`y`/`a`/`n`) or question (`1`-`9`) asks right there; press enter to
  fully switch into a session.

### Fixed

- **A permission or question ask for a different session no longer
  hijacks your current screen.** Previously, an ask arriving for a
  backgrounded session would pop open its prompt over whatever you were
  looking at; it now only surfaces for the session you're attached to
  (the dashboard is what shows you asks from other sessions).

## [0.10.0] - 2026-07-17

### Added

- **The `question` tool is now interactive.** Previously it unconditionally
  errored `"question tool requires an interactive client"`; now the model
  can ask you a multiple-choice question mid-turn and get a real answer
  back. Works in `otto run` (TTY prompt), `otto tui` (a new question
  overlay — arrow keys to highlight, space to multi-select, enter to
  confirm/advance through a batch), and `otto serve`
  (`GET /question`, `POST /question/{id}/reply`, `question.asked` SSE
  event).

## [0.9.0] - 2026-07-17

### Added

- **`otto tui` auto-detects OS light/dark appearance.** `theme = "auto"`
  follows the OS's light/dark setting (macOS/Linux-GNOME/Windows), live-
  repolling every 5s; SSH sessions get a one-shot terminal query instead. A
  new `light` preset backs auto-detected light mode.
- **`otto tui` degrades color output on non-truecolor terminals.** Theme
  colors quantize to a 256-color or 16-color palette based on `COLORTERM`/
  `TERM`, instead of assuming full RGB support.
- **`otto tui` sends an OS notification when a turn finishes unfocused.** A
  terminal notification (OSC 9) and title update appear when otto isn't the
  focused window when a turn completes, and clear on refocus.

## [0.8.0] - 2026-07-16

### Added

- **`otto workflow sdd` dispatches implementers in parallel again — safely.**
  v0.7.1 made Phase A sequential to stop implementers from clobbering a
  shared working tree. Each task's implementer now runs in its own isolated
  git worktree instead, dispatched all at once; a successful task's changes
  are folded back into the shared tree afterward. A merge conflict (two
  tasks touching overlapping lines) degrades only that one task to
  `BLOCKED` rather than corrupting the shared tree or failing the run.

## [0.7.1] - 2026-07-16

### Fixed

- **`otto workflow sdd` could clobber files** — Phase A fanned out every
  task's implementer in one parallel batch, all writing into the same
  shared working tree at once; a real report of lost/overwritten work
  confirmed this. Implementers now dispatch one at a time, eliminating the
  concurrent-write race. (Per-task worktree isolation — parallel dispatch,
  safely — remains a larger, separate follow-up.)

## [0.7.0] - 2026-07-15

### Added

- **`otto workflow sdd/tdd/plan` is now actually cancellable.** `WfCtx` had
  no cancellation field at all — every workflow engine constructed its own
  disconnected `CancellationToken`, so Ctrl+C in the CLI and
  `POST /workflow/{session}/cancel` on the server (which already built and
  registered a real, working token!) had zero effect on a running engine.
  The CLI now wires `Ctrl+C` the same way plain `otto run` does; the server
  threads its already-existing per-session token through.

### Fixed

- **A single unparseable review verdict crashed the entire `sdd` run**,
  discarding every other task's already-completed result. `SddWorkflow::drive`
  now degrades just the affected task (to `NEEDS_CONTEXT` or `BLOCKED`) and
  continues — the same fix also applies when cancellation cuts a review
  turn short mid-run.

## [0.6.1] - 2026-07-15

### Fixed

- **Vertex AI turns retry-looped until budget exhaustion (looked like a
  hang)** — Vertex's `alt=sse` proxy sometimes omits the blank-line event
  terminator between consecutive chunks, so the SSE framer merged two
  independent JSON events into one frame, which failed to parse and got
  classified as a retryable provider failure. The stream decoder now
  recovers by splitting a failed frame on newlines and decoding each line
  independently when every line parses on its own.

## [0.6.0] - 2026-07-15

### Added

- **Native Google Vertex AI provider** (`vertex/<model>`, Gemini models
  only) — authenticates via GCP Application Default Credentials (the
  `gcloud-auth` crate) instead of a static API key, reusing the existing
  Gemini wire protocol against Vertex's project/location endpoint
  (`https://{location}-aiplatform.googleapis.com/...`). An async
  ADC-backed token cache refreshes the bearer token in the background (a
  `tokio::sync::watch` channel bridges it into the fully-synchronous route
  builder), fetching an initial token at startup so bad/missing GCP
  credentials fail immediately rather than on the first chat turn.
  Configure with `provider.vertex.options.project` (required) and
  `.location` (optional, defaults `us-central1`):
  ```json
  {
    "model": "vertex/gemini-2.5-pro",
    "provider": {
      "vertex": {
        "options": { "project": "my-gcp-project", "location": "us-central1" },
        "models": { "gemini-2.5-pro": { "limits": { "context": 1048576, "output": 8192 } } }
      }
    }
  }
  ```

## [0.5.0] - 2026-07-15

### Added

- **Hook `ask` verdicts now escalate through a real interactive prompt**
  instead of being treated identically to `deny`. All four blocking
  lifecycle-hook events (`PreToolUse`, `UserPromptSubmit`, `Stop`,
  `SubagentStop`) route an `ask` verdict through the same permission
  ask/reply flow tool-call permissions already use — the TUI overlay, CLI
  `y/n/a` prompt, and server `/permission/{id}/reply` all handle a
  hook-originated ask exactly like a tool-call one, under a new `hook`
  permission bucket (so existing mode/ruleset config — full-auto,
  approve-each, danger rules — applies to hook asks for free). A human's
  typed rejection message now reaches the model/turn the same way a hook's
  own `reason` does (previously dropped for every kind of permission
  rejection, not just hooks). `deny` verdicts are unchanged.

### Fixed

- **TUI command palette (`ctrl+k`) showed stale or entirely wrong
  keybindings** next to several commands — `Switch session…`/
  `Change model…` displayed keys (`s`/`m`) that don't exist (they're
  palette-only), and `Change agent…`/`Toggle tool detail`/`Quit` showed
  bare letters (`g`/`t`/`q`) left over from before those bindings moved to
  ctrl-chords. Corrected, with a regression test guarding against future
  drift.
- **TUI: the mid-turn "turn in flight" warning used the same ✓ checkmark**
  as success confirmations like "copied"/"attached". It now renders with a
  distinct ⚠ warning glyph/color.

## [0.4.0] - 2026-07-15

### Added

- **Lifecycle hooks** — user-configured external shell commands that fire at
  points across the session/tool/compaction pipeline, otto's answer to Claude
  Code's hooks (an otto-native JSON contract, not a literal field-for-field
  mirror; no opencode analog). Eight events: `SessionStart`,
  `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`, `Stop`,
  `SubagentStop`, `Notification`. Each configured hook gets a JSON payload on
  stdin and returns a verdict (`allow`/`deny`/`ask`, plus optional
  `reason`/`additional_context`/`system_message`) on stdout; a hook that
  crashes, times out, or emits unparseable output fails open with a loud
  warning rather than breaking the turn. `PreToolUse` can block a tool call
  before it runs; `PostToolUse` can attach a message to the result;
  `UserPromptSubmit` can block a turn before any provider call and inject
  extra context into the system prompt; `SessionStart`'s context persists for
  every turn of the session; a denying `Stop`/`SubagentStop` doesn't just
  block — it synthesizes a follow-up turn and the agent keeps going instead of
  stopping; `Notification` fires (fire-and-forget) whenever a permission
  request starts waiting on a human decision. Configure via the new `hooks`
  config block: `hooks.<event>[].matcher` (a regex over the tool id, tool
  events only) + `.hooks[].command`/`.timeout_ms`.

## [0.3.3] - 2026-07-09

### Fixed

- **A ruleset deny no longer kills the whole turn.** The `general` agent's
  builtin `todowrite: deny` (enforced since v0.3.0) made every sdd implementer
  hard-stop on its very first tool call — all tasks came back `NEEDS_CONTEXT`
  with no status marker. `PermissionDenied` now distinguishes a human
  rejecting an ask (turn stops, as before) from a policy rule denying a call
  (the tool fails with an error the model can adapt to, and the turn
  continues).
- The plan agent's suggested command now includes the required `--plan` flag
  (`otto workflow sdd --plan .otto/plans/<name>.md --auto`).

## [0.3.2] - 2026-07-09

### Fixed

- **Full-auto can no longer block on agent-level `ask` rules.** v0.3.0's
  ruleset enforcement made the builtin agents' `ask` rules (`doom_loop`,
  `external_directory`, `.env` reads) outrank the full-auto overlay, so a
  full-auto session could raise a blocking permission prompt with no timeout —
  a turn that "just stops" with no error. Permission resolution is now
  two-phase: a deny gate (agent ruleset + user config + `Always` approvals —
  agent denies hold in every mode, explicit user statements still outrank the
  agent) followed by mode-driven interactivity WITHOUT the agent layer, so
  the mode answers asks (full-auto auto-allows, danger still prompts) and an
  agent's broad `* allow` defaults no longer bypass approve-each prompting.

### Added

- **Logging actually works.** `--log-level` / `--print-logs` / config
  `logLevel` were parsed but no `tracing` subscriber was ever installed, so a
  stalled or failed turn left no forensics anywhere. otto now writes a
  daily-rolling log file under `{data_dir}/otto/logs/otto.log.YYYY-MM-DD`
  (stderr with `--print-logs`); `OTTO_LOG` accepts full `EnvFilter` directives
  (e.g. `OTTO_LOG=otto_session=debug,otto_llm=trace`). Default level `warn`.
  The run loop now logs every retry (attempt, backoff, salvage, error), retry
  exhaustion, and accepted-truncation decisions, joining the existing
  skipped-frame warnings from the stream decoder.

## [0.3.1] - 2026-07-09

### Fixed

- **`provider.anthropic.options.baseURL` / `provider.openai.options.baseURL`
  are honored.** Named providers previously ignored config overrides (only
  unknown provider ids got a base URL), so otto's native Anthropic Messages
  protocol could not be pointed at a gateway. Now
  `provider.anthropic.options.baseURL = http://litellm:4000/v1` +
  `model = anthropic/github_copilot/claude-opus-4.8` drives litellm's
  `/v1/messages` endpoint exactly like Anthropic-native clients (model names
  containing `/` are preserved — the provider is split on the first slash
  only). `options.apiKey` works as the key fallback for named providers too.

## [0.3.0] - 2026-07-09

Reliability overhaul: provider retry/streaming robustness against
OpenAI-compatible gateways (litellm, Ollama, OpenRouter), working workflow
permission handling, and wrapped-row-accurate TUI scrolling.

### Fixed

- **Zero-event provider streams no longer loop forever.** An OpenAI-compatible
  gateway (litellm etc.) whose stream carried only unrecognized frames used to
  trigger an immediate, backoff-free re-request loop (up to 1000 iterations,
  one empty assistant message each). Empty attempts are now a retryable
  `EmptyStream` error with normal backoff, Retry events, and the retry cap.
- **Retries keep completed tool work.** A mid-stream failure after tools had
  executed no longer purges and re-runs them — the finished step is kept and
  the turn continues from it (no more repeated re-reading/re-narration).
- **`otto workflow tdd|sdd|plan` no longer deadlocks silently.** The CLI now
  installs a permission responder (interactive prompt, or `--auto`/`-y` for
  full-auto) and prints per-task progress.
- **TUI transcript scrolling.** Wrapped lines are now measured correctly, so
  the bottom of the transcript is reachable, follow-mode really follows, and
  resize no longer hides content; PageUp overscroll no longer deadens
  PageDown; Enter during an in-flight turn no longer interleaves two streams.
- **Gateway stream tolerance.** Numeric/bare-string error frames decode,
  undecodable frames are skipped (retryable failure only when they dominate),
  `delta.reasoning` (OpenRouter/vLLM) maps to reasoning output, and a stream
  that ends without `finish_reason` is retried as a truncation instead of
  silently accepted (accepted-with-warning only after the retry budget).

### Changed

- **Permission mode now inherits down the session chain.** Workflow sessions
  and subagents resolve the nearest ancestor's mode live — full-auto in the
  TUI finally applies to sdd/tdd/plan runs; flipping mode mid-run affects
  in-flight subagents' next ask.
- **Agent permission rulesets are enforced at the gate.** Previously they were
  only stored in session metadata: e.g. plan mode could edit any file in
  full-auto. Precedence (low → high): mode overlay < agent ruleset <
  user config < in-session `Always` approvals < danger rules. If you relied on
  full-auto bypassing an agent's deny (plan mode editing outside
  `.otto/plans/`), adjust the agent's `permission` config instead.
- New config knobs: `retry.max_attempts` (per step, default 5),
  `retry.max_total_attempts` (per prompt, default 20),
  `retry.turn_timeout_seconds` (default off).

## [0.2.2] - 2026-07-08

### Changed

- **otto no longer shares identity or storage with opencode.** otto was
  originally a port of opencode; this release finishes disentangling the two:
  - GitHub Copilot device-flow auth now uses otto's own registered GitHub
    OAuth App client id instead of opencode's.
  - Global config/data/cache/state dirs, the `auth.json` credential store, and
    the Basic-auth server username now live under `otto`, not `opencode`.
  - Project config files are `otto.json` / `otto.jsonc` (or `.otto/otto.json`)
    instead of `opencode.json` / `.opencode/opencode.json`; plan mode writes
    plans to `.otto/plans/*.md`.
  - Env var overrides renamed `OPENCODE_*` -> `OTTO_*`
    (`OTTO_CONFIG_DIR`, `OTTO_CONFIG`, `OTTO_CONFIG_CONTENT`,
    `OTTO_DISABLE_PROJECT_CONFIG`, `OTTO_AUTH_CONTENT`, `OTTO_SERVER_PASSWORD`,
    `OTTO_WEBSEARCH_PROVIDER`, `OTTO_MODELS_URL`, `OTTO_MODELS_PATH`,
    `OTTO_DISABLE_MODELS_FETCH`).
  - The config `$schema` default now points at a JSON schema generated from
    otto's own `Config` struct and hosted in this repo
    (`schema/config.json`, regenerate via
    `cargo run -p otto-config --example gen_schema`) instead of
    `opencode.ai`.

  Existing `opencode.json` / `.opencode/` project configs and
  `OPENCODE_*`-prefixed env vars are no longer read — rename them to the
  `otto` equivalents above.

## [0.2.1] - 2026-07-08

### Fixed

- **bash tool: timeout/abort now kills the whole process tree.** The kill
  only signalled the `sh` wrapper; when the shell forks the command instead
  of exec'ing it (dash always does, bash does for compound commands), the
  real work survived as an orphan — and, holding the stdout/stderr pipe
  write ends, blocked the tool until it exited. On Linux this meant Esc /
  timeout on a running shell command did nothing. The child now runs in its
  own process group and the group is killed; pipes are drained incrementally
  with a bounded grace period so background/daemon grandchildren can't hold
  the tool hostage (captured output is kept).

## [0.2.0] - 2026-07-08

### Added

- **Session token counter (TUI)** — the header usage line now ends with a
  `Σ 12.3k` suffix: real measured input+output tokens accumulated across the
  session, counted once per assistant message. Gives an honest baseline for
  comparing runs (e.g. tersemode on vs off). Resets on session switch.
- **Per-model context limits in config** —
  `provider.<id>.models.<model-id>.limits.{context,output}` in `opencode.json`
  overlays the resolved model's limits, so history compaction triggers for
  models the embedded registry doesn't know (local ollama models) before the
  provider silently truncates the prompt. A limits-only provider entry no
  longer requires a `baseURL`.
- **Plan mode writes plan files** — the `plan` agent now carries a system
  prompt directing it to persist the final plan to
  `.opencode/plans/<kebab-title>.md` using the sdd-parseable
  `### Task N: Title` headings, and to end with the
  `otto workflow sdd` command that executes it. Previously the ruleset allowed
  the write but nothing instructed the model to do it.

### Fixed

- **CI** — rustfmt import-ordering violations and a clippy `collapsible_if`
  warning failed the `fmt · clippy · test` job under `-D warnings`.
- **Release pipeline** — the `x86_64-apple-darwin` build targeted the retired
  `macos-13` intel runners and queued forever; it now cross-compiles on the
  arm `macos-latest` image. `actions/checkout` bumped v4 → v5 (Node 20
  deprecation).
- **License metadata** — `Cargo.toml` declared MIT while the repository
  LICENSE is Apache 2.0; now `Apache-2.0`.

## [0.1.1] - 2026-07-08

### Fixed

- **Mid-turn streaming stalls** (four root causes):
  - Client-visible event delivery was serialized behind per-delta SQLite
    writes; a dedicated pump now forwards provider events at wire speed.
  - Per-delta persistence rewrote the entire accumulated text blob (O(N²)
    bytes per message); now debounced to one snapshot per 250ms with a
    guaranteed flush on block end and cleanup.
  - The file-backed store ran with the default rollback journal; now WAL plus
    a 5s busy timeout, so history reads no longer block the streaming writer.
  - A new prompt on a busy session raced the prior turn instead of
    interrupting it; run tokens are generation-tagged and a new prompt cancels
    the turn it replaces.
- **Retry visibility** — provider `Retry-After` honored up to 60s (was 5min);
  the TUI header renders a live backoff countdown instead of freezing.
- **Stream robustness** — TUI SSE decoder survives UTF-8 sequences split
  across chunks and flushes unterminated trailing frames; the `/event` pump
  reconnects with backoff; an errored turn triggers a history refetch to
  reconcile missed deltas; a frame-level watchdog catches providers that
  stall while emitting keepalives.
- **Transcript correctness** — tool results match rows by call id (parallel
  tools finish out of order); orphan reasoning deltas open a block instead of
  vanishing; a mid-stream retry rolls back the partial attempt so re-streamed
  text isn't duplicated.
- **Subagent liveness** — `task`-tool child runs forward their tool lifecycle
  events to the parent turn, so a subagent is no longer a silent pause.
- **Performance** — per-item transcript render cache (a streaming delta
  re-renders only the open item, not the whole transcript's markdown);
  session history hydration is two queries instead of 1+N per run-loop step.

## [0.1.0] - 2026-07-07

Initial pre-release: `otto serve` (HTTP/SSE server), `otto tui` (ratatui
client), agent run loop with tools/permissions/compaction, multi-provider LLM
routing (Anthropic, OpenAI, Gemini, Bedrock, OpenAI-compatible), SQLite
persistence, and the sdd/plan workflow engine.
