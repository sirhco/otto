# Changelog

All notable changes to otto are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/); versions follow
[SemVer](https://semver.org/) (pre-1.0: minor bumps may break).

## [Unreleased]

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
