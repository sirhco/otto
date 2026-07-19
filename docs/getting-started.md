# Getting started

Install otto, authenticate a provider, and run your first turn.

## Prerequisites

Rust 1.95 or newer, edition 2024.
<!-- src: Cargo.toml:27-31 (edition = "2024", rust-version = "1.95") -->

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

A pinned toolchain lives in `rust-toolchain.toml`; `cargo` picks it up automatically inside the repo.

## Install

There is no `cargo install`, no Homebrew formula, and no install script. Two options:

### Build from source

```bash
git clone https://github.com/<owner>/otto
cd otto
cargo build -p otto-cli        # produces target/debug/otto
```

The binary is named `otto` and is produced by the `otto-cli` crate.
<!-- src: crates/otto-cli/Cargo.toml [[bin]] name = "otto", path = "src/main.rs" -->

For an optimized build:

```bash
cargo build --release -p otto-cli   # target/release/otto
```

### Download a release tarball

Pushing a `v*` tag builds and publishes GitHub Release assets named
`otto-<tag>-<target>.tar.gz` for three targets:

| Target | Runner |
| --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `aarch64-apple-darwin` | `macos-latest` |
| `x86_64-apple-darwin` | `macos-latest`, cross-compiled from the arm runner |

<!-- src: .github/workflows/release.yml matrix + Package step -->

