# Workflows

otto ships three native dev-loop engines — `tdd`, `sdd`, and `plan` — driven by `otto workflow <kind>` or `POST /workflow/{kind}`.

## Choosing an engine

| | `tdd` | `sdd` | `plan` |
|---|---|---|---|
| For | one feature, test-first | a plan whose tasks are independent | a plan whose tasks are ordered and dependent |
| Input | `--feature "<text>"` | `--plan <file>` | `--plan <file>` |
| Dispatch | single track, many phases | **parallel** (one batch) | **sequential** (one at a time) |
| Git worktrees | no | yes, one per task | no |
| Commits | yes — commits the failing test | no (merges land unstaged) | yes — implementers commit their own work |
| Review→fix loop | no (LLM judge on the test only) | yes, bounded | no |
| Verification gate | no (real test-suite runs instead) | no | yes, after every task |
| Halts whole run on failure | yes | no — degrades one task | yes |
| Writes the ledger | **no** | yes | yes |

<!-- src: crates/otto-workflow/src/{tdd,sdd,plan}.rs -->

## `otto workflow tdd --feature "<text>"`

A state machine over `TddPhase`: `WriteTest, VerifyRed, GreenImpl, VerifyGreen, Regression, Refactor, Done`.
<!-- src: crates/otto-workflow/src/tdd.rs:19-28 -->

> [!NOTE]
> `Refactor` is declared in the enum but no code path ever pushes or reaches it. Treat it as a known wart, not a phase you can rely on.

### The Iron Law

No production code is accepted before a test has been seen to fail for a genuine reason. This is enforced structurally rather than by a check: `VerifyRed` sits on the only path out of the red loop into `GreenImpl`.
<!-- src: crates/otto-workflow/src/tdd.rs:1-3,122-130 -->

### The red loop

`max_attempts` defaults to **3** and bounds the loop.

1. Spawn a subagent to write one minimal failing test (retries carry a note that the previous test "did not fail for a real reason").
2. An LLM judge answers `{"meaningful": bool, "reason": string}`. If not meaningful, the loop retries **without running the suite at all** — `VerifyRed` is skipped on that attempt.
3. Otherwise run the suite and classify with `classify_red`: `GenuineRed` breaks out to green; `CompileError` and `Passed` bounce back to `WriteTest`.

<!-- src: crates/otto-workflow/src/tdd.rs:77-130 -->

### Committing the red

Once a genuine red is reached, the engine runs `git add -A` + `git commit -q -m "test: failing test for {feature} (red)"` before any production code exists. That makes the implementation the *only* working-tree delta at regression time, so the regression stash removes the impl and never the test — including when test and impl live in the same file.
<!-- src: crates/otto-workflow/src/tdd.rs:132-142,224-232 -->

### Green and regression

The green loop (`GreenImpl` → `VerifyGreen`) is bounded by the same `max_attempts`. Then the regression check:

```bash
git stash push --include-untracked   # remove the impl (test is committed)
# run the suite: it must be a GenuineRed
git stash pop                        # restore the impl
# run the suite again: it must pass
```

<!-- src: crates/otto-workflow/src/regression.rs:77-124 -->

If `git stash pop` fails, the production change is **not lost** — it stays in the stash, and the error text says so.

### Failure messages

| Message | Raised by |
|---|---|
| `no genuine failing test after {n} attempts` | red loop exhausted `max_attempts` |
| `implementation did not pass the test after {n} attempts` | green loop exhausted `max_attempts` |
| `regression: the test passes even without the implementation` | suite passed with the impl stashed |
| `regression: removing the production change broke compilation, so the test's genuine failure cannot be verified` | stashed run was a `CompileError` |
| `regression: suite did not pass after restoring the production change` | post-`stash pop` run failed |
| `regression: failed to restore the production change with git stash pop (…)` | `stash pop` collided |

<!-- src: crates/otto-workflow/src/tdd.rs:81-84,149-152,178-182; regression.rs:93-122 -->

### `classify_red` heuristic

A run is `CompileError` only when a compile signature is present **and** no test-ran signature is:

