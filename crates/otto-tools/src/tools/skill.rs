//! The `skill` tool — a port of opencode
//! `packages/opencode/src/tool/skill.ts` plus the discovery in
//! `packages/opencode/src/skill/index.ts:20-24`.
//!
//! Discovers `SKILL.md` files under `ctx.directory` (and an optional global
//! skills dir), parses their `name`/`description` frontmatter, and returns the
//! named skill's body wrapped in the `<skill_content>` envelope
//! (`skill.ts:45-66`). An unknown name yields an error listing the available
//! skills (`skill/index.ts:74-80`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;

use super::parallel_walk::parallel_collect;
use crate::tool::{ExecuteResult, PermissionRequest, Tool, ToolContext, ToolError, decode_args};

#[derive(Debug, Deserialize)]
struct SkillParams {
    name: String,
    #[serde(default)]
    section: Option<String>,
}

/// The heading text of every `##`/`###` section in `body`, in order.
fn section_headings(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|l| {
            let t = l.trim_start();
            let hashes = t.chars().take_while(|&c| c == '#').count();
            if (2..=3).contains(&hashes) && t[hashes..].starts_with(' ') {
                Some(t[hashes..].trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

/// The body of the `##`/`###` section titled `name` (case-insensitive), from its
/// heading line to the next heading of the same-or-shallower level (or EOF).
/// Returns `None` if no such section exists.
fn extract_section(body: &str, name: &str) -> Option<String> {
    let want = name.trim().to_lowercase();
    let lines: Vec<&str> = body.lines().collect();
    let mut start: Option<(usize, usize)> = None; // (line index, heading level)
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        let level = t.chars().take_while(|&c| c == '#').count();
        if (2..=3).contains(&level) && t[level..].starts_with(' ') {
            let title = t[level..].trim().to_lowercase();
            if start.is_none() && title == want {
                start = Some((i, level));
            } else if let Some((_, lvl)) = start
                && level <= lvl
            {
                // next same-or-shallower heading ends the section
                let (s, _) = start.unwrap();
                return Some(lines[s..i].join("\n").trim().to_string());
            }
        }
    }
    start.map(|(s, _)| lines[s..].join("\n").trim().to_string())
}

/// A discovered skill.
struct SkillInfo {
    name: String,
    description: Option<String>,
    location: PathBuf,
    body: String,
}

/// The `skill` tool (skill.ts:12).
#[derive(Debug, Default, Clone, Copy)]
pub struct SkillTool;

/// Split YAML frontmatter (`--- … ---`) off a `SKILL.md`, returning
/// `(name, description, body)`. Falls back to the parent directory name.
fn parse_skill(path: &Path, raw: &str) -> SkillInfo {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut body = raw;

    if let Some(rest) = raw.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        let front = &rest[..end];
        // Body starts after the closing '---' line.
        let after = &rest[end + 4..];
        body = after.strip_prefix('\n').unwrap_or(after);
        for line in front.lines() {
            if let Some(v) = line.strip_prefix("name:") {
                name = Some(v.trim().trim_matches(['"', '\'']).to_string());
            } else if let Some(v) = line.strip_prefix("description:") {
                description = Some(v.trim().trim_matches(['"', '\'']).to_string());
            }
        }
    }

    let name = name.unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    SkillInfo {
        name,
        description,
        location: path.to_path_buf(),
        body: body.to_string(),
    }
}

/// A skill's index entry — name + description only, no body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: Option<String>,
}

/// Process-lifetime discovery cache keyed by the exact roots vector. A skill
/// file added or edited after the first scan for a given roots set is not
/// re-scanned until the process restarts — acceptable because the CLI runs a
/// fresh process per invocation, and it removes the per-`execute` filesystem
/// walk that this phase targets.
#[allow(clippy::type_complexity)]
static DISCOVERY_CACHE: OnceLock<
    Mutex<std::collections::HashMap<Vec<PathBuf>, Arc<BTreeMap<String, SkillInfo>>>>,
> = OnceLock::new();

/// Cached discovery: memoizes [`discover_uncached`] keyed by `roots`.
fn cached_discover(roots: &[PathBuf]) -> Arc<BTreeMap<String, SkillInfo>> {
    let cache = DISCOVERY_CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = roots.to_vec();
    let mut guard = cache.lock().unwrap();
    if let Some(hit) = guard.get(&key) {
        return hit.clone();
    }
    let fresh = Arc::new(discover_uncached(roots));
    guard.insert(key, fresh.clone());
    fresh
}

/// Discover all `SKILL.md` files under `roots`.
fn discover_uncached(roots: &[PathBuf]) -> BTreeMap<String, SkillInfo> {
    let mut skills: BTreeMap<String, SkillInfo> = BTreeMap::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        let walker = WalkBuilder::new(root).hidden(false).build();
        for entry in walker.flatten() {
            if entry.file_name() != "SKILL.md" {
                continue;
            }
            let path = entry.path();
            if let Ok(raw) = std::fs::read_to_string(path) {
                let info = parse_skill(path, &raw);
                skills.entry(info.name.clone()).or_insert(info);
            }
        }
    }
    skills
}