Each tarball contains a single `otto` binary at the archive root. **No checksums
or signatures are published** — the release job uploads only `dist/*.tar.gz`.
<!-- src: .github/workflows/release.yml, softprops/action-gh-release with files: dist/*.tar.gz -->

```bash
tar -xzf otto-v0.13.1-aarch64-apple-darwin.tar.gz
./otto --version
```

### Put it on PATH

```bash
# from a source build
install -m 755 target/release/otto ~/.local/bin/otto

# from a tarball
install -m 755 ./otto ~/.local/bin/otto
```

Ensure `~/.local/bin` is on `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"   # add to ~/.zshrc or ~/.bashrc
```

## Authenticate

`otto auth login <provider>` (aliased as `otto providers login <provider>`) walks
an interactive prompt. For `anthropic`:

```text
anthropic login:
  [1] API key
  [2] OAuth (Claude Pro/Max)
choice [1/2]:
```

<!-- src: crates/otto-cli/src/commands.rs, login(): the three println! lines + prompt("choice [1/2]: ") -->

Choosing `2` prints an authorize URL and reads a pasted code (PKCE exchange).
Choosing `1` — or any other provider id — falls through to a single API-key
paste: `<provider> API key:`.

Login **requires a TTY**. With stdin not a terminal it bails immediately:

```text
login requires an interactive terminal
```

<!-- src: crates/otto-cli/src/commands.rs, login(): `if !std::io::stdin().is_terminal() { bail!("login requires an interactive terminal"); }` -->

### Alternative: environment variables

With no stored credential, a provider falls back to its environment variable.
This is the path for CI and non-interactive shells.

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

See [guide/providers.md](guide/providers.md) for the per-provider table and
[reference/env.md](reference/env.md) for every variable otto reads.

## First run

```bash
otto run "list the files in this directory and summarize what this project is"
```

Useful flags:

| Flag | Effect |
| --- | --- |
| `--model <provider/model>` | Generate with this model instead of the configured default |
| `--agent <name>` | Run as a specific agent |
| `--session <id>` | Continue the session with this id |
| `--continue` | Continue the most recent session in this directory |
| `--yes` | Auto-allow permission requests when running non-interactively |

<!-- src: crates/otto-cli/src/cli.rs, RunArgs -->

Global flags apply to every subcommand: `--cwd <dir>`, `--log-level <level>`,
`--print-logs`.
<!-- src: crates/otto-cli/src/cli.rs, Cli -->

If no message argument is given, the prompt is read from stdin:

```bash
echo "explain crates/otto-session/src/run.rs" | otto run
git diff | otto run --yes "review this diff"
```

<!-- src: crates/otto-cli/src/cli.rs — "The prompt. Multiple words are joined; if omitted, read from stdin." -->

## First TUI session

```bash
otto tui
```

With no `--server`, the TUI auto-spawns a local `otto serve` process and attaches
to it; the server shuts down with the TUI.
<!-- src: crates/otto-tui/src/lib.rs — "Attach or auto-spawn." → spawn::spawn_local_server(&opts.cwd) -->

To attach to an already-running server instead:

```bash
otto serve --port 4096            # terminal 1
otto tui --server http://127.0.0.1:4096   # terminal 2
```

Press `?` on an empty input line to open the help overlay.
<!-- src: crates/otto-tui/src/input.rs — KeyCode::Char('?') if self.input.is_empty() -->

More in [guide/tui.md](guide/tui.md).

## Where otto puts things

otto uses the `dirs` crate, which resolves platform-native locations. **On macOS
`config_dir()` and `data_dir()` both resolve to `~/Library/Application Support`,
so config and data share one directory.** Linux splits them per XDG.
<!-- src: crates/otto-config/src/paths.rs — global_config_dir() uses dirs::config_dir(), global_data_dir() uses dirs::data_dir(), both joined with "otto" -->

| | macOS | Linux |
| --- | --- | --- |
| Config dir (read only, never created by otto) | `~/Library/Application Support/otto/` | `~/.config/otto/` |
| SQLite database | `~/Library/Application Support/otto/otto.db` | `~/.local/share/otto/otto.db` |
| Logs | `~/Library/Application Support/otto/logs/otto.log.YYYY-MM-DD` | `~/.local/share/otto/logs/otto.log.YYYY-MM-DD` |
| `auth.json` (mode `0600`) | `~/Library/Application Support/otto/auth.json` | `~/.local/share/otto/auth.json` |
| models.dev cache | `~/Library/Caches/otto/models.json` | `~/.cache/otto/models.json` |
| Worktrees | `~/Library/Application Support/otto/worktree/` | `~/.local/share/otto/worktree/` |

<!-- src: crates/otto-app/src/runtime.rs (global_data_dir()/otto.db), crates/otto-cli/src/logging.rs (global_data_dir()/logs), crates/otto-auth/src/store.rs (dirs::data_dir()/otto/auth.json, set_permissions 0o600), crates/otto-cli/src/lib.rs + crates/otto-app/src/runtime.rs (global_cache_dir()/models.json), crates/otto-cli/src/commands.rs (global_data_dir()/worktree) -->

`OTTO_CONFIG_DIR` overrides the **config** directory only. The data, cache, and
state directories ignore it — setting it does not relocate `otto.db`, logs,
`auth.json`, or the models cache.
<!-- src: crates/otto-config/src/paths.rs — the CONFIG_DIR_ENV early-return lives only in global_config_dir(); global_data_dir/global_cache_dir/global_state_dir read dirs::* directly -->

## Minimal config

The smallest working setup is **no config file at all** plus `ANTHROPIC_API_KEY`
in the environment. With `config.model` unset, otto defaults to
`anthropic/claude-sonnet-4-5`.
<!-- src: crates/otto-app/src/route_factory.rs — const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4-5"; used by default_model() -->

To pin a model, drop an `otto.json` at the project root:

```json
{
  "model": "anthropic/claude-opus-4-8"
}
```

Config files are discovered by walking **up** from the working directory,
stopping at the first ancestor containing `.git` (or the worktree root, or the
filesystem root). In each directory otto reads `config.json` (legacy),
`otto.json`, `otto.jsonc`, then the same names under `.otto/`. Files closer to
the working directory win.
<!-- src: crates/otto-config/src/loader.rs — FILE_NAMES, DOT_DIR_NAMES, discover() -->

JSONC (comments, trailing commas) is accepted. Full schema in
[reference/config.md](reference/config.md); generate it with:

```bash
cargo run -q -p otto-config --example gen_schema > schema/config.json
```

## Project instructions

otto loads `AGENTS.md` and `CLAUDE.md` into the system prompt. The loader walks
from the working directory up to the **filesystem root** — unlike config
discovery, it does not stop at `.git`. Within each directory `AGENTS.md` is read
before `CLAUDE.md`, and nearer directories come first.
<!-- src: crates/otto-session/src/system.rs, instructions(): `while let Some(current) = dir { for name in ["AGENTS.md", "CLAUDE.md"] ... } dir = current.parent();` — no .git check -->

Empty files are skipped. A practical layout:

```text
~/CLAUDE.md              # personal, global
myrepo/AGENTS.md         # project conventions
myrepo/CLAUDE.md         # project conventions (both are read)
```

The config keys `instructions` and `skills` are accepted by the schema and
merged by the loader, but **nothing reads them at runtime**. Do not rely on them —
use `AGENTS.md` / `CLAUDE.md`.
<!-- src: crates/otto-config/src/schema.rs (skills: Option<Value>, instructions: Option<Vec<String>>) and crates/otto-config/src/loader.rs (merge concatenates instructions); no consumer outside otto-config -->

## Next steps

| Topic | Doc |
| --- | --- |
| Providers, gateways, local models | [guide/providers.md](guide/providers.md) |
| Terminal UI | [guide/tui.md](guide/tui.md) |
| Agents and subagents | [reference/agents.md](reference/agents.md) |
| Permissions | [guide/permissions.md](guide/permissions.md) |
| Workflows (`otto workflow`) | [guide/workflows.md](guide/workflows.md) |
| CLI reference | [reference/cli.md](reference/cli.md) |
| Config schema | [reference/config.md](reference/config.md) |
| Environment variables | [reference/env.md](reference/env.md) |
| HTTP / SSE API | [reference/http-api.md](reference/http-api.md) |
