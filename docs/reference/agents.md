# Agents

The 7 builtin agents, their permission rulesets, step caps, and how to define your own via `config.agent`.

| agent | mode | hidden | steps | temperature | prompt asset |
|---|---|---|---|---|---|
| `build` | primary (the default) | no | 200 | — | none |
| `plan` | primary | no | 200 | — | `assets/plan.txt` |
| `general` | subagent | no | 200 | — | none |
| `explore` | subagent | no | 100 | — | `assets/explore.txt` |
| `compaction` | primary | yes | 4 | — | `assets/compaction.txt` |
| `title` | primary | yes | 4 | 0.5 | `assets/title.txt` |
| `summary` | primary | yes | 4 | — | `assets/summary.txt` |
<!-- src: crates/otto-agent/src/builtins.rs:74-238 -->

No builtin sets a model override (`model: None` on all 7); `title` is the only one with a temperature. Declaration order is `build, plan, general, explore, compaction, title, summary`.
<!-- src: crates/otto-agent/src/builtins.rs:244-254 -->

## The base ruleset

Every builtin's permission is `merge(defaults(), <agent-specific>)`. `defaults()` is:

```json
{
  "*": "allow",
  "doom_loop": "ask",
  "external_directory": { "*": "ask" },
  "question": "deny",
  "plan_enter": "deny",
  "plan_exit": "deny",
  "read": {
    "*": "allow",
    "*.env": "ask",
    "*.env.*": "ask",
    "*.env.example": "allow"
  }
}
```
<!-- src: crates/otto-agent/src/builtins.rs:41-57 -->

Two families of the upstream base ruleset are runtime-scoped and injected per session rather than baked in here: the `external_directory` whitelist (tmp dir, skill dirs, reference dirs) and the global `config.permission` layer.
<!-- src: crates/otto-agent/src/builtins.rs:7-11 -->

**The catch with those `ask` rules.** Permission resolution is two-phase. The deny gate evaluates `[agent ruleset, config ruleset, session approvals]`, so an agent's deny holds in every mode. The interactivity phase evaluates `[mode overlay, config ruleset, session approvals, danger]` — the agent layer is **deliberately excluded**. So in full-auto, the agent-level `ask` rules above (`doom_loop`, `external_directory`, `.env` reads) are answered silently by the mode overlay instead of raising a prompt. Only rules the user stated explicitly in `config.permission`, or the danger ruleset, will still prompt.
<!-- src: crates/otto-permission/src/permission.rs:262-278 -->

## build

The default primary coding agent. Executes tools based on configured permissions; since `defaults()` allows `*`, with no user restrictions every tool is permitted.

```json
{ "question": "allow", "plan_enter": "allow" }
```
<!-- src: crates/otto-agent/src/builtins.rs:86-89 -->

Merged over `defaults()`, so: everything allowed, `doom_loop`/`external_directory`/`.env` reads ask, `plan_exit` denied, `question` and `plan_enter` allowed. 200 steps, no prompt override (it uses the base system prompt).

## plan

Read-only planning primary. Its prompt directs it to persist the plan to `.otto/plans/*.md` in the sdd-parseable `### Task N:` heading form.

```json
{
  "question": "allow",
  "plan_exit": "allow",
  "task": { "general": "deny" },
  "edit": {
    "*": "deny",
    ".otto/plans/*.md": "allow"
  },
  "write": {
    "*": "deny",
    ".otto/plans/*.md": "allow"
  }
}
```
<!-- src: crates/otto-agent/src/builtins.rs, plan() -->

**The edit deny holds in every permission mode, including full-auto.** The agent ruleset sits in the deny gate, which runs before the mode overlay is consulted; a mode can answer an `ask` but can never turn a deny into an allow. Only two things outrank it: an explicit user rule in `config.permission` or an in-session `Always` approval (both of which are the user speaking), and the danger ruleset, which prompts regardless of mode.
<!-- src: crates/otto-permission/src/permission.rs:262-296 -->

Two caveats on this ruleset:

