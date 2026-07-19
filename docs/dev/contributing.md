# Contributing

Build, test, and release mechanics for otto, plus the conventions and known warts a first change should be aware of.

Read [`architecture.md`](architecture.md) first if you have not — most non-obvious review feedback traces back to a pipeline invariant documented there.

## Build and test

```bash
cargo build -p otto-cli                 # produces target/debug/otto
cargo test --workspace                  # 1369 tests, headless, no network
cargo test -p otto-session --test run_loop            # one integration-test file
cargo test -p otto-tui state::tests::scroll_up_moves_away_from_bottom   # one test
cargo test -p otto-llm -- --ignored     # live Anthropic (needs ANTHROPIC_API_KEY)
cargo test -p otto-lsp -- --ignored     # live rust-analyzer (skips if not on PATH)
cargo clippy --workspace --all-targets  # must be clean (-D warnings discipline)
cargo fmt --all
```

The default suite touches no network and spawns no external processes. If a test you wrote needs either, it belongs behind `#[ignore]`.

Clippy is held to `-D warnings`. A PR with clippy output is not ready, and `#[allow(...)]` needs a comment saying why.

## Required after changing `otto-config`

Any change to the `Config` structs in `crates/otto-config/src/schema.rs` must regenerate the JSON schema:

```bash
cargo run -q -p otto-config --example gen_schema > schema/config.json
```

`schema/config.json` is checked in and is what editors use for completion and validation of a user's `otto.jsonc`. CI and review expect the regenerated file in the same commit as the struct change — a schema that lags the code is worse than no schema, because editors then flag valid config as invalid.

Update [`../reference/config.md`](../reference/config.md) in the same change. The schema tells an editor the shape; the reference tells a human what the field means.

## Testing conventions

Everything runs headless through scripted seams. Use the existing ones rather than inventing a new mock.

| Seam | Where | What it replaces |
| --- | --- | --- |
| `ScriptedRoute` | `crates/otto-session/tests/run_loop.rs` | A provider. Emits a canned `Vec<LLMEvent>` stream. |
| `RetryOnceRoute` | `crates/otto-session/tests/run_loop.rs` | A provider that fails the first attempt, then succeeds. |
| `ScriptedRouteFactory` + `Runtime::in_memory` | `crates/otto-app/tests/runtime.rs` | The whole runtime boot — config, auth, storage. |
| `Store::open_in_memory` | `otto-storage` | The on-disk SQLite database. |
| `TestBackend` renders | `crates/otto-tui` | A terminal. `render(&app)` returns the buffer as a string, so assertions are plain string matching. |

**Retry tests must not use `#[tokio::test(start_paused = true)]`.** The sqlx in-memory pool races a paused clock and the test hangs or flakes. Keep real backoffs short instead:

```rust
// either make the delay genuinely zero...
retry_after: Some(Duration::from_millis(0)),
// ...or cap the attempts so the total wait stays small
max_retries: 2,
```

When adding a `RunConfig` field, the test fixtures in `crates/otto-session/tests/{run_loop,subagent,mcp_loop,compaction}.rs` need updating alongside `crates/otto-app/src/runtime.rs` and `crates/otto-session/src/subagent.rs`. The compiler finds them one at a time; expect roughly eight sites.

## Versioning and releases

The version lives in exactly one place: `[workspace.package] version` in the root `Cargo.toml`. Every crate inherits it with `version.workspace = true`.

A release is three edits and a tag:

1. Bump `[workspace.package] version` in the root `Cargo.toml`.
2. Add a `## [x.y.z]` section to `CHANGELOG.md` in Keep-a-Changelog format.
3. Run a build so `Cargo.lock` picks up the new version, and commit it.
4. Tag `vx.y.z` and push the tag.

`.github/workflows/release.yml` fires on `v*` tags. It builds `cargo build --release -p otto-cli` for three targets and uploads a `otto-<tag>-<target>.tar.gz` per target:
<!-- src: .github/workflows/release.yml -->

