//! Tests for the listing subcommands against an in-memory runtime.

use otto_app::Runtime;
use otto_cli::commands::{render_agents, render_mcp, render_models, render_providers};
use otto_config::Config;

#[test]
fn models_lists_all_providers() {
    let mut out = Vec::new();
    render_models(None, &mut out).expect("render");
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.lines().any(|l| l.starts_with("anthropic/")),
        "has anthropic: {text}"
    );
    assert!(text.contains("openai/gpt-4o"), "has openai: {text}");
    assert!(text.contains("context="), "shows context: {text}");
}

#[test]
fn models_filters_by_provider() {
    let mut out = Vec::new();
    render_models(Some("openai"), &mut out).expect("render");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("openai/gpt-4o"));
    assert!(
        !text.contains("anthropic/"),
        "filtered out anthropic: {text}"
    );
}

#[tokio::test]
async fn agents_lists_builtins() {
    let runtime = Runtime::in_memory(Config::default())
        .await
        .expect("runtime");
    let mut out = Vec::new();
    render_agents(&runtime, &mut out).expect("render");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("build"), "lists build agent: {text}");
    assert!(text.contains("plan"), "lists plan agent: {text}");
    assert!(!text.trim().is_empty());
}

#[tokio::test]
async fn providers_lists_with_status() {
    let runtime = Runtime::in_memory(Config::default())
        .await
        .expect("runtime");
    let mut out = Vec::new();
    render_providers(&runtime, &mut out).expect("render");
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("anthropic"), "lists anthropic: {text}");
    // The in-memory auth store is empty, so every provider is logged out.
    assert!(text.contains("not logged in"), "shows status: {text}");
}

#[tokio::test]
async fn mcp_reports_none_configured() {
    let runtime = Runtime::in_memory(Config::default())
        .await
        .expect("runtime");
    let mut out = Vec::new();
    render_mcp(&runtime, &mut out).await.expect("render");
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("no MCP servers configured"),
        "empty mcp: {text}"
    );
}

#[test]
fn renders_worktree_list() {
    use otto_cli::commands::render_worktrees;
    use otto_vcs::worktree::WorktreeInfo;
    let list = vec![WorktreeInfo {
        name: "feat".into(),
        branch: Some("otto/feat".into()),
        directory: "/data/worktree/prj_x/feat".into(),
    }];
    let mut buf = Vec::new();
    render_worktrees(&list, &mut buf).unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("feat"));
    assert!(s.contains("otto/feat"));
    assert!(s.contains("/data/worktree/prj_x/feat"));
}
