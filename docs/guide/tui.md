# TUI Guide

The `otto tui` terminal client — layout, the complete keymap, overlays, permission modes, and theming.

## What it is

The TUI is a pure HTTP/SSE client of `otto serve`. It holds no session state of its own beyond view state: turns stream from `POST /session/{id}/message` and the global bus (workflow progress, permission asks) arrives on `/event`.

```bash
otto tui                                    # auto-spawns a local server
otto tui --server http://127.0.0.1:4096     # attach to an existing one
otto tui --server https://box:4096 --password hunter2
```

With `--server` absent, a local server process is spawned and its base URL used; with `--server` present nothing is spawned. <!-- src: crates/otto-tui/src/lib.rs:69-76 -->

On startup the TUI loads the agent/model/session catalogs, reopens the most recently active session (creating one if there are none), and replays its history.

## Layout

Rows, top to bottom: <!-- src: crates/otto-tui/src/view.rs:47-70 -->

| Row | Contents |
| --- | --- |
| Header (1 line) | `otto · <session> · <agent> · <model> · mode:<permission-mode>` on the left; a colored status dot + word on the right, with a spinner and elapsed time while busy. Segments are dropped right-to-left (model → agent → session) at narrow widths so the status never clips. |
| Transcript | Scrollable message/tool history. Renders a `▼ more` hint when scrolled off the bottom. |
| Todo panel | Only when todos exist and `ctrl+o` has them shown. |
| Attachment chip | Only when files are attached. |
| Activity line | Only while a turn is in flight. |
| Input | Grows with content up to a cap. |
| Hints (2 lines) | Footer key hints, plus workflow chords while a workflow runs, plus token/cost usage and a `NN% ctx` context-window gauge. |

Note that **token and cost usage live in the footer hints row, not the header**: `<N> tok · $<cost> · Σ <session total>`, followed by `NN% ctx` colored muted below 70%, warn at 70–90%, error above 90%. Cost is omitted when it rounds below `$0.001`. <!-- src: crates/otto-tui/src/view.rs:879-893, crates/otto-tui/src/state.rs:2543-2578 -->

The header's `mode:` segment is color-coded: dim for `approve-each`, yellow for `accept-edits`, bold red for `full-auto`. <!-- src: crates/otto-tui/src/view.rs:257-264 -->

## Keymap — complete

> [!NOTE]
> The in-app `?` help (`HELP_FULL`) is **incomplete** — it omits suspend, workflow cancel/status, undo/redo, the `ctrl+j` newline alias, all scroll and cursor-navigation keys, and the search overlay's `n`/`N` match jumps. This table is authoritative; it is derived from `on_key` in full. <!-- src: crates/otto-tui/src/view.rs:137, crates/otto-tui/src/input.rs:410-854 -->

Keys marked **†** exist in code but are absent from the in-app help.

### Global (fire from any context unless noted)

| Key | Action | Notes |
| --- | --- | --- |
| any key | Dismiss the startup splash | Swallowed — the first keystroke does not also type. <!-- src: crates/otto-tui/src/input.rs:413-416 --> |
| `ctrl+c` | Quit | Wins over every overlay. |
| `ctrl+n` | New session | Clears the transcript / context. |
| `ctrl+z` **†** | Suspend to the shell (SIGTSTP) | Unix only. Raw mode delivers it as a key, so the TUI self-suspends. <!-- src: crates/otto-tui/src/input.rs:428 --> |
| `ctrl+k` | Command palette | Only when no overlay is open. |
| `ctrl+f` | File-attachment picker | Only when no overlay is open. |
| `ctrl+x` **†** | Cancel the in-flight workflow | Only fires while a workflow exists and is not done; otherwise swallowed (never types a stray `x`). <!-- src: crates/otto-tui/src/input.rs:452 --> |
| `ctrl+w` **†** | Toggle the workflow status overlay | Works whether or not that panel is the active overlay. <!-- src: crates/otto-tui/src/input.rs:462 --> |
| `ctrl+_` **†** | **Undo** the prompt buffer | `ctrl+shift+_` **redoes**. Only when no overlay is open — overlay text buffers are not tracked by the editor's undo stack. `ctrl+z` and `ctrl+y` were already taken (suspend, yank), hence the readline `ctrl+_` convention. <!-- src: crates/otto-tui/src/input.rs:471-480 --> |