| Target | Runner |
| --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `aarch64-apple-darwin` | `macos-latest` |
| `x86_64-apple-darwin` | `macos-latest`, cross-compiled |

The intel macOS runners are retired, so the x86_64 Darwin build cross-compiles from the arm runner. The workflow explicitly runs `rustup target add` *after* the toolchain action, because builds use the pinned toolchain from `rust-toolchain.toml` and the action only adds targets to stable — without that step the x86_64 build fails with a missing-`core` E0463.

## Code conventions

- **Edition 2024.** Let-chains (`if let Some(x) = a && let Some(y) = b`) are used freely; prefer them to nested `if let`.
- **Cite upstream when porting.** Ported behavior carries an inline `file.ts:line` comment pointing into `opencode/packages/`. Keep the citation accurate when you change the code, or replace it with a divergence note.
- **Mark divergences.** Anything that intentionally differs from upstream says so: "otto extension" or "no opencode analog". Whole crates without upstream counterparts (`otto-hooks`, `otto-vcs`) say it in the crate-level doc comment.
- **Doc comments explain why.** The existing comments are unusually dense about *why* a line is the way it is, frequently naming the bug it prevents. Match that — a comment restating what the code does is noise; one naming the failure mode is the reason the invariant survives the next refactor.
- **Match surrounding style.** Do not reformat, rename, or "improve" adjacent code in an unrelated change.

## Known warts

Real, verified, and not worth a drive-by fix inside an unrelated PR — but worth knowing before you spend an hour confused.

| Wart | Where |
| --- | --- |
| Five env vars use a lowercase `otto_` prefix while everything else uses `OTTO_`: `otto_SKILLS_DIR`, `otto_DEBUG_STREAM`, `otto_STREAM_IDLE_TIMEOUT_SECS`, `otto_NO_SPLASH`, and `otto_SERVER_PASSWORD` (via clap's `env =`). | `crates/otto-tools/src/tools/skill.rs`, `crates/otto-llm/src/transport/sse.rs`, `crates/otto-tui/src/{lib,splash}.rs`, `crates/otto-cli/src/cli.rs` |
| `config.instructions` and `config.skills` are parsed, typed, and merged, but nothing reads them at runtime. The `instructions(cwd)` used by the system prompt is an unrelated filesystem walk. | `crates/otto-config/src/schema.rs`; contrast `crates/otto-session/src/system.rs` |
| `TddPhase::Refactor` is declared on the enum and never constructed or matched anywhere else — the tdd engine never reaches it. | `crates/otto-workflow/src/tdd.rs` |
| The `explore` agent's ruleset grants a `list` permission, but no tool named `list` exists. The tool ids are grep, glob, read, write, edit, apply_patch, bash, task, skill, question, todowrite, webfetch, websearch, invalid. | `crates/otto-agent/src/builtins.rs` |
| The TUI's in-app help string omits several real bindings: `ctrl+z` (suspend), `ctrl+x`, `ctrl+w`, `ctrl+_` / `ctrl+shift+_` (undo/redo), and PageUp/PageDown (scroll). A test guards that every *palette* hint appears in the help, which is why these gaps survive. | `HELP_FULL` in `crates/otto-tui/src/view.rs`; bindings in `crates/otto-tui/src/input.rs` |
| `PlanWorkflow`'s verification gate is built from cargo-based claims only. In a non-cargo directory the gate is empty and every task verifies vacuously. | `crates/otto-workflow/src/{plan,verify}.rs` |

If you fix one of these, do it in its own commit.

## When a turn fails

Do not add logging blindly. The debugging procedure — log file locations, `OTTO_LOG` filters, and the SQLite queries that show whether a turn errored, truncated, or was killed mid-stream — is in the debugging section of [`../guide/cli.md`](../guide/cli.md).
