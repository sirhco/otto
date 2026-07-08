# Changelog

All notable changes to otto are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/); versions follow
[SemVer](https://semver.org/) (pre-1.0: minor bumps may break).

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