### Editor / main focus (no overlay open)

| Key | Action | Notes |
| --- | --- | --- |
| `enter` | Send the message | With an empty input **and** a selected tool, toggles that tool's detail instead. Refuses while a turn is in flight, flashing `turn in flight — Esc to interrupt it first`. <!-- src: crates/otto-tui/src/input.rs:738-756 --> |
| `shift+enter` | Newline | |
| `ctrl+j` **†** | Newline | Alias of `shift+enter`, for terminals that don't send it. <!-- src: crates/otto-tui/src/input.rs:734 --> |
| `esc` | Interrupt the running turn | Checked first, so it wins whenever the agent is busy. Keeps the session. |
| `esc` | Clear the tool cursor, scroll to bottom | Only when the input is empty, no turn in flight, and a tool is selected. |
| `↑` / `↓` | Select previous / next tool | **Only when the input is empty.** With text in the buffer they move the cursor between lines. |
| `ctrl+t` | Toggle tool detail | |
| `ctrl+g` | Agent picker | |
| `ctrl+o` | Toggle the todos panel | |
| `ctrl+y` | Yank the last assistant message (OSC-52) | |
| `shift+tab` | Cycle permission mode | |
| `?` | Help overlay | **Only when the input is empty.** |
| `/` | Search overlay | **Only when the input is empty.** |
| `@` | Inline file/folder mention | Only at a word boundary (preceding char is whitespace or start of buffer); a mid-word `@` types literally. <!-- src: crates/otto-tui/src/input.rs:838-847 --> |

### Scrolling and cursor navigation (editor focus)

| Key | Action | Notes |
| --- | --- | --- |
| `PageUp` **†** | Scroll the transcript up | <!-- src: crates/otto-tui/src/input.rs:809 --> |
| `PageDown` **†** | Scroll the transcript down | <!-- src: crates/otto-tui/src/input.rs:810 --> |
| `End` **†** | Scroll to bottom | **Only when the input is empty.** <!-- src: crates/otto-tui/src/input.rs:811 --> |
| `←` / `→` **†** | Move cursor left / right | |
| `↑` / `↓` **†** | Move cursor up / down a wrapped row | Only with text in the buffer (empty input selects tools instead). |
| `Home` **†** | Cursor to line start | |
| `End` **†** | Cursor to line end | Only with text in the buffer. <!-- src: crates/otto-tui/src/input.rs:812-835 --> |
| `Backspace` | Delete the char before the cursor | |

There is **no mouse-wheel scrolling**: the TUI deliberately does not enable mouse capture, so the terminal keeps its native selection and scrollback. <!-- src: crates/otto-tui/src/lib.rs:718 -->

## Overlays

While any overlay is open, the global keys above still fire; everything else is scoped to the overlay.

### Permission prompt

| Key | Action |
| --- | --- |
| `y` | Allow once |
| `a` | Allow **always** |
| `n` | Deny |
| `esc` | Close the overlay |

<!-- src: crates/otto-tui/src/input.rs:667-669 -->

> [!WARNING]
> `a` grants **broadly**, not just for this one call. An Always approval registers the asked pattern in the session's approval ruleset — one Always on an edit grants every edit for the rest of the session. Use `y` when you mean "this one". See [permissions](./permissions.md).

### Question prompt

| Key | Action |
| --- | --- |
| `↑` / `↓` | Move the highlight |
| `space` | Toggle the highlighted option |
| `enter` | Submit |
| `esc` | Cancel the question |

<!-- src: crates/otto-tui/src/input.rs:665, 707-716 -->

### Multi-agent dashboard

