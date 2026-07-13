//! Integration tests for `otto-config` — port fidelity against opencode
//! `config/config.ts` behavior. All filesystem-touching tests use `tempfile`
//! dirs and pass explicit paths / [`EnvOverrides`] so they never mutate process
//! env or read the real global config dir.

use std::fs;

use otto_config::{Config, EnvOverrides, LogLevel, Share, discover, load_with, merge, parse};
use serde_json::json;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

#[test]
fn parse_jsonc_with_comments_and_trailing_commas() {
    let text = r#"{
        // leading line comment
        "$schema": "https://example.com/config.json",
        "model": "anthropic/claude-2", /* inline block */
        "instructions": [
            "AGENTS.md",
            "docs/*.md", // trailing comma below is legal jsonc
        ],
    }"#;
    let cfg = parse(text).expect("jsonc should parse");
    assert_eq!(
        cfg.schema.as_deref(),
        Some("https://example.com/config.json")
    );
    assert_eq!(cfg.model.as_deref(), Some("anthropic/claude-2"));
    assert_eq!(
        cfg.instructions.as_deref(),
        Some(["AGENTS.md".to_string(), "docs/*.md".to_string()].as_slice())
    );
}

#[test]
fn parse_empty_is_default() {
    let cfg = parse("").expect("empty parses to default");
    assert!(cfg.model.is_none());
    assert!(cfg.instructions.is_none());
}

// ---------------------------------------------------------------------------
// merge
// ---------------------------------------------------------------------------

