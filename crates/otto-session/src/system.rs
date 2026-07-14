//! System-prompt assembly — a faithful Rust port of opencode's
//! `session/system.ts` (prompt selection + environment) and the joining/order
//! logic in `session/llm/request.ts:58-66`.
//!
//! The base prompt `.txt` files are copied from
//! `packages/opencode/src/session/prompt/` into `assets/prompt/` and embedded
//! via [`include_str!`].

use std::path::Path;

use otto_llm::model::{ModelId, ProviderId};

use crate::warm::WarmCache;

/// Anthropic / Claude base prompt (`system.ts:6`).
const PROMPT_ANTHROPIC: &str = include_str!("../assets/prompt/anthropic.txt");
/// GPT base prompt (`system.ts:10`).
const PROMPT_GPT: &str = include_str!("../assets/prompt/gpt.txt");
/// Gemini base prompt (`system.ts:9`).
const PROMPT_GEMINI: &str = include_str!("../assets/prompt/gemini.txt");
/// "Beast" GPT-4/o1/o3 base prompt (`system.ts:8`).
const PROMPT_BEAST: &str = include_str!("../assets/prompt/beast.txt");
/// Codex base prompt (`system.ts:13`).
const PROMPT_CODEX: &str = include_str!("../assets/prompt/codex.txt");
/// Kimi base prompt (`system.ts:11`).
const PROMPT_KIMI: &str = include_str!("../assets/prompt/kimi.txt");
/// Default fallback base prompt (`system.ts:7`).
const PROMPT_DEFAULT: &str = include_str!("../assets/prompt/default.txt");
/// Trinity base prompt (`system.ts:14`).
const PROMPT_TRINITY: &str = include_str!("../assets/prompt/trinity.txt");

/// Select the base system prompt for a model.
///
/// Port of `SystemPrompt.provider` (`system.ts:26-40`), keyed on the model id
/// string. Order matters — the first matching family wins.
#[must_use]
pub fn base_prompt(_provider: &ProviderId, model: &ModelId) -> &'static str {
    let id = model.0.as_str();
    let lower = id.to_lowercase();
    if id.contains("gpt-4") || id.contains("o1") || id.contains("o3") {
        return PROMPT_BEAST;
    }
    if id.contains("gpt") {
        if id.contains("codex") {
            return PROMPT_CODEX;
        }
        return PROMPT_GPT;
    }
    if id.contains("gemini-") {
        return PROMPT_GEMINI;
    }
    if id.contains("claude") {
        return PROMPT_ANTHROPIC;
    }
    if lower.contains("trinity") {
        return PROMPT_TRINITY;
    }
    if lower.contains("kimi") {
        return PROMPT_KIMI;
    }
    PROMPT_DEFAULT
}

/// Build the environment system segment(s).
///
/// Port of `SystemPrompt.environment` (`system.ts:58-94`). Returns a single
/// entry: the `You are powered by …` line followed by the `<env>` block.
/// Project references (`system.ts:75-92`) are omitted until that subsystem is
/// wired.
#[must_use]
pub fn environment(
    provider: &ProviderId,
    model: &ModelId,
    cwd: &Path,
    is_git: bool,
    platform: &str,
    date: &str,
) -> Vec<String> {
    let cwd = cwd.display();
    let block = [
        format!(
            "You are powered by the model named {model}. The exact model ID is {provider}/{model}",
            model = model.0,
            provider = provider.0,
        ),
        "Here is some useful information about the environment you are running in:".to_string(),
        "<env>".to_string(),
        format!("  Working directory: {cwd}"),
        format!("  Workspace root folder: {cwd}"),
        format!(
            "  Is directory a git repo: {}",
            if is_git { "yes" } else { "no" }
        ),
        format!("  Platform: {platform}"),
        format!("  Today's date: {date}"),
        "</env>".to_string(),
    ]
    .join("\n");
    vec![block]
}

/// Minimal `AGENTS.md` / `CLAUDE.md` loader.
///
/// Walks from `cwd` up to the filesystem root, collecting the contents of any
/// `AGENTS.md` then `CLAUDE.md` found in each directory (nearest first). The
/// full instruction system (globs, config-declared files) is a later phase.
#[must_use]
pub fn instructions(cwd: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            if let Ok(contents) = std::fs::read_to_string(current.join(name)) {
                let trimmed = contents.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
        }
        dir = current.parent();
    }
    out
}

/// MCP server instructions segment.
///
/// The `<mcp_instructions>` block (`system.ts:110-126`) is built by the MCP
/// subsystem (`otto_mcp::McpClient::instructions`) and threaded into
/// [`build_system`] as the `mcp_instructions` parameter — otto-session has no
/// production dependency on otto-mcp, so the caller (CLI/server) supplies the
/// pre-built string. This stub is retained only to document the seam; it always
/// returns `None`.
#[must_use]
pub fn mcp() -> Option<String> {
    None
}