| | Signatures |
|---|---|
| compile | `error[E`, `could not compile`, `syntaxerror`, `error: expected`, `cannot find` |
| test ran | `test result:`, `running ` (trailing space), `panicked` |

Everything else that failed is `GenuineRed`; anything that passed is `Passed`. Matching is case-insensitive except for `error[E`, `test result:`, and `running `.
<!-- src: crates/otto-workflow/src/classify.rs:22-41 -->

### Test command auto-detection

First marker match wins:

| Marker in the working directory | Command |
|---|---|
| `Cargo.toml` | `cargo test` |
| `package.json` | `npm test` |
| `pyproject.toml` or `pytest.ini` | `pytest` |
| `go.mod` | `go test ./...` |

No marker → no command, and the runner cannot verify anything.
<!-- src: crates/otto-workflow/src/runner.rs:55-67 -->

`tdd` writes nothing to the ledger.

## `otto workflow sdd --plan <file>`

Parses the plan, dispatches every task's implementer in parallel into its own git worktree, merges each success back into the shared tree, then runs a bounded review→fix loop per task.

### Plan file format

The parser is strict. Each heading must be `line.strip_prefix("### Task ")`:
<!-- src: crates/otto-workflow/src/sdd.rs:58-67 -->

- exactly `### Task ` at **column 0** — three hashes, one space, literal `Task`, one space
- no leading whitespace; `#### Task 1` does **not** match
- then `N: Title`, or a bare `N` (title empty)
- `N` must parse as a `u32`; anything else makes the line ordinary body text
- the title splits on the **first** `:` — later colons stay in the title
- everything before the first heading is parsed and discarded
- a task's body is every line until the next heading, trimmed

```markdown
# Export session slugs

Preamble before the first heading is parsed and discarded.

### Task 1: Add slug helper
Add `pub fn slugify(raw: &str) -> String` to `crates/otto-vcs/src/lib.rs`.
Verify: `cargo test -p otto-vcs slugify` passes.

### Task 2: Wire slug into export
Call `slugify` from `export_filename` in `crates/otto-cli/src/commands.rs`.
Verify: `cargo test -p otto-cli export` passes.
```

Task bodies are free-form and pasted verbatim into the implementer prompt. Give each task concrete file paths and an acceptance criterion.

### Worktrees

Created sequentially before the parallel dispatch (concurrent `git worktree add` against one repo buys nothing and risks correctness):

| | Value |
|---|---|
| name | `sdd-task-{task index}` — the index from the heading |
| branch | `otto/sdd-task-{task index}` |
| directory | `<data_root>/<name>`, where `data_root` = `global_data_dir()/worktree/<project-slug>` |
| command | `git worktree add -b <branch> <path> HEAD` |
| collision | appends `-2` … `-50`, then errors |

<!-- src: crates/otto-workflow/src/sdd.rs:299-322; crates/otto-vcs/src/worktree.rs:134-179 -->

> [!WARNING]
> The worktree branches off the **current HEAD**, not the repository's default branch. Whatever is committed on your current branch is the baseline every implementer sees.

### Dispatch and merge-back

Implementers go out as ONE parallel batch (`spawn_many`) and are told: *"DO NOT run any git commands (no add / stage / commit) — the workflow manages version control."*
<!-- src: crates/otto-workflow/src/sdd.rs:520-532 -->

Merge-back is sequential and only runs for `Done` / `DoneWithConcerns`:

```bash
git add -A           # in the task worktree
git diff --cached    # capture the patch
git reset            # unstage again
git apply <patch>    # into the primary tree
```

The result lands in the primary working tree **unstaged and uncommitted**. Run `sdd` on a dedicated feature branch so you can inspect and commit it yourself. Every worktree is removed once its task finishes, win or lose (a failed removal is logged and leaks harmlessly).
<!-- src: crates/otto-vcs/src/worktree.rs:304-330; sdd.rs:359-398 -->

### Review→fix loop

