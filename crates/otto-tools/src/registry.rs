//! The tool registry + executor — a port of the model-gating and truncation
//! wiring in opencode `packages/opencode/src/tool/registry.ts` and the
//! post-execute truncation from `tool.ts:131-144`.

use std::sync::Arc;

use serde_json::Value;

use crate::hook::{HookOutcome, ToolHook};
use crate::tool::{ExecuteResult, Tool, ToolContext, ToolError};
use crate::tools::{
    ApplyPatchTool, BashTool, EditTool, GlobTool, GrepTool, InvalidTool, QuestionTool, ReadTool,
    SkillTool, TaskTool, TodoWriteTool, WebFetchTool, WebSearchTool, WriteTool,
};
use crate::truncate::{MAX_BYTES, MAX_LINES, truncate_output};

/// Holds the set of available tools and dispatches execution.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    hooks: Vec<Arc<dyn ToolHook>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            hooks: Vec::new(),
        }
    }

    /// A registry pre-populated with every built-in tool implemented in this
    /// crate. `websearch` is registered without a provider (it errors until one
    /// is injected) and `task`/`question` are Phase-later stubs.
    ///
    /// `lsp` is an optional [`crate::LspHandle`] threaded into the
    /// `write`/`edit`/`apply_patch` tools so they can append a `<diagnostics>`
    /// block to their output after a write; pass `None` to disable diagnostics.
    pub fn with_builtins(lsp: Option<Arc<dyn crate::LspHandle>>) -> Self {
        let mut r = Self::new();
        r.register(Arc::new(ReadTool));
        r.register(Arc::new(WriteTool::new(lsp.clone())));
        r.register(Arc::new(EditTool::new(lsp.clone())));
        r.register(Arc::new(GlobTool));
        r.register(Arc::new(GrepTool));
        r.register(Arc::new(BashTool));
        r.register(Arc::new(ApplyPatchTool::new(lsp.clone())));
        r.register(Arc::new(WebFetchTool));
        r.register(Arc::new(TodoWriteTool));
        r.register(Arc::new(WebSearchTool::default()));
        r.register(Arc::new(SkillTool));
        r.register(Arc::new(QuestionTool));
        r.register(Arc::new(InvalidTool));
        r.register(Arc::new(TaskTool));
        r
    }

    /// Add a tool to the registry.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Add a pre-execute [`ToolHook`]. Hooks run in registration order before
    /// every tool call; see [`crate::hook`].
    pub fn register_hook(&mut self, hook: Arc<dyn ToolHook>) {
        self.hooks.push(hook);
    }

    /// Look up a tool by id.
    pub fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.id() == id).cloned()
    }

    /// All registered tools.
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    /// The tools available for a given model, applying the gating switch from
    /// `registry.ts:266-278`:
    ///
    /// * gpt-5-class models (`gpt-` but not `oss`/`gpt-4`) use `apply_patch` in
    ///   place of `edit`/`write`;
    /// * other models use `edit`/`write` and drop `apply_patch`.
    ///
    /// `apply_patch`/`websearch` are gated by id string only (they are supplied
    /// by a sibling task); their absence is tolerated.
    pub fn tools_for_model(&self, model_id: &str) -> Vec<Arc<dyn Tool>> {
        let use_patch =
            model_id.contains("gpt-") && !model_id.contains("oss") && !model_id.contains("gpt-4");
        self.tools
            .iter()
            .filter(|t| match t.id() {
                "apply_patch" => use_patch,
                "edit" | "write" => !use_patch,
                // websearch: cannot evaluate provider gating here; keep it.
                _ => true,
            })
            .cloned()
            .collect()
    }

    /// Execute a tool by id, then apply output truncation unless the tool
    /// already set a `truncated` key in its metadata (the opt-out at
    /// `tool.ts:131-133`).
    pub async fn execute(
        &self,
        tool_id: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ExecuteResult, ToolError> {
        let tool = self
            .get(tool_id)
            .ok_or_else(|| ToolError::Execution(format!("unknown tool: {tool_id}")))?;

        // Pre-execute hook seam: each hook may rewrite the args or block the
        // call. Hooks run in registration order, each seeing the prior output.
        let mut args = args;
        for hook in &self.hooks {
            match hook.before_execute(tool_id, args, ctx).await {
                HookOutcome::Continue(next) => args = next,
                HookOutcome::Deny(reason) => return Err(ToolError::Execution(reason)),
            }
        }

        let mut result = tool.execute(args, ctx).await?;

        let opted_out = result
            .metadata
            .as_object()
            .map(|m| m.contains_key("truncated"))
            .unwrap_or(false);
        if opted_out {
            return Ok(result);
        }

        let has_task = self.get("task").is_some();
        let truncated = truncate_output(&result.output, MAX_LINES, MAX_BYTES, has_task);
        result.output = truncated.content;
        if let Some(map) = result.metadata.as_object_mut() {
            map.insert("truncated".to_string(), Value::Bool(truncated.truncated));
        } else {
            result.metadata = serde_json::json!({ "truncated": truncated.truncated });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use async_trait::async_trait;

    struct BigOutput;
    #[async_trait]
    impl Tool for BigOutput {
        fn id(&self) -> &str {
            "big"
        }
        fn description(&self) -> &str {
            "emits many lines with no truncated metadata"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ExecuteResult, ToolError> {
            let body = (0..5000)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            Ok(ExecuteResult::new("big", body))
        }
    }

    struct OptOut;
    #[async_trait]
    impl Tool for OptOut {
        fn id(&self) -> &str {
            "optout"
        }
        fn description(&self) -> &str {
            "pre-sets truncated metadata to opt out"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolContext,
        ) -> Result<ExecuteResult, ToolError> {
            let body = (0..5000)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            Ok(ExecuteResult::new("optout", body)
                .with_metadata(serde_json::json!({ "truncated": false })))
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::builder(std::env::temp_dir()).build()
    }

    #[test]
    fn builtins_registered() {
        let r = ToolRegistry::with_builtins(None);
        for id in [
            "read",
            "write",
            "edit",
            "glob",
            "grep",
            "bash",
            "apply_patch",
            "webfetch",
            "todowrite",
            "websearch",
            "skill",
            "question",
            "invalid",
            "task",
        ] {
            assert!(r.get(id).is_some(), "missing {id}");
        }
        assert_eq!(r.list().len(), 14);
    }

    #[test]
    fn model_gating_swaps_edit_write_for_patch() {
        let mut r = ToolRegistry::with_builtins(None);
        // stand-in apply_patch tool (id-gated).
        struct Patch;
        #[async_trait]
        impl Tool for Patch {
            fn id(&self) -> &str {
                "apply_patch"
            }
            fn description(&self) -> &str {
                "x"
            }
            fn parameters_schema(&self) -> Value {
                serde_json::json!({})
            }
            async fn execute(
                &self,
                _a: Value,
                _c: &ToolContext,
            ) -> Result<ExecuteResult, ToolError> {
                Ok(ExecuteResult::new("", ""))
            }
        }
        r.register(Arc::new(Patch));

        let gpt5: Vec<String> = r
            .tools_for_model("gpt-5")
            .iter()
            .map(|t| t.id().to_string())
            .collect();
        assert!(gpt5.contains(&"apply_patch".to_string()));
        assert!(!gpt5.contains(&"edit".to_string()));
        assert!(!gpt5.contains(&"write".to_string()));

        let other: Vec<String> = r
            .tools_for_model("claude-sonnet")
            .iter()
            .map(|t| t.id().to_string())
            .collect();
        assert!(!other.contains(&"apply_patch".to_string()));
        assert!(other.contains(&"edit".to_string()));
        assert!(other.contains(&"write".to_string()));

        // gpt-4 is explicitly excluded from the patch path.
        let gpt4: Vec<String> = r
            .tools_for_model("gpt-4o")
            .iter()
            .map(|t| t.id().to_string())
            .collect();
        assert!(gpt4.contains(&"edit".to_string()));
        assert!(!gpt4.contains(&"apply_patch".to_string()));
    }

    #[tokio::test]
    async fn execute_truncates_when_not_opted_out() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(BigOutput));
        let res = r
            .execute("big", serde_json::json!({}), &ctx())
            .await
            .unwrap();
        assert!(res.output.contains("lines truncated"));
        assert_eq!(res.metadata["truncated"], Value::Bool(true));
    }

    #[tokio::test]
    async fn execute_respects_opt_out() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(OptOut));
        let res = r
            .execute("optout", serde_json::json!({}), &ctx())
            .await
            .unwrap();
        // untouched: full 5000-line body, no marker.
        assert!(!res.output.contains("lines truncated"));
        assert_eq!(res.metadata["truncated"], Value::Bool(false));
    }

    /// Echoes back its `command` arg as output, so a test can observe whether a
    /// hook rewrote the args before execution.
    struct EchoCommand;
    #[async_trait]
    impl Tool for EchoCommand {
        fn id(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "echoes the command arg"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({})
        }
        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolContext,
        ) -> Result<ExecuteResult, ToolError> {
            let cmd = args["command"].as_str().unwrap_or_default().to_string();
            Ok(ExecuteResult::new("echo", cmd))
        }
    }

    /// A hook that prefixes the `command` arg for the `bash` tool.
    struct PrefixHook;
    #[async_trait]
    impl crate::hook::ToolHook for PrefixHook {
        async fn before_execute(
            &self,
            tool_id: &str,
            mut args: Value,
            _ctx: &ToolContext,
        ) -> crate::hook::HookOutcome {
            if tool_id == "bash" {
                if let Some(cmd) = args["command"].as_str() {
                    args["command"] = Value::String(format!("wrapped {cmd}"));
                }
            }
            crate::hook::HookOutcome::Continue(args)
        }
    }

    /// A hook that always blocks.
    struct DenyHook;
    #[async_trait]
    impl crate::hook::ToolHook for DenyHook {
        async fn before_execute(
            &self,
            _tool_id: &str,
            _args: Value,
            _ctx: &ToolContext,
        ) -> crate::hook::HookOutcome {
            crate::hook::HookOutcome::Deny("blocked by policy".to_string())
        }
    }

    #[tokio::test]
    async fn hook_rewrites_args_before_execute() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoCommand));
        r.register_hook(Arc::new(PrefixHook));
        let res = r
            .execute("bash", serde_json::json!({ "command": "git status" }), &ctx())
            .await
            .unwrap();
        assert_eq!(res.output, "wrapped git status");
    }

    #[tokio::test]
    async fn hook_deny_short_circuits() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(EchoCommand));
        r.register_hook(Arc::new(DenyHook));
        let err = r
            .execute("bash", serde_json::json!({ "command": "git status" }), &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked by policy"));
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let r = ToolRegistry::new();
        let err = r
            .execute("nope", serde_json::json!({}), &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown tool"));
    }
}