| Key | Action |
| --- | --- |
| `↑` / `↓` | Move the selection |
| `enter` | Open the selected session (peek / reply) |
| `/` | Enter filter mode |
| `c` | **Create a new session** — enters new-session mode and prompts for a title |
| `p` | Toggle pin on the selected session |
| `y` / `a` / `n` | Answer a permission ask belonging to a dashboard session |
| `1`–`9` | Answer a dashboard question by option number |
| `esc` | Close the dashboard |

<!-- src: crates/otto-tui/src/input.rs:670-706 -->

> [!NOTE]
> `c` is **new session**, not cancel. In filter mode and new-session mode the dashboard captures all typing: `esc` returns to browsing (clearing and re-applying an empty filter when leaving filter mode), `enter` confirms, `backspace` deletes. <!-- src: crates/otto-tui/src/input.rs:635-663, 702-705 -->

### File picker (`ctrl+f`)

| Key | Action |
| --- | --- |
| `enter` | **Toggle** attachment on the highlighted file — the picker stays open, so several files can be attached in one visit |
| `↑` / `↓` | Move the highlight |
| `backspace` | Delete a char from the filter query |
| any unmodified char | Type into the filter query |
| `esc` | Close |

<!-- src: crates/otto-tui/src/input.rs:483-500, crates/otto-tui/src/state.rs:1696-1713 -->

### Command palette (`ctrl+k`)

| Key | Action |
| --- | --- |
| `enter` | Run the highlighted command |
| `↑` / `↓` | Move the highlight |
| `backspace` | Delete a char from the query |
| any unmodified char | Fuzzy-filter (subsequence match, case-insensitive) |
| `esc` | Close |

Ctrl/alt chords never leak a bare letter into the query. <!-- src: crates/otto-tui/src/input.rs:501-522 -->

### Search (`/`)

| Key | Action |
| --- | --- |
| `n` | Jump to the **next** match |
| `N` | Jump to the **previous** match |
| `backspace` | Delete a char from the pattern |
| any other unmodified char | Type into the pattern |
| `enter` | Deliberate no-op — search stays open |
| `esc` | Close |

<!-- src: crates/otto-tui/src/input.rs:553-588 -->

> [!NOTE]
> Because `n`/`N` are intercepted for match navigation, a literal `n` or `N` cannot be typed into the search pattern. This is a deliberate tradeoff for the single-key jump.

### `@` mention dropdown

| Key | Action |
| --- | --- |
| `enter` / `tab` | Accept the highlight. With no match this only dismisses — it **never** submits the message |
| `↑` / `↓` | Move the highlight |
| `shift+enter` / `ctrl+j` | Insert a newline and dismiss the dropdown |
| `space` | Type the space and dismiss (a space delimits the token) |
| `backspace` | Delete a char and re-filter |
| `esc` | Dismiss the dropdown |
| any other key | Dismiss, swallowed |

<!-- src: crates/otto-tui/src/input.rs:589-630 -->

### Text-input overlay (workflow prompts)

Used by the `Workflow: …` palette entries to collect a plan path or feature description. `enter` confirms, `backspace` deletes, `esc` closes, unmodified chars type. When an `@` mention is active inside it, `↑`/`↓` move the mention highlight, `tab`/`enter` accept it (Enter does **not** fall through and start the workflow), and `esc` dismisses just the mention — a second `esc` closes the overlay. <!-- src: crates/otto-tui/src/input.rs:523-552 -->

### Simple pickers (sessions / models / agents) and help

`↑` / `↓` move, `enter` confirms, `esc` closes. These pickers have no type-to-filter. <!-- src: crates/otto-tui/src/input.rs:717-724 -->

## Command palette items

`ctrl+k` opens a fuzzy-filtered list. Labels ending in `…` open a picker or prompt rather than acting immediately. <!-- src: crates/otto-tui/src/state.rs:2729-2754 -->