- Two deny keys are needed, not one. `edit` gates the `edit` and `apply_patch` tools (`apply_patch` asks under `edit`), but the `write` tool asks under its own `write` permission — see [tools](./tools.md#write). Both keys carry the same `.otto/plans/*.md` exception. Adding a new file-mutating tool means checking which permission it asks and extending this ruleset to match; `defaults()` is `"*": "allow"`, so an unlisted permission falls through to allow.
- `task: { "general": "deny" }` is inert as a gate: nothing asks the `task` permission. It only affects `derive_subagent_permission`, which checks whether an agent's ruleset mentions `task` at all.
  <!-- src: crates/otto-agent/src/subagent.rs:24-52 -->

200 steps.

## general

General-purpose subagent — the agent every workflow spawns. Its ruleset is `defaults()` plus a single deny:

```json
{ "todowrite": "deny" }
```
<!-- src: crates/otto-agent/src/builtins.rs:150 -->

Everything else stays as `defaults()`, so `general` can edit, write, and run bash. 200 steps, no prompt override, subagent mode (it is not selectable as a primary agent).

## explore

Read-only exploration subagent. Denies everything, then re-allows the read-only surface:

```json
{
  "*": "deny",
  "grep": "allow",
  "glob": "allow",
  "list": "allow",
  "bash": "allow",
  "webfetch": "allow",
  "websearch": "allow",
  "read": "allow",
  "external_directory": { "*": "ask" }
}
```
<!-- src: crates/otto-agent/src/builtins.rs:182-192 -->

The blanket `"*": "deny"` is what blocks `edit`, `write`, `apply_patch`, `todowrite`, and `task` — not per-key denies. `bash` is allowed, so "read-only" is a convention of the prompt, not an enforced guarantee for shell commands. `list` allows a permission for a tool that does not exist; see the wart note in [tools](./tools.md#known-warts).

100 steps — the only builtin with a cap below 200 that is not a hidden internal.

## compaction, title, summary

Hidden internal primaries built by the shared `hidden_internal` helper: `mode: primary`, `hidden: true`, ruleset `merge(defaults(), { "*": "deny" })`, 4 steps, no description.
<!-- src: crates/otto-agent/src/builtins.rs:221-238 -->

| agent | purpose (from its prompt asset) |
|---|---|
| `compaction` | Anchored context summarizer — summarizes older conversation history when newer turns are kept verbatim |
| `title` | Outputs only a thread title for the conversation, nothing else. `temperature: 0.5` |
| `summary` | Summarizes what was done, PR-description style, 2-3 sentences max |

All three deny every tool, which is the point: they are single-shot text generators driven by the session layer, not tool-using agents.

> [!NOTE]
> `hidden: true` suppresses these agents from the TUI's agent picker, **not** from `otto agent list` — that command iterates every registered agent without filtering, so `compaction`, `title`, and `summary` do appear there (with empty descriptions).
<!-- src: crates/otto-cli/src/commands.rs:301 — `for agent in runtime.agents()`, no hidden filter; verified by running the command -->

Do not select them as `--agent`; they are driven by the session layer.

## Step caps and MAX_STEPS_PROMPT

An agent's `steps` value bounds the turn loop. On the **final allowed step**, the run loop appends an assistant-role message carrying `MAX_STEPS_PROMPT` before the request — a hard instruction to make no tool calls and respond with text only, summarizing work done, remaining tasks, and recommendations.
<!-- src: crates/otto-session/src/run.rs:56-58, :747-751 -->

Separately, `MAX_ITERATIONS = 1000` is a safety net on the outer loop, independent of the agent's step cap.
<!-- src: crates/otto-session/src/run.rs:64 -->

## Custom agents

`config.agent` is a permissive object map overlaid onto the builtins by `resolve_agents`.
<!-- src: crates/otto-config/src/schema.rs:184-186, crates/otto-agent/src/config.rs:24-70 -->

```json
{
  "agent": {
    "reviewer": {
      "description": "Reviews diffs and reports findings",
      "mode": "subagent",
      "prompt": "You review code. Report findings only.",
      "steps": 40,
      "temperature": 0.2,
      "permission": {
        "*": "deny",
        "read": "allow",
        "grep": "allow",
        "glob": "allow",
        "bash": "allow"
      }
    },
    "plan": { "steps": 400 },
    "explore": { "disable": true }
  }
}
```

Rules:

- A truthy `disable` removes the agent entirely.
- An unknown key creates a new agent defaulting to `mode: "all"`, `native: false`, ruleset `defaults()`, `steps: None`.
- A known key overlays onto the builtin: scalar fields (`model`, `variant`, `prompt`, `description`, `temperature`, `top_p`, `mode`, `color`, `hidden`, `name`, `steps`) replace when present; `options` is deep-merged; `permission` is `merge(existing, fromConfig(value.permission))`, so config rules layer **on top of** the builtin's rules and win on conflict.
- Iteration order of the config map is preserved.
<!-- src: crates/otto-agent/src/config.rs:31-116 -->

`model` is parsed as a `ModelRef`, which splits on the first slash only.

Regenerate the JSON schema after touching config structs:

```bash
cargo run -q -p otto-config --example gen_schema > schema/config.json
```

## Listing agents

```bash
otto agent list
```

Prints one line per resolved agent as `{name}  [{mode}]  {description}`, where mode is `primary`, `subagent`, or `all`. This renders `runtime.agents()`, i.e. the builtins with `config.agent` already applied.
<!-- src: crates/otto-cli/src/commands.rs:300-322 -->

See also: [permissions](../guide/permissions.md), [tools](./tools.md), [config](./config.md).