`max_fix_rounds` defaults to **2**, so at most 3 reviews and 2 fixes. Exceeding it records `DONE_WITH_CONCERNS` with the note `unresolved review findings`. Phase B is sequential and runs directly against the shared working tree.
<!-- src: crates/otto-workflow/src/sdd.rs:434-502 -->

### Status protocol

Implementers must end their reply with one JSON line:

```json
{"status": "DONE"}
```

Valid values: `DONE`, `DONE_WITH_CONCERNS`, `NEEDS_CONTEXT`, `BLOCKED`. (`CANCELLED` exists but is engine-assigned only.) The parser scans every `{…}` candidate in the output — brace-matching honors nesting and JSON strings — and keeps the **last** one that deserializes. No marker at all → `NeedsContext` with the note `no status marker in output (possibly a rejected permission ask ended the turn early)`.
<!-- src: crates/otto-workflow/src/sdd.rs:81-144,349-355; error.rs:34-44 -->

The reviewer returns `{"approved": bool, "findings": [string]}`, parsed the same way.

### Failure isolation

`sdd` almost never fails the whole run. Each of these degrades exactly one task and lets the rest continue:

| Failure | Task status |
|---|---|
| worktree creation failed | `Blocked` — `failed to create an isolated worktree: {e}` |
| implementer failed to spawn/run | `Blocked` — `implementer failed to spawn/run` |
| merge conflict on apply | `Blocked` — `implementer succeeded but its changes failed to merge into the shared working tree: {e}` |
| review verdict unparseable (`WfError::Parse`) | `NeedsContext` — `review verdict unusable: {e}` |
| review spawn/infra failure | `Blocked` — `review verdict unusable: {e}` |
| fix subagent failed | `Blocked` — `fix failed: {e}` |
| cancelled before start / before review | `Cancelled` |

Only two hard errors abort the run: a **duplicate task index** (`plan has duplicate task index {n}`, checked before anything is spawned, because ledger rows are keyed on the index) and a **ledger write failure**.
<!-- src: crates/otto-workflow/src/sdd.rs:231-241,324-330,348-374,439-458,492-501 -->

> [!WARNING]
> Parallel isolation assumes tasks touch **disjoint files**. Nothing enforces it — the assumption is described in the source as "expected, unenforced" and is caught only reactively, when `git apply` fails and the conflicting task degrades to `Blocked`. Write plans whose tasks do not share files.
> <!-- src: crates/otto-workflow/src/sdd.rs:184-186 -->

## `otto workflow plan --plan <file>`

Same plan-file format as `sdd`. Everything else differs:

- **Sequential** — one implementer spawn at a time, in heading order.
- **No worktrees** — everything happens in the primary working tree.
- Implementers are told to **commit their work** ("you are the only agent working in this tree"), which is safe precisely because the run is sequential.
- **No review→fix loop.**
- A `VerificationGate` runs after **every** task.
- Any blocker **halts the whole run**.

<!-- src: crates/otto-workflow/src/plan.rs:1-6,142-173 -->

### The verification gate

| Claim | Command |
|---|---|
| `Builds` | `cargo build` |
| `TestsPass` | `cargo test` |
| `LintClean` | `cargo clippy -- -D warnings` |
| `Formatted` | `cargo fmt -- --check` |

`PlanWorkflow` uses only `[Builds, TestsPass]`. Each command gets a 600 s timeout.
<!-- src: crates/otto-workflow/src/verify.rs:25-41; plan.rs:47,106 -->

> [!WARNING]
> **The claim→command mapping is cargo-only and gated on `Cargo.toml` existing in the run directory.** In any other project, every claim maps to `None`, the gate is built empty, and an empty report is **vacuously passed** (`all_passed()` over zero results is `true`). `plan` therefore silently accepts every task outside a Rust project. Use `--dry-run` first: it prints `(none — no cargo/known toolchain detected in this directory)` when this is about to happen.
> <!-- src: crates/otto-workflow/src/verify.rs:25-41,63-92 -->

### Halt messages

```text
task {i} reported {STATUS} — halting (executing-plans stops on a blocker)
task {i} failed verification: {claims}
```