| Item | Key hint | Effect |
| --- | --- | --- |
| New session | `ctrl+n` | Start a fresh session |
| Switch session… | — | Open the session picker |
| Dashboard… | — | Open the multi-agent dashboard |
| Change model… | — | Open the model picker |
| Change agent… | `ctrl+g` | Open the agent picker |
| Toggle tool detail | `ctrl+t` | Expand/collapse tool output |
| Help | `?` | Open the help overlay |
| Quit | `ctrl+c` | Exit |
| Attach file… | `ctrl+f` | Open the file picker |
| Workflow: SDD… | run subagent-driven dev on a plan file | Prompt for a plan path, then start |
| Workflow: Plan… | execute a plan file with verification | Prompt for a plan path, then start |
| Workflow: TDD… | drive a TDD cycle for a feature | Prompt for a feature, then start |

## Permission modes

`shift+tab` cycles the session's permission mode: <!-- src: crates/otto-tui/src/lib.rs:497-503 -->

```text
approve-each → accept-edits → full-auto → approve-each
```

| Mode | Header color | Behavior |
| --- | --- | --- |
| `approve-each` | dim | Every gated tool call asks (the safe default). |
| `accept-edits` | yellow | Edits are auto-approved; other gated calls still ask. |
| `full-auto` | bold red | Everything auto-approves except the built-in danger ruleset, which always asks. |

The mode resolves live up the parent chain, so a TUI session in full-auto reaches the workflow subagents it spawns. See [permissions](./permissions.md) for the full precedence order.

## Interrupting

- **`Esc`** while a turn is streaming interrupts that turn and keeps the session. The check runs before the tool-cursor and overlay `Esc` handling, so it wins whenever the agent is busy. <!-- src: crates/otto-tui/src/input.rs:760-762 -->
- **`ctrl+x`** cancels an in-flight workflow. The key emits the intent; the event loop issues the HTTP cancel. It only fires while a workflow run exists and has not finished. <!-- src: crates/otto-tui/src/input.rs:452-457 -->
- Submitting a second message mid-turn is refused (a single SSE stream at a time) with a `turn in flight — Esc to interrupt it first` flash.

## Theming

Theme selection precedence: `NO_COLOR` (set to any value) forces the monochrome theme and always wins; otherwise the `theme` config key applies. <!-- src: crates/otto-tui/src/lib.rs:80-113 -->

| `config.theme` | Result |
| --- | --- |
| `catppuccin` | Catppuccin Mocha |
| `gruvbox` | Gruvbox dark |
| `nord` | Nord |
| `base16` | Neutral base16 (Ocean-ish) — the explicit "default colored" |
| `light` | Light palette |
| `auto` | Detect the OS light/dark preference at startup and poll for changes; over SSH, falls back to an SSH-specific probe and then to dark |

<!-- src: crates/otto-tui/src/theme.rs:105-144 -->

Terminal color depth is detected once at startup from `COLORTERM` and `TERM`, and every theme is quantized to that depth. <!-- src: crates/otto-tui/src/appearance/mod.rs:25-33 -->

### Splash

The startup splash is shown only when stdout is a TTY and neither `--no-splash` nor the `otto_NO_SPLASH` environment variable is set. Note the **lowercase `otto_` prefix**. <!-- src: crates/otto-tui/src/splash.rs:99-103, crates/otto-tui/src/lib.rs:246 -->

```bash
otto tui --no-splash
otto_NO_SPLASH=1 otto tui
```

Any keystroke also dismisses the splash, and that keystroke is swallowed rather than typed.

## Yank and copy

- **`ctrl+y`** copies the last assistant message to the system clipboard using OSC-52 (`ESC ] 52 ; c ; <base64> BEL`), written straight to stdout. This works through SSH and tmux in terminals that honour OSC-52. <!-- src: crates/otto-tui/src/lib.rs:290-297, 788-798 -->
- **Native drag-select** still works: the TUI never enables mouse capture, so the terminal's own selection, copy, and scrollback are untouched. <!-- src: crates/otto-tui/src/lib.rs:718 -->
