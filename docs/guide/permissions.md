# Permissions

Every mutating tool call asks otto's permission service, which resolves the request against layered rulesets and either allows it, denies it, or blocks on a human answer.

## Rule syntax

A rule is `{permission, pattern, action}`. Both `permission` and `pattern` are globs; `action` is `allow`, `deny`, or `ask`. Unknown action strings are silently dropped — a typo like `"Allow"` produces no rule at all.
<!-- src: crates/otto-permission/src/ruleset.rs:26-34,202-210 -->

Three config shapes are accepted under the `permission` key:
<!-- src: crates/otto-permission/src/ruleset.rs:70-114 -->

```jsonc
{ "permission": "allow" }                                  // bare → { "*": { "*": allow } }

{ "permission": { "edit": "deny", "bash": "allow" } }      // per-permission → pattern "*"

{ "permission": {                                          // per-pattern (the tool-specific form)
    "edit": { "*.rs": "allow", "*.lock": "deny" },
    "bash": { "git *": "allow", "rm *": "deny" },
    "read": { "~/secrets/*": "deny" }
}}
```

> [!NOTE]
> There is no `bash:git *` colon syntax. The tool-specific form is the nested object above — the permission name is the outer key, the pattern is the inner key.

Rule order follows the iteration order of the parsed JSON object, which matters because precedence is last-match-wins.

## Glob semantics

The matcher is a port of opencode's `util/wildcard.ts`:
<!-- src: crates/otto-permission/src/wildcard.rs:22-91 -->

