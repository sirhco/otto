# Task 1 Report: `otto-config` — typed `hooks` field

*(Written retroactively — the original implementer's final message claimed this
file existed but it was never written. This report reconstructs what
`deb73d5` actually did, from `git show deb73d5`, and appends a section for the
follow-up fix that repaired the corrupted schema file.)*

## Commit `deb73d5 feat(otto-config): add typed hooks config field`

Added a typed, otto-native `hooks` field to `otto-config`'s `Config` struct so
lifecycle hooks (pre/post tool use, etc.) can be declared in user config and
consumed downstream by the runtime.

**Files changed (4 files, 151 insertions):**

- `crates/otto-config/Cargo.toml` (+1) — added `otto-hooks.workspace = true` as
  a dependency.
- `crates/otto-config/src/schema.rs` (+8) — re-exported
  `pub use otto_hooks::HooksConfig;` and added
  `pub hooks: Option<HooksConfig>` to `Config`, with
  `#[serde(default, skip_serializing_if = "Option::is_none")]` so the field is
  optional and omitted from serialized output when absent. Both
  `otto_config::HooksConfig` and `otto_hooks::HooksConfig` refer to the same
  type, which is what Task 3's `Runtime::load`/`Runtime::in_memory` is
  expected to consume.
- `crates/otto-config/tests/config.rs` (+31) — two new tests:
  - `parses_and_merges_hooks_config` — parses a `hooks.pre_tool_use` block,
    verifies the matcher/command round-trip, then merges a second config that
    only sets `hooks.post_tool_use` and asserts the base's `pre_tool_use`
    entries survive the merge (object-level fields merge rather than replace
    wholesale, consistent with every other typed sub-config field on
    `Config`).
  - `hooks_absent_when_not_configured` — an empty `{}` config parses with
    `cfg.hooks` as `None`.
- `schema/config.json` (+111) — regenerated via
  `cargo run -p otto-config --example gen_schema` to reflect the new `hooks`
  field.

**Test command/output (at the time of `deb73d5`):** `cargo test -p otto-config`
— 28 tests passed (3 suites).

## Follow-up fix: corrupted `schema/config.json` repair

A review of `deb73d5` found `schema/config.json` was not valid JSON: the first
three lines were literal `cargo build`/`cargo run` progress output —

```
   Compiling otto-config v0.3.3 (.../crates/otto-config)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.64s
     Running `target/debug/examples/gen_schema`
```

— prepended before the real `{ "$schema": ... }` JSON content. This happens
when the `gen_schema` example's stdout is captured together with cargo's
build-progress stderr (merged streams, or a shell/wrapper that redirects
`2>&1` before the `>` redirect).

**Fix applied:**

```bash
cargo run -p otto-config --example gen_schema > schema/config.json
```

run with only stdout redirected to the file (cargo's own progress lines go to
stderr and are not captured).

**Verification:**

- `python3 -m json.tool schema/config.json > /dev/null && echo VALID` → printed
  `VALID`.
- `git diff schema/config.json` against the pre-fix version shows exactly a
  3-line deletion — the three stray build-log lines — and nothing else;
  the schema content itself is unchanged.
- `cargo test -p otto-config` → 28 tests passed (3 suites), same as before
  this fix (it only touches the generated schema file, not source code).

**Files changed by this fix:**

- `schema/config.json` — stray build-log lines removed, now valid JSON.
- `.superpowers/sdd/task-1-report.md` — this report, written retroactively.