fn global_skill_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var("otto_SKILLS_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".claude/skills"));
    }
    dirs
}

/// The discovery roots for `cwd`: the working directory first, then the global
/// skill dirs (`$otto_SKILLS_DIR`, `~/.claude/skills`).
#[must_use]
pub fn skill_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.to_path_buf()];
    roots.extend(global_skill_dirs());
    roots
}

/// A thin `<available_skills>` index — name + description per discovered skill,
/// NEVER bodies — or `None` if no skills are found. The model reads this to know
/// which skills exist and calls the `skill` tool to load a body on demand.
#[must_use]
pub fn skill_index_block(roots: &[PathBuf]) -> Option<String> {
    let skills = cached_discover(roots);
    if skills.is_empty() {
        return None;
    }
    let metas: Vec<SkillMeta> = skills
        .values()
        .map(|info| SkillMeta {
            name: info.name.clone(),
            description: info.description.clone(),
        })
        .collect();
    let mut out = String::from(
        "The following skills are available. Use the `skill` tool with a skill's name to load its full instructions on demand.\n<available_skills>",
    );
    for meta in &metas {
        out.push_str("\n  <skill><name>");
        out.push_str(&meta.name);
        out.push_str("</name>");
        if let Some(d) = &meta.description {
            out.push_str("<description>");
            out.push_str(d);
            out.push_str("</description>");
        }
        out.push_str("</skill>");
    }
    out.push_str("\n</available_skills>");
    Some(out)
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn id(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        include_str!("../../descriptions/skill.txt")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The name of the skill from available_skills" },
                "section": { "type": "string", "description": "Optional: load only this named section (a ##/### heading) of the skill instead of the full body." }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ExecuteResult, ToolError> {
        let params: SkillParams = decode_args(self.id(), args)?;

        let roots = skill_roots(&ctx.directory);
        let skills = cached_discover(&roots);

        let Some(info) = skills.get(&params.name) else {
            let available: Vec<&str> = skills.keys().map(String::as_str).collect();
            let list = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            return Err(ToolError::Execution(format!(
                "Skill \"{}\" not found. Available skills: {list}",
                params.name
            )));
        };

        ctx.permission
            .ask(PermissionRequest {
                permission: "skill".to_string(),
                patterns: vec![params.name.clone()],
                always: vec![params.name.clone()],
                metadata: serde_json::json!({}),
            })
            .await?;

        let dir = info
            .location
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();

        // Sample sibling files (excluding SKILL.md), up to 10 (skill.ts:36-43).
        let files: Vec<String> = parallel_collect(dir.clone(), Some(10), |entry| {
            if entry.file_name() == "SKILL.md" {
                return Vec::new();
            }
            vec![entry.path().display().to_string()]
        })
        .await;

        let body = if let Some(section) = &params.section {
            match extract_section(&info.body, section) {
                Some(b) => b,
                None => {
                    let heads = section_headings(&info.body).join(", ");
                    return Err(ToolError::Execution(format!(
                        "Section \"{section}\" not found in skill \"{}\". Available sections: {}",
                        info.name,
                        if heads.is_empty() {
                            "none".to_string()
                        } else {
                            heads
                        }
                    )));
                }
            }
        } else {
            info.body.trim().to_string()
        };

        let output = [
            format!("<skill_content name=\"{}\">", info.name),
            format!("# Skill: {}", info.name),
            String::new(),
            body,
            String::new(),
            format!("Base directory for this skill: {}", dir.display()),
            "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.".to_string(),
            "Note: file list is sampled.".to_string(),
            String::new(),
            "<skill_files>".to_string(),
            files
                .iter()
                .map(|f| format!("<file>{f}</file>"))
                .collect::<Vec<_>>()
                .join("\n"),
            "</skill_files>".to_string(),
            "</skill_content>".to_string(),
        ]
        .join("\n");

        Ok(
            ExecuteResult::new(format!("Loaded skill: {}", info.name), output).with_metadata(
                serde_json::json!({ "name": info.name, "dir": dir.display().to_string() }),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discovers_and_returns_named_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skills/pdf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pdf\ndescription: work with pdfs\n---\nDo the PDF thing.\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("helper.py"), "print(1)").unwrap();

        // ctx.directory is scanned before any global dir, so this skill wins.
        let ctx = ToolContext::builder(dir.path()).build();
        let res = SkillTool
            .execute(serde_json::json!({ "name": "pdf" }), &ctx)
            .await
            .unwrap();
        assert!(res.output.contains("<skill_content name=\"pdf\">"));
        assert!(res.output.contains("Do the PDF thing."));
        assert!(res.output.contains("helper.py"));
        assert_eq!(res.metadata["name"], "pdf");
    }

    #[tokio::test]
    async fn unknown_name_lists_available() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skill/foo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\nname: foo\n---\nbody").unwrap();
        let ctx = ToolContext::builder(dir.path()).build();
        let err = SkillTool
            .execute(serde_json::json!({ "name": "missing" }), &ctx)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"));
        assert!(msg.contains("foo"));
    }

    #[test]
    fn skill_index_block_lists_name_and_description_no_body() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path().join("skills/pdf");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(
            sd.join("SKILL.md"),
            "---\nname: pdf\ndescription: work with pdfs\n---\nSECRET BODY should not appear\n",
        )
        .unwrap();
        let block = skill_index_block(&[dir.path().to_path_buf()]).expect("some skills");
        assert!(block.contains("pdf"));
        assert!(block.contains("work with pdfs"));
        assert!(
            !block.contains("SECRET BODY"),
            "index must never include the body"
        );
    }

    #[test]
    fn skill_index_block_none_when_empty() {
        let dir = tempfile::tempdir().unwrap(); // no SKILL.md
        assert!(skill_index_block(&[dir.path().to_path_buf()]).is_none());
    }

    fn skill_dir_with_sections() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path().join("skills/multi");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("SKILL.md"),
            "---\nname: multi\ndescription: d\n---\nintro line\n\n## Setup\nsetup body here\n\n## Usage\nusage body here\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn section_arg_returns_only_that_section() {
        let dir = skill_dir_with_sections();
        let ctx = ToolContext::builder(dir.path()).build();
        let res = SkillTool
            .execute(
                serde_json::json!({ "name": "multi", "section": "Usage" }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.output.contains("usage body here"));
        assert!(
            !res.output.contains("setup body here"),
            "only the named section"
        );
        assert!(
            !res.output.contains("intro line"),
            "section arg excludes the preamble"
        );
    }

    #[tokio::test]
    async fn absent_section_returns_full_body_unchanged() {
        let dir = skill_dir_with_sections();
        let ctx = ToolContext::builder(dir.path()).build();
        let res = SkillTool
            .execute(serde_json::json!({ "name": "multi" }), &ctx)
            .await
            .unwrap();
        // Full body: preamble + both sections all present (backward-compatible).
        assert!(res.output.contains("intro line"));
        assert!(res.output.contains("setup body here"));
        assert!(res.output.contains("usage body here"));
    }

    #[tokio::test]
    async fn unknown_section_errors_with_available_list() {
        let dir = skill_dir_with_sections();
        let ctx = ToolContext::builder(dir.path()).build();
        let err = SkillTool
            .execute(
                serde_json::json!({ "name": "multi", "section": "Nope" }),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Setup") && msg.contains("Usage"),
            "lists available sections: {msg}"
        );
    }

    #[test]
    fn discovery_is_cached_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let sd = dir.path().join("skills/a");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(
            sd.join("SKILL.md"),
            "---\nname: a\ndescription: d\n---\nbody",
        )
        .unwrap();
        let roots = vec![dir.path().to_path_buf()];
        let b1 = skill_index_block(&roots).unwrap();
        // A file added AFTER the first (cached) discovery is not re-scanned.
        let sd2 = dir.path().join("skills/b");
        std::fs::create_dir_all(&sd2).unwrap();
        std::fs::write(
            sd2.join("SKILL.md"),
            "---\nname: b\ndescription: d2\n---\nbody",
        )
        .unwrap();
        let b2 = skill_index_block(&roots).unwrap();
        assert_eq!(
            b1, b2,
            "cached discovery returns stable results for the same roots"
        );
    }
}