The first fires when an implementer reports `BLOCKED` or `NEEDS_CONTEXT`; the driver halts *before* dispatching the dependent next task. The second lists the failed claims (`Builds`, `TestsPass`) and is preceded by a best-effort ledger write of `BLOCKED` / `verification failed`.
<!-- src: crates/otto-workflow/src/plan.rs:96-133 -->

## `--dry-run` and `--auto` / `-y`

Both flags exist on all three subcommands.

### `--dry-run`

Short-circuits before any runtime loads: no LLM, no session, no working-tree change.

- `tdd` prints a fixed notice describing the cycle.
- `sdd` / `plan` read and parse the plan file and print each task with its index, title, body-line count, and an ~80-char preview of its first non-empty line.
- `plan` additionally prints the verification commands, or `(none — no cargo/known toolchain detected in this directory)`.
- An empty parse is a hard error: ``no `### Task N` sections found in {path}`` — this check also runs on real (non-dry) `sdd`/`plan` invocations.

<!-- src: crates/otto-cli/src/workflow.rs:142-214,263-265,323-325 -->

### `--auto` / `-y`

Two effects at once: the workflow session is put in `FullAuto` permission mode (children inherit it live via the parent chain), and the non-interactive permission responder is set to auto-approve.

- Danger patterns still resolve to `Ask`, because the danger ruleset is the highest-precedence layer and outranks full-auto. On a TTY you are prompted for them even under `--auto`.
- Without `--auto`, a non-TTY run auto-rejects every ask with `permission auto-rejected (non-interactive; pass --yes to allow)`. A rejected ask can end an implementer's turn early, which surfaces as `NEEDS_CONTEXT` with `no status marker in output`.
- Ctrl-C cancels through the shared abort token, which is also `WfCtx.abort` — it stops new dispatches, not just the permission pump.

<!-- src: crates/otto-cli/src/workflow.rs:79-104; crates/otto-cli/src/run.rs:216-252 -->

See [permissions](./permissions.md) for the full layering rules.

## Ledger

`sdd` and `plan` upsert one row per `(session, kind, task_index)` into the sqlite `workflow_task` table, with the deterministic id `"{session}:{kind}:{index}"`. Later writes for the same task overwrite the earlier ones, so the table holds each task's *latest* state, not a history.

| Column | Notes |
|---|---|
| `id` | `{session}:{kind}:{index}` |
| `session_id` | the workflow's root session |
| `workflow_kind` | `sdd` or `plan` |
| `task_index` | from the `### Task N` heading |
| `status` | `DONE` / `DONE_WITH_CONCERNS` / `NEEDS_CONTEXT` / `BLOCKED` / `CANCELLED` |
| `notes` | free text, `NULL` when empty |
| `updated_at` | epoch millis |

<!-- src: crates/otto-workflow/src/ledger.rs:39-63; crates/otto-storage/src/store.rs:145-155 -->

The CLI renders the ledger after an `sdd` or `plan` run. To inspect it later:

```bash
sqlite3 ~/Library/Application\ Support/otto/otto.db "SELECT * FROM workflow_task"
```

## Running workflows from the server or TUI

`POST /workflow/{kind}` with body `{"arg": "<plan path or feature text>", "parent": "<session id>"}` starts a workflow on a background task and returns `{"session": "<id>"}` immediately. `kind` must be `tdd`, `sdd`, or `plan`. The `parent` field links the workflow session into the permission service's parent chain, so it — and every subagent under it — inherits the caller's permission mode live. The engine emits `workflow.started` then `workflow.done` on the `/event` bus, and `POST /workflow/{session}/cancel` stops it.
<!-- src: crates/otto-server/src/lib.rs:213-214,1127-1175 -->

The TUI command palette (`ctrl+k`) offers `Workflow: SDD…`, `Workflow: Plan…`, and `Workflow: TDD…`, which post to the same endpoint parented to the current chat session.
<!-- src: crates/otto-tui/src/state.rs:2740-2750 -->

See [the HTTP API reference](../reference/http-api.md) and [the TUI guide](./tui.md).
