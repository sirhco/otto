<p align="center">
  <img src="otto-lockup.svg" alt="otto" width="420">
</p>

# otto — an agentic coding harness (Rust port of opencode) with some additions

> ⚠️ **Work in progress — not stable.** otto is under active development and being actively built out. Config keys, wire formats, HTTP routes, CLI flags, TUI keybindings, and crate APIs can change without notice, and things may break between commits. It is **not recommended for production use** yet. If you depend on current behavior, pin a specific commit.

**otto** is a Rust (edition 2024) implementation of the [opencode](https://github.com/sst/opencode) agentic coding harness. It ships an integrated **CLI**, a native **TUI**, and an opencode-compatible **HTTP/SSE server** for building LLM-driven developer tooling — SDK-free, with its own wire-protocol client for every provider.

Source of behavioral truth: the upstream `opencode/packages/` (primarily `opencode/src` and `llm/src`). otto is a faithful port, not a fork.

> **Status:** `v0.11.0` · **pre-release / unstable** · 20-crate workspace · `clippy -D warnings` + `fmt` clean.

## Features

- **Six wire protocols, SDK-free** — `anthropic-messages`, `openai-chat`, `openai-compatible-chat`, **`openai-responses`** (gpt-5 class), `gemini`, `bedrock-converse` — plus two transports (`HttpTransport`, and a SigV4-signing `BedrockTransport` with AWS binary event-stream decode).
- **Broad provider reach** — Anthropic, OpenAI (incl. **gpt-5 via the Responses API**), OpenAI-compatible (DeepSeek / Groq / Together / OpenRouter / xAI / **Ollama** / any custom base URL), Azure OpenAI, Google Gemini, Amazon Bedrock, and **GitHub Copilot** (Claude + OpenAI models, incl. gpt-5 over `/responses`).
- **Model registry** — the full [models.dev](https://models.dev) catalog (149 providers / 5000+ models) embedded, with fetch-or-embed refresh and capability gating.
- **Native terminal UI (`otto tui`)** — a `ratatui`/`crossterm` client of `otto serve`: live transcript with markdown + syntax highlighting + colorized diffs, command palette (`ctrl+k`), fuzzy file attachments (`ctrl+f`), live todo panel (`ctrl+o`), transcript search (`/` + `n`/`N`), interactive permission prompts, **cyclable permission modes** (`shift+tab`: approve-each / accept-edits / full-auto, with a color-coded header indicator), **`Esc` to interrupt a running turn** without ending the session, session/model/agent pickers, token/cost/context usage, `NO_COLOR` monochrome theme, native drag-select copy + OSC-52 yank (`ctrl+y`), `ctrl+z` suspend, a **multi-agent dashboard** (peek/reply to other backgrounded sessions — including nested workflow subagents — with live push-driven status, pin, and filter), and a read-only **workflow status overlay** (`ctrl+w`) for in-flight `otto workflow` runs.
- **Workflow engine (`otto workflow`)** — three working driver engines above the run loop: `tdd` (red/green/refactor cycle with test-runner integration and regression checks), `sdd` (plan-task parsing + subagent orchestration), and `plan` (ordered task execution with a verification gate) — each supports `--dry-run` and `--auto`/`-y`.
- **Comprehensive tooling** — 14 built-in tools (read / write / edit[9-strategy replacer] / glob / grep / bash / apply_patch / webfetch / todowrite / websearch / skill / question / task-subagent / invalid-fallback) with truncation and a registry.
- **LSP diagnostics** — stdio clients for TypeScript, Rust (`rust-analyzer`), Python (`pyright`), and Go (`gopls`); post-edit diagnostics are surfaced back to the model.
- **Worktree management** — isolated Git sandboxes (`otto worktree list|create|remove|reset`) under a per-project data root.
- **Session persistence** — SQLite (via `sqlx`) for conversational history, parts, tool output, and compaction.
- **Lifecycle hooks** — user-configured external commands fired at pipeline points (pre/post-tool-use, compaction, etc.).

## Getting started

### Prerequisites

Rust 1.95+ (edition 2024):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Build

```bash
cargo build -p otto-cli        # produces target/debug/otto
```

### Authenticate

```bash
otto auth login anthropic              # API key or OAuth
otto auth login github-copilot         # GitHub device flow
# OpenAI / Gemini / Bedrock read standard env vars (OPENAI_API_KEY, GEMINI_API_KEY, AWS_*)
```

### Use

```bash
# One-shot / interactive CLI session (persistent, streamed in the terminal)
otto run "Refactor the factorial helper to be iterative."

# gpt-5 via the Responses API (native OpenAI or through Copilot)
otto --model openai/gpt-5 run "Explain this stack trace."
otto --model github-copilot/gpt-5 run "Explain this stack trace."

# Point at a local Ollama / any OpenAI-compatible endpoint (via config)
otto --model ollama/llama3.1 run "Summarize this file."

# Native TUI (auto-spawns a local server; ? for help)
otto tui
otto tui --server http://host:4096      # attach to a remote otto serve

# opencode-compatible HTTP/SSE API
otto serve --port 4096

# Driven workflow engines (TDD / SDD / plan-execution)
otto workflow tdd --feature "add retry backoff"
otto workflow sdd --plan path/to/plan.md
otto workflow plan --plan path/to/plan.md --auto

# Introspection
otto models | otto providers | otto agent list | otto mcp list | otto worktree list
```

## Architecture

A 20-crate workspace under `crates/`, separated by responsibility.

| Crate | Purpose |
| :--- | :--- |
| `otto-id` | Sortable / monotonic IDs. |
| `otto-events` | The provider-neutral `LLMEvent` union, `Usage`, event bus. |
| `otto-llm` | SDK-free LLM client: six wire protocols, two transports, models.dev registry. |
| `otto-tools` | `Tool` trait + 14 built-ins + truncation + registry. |
| `otto-hooks` | Lifecycle hooks — user-configured external commands fired at pipeline points (pre/post-tool-use, compaction, etc.). |
| `otto-storage` | Session/message/part model over SQLite (`sqlx`). |
| `otto-session` | The agent loop: convert, event→part processor, tool-augmented stream, run loop, compaction, retry, subagents. |
| `otto-workflow` | Deterministic drivers above the run loop: working `tdd`/`sdd`/`plan` engines, `judge()` node, ledger/gate/verify. |
| `otto-permission` | Ruleset evaluation + ask/reply gate. |
| `otto-question` | Interactive question ask/reply gate (sibling of `otto-permission`, no ruleset/mode). |
| `otto-agent` | Agent identity, config merge, subagent-permission narrowing. |
| `otto-config` | Config load + provider overrides (custom base URLs / keys). |
| `otto-auth` | Credential store (0600 `auth.json`), API-key + OAuth device flows. |
| `otto-mcp` | Model Context Protocol client integration. |
| `otto-lsp` | Language-server clients + post-edit diagnostics. |
| `otto-vcs` | Local Git operations, worktree service, file listing. |
| `otto-app` | Route/auth wiring that composes providers + credentials. |
| `otto-server` | The opencode-compatible HTTP/SSE API (`axum`). |
| `otto-cli` | The `otto` binary. |
| `otto-tui` | The `ratatui`/`crossterm` TUI (a pure HTTP/SSE client of the server). |

## Testing

```bash
cargo test --workspace                          # 1400+ tests
cargo test -p otto-llm -- --ignored            # live Anthropic (needs ANTHROPIC_API_KEY)
cargo test -p otto-lsp -- --ignored            # live rust-analyzer (skips if not on PATH)
```

## Status & caveats

Actively developed toward parity with upstream opencode. Known limitations:

- **OpenAI Responses (v1):** streaming reasoning + tool calls work; reasoning *input-replay* across turns is intentionally dropped, and hosted (provider-executed) tools + the WebSocket transport are deferred.
- **share / sync / GitHub-app** features are deliberately not ported (they depend on opencode's hosted cloud backend).
- **Workflow engine (`otto-workflow`)** ships three working driver engines (`otto workflow tdd|sdd|plan`) above the run loop; ongoing hardening as usage grows.
- **Anthropic OAuth** endpoints are `TODO(confirm)` — the API-key path is solid; OAuth is unverified against a live login.
- **Bedrock** reads AWS credentials from environment only (no profile/SSO chain); **Gemini** is AI-Studio only (no Vertex).
- The server mirrors opencode API *shapes* but has not been byte-diffed against an external OpenAPI spec.

## License

Apache-2.0. See [LICENSE](LICENSE).

---
*A Rust port built for robust, reproducible agentic workflows.*