/// Skill instructions segment: a thin `<available_skills>` index (name +
/// description per discovered skill, NEVER bodies) so the model can discover
/// skills cheaply and load a body on demand via the `skill` tool. Port of
/// `SystemPrompt.skills` (`system.ts:96-108`), token-dieted to the index only.
#[must_use]
pub fn skills(cwd: &Path) -> Option<String> {
    otto_tools::skill_index_block(&otto_tools::skill_roots(cwd))
}

/// Join the base prompt, system array, and optional user system into the single
/// system string opencode sends.
///
/// Port of the ordering in `request.ts:58-66`: `[agent_prompt ?? base,
/// ...system_array, user_system?]`, empties filtered, joined with `\n`. Returns
/// a one-element `Vec` (opencode's `system[0]`).
#[must_use]
pub fn assemble(
    agent_prompt: Option<&str>,
    base: &str,
    system_array: &[String],
    user_system: Option<&str>,
) -> Vec<String> {
    let head = agent_prompt.unwrap_or(base);
    let mut parts: Vec<&str> = Vec::new();
    parts.push(head);
    for entry in system_array {
        parts.push(entry.as_str());
    }
    if let Some(us) = user_system {
        parts.push(us);
    }
    let joined = parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    vec![joined]
}

/// High-level system assembly: select the base prompt, build the environment +
/// instruction array, then [`assemble`] into the final system string.
///
/// Combines `SystemPrompt.provider` + `environment` (`system.ts`) with the join
/// in `request.ts:58-66`. `agent_prompt`, when set, overrides the base prompt.
///
/// `mcp_instructions` is the optional pre-built `<mcp_instructions>` block
/// (`system.ts:110-126`, produced by `otto_mcp::McpClient::instructions`). It
/// is inserted after the instruction files and before skills, matching the
/// system-array order in `prompt.ts:1263-1268` (`env → instructions → mcp →
/// skills`). Passed in by the caller so otto-session keeps no production
/// dependency on otto-mcp.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_system(
    provider: &ProviderId,
    model: &ModelId,
    agent_prompt: Option<&str>,
    cwd: &Path,
    is_git: bool,
    platform: &str,
    date: &str,
    mcp_instructions: Option<&str>,
    hook_context: Option<&str>,
    user_system: Option<&str>,
    cache: Option<&WarmCache>,
) -> Vec<String> {
    if let Some(c) = cache {
        // Warm boot: return the memoized prompt, skipping base/env select and
        // the `instructions(cwd)` fs-walk entirely.
        return c.system.as_ref().clone();
    }
    let base = base_prompt(provider, model);
    let mut system_array = environment(provider, model, cwd, is_git, platform, date);
    system_array.extend(instructions(cwd));
    // env → instructions → mcp → hooks → skills (`prompt.ts:1263-1268`; the
    // hook-context slot has no opencode analog — otto extension). The mcp
    // block, when present, slots in here; SessionStart/UserPromptSubmit hook
    // context follows it; the skills index follows that.
    if let Some(mcp) = mcp_instructions.filter(|s| !s.is_empty()) {
        system_array.push(mcp.to_string());
    }
    if let Some(ctx) = hook_context.filter(|s| !s.is_empty()) {
        system_array.push(ctx.to_string());
    }
    if let Some(sk) = skills(cwd) {
        system_array.push(sk);
    }
    assemble(agent_prompt, base, &system_array, user_system)
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use otto_llm::model::{ModelId, ProviderId};
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn build_system_returns_cache_when_present() {
        let sentinel = vec!["CACHED-SYSTEM".to_string()];
        let cache = WarmCache {
            system: Arc::new(sentinel.clone()),
        };
        let out = build_system(
            &ProviderId("p".into()),
            &ModelId("m".into()),
            None,
            Path::new("/nonexistent"),
            false,
            "linux",
            "",
            None,
            None,
            None,
            Some(&cache),
        );
        assert_eq!(out, sentinel);
    }

    #[test]
    fn skills_block_lists_discovered_skill_names_and_descs() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path().join("skills/pdf");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(
            sd.join("SKILL.md"),
            "---\nname: pdf\ndescription: work with pdfs\n---\nBODY MUST NOT APPEAR\n",
        )
        .unwrap();
        let block = super::skills(dir.path()).expect("a skill is present");
        assert!(block.contains("pdf"));
        assert!(block.contains("work with pdfs"));
        assert!(!block.contains("BODY MUST NOT APPEAR"));
    }

    #[test]
    fn build_system_includes_skill_index_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path().join("skills/pdf");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(
            sd.join("SKILL.md"),
            "---\nname: pdf\ndescription: work with pdfs\n---\nbody\n",
        )
        .unwrap();
        let out = build_system(
            &ProviderId("p".into()),
            &ModelId("claude-x".into()),
            None,
            dir.path(),
            false,
            "linux",
            "",
            None,
            None,
            None,
            None,
        );
        assert!(out[0].contains("<available_skills>"), "index injected");
        assert!(out[0].contains("pdf"));
    }

    #[test]
    fn build_system_builds_when_cache_absent() {
        let out = build_system(
            &ProviderId("p".into()),
            &ModelId("claude-x".into()),
            None,
            Path::new("."),
            false,
            "linux",
            "",
            None,
            None,
            None,
            None,
        );
        assert!(!out.is_empty());
    }
}