#[test]
fn merge_deep_objects_and_scalar_override() {
    let base =
        parse(r#"{ "model": "a/1", "compaction": { "auto": true, "reserved": 100 } }"#).unwrap();
    let over = parse(r#"{ "model": "b/2", "compaction": { "reserved": 200 } }"#).unwrap();
    let merged = merge(base, over);

    // scalar override
    assert_eq!(merged.model.as_deref(), Some("b/2"));
    // deep merge keeps base.auto while overriding reserved
    let compaction = merged.compaction.expect("compaction present");
    assert_eq!(compaction.auto, Some(true));
    assert_eq!(compaction.reserved, Some(200));
}

#[test]
fn merge_instructions_concat_and_dedupe() {
    let base = parse(r#"{ "instructions": ["a"] }"#).unwrap();
    let over = parse(r#"{ "instructions": ["a", "b"] }"#).unwrap();
    let merged = merge(base, over);
    assert_eq!(
        merged.instructions.as_deref(),
        Some(["a".to_string(), "b".to_string()].as_slice())
    );
}

#[test]
fn merge_non_instructions_arrays_replace() {
    let base = parse(r#"{ "plugin": ["p1", "p2"] }"#).unwrap();
    let over = parse(r#"{ "plugin": ["p3"] }"#).unwrap();
    let merged = merge(base, over);
    assert_eq!(
        merged.plugin.as_deref(),
        Some(["p3".to_string()].as_slice())
    );
}

#[test]
fn merge_none_does_not_clobber() {
    let base = parse(r#"{ "model": "a/1" }"#).unwrap();
    let over = Config::default();
    let merged = merge(base, over);
    assert_eq!(merged.model.as_deref(), Some("a/1"));
}

#[test]
fn parses_and_merges_hooks_config() {
    let base = parse(
        r#"{ "hooks": { "pre_tool_use": [ { "matcher": "bash", "hooks": [ { "command": "check.sh" } ] } ] } }"#,
    )
    .unwrap();
    let hooks = base.hooks.as_ref().expect("hooks present");
    assert_eq!(hooks.pre_tool_use.len(), 1);
    assert_eq!(hooks.pre_tool_use[0].matcher.as_deref(), Some("bash"));
    assert_eq!(hooks.pre_tool_use[0].hooks[0].command, "check.sh");

    // Merge: an override with a different event's hooks must not clobber the
    // base's `pre_tool_use` entries — object-level fields merge, they don't
    // replace wholesale (same deep-merge guarantee as every other typed
    // sub-config field in `Config`).
    let over = parse(
        r#"{ "hooks": { "post_tool_use": [ { "hooks": [ { "command": "notify.sh" } ] } ] } }"#,
    )
    .unwrap();
    let merged = merge(base, over);
    let merged_hooks = merged.hooks.expect("hooks present after merge");
    assert_eq!(merged_hooks.pre_tool_use.len(), 1, "base's pre_tool_use survives the merge");
    assert_eq!(merged_hooks.post_tool_use.len(), 1, "override's post_tool_use is present");
}

#[test]
fn hooks_absent_when_not_configured() {
    let cfg = parse("{}").unwrap();
    assert!(cfg.hooks.is_none());
}

// ---------------------------------------------------------------------------
// discover
// ---------------------------------------------------------------------------

#[test]
fn discover_uptree_order_legacy_and_alias() {
    let root = tempdir().unwrap();
    let root_p = root.path();
    // mark the git root so discovery stops here
    fs::create_dir(root_p.join(".git")).unwrap();
    let child = root_p.join("a").join("b");
    fs::create_dir_all(&child).unwrap();

    fs::write(root_p.join("config.json"), "{}").unwrap();
    fs::write(child.join("otto.jsonc"), "{}").unwrap();

    let found = discover(&child, Some(root_p), false);
    // root (ancestor) comes before child (closer to cwd wins → merged last)
    let root_idx = found
        .iter()
        .position(|p| p.ends_with("config.json"))
        .unwrap();
    let child_idx = found
        .iter()
        .position(|p| p.ends_with("otto.jsonc"))
        .unwrap();
    assert!(
        root_idx < child_idx,
        "ancestor config must precede cwd config: {found:?}"
    );
}

#[test]
fn discover_picks_up_dot_otto_dir() {
    let root = tempdir().unwrap();
    let root_p = root.path();
    fs::create_dir(root_p.join(".git")).unwrap();
    fs::create_dir(root_p.join(".otto")).unwrap();
    fs::write(root_p.join(".otto").join("otto.jsonc"), "{}").unwrap();

    let found = discover(root_p, Some(root_p), false);
    assert!(
        found.iter().any(|p| p.ends_with(".otto/otto.jsonc")),
        "should find .otto dir config: {found:?}"
    );
}

#[test]
fn discover_disabled_returns_empty() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("otto.json"), "{}").unwrap();
    assert!(discover(root.path(), Some(root.path()), true).is_empty());
}

// ---------------------------------------------------------------------------
// load
// ---------------------------------------------------------------------------

#[test]
fn load_project_overrides_global_model() {
    let global = tempdir().unwrap();
    fs::write(
        global.path().join("otto.json"),
        r#"{ "model": "global/model", "small_model": "keep/me" }"#,
    )
    .unwrap();

    let proj = tempdir().unwrap();
    fs::create_dir(proj.path().join(".git")).unwrap();
    fs::write(
        proj.path().join("otto.json"),
        r#"{ "model": "project/model" }"#,
    )
    .unwrap();

    let cfg = load_with(proj.path(), global.path(), &EnvOverrides::default()).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("project/model"));
    // untouched global field survives the merge
    assert_eq!(cfg.small_model.as_deref(), Some("keep/me"));
    // $schema injected on load
    assert!(cfg.schema.is_some());
}

#[test]
fn load_config_content_wins() {
    let global = tempdir().unwrap();
    let proj = tempdir().unwrap();
    fs::create_dir(proj.path().join(".git")).unwrap();
    fs::write(
        proj.path().join("otto.json"),
        r#"{ "model": "project/model" }"#,
    )
    .unwrap();

    let env = EnvOverrides {
        config_content: Some(r#"{ "model": "content/model" }"#.to_string()),
        ..Default::default()
    };
    let cfg = load_with(proj.path(), global.path(), &env).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("content/model"));
}

#[test]
fn load_disable_project_config_skips_project() {
    let global = tempdir().unwrap();
    fs::write(
        global.path().join("otto.json"),
        r#"{ "model": "global/model" }"#,
    )
    .unwrap();

    let proj = tempdir().unwrap();
    fs::create_dir(proj.path().join(".git")).unwrap();
    fs::write(
        proj.path().join("otto.json"),
        r#"{ "model": "project/model" }"#,
    )
    .unwrap();

    let env = EnvOverrides {
        disable_project_config: true,
        ..Default::default()
    };
    let cfg = load_with(proj.path(), global.path(), &env).unwrap();
    assert_eq!(cfg.model.as_deref(), Some("global/model"));
}

#[test]
fn load_default_when_no_config() {
    let global = tempdir().unwrap();
    let proj = tempdir().unwrap();
    fs::create_dir(proj.path().join(".git")).unwrap();
    let cfg = load_with(proj.path(), global.path(), &EnvOverrides::default()).unwrap();
    // default is { "$schema": ... }
    assert_eq!(cfg.schema.as_deref(), Some(otto_config::DEFAULT_SCHEMA));
    assert!(cfg.model.is_none());
}

// ---------------------------------------------------------------------------
// schema round-trip
// ---------------------------------------------------------------------------

#[test]
fn realistic_config_roundtrips_with_unknown_keys() {
    let text = r#"{
        "$schema": "https://example.com/config.json",
        "model": "anthropic/claude-sonnet-4",
        "logLevel": "DEBUG",
        "share": "disabled",
        "instructions": ["AGENTS.md"],
        "permission": { "edit": "ask", "bash": { "git status": "allow" } },
        "mcp": { "fs": { "type": "local", "command": ["mcp-fs"] } },
        "agent": { "build": { "model": "anthropic/claude-opus-4", "temperature": 0.2 } },
        "compaction": { "auto": false, "tail_turns": 4 },
        "tool_output": { "max_lines": 1000 },
        "tools": { "bash": true, "webfetch": false },
        "unknown_future_key": { "nested": [1, 2, 3] },
        "someScalar": 42
    }"#;
    let cfg = parse(text).expect("realistic config parses, unknown keys tolerated");
    assert_eq!(cfg.log_level, Some(LogLevel::Debug));
    assert_eq!(cfg.share, Some(Share::Disabled));
    assert_eq!(cfg.compaction.unwrap().tail_turns, Some(4));
    assert_eq!(cfg.tools.as_ref().unwrap().get("bash"), Some(&true));
    // Value-typed sub-objects preserved as-is
    assert_eq!(cfg.permission.unwrap()["edit"], json!("ask"));
    assert!(cfg.mcp.is_some());
    assert!(cfg.agent.is_some());
}
