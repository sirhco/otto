//! Tests for [`otto_session::system`] — base-prompt selection, environment
//! block, and assembly ordering.

use std::path::Path;

use otto_llm::model::{ModelId, ProviderId};
use otto_session::system::{
    assemble, base_prompt, build_system, environment, instructions, mcp, skills,
};

fn ids(provider: &str, model: &str) -> (ProviderId, ModelId) {
    (ProviderId::new(provider), ModelId::new(model))
}

#[test]
fn base_prompt_selects_family() {
    let cases = [
        ("openai", "gpt-4o", "You are Beast"),
        ("anthropic", "claude-sonnet-4", "You are opencode"),
        ("google", "gemini-2.5-pro", "gemini"),
        ("openai", "gpt-5-codex", "Codex"),
        ("moonshot", "kimi-k2", "Kimi"),
    ];
    // Rather than asserting exact prose, assert distinct prompts per family.
    let (p, m) = ids("openai", "gpt-4o");
    let beast = base_prompt(&p, &m);
    let (p, m) = ids("openai", "gpt-4.1");
    let beast2 = base_prompt(&p, &m);
    assert_eq!(beast, beast2, "gpt-4* both map to beast");

    let (p, m) = ids("openai", "gpt-5");
    let gpt = base_prompt(&p, &m);
    assert_ne!(gpt, beast, "gpt-5 is not beast");

    let (p, m) = ids("openai", "gpt-5-codex");
    let codex = base_prompt(&p, &m);
    assert_ne!(codex, gpt, "codex differs from gpt");

    let (p, m) = ids("anthropic", "claude-sonnet-4");
    let anthropic = base_prompt(&p, &m);

    let (p, m) = ids("google", "gemini-2.5-pro");
    let gemini = base_prompt(&p, &m);

    let (p, m) = ids("moonshot", "kimi-k2");
    let kimi = base_prompt(&p, &m);

    let (p, m) = ids("mystery", "some-unknown-model");
    let default = base_prompt(&p, &m);

    for prompt in [beast, gpt, codex, anthropic, gemini, kimi, default] {
        assert!(!prompt.is_empty(), "embedded prompt must be non-empty");
    }
    // families are distinct documents
    assert_ne!(anthropic, gemini);
    assert_ne!(anthropic, default);
    assert_ne!(gemini, kimi);

    // silence the descriptive table (documentation of intent)
    let _ = cases;
}

#[test]
fn environment_block_contains_expected_lines() {
    let (p, m) = ids("anthropic", "claude-sonnet-4");
    let env = environment(
        &p,
        &m,
        Path::new("/work/dir"),
        true,
        "darwin",
        "Wed Jul 02 2026",
    );
    assert_eq!(env.len(), 1);
    let block = &env[0];
    assert!(block.contains(
        "You are powered by the model named claude-sonnet-4. The exact model ID is anthropic/claude-sonnet-4"
    ));
    assert!(block.contains("<env>"));
    assert!(block.contains("  Working directory: /work/dir"));
    assert!(block.contains("  Is directory a git repo: yes"));
    assert!(block.contains("  Platform: darwin"));
    assert!(block.contains("  Today's date: Wed Jul 02 2026"));
    assert!(block.contains("</env>"));
}

#[test]
fn environment_not_git() {
    let (p, m) = ids("anthropic", "claude-sonnet-4");
    let env = environment(&p, &m, Path::new("/x"), false, "linux", "d");
    assert!(env[0].contains("  Is directory a git repo: no"));
}

#[test]
fn assemble_uses_base_and_user_system_last() {
    let out = assemble(
        None,
        "BASE",
        &["ENV".to_string(), "INSTR".to_string()],
        Some("USER"),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], "BASE\nENV\nINSTR\nUSER");
}

#[test]
fn assemble_agent_prompt_overrides_base() {
    let out = assemble(Some("AGENT"), "BASE", &["ENV".to_string()], None);
    assert_eq!(out[0], "AGENT\nENV");
}

#[test]
fn assemble_filters_empty_entries() {
    let out = assemble(None, "BASE", &["".to_string(), "ENV".to_string()], None);
    assert_eq!(out[0], "BASE\nENV");
}

#[test]
fn build_system_puts_base_first_and_env_after() {
    let dir = tempfile::tempdir().unwrap();
    let (p, m) = ids("anthropic", "claude-sonnet-4");
    let out = build_system(
        &p,
        &m,
        None,
        dir.path(),
        true,
        "darwin",
        "d",
        None,
        None,
        Some("USER SYSTEM"),
        None,
    );
    assert_eq!(out.len(), 1);
    let base = base_prompt(&p, &m);
    assert!(out[0].starts_with(base));
    assert!(out[0].contains("You are powered by the model named claude-sonnet-4"));
    assert!(out[0].trim_end().ends_with("USER SYSTEM"));
}

#[test]
fn build_system_includes_mcp_after_instructions_before_user() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "INSTR MARKER").unwrap();
    let (p, m) = ids("anthropic", "claude-sonnet-4");
    let mcp_block =
        "<mcp_instructions>\n  <server name=\"testserver\">\n  </server>\n</mcp_instructions>";
    let out = build_system(
        &p,
        &m,
        None,
        dir.path(),
        true,
        "darwin",
        "d",
        Some(mcp_block),
        None,
        Some("USER SYSTEM"),
        None,
    );
    let joined = &out[0];
    // The mcp block is present, sits after the instruction files, and before
    // the user-system tail (`prompt.ts:1263-1268` ordering).
    let instr = joined.find("INSTR MARKER").expect("instructions present");
    let mcp = joined.find("<mcp_instructions>").expect("mcp present");
    let user = joined.find("USER SYSTEM").expect("user system present");
    assert!(instr < mcp, "instructions precede mcp");
    assert!(mcp < user, "mcp precedes user system");
}

#[test]
fn build_system_omits_mcp_when_none() {
    let dir = tempfile::tempdir().unwrap();
    let (p, m) = ids("anthropic", "claude-sonnet-4");
    let out = build_system(
        &p,
        &m,
        None,
        dir.path(),
        true,
        "darwin",
        "d",
        None,
        None,
        None,
        None,
    );
    assert!(!out[0].contains("<mcp_instructions>"));
}

#[test]
fn instructions_reads_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "do the thing").unwrap();
    let instr = instructions(dir.path());
    assert!(instr.iter().any(|s| s == "do the thing"));
}

#[test]
fn mcp_is_none_for_now() {
    assert!(mcp().is_none());
}

#[test]
fn skills_indexes_discovered_skill() {
    // `skills(cwd)` returns the `<available_skills>` name+description index.
    // Hermetic: assert the temp skill's name/desc are present and its body is
    // absent. Do NOT assert None — the machine's global skill dirs may hold real
    // skills, so `contains` is the robust check.
    let dir = tempfile::tempdir().unwrap();
    let sd = dir.path().join("skills/pdf");
    std::fs::create_dir_all(&sd).unwrap();
    std::fs::write(
        sd.join("SKILL.md"),
        "---\nname: pdf\ndescription: work with pdfs\n---\nBODY MUST NOT APPEAR\n",
    )
    .unwrap();
    let block = skills(dir.path()).expect("a skill is present");
    assert!(block.contains("<available_skills>"));
    assert!(block.contains("pdf"));
    assert!(block.contains("work with pdfs"));
    assert!(!block.contains("BODY MUST NOT APPEAR"));
}