| Construct | Meaning |
|---|---|
| `*` | any run of characters, including empty |
| `?` | exactly one character |
| `\` | normalized to `/` in both input and pattern |
| everything else | literal — regex metacharacters are **not** special |
| `.` in the *input* | matched only by a literal `.`, `?`, or `*` |

So `a.b` matches `a.b` but **not** `axb`. Matching is dotall: `*` and `?` cross newlines.

**Trailing ` *` (space-star) is special.** A pattern ending in a literal space followed by `*` compiles to `( .*)?`, making the space-suffix optional:

| Pattern | Input | Matches |
|---|---|---|
| `git *` | `git` | yes |
| `git *` | `git status` | yes |
| `git *` | `git ` | yes |
| `git *` | `gitx` | **no** |

<!-- src: crates/otto-permission/src/wildcard.rs:31-35,50-56,135-142 -->

## Precedence

Rulesets are flattened in order and the **last** rule whose permission glob and pattern glob both match wins. There is no specificity ranking:

```jsonc
{ "edit": { "*": "allow", "*.rs": "deny" } }   // .rs denied — only because it is written second
{ "edit": { "*.rs": "deny", "*": "allow" } }   // .rs ALLOWED — the broad rule now wins
```

When nothing matches at all, the default action is **`Ask`**.
<!-- src: crates/otto-permission/src/ruleset.rs:141-162 -->

`~`, `~/…`, `$HOME`, and `$HOME/…` expand to the home directory — in **pattern keys only**, never in the value being matched.
<!-- src: crates/otto-permission/src/ruleset.rs:181-195 -->

## Layering

Lowest to highest precedence:

```text
mode overlay < agent session ruleset < user config ruleset < in-session Always approvals < danger rules
```

Resolution is **two-phase**, once per pattern in the request:
<!-- src: crates/otto-permission/src/permission.rs:264-313 -->

1. **Deny gate** over `[agent session ruleset, config ruleset, session approvals]`. A `Deny` returns `PermissionDenied { by_user: false }` immediately — the tool errors and the turn continues, rather than stopping like a human rejection. This is why an agent's deny (plan mode's edit-deny outside `.otto/plans/`) holds in every mode, while anything the user stated explicitly still outranks the agent.
2. **Interactivity** over `[mode overlay, config ruleset, session approvals, danger ruleset]`. If every pattern resolves to `Allow`, the call returns `Ok` with no prompt; any `Ask` registers a pending request, publishes an `Asked` event, and blocks on a oneshot until someone replies.

> [!NOTE]
> The agent layer is **deliberately absent from phase two**. A builtin agent's broad `"*": "allow"` default therefore cannot bypass approve-each consent, and an agent's own `ask` rules (`doom_loop`, `external_directory`, `.env` reads) are answered by full-auto instead of raising a blocking prompt that reads as a silent hang.

Two registration duties exist wherever sessions are created: `link_parent` (so permission **mode** resolves live up the parent chain, walking at most 64 links) and `set_session_ruleset` (the agent's ruleset — agent metadata alone is not enforced).
<!-- src: crates/otto-permission/src/permission.rs:117-136,202-219 -->

## The three modes

Set per session as `permission_mode` (kebab-case). Each mode installs a ruleset as the **lowest-precedence layer**:
<!-- src: crates/otto-permission/src/mode.rs:9-17,106-120 -->

| Mode | Overlay rules |
|---|---|
| `approve-each` (default) | `("*", "*", Ask)` |
| `accept-edits` | `("*", "*", Ask)`, `("edit", "*", Allow)`, `("write", "*", Allow)` |
| `full-auto` | `("*", "*", Allow)` |

Under `accept-edits`, `bash` still prompts. `apply_patch` needs no rule of its own — it requests under the `edit` permission.

The cycle order is `approve-each → accept-edits → full-auto → approve-each`; the TUI cycles with `shift+tab`.
<!-- src: crates/otto-permission/src/mode.rs:19-28; crates/otto-tui/src/input.rs:792 -->

Because the overlay is the *lowest* layer, a config `deny` beats `full-auto`, and the `plan` agent's edit-deny holds in every mode.

## Danger rules

The highest-precedence layer. Every entry is `Ask`, never `Deny` — they force a prompt even in full-auto, but they can be approved.
<!-- src: crates/otto-permission/src/mode.rs:124-160 -->

Bash patterns:

```text
*rm -rf*        *mkfs*            *chmod *777*      *curl*|*sh*
*rm -fr*        *dd *of=/dev*     *chmod -R *777*   *wget*|*sh*
*git push*--force*                *> /dev/sd*       *sudo *
*git push*-f*
```

File patterns, applied to both `edit` and `write` (and so also to `apply_patch`, which requests under `edit`):

```text
*.env    **/.env    *id_rsa*    **/.ssh/*    *credentials*    *.pem
```

The leading and trailing `*` wrapping means these match **anywhere** in the value, not just at the start. `*git push*-f*` therefore also catches `git push --force-with-lease`, and `*sudo *` catches a `sudo` anywhere in a compound command.

## ⚠️ "Always" grants more than you approved

> [!WARNING]
> `Reply::Always` does not grant the pattern you were shown. It grants the request's `always` patterns, which are deliberately broader.
> <!-- src: crates/otto-permission/src/permission.rs:419-429 -->

| Permission | Pattern shown | `always` granted |
|---|---|---|
| `bash` | the exact command | `"{command} *"` — every invocation of that command with any args |
| `edit` | the repo-relative file path | `"*"` — **every file in the session** |
| `write` | the repo-relative file path | `"*"` |
| `apply_patch` | the touched paths | `"*"` |
| `todowrite` | `"*"` | `"*"` |
| `webfetch` | the URL | `"*"` — every URL |
| `websearch` | the query | `"*"` — every query |
| `external_directory` | `"<dir>/*"` | `"<dir>/*"` |
| `skill` | the skill name | the skill name |
| `doom_loop` | the tool name | the tool name |

<!-- src: crates/otto-tools/src/tools/{bash.rs:313-317,edit.rs:633-638,write.rs:69-75,apply_patch.rs:215-218,todo.rs:98-104,webfetch.rs:79-86,websearch.rs:111-117,skill.rs:266-272,mod.rs:77-84}; crates/otto-session/src/processor.rs:865-870 -->

Approving `cargo test` with **a** grants `cargo test --all-features -- --nocapture` too. Answering **a** to a single one-line file edit grants every edit for the rest of the session.

Two more properties of Always:

- It is **memory-only** — held in a `HashMap` keyed by session, lost on process exit, and scoped to that one session. It is not written to config.
- It **auto-resolves** other pending requests in the same session whose patterns are now all `Allow` under the session's approvals.

And `Reject` **cascades**: rejecting one request fails every other pending request in the same session. Only the request you actually answered carries your correction message; the cascaded ones carry none.
<!-- src: crates/otto-permission/src/permission.rs:392-452 -->

## Making a grant durable

Since Always is memory-only, the way to make a decision persist is a config rule — which also sits at a useful precedence: above the mode overlay and below in-session approvals.

```jsonc
{ "permission": { "bash": { "cargo test *": "allow", "git push *": "ask" } } }
```

See [the config reference](../reference/config.md).

The CLI prompt is:

```text
permission: {perm} {patterns} — allow? [y]es / [n]o / [a]lways:
```

| Input | Reply |
|---|---|
| `a`, `always` | `Always` |
| `y`, `yes` | `Once` |
| **anything else** | `Reject` (message `rejected by user`) |

On a non-TTY there is no prompt: with `--yes` every ask is approved `Once`, otherwise every ask is rejected with `permission auto-rejected (non-interactive; pass --yes to allow)`.
<!-- src: crates/otto-cli/src/run.rs:216-252 -->

## Permission names actually requested

These are the names a tool really passes to `ask`. A rule for any other name is inert.

| Permission | Pattern | Requested by |
|---|---|---|
| `bash` | the whole command string | `bash` |
| `edit` | repo-relative file path | `edit`, `apply_patch` |
| `write` | repo-relative file path | `write` |
| `external_directory` | `<dir>/*` | `read`, `grep`, `glob`, `write`, `edit`, `apply_patch`, `bash` — whenever a target escapes the session directory |
| `todowrite` | `"*"` | `todowrite` |
| `webfetch` | the URL | `webfetch` |
| `websearch` | the query | `websearch` |
| `skill` | the skill name | `skill` |
| `doom_loop` | the repeated tool's name | the session processor, when N identical tool calls repeat |
| `hook` | hook-supplied pattern | hook escalation |

<!-- src: grep for `permission: "` across crates/otto-tools/src and crates/otto-session/src -->

> [!NOTE]
> `read`, `grep`, `glob`, `list`, `question`, `task`, `plan_enter`, and `plan_exit` appear as rule names in the builtin agent rulesets (ported from opencode's `agent.ts`), but **no otto tool currently asks under those names**. `read`/`grep`/`glob` only ever ask `external_directory`; `task` and `question` never ask at all; there is no `list` tool. Rules for those names are inert today — write path restrictions as `read`-independent `external_directory` or `edit`/`write` rules if you need them enforced.
> <!-- src: crates/otto-agent/src/builtins.rs:40-57; crates/otto-tools/src/tools/{read,grep,glob,task,question}.rs -->

See [the agents reference](../reference/agents.md) for each builtin's ruleset.

## Worktrees

`otto worktree list | create | remove | reset` manages isolated working trees under `global_data_dir()/worktree/<project-slug>`.
<!-- src: crates/otto-vcs/src/worktree.rs; crates/otto-cli/src/cli.rs:223-244 -->

### `otto worktree create [--name <name>]`

The name is slugified (ASCII alphanumerics lowercased, everything else collapsed to single `-`, trailing dashes stripped) and defaults to `workspace` when absent or empty after slugifying. The branch is `otto/<slug>`, and creation is:

```bash
git worktree add -b otto/<slug> <data_root>/<slug> HEAD
```

Branched off the **current HEAD**, not the default branch. If either the directory or the branch already exists, it retries `<slug>-2` … `<slug>-50`, then fails with `could not find a free worktree name for '<slug>'`.
<!-- src: crates/otto-vcs/src/worktree.rs:134-179,334-355 -->

### `otto worktree remove <absolute-dir>`

Runs `git worktree remove --force -- <dir>`, then best-effort `git branch -D <branch>` (branch deletion never fails the call).
<!-- src: crates/otto-vcs/src/worktree.rs:209-226 -->

### `otto worktree reset <absolute-dir>`

> [!WARNING]
> `reset` is **guarded**: it refuses any directory that is not in the managed worktree list *before* touching git. `git reset --hard` and `git clean -ffdx` are destructive and git places no bound on the directory it is pointed at — it would happily hard-reset and wipe gitignored files anywhere. A non-managed path errors with `not a managed worktree: {dir}`.
> <!-- src: crates/otto-vcs/src/worktree.rs:268-288 -->

The default branch resolves in order: `git rev-parse --abbrev-ref origin/HEAD` (stripping `origin/`), then `refs/remotes/origin/main`, then `refs/remotes/origin/master`; failing all three, `could not resolve a default branch`. Then:

```bash
git fetch origin <branch>          # in the primary repo
git reset --hard origin/<branch>   # in the worktree
git clean -ffdx                    # in the worktree
```

<!-- src: crates/otto-vcs/src/worktree.rs:228-288 -->

### `otto worktree list`

Parses `git worktree list --porcelain` and **excludes the primary worktree**, comparing canonicalized paths. Detached worktrees report `branch: None`.
<!-- src: crates/otto-vcs/src/worktree.rs:82-128 -->

`otto workflow sdd` uses this same machinery for its per-task isolation — see [workflows](./workflows.md). For the CLI surface generally, see [the CLI reference](../reference/cli.md); for mode cycling in the TUI, [the TUI guide](./tui.md).
