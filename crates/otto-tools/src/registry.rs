//! The tool registry + executor — a port of the model-gating and truncation
//! wiring in opencode `packages/opencode/src/tool/registry.ts` and the
//! post-execute truncation from `tool.ts:131-144`.

use std::sync::Arc;

use otto_hooks::{Decision, HookEvent, HookRunner};
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
    /// External lifecycle-hooks runner (`PreToolUse`/`PostToolUse`); `None`
    /// disables lifecycle hooks entirely. Distinct from `hooks` above (the
    /// existing in-process, compiled-in `ToolHook` seam) — the two coexist.
    lifecycle_hooks: Option<Arc<HookRunner>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            hooks: Vec::new(),
            lifecycle_hooks: None,
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

    /// Install the external lifecycle-hooks runner. `None` (the default)
    /// disables `PreToolUse`/`PostToolUse` firing entirely; the existing
    /// in-process [`ToolHook`] seam is unaffected either way.
    pub fn set_lifecycle_hooks(&mut self, runner: Arc<HookRunner>) {
        self.lifecycle_hooks = Some(runner);
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

        // External lifecycle PreToolUse hook: fires first, ahead of the
        // in-process ToolHook stage below, so a denying hook blocks before
        // any built-in arg-rewriting or the tool itself runs. `Ask` escalates
        // through the real permission gate; `Deny` blocks outright.
        if let Some(runner) = &self.lifecycle_hooks {
            let verdict = runner
                .fire(HookEvent::PreToolUse {
                    session_id: ctx.session_id.clone(),
                    tool_id: tool_id.to_string(),
                    args: args.clone(),
                    cwd: ctx.directory.clone(),
                })
                .await;
            match verdict.decision {
                Decision::Allow => {}
                Decision::Deny => {
                    let reason = verdict
                        .reason
                        .unwrap_or_else(|| "blocked by pre_tool_use hook".to_string());
                    return Err(ToolError::Execution(reason));
                }
                Decision::Ask => {
                    let req = crate::hook_escalation::build_hook_permission_request(
                        "pre_tool_use",
                        &verdict,
                        Some(tool_id),
                    );
                    let result = ctx.permission.ask(req).await;
                    let outcome =
                        crate::hook_escalation::interpret_hook_ask_result(result, &verdict);
                    if !outcome.approved {
                        let reason = outcome
                            .message
                            .unwrap_or_else(|| "blocked by pre_tool_use hook".to_string());
                        return Err(ToolError::Execution(reason));
                    }
                }
            }
        }

        // Pre-execute hook seam: each hook may rewrite the args or block the
        // call. Hooks run in registration order, each seeing the prior output.
        let mut args = args;
        for hook in &self.hooks {
            match hook.before_execute(tool_id, args, ctx).await {
                HookOutcome::Continue(next) => args = next,
                HookOutcome::Deny(reason) => return Err(ToolError::Execution(reason)),
            }
        }

        let exec_result = tool.execute(args.clone(), ctx).await;

        // External lifecycle PostToolUse hook: fires whether the call
        // succeeded or failed, so a hook can react to tool failures too.
        let post_verdict = if let Some(runner) = &self.lifecycle_hooks {
            Some(
                runner
                    .fire(HookEvent::PostToolUse {
                        session_id: ctx.session_id.clone(),
                        tool_id: tool_id.to_string(),
                        args: args.clone(),
                        success: exec_result.is_ok(),
                        cwd: ctx.directory.clone(),
                    })
                    .await,
            )
        } else {
            None
        };

        let mut result = exec_result?;

        if let Some(message) = post_verdict.and_then(|v| v.system_message) {
            if let Some(map) = result.metadata.as_object_mut() {
                map.insert("hookMessage".to_string(), Value::String(message));
            } else {
                result.metadata = serde_json::json!({ "hookMessage": message });
            }
        }

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
    use otto_hooks::{HookCommand, HookMatcherGroup, HookRunner, HooksConfig};

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

    fn hook_runner_denying(tool_id_pattern: &str, reason: &str) -> Arc<HookRunner> {
        let cfg = HooksConfig {
            pre_tool_use: vec![HookMatcherGroup {
                matcher: Some(tool_id_pattern.to_string()),
                hooks: vec![HookCommand {
                    command: format!("echo '{{\"decision\":\"deny\",\"reason\":\"{reason}\"}}'"),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        };
        Arc::new(HookRunner::new(cfg))
    }

    fn hook_runner_with_system_message(message: &str) -> Arc<HookRunner> {
        let cfg = HooksConfig {
            post_tool_use: vec![HookMatcherGroup {
                matcher: None,
                hooks: vec![HookCommand {
                    command: format!("echo '{{\"system_message\":\"{message}\"}}'"),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        };
        Arc::new(HookRunner::new(cfg))
    }

    struct RejectingGate {
        message: &'static str,
    }
    #[async_trait]
    impl crate::tool::PermissionGate for RejectingGate {
        async fn ask(
            &self,
            req: crate::tool::PermissionRequest,
        ) -> Result<(), crate::tool::PermissionDenied> {
            Err(crate::tool::PermissionDenied {
                permission: req.permission,
                by_user: true,
                message: Some(self.message.to_string()),
            })
        }
    }

    fn ctx_with_gate(gate: std::sync::Arc<dyn crate::tool::PermissionGate>) -> ToolContext {
        ToolContext::builder(std::env::temp_dir())
            .permission(gate)
            .build()
    }

    fn hook_runner_asking(tool_id_pattern: &str, reason: &str) -> Arc<HookRunner> {
        let cfg = HooksConfig {
            pre_tool_use: vec![HookMatcherGroup {
                matcher: Some(tool_id_pattern.to_string()),
                hooks: vec![HookCommand {
                    command: format!("echo '{{\"decision\":\"ask\",\"reason\":\"{reason}\"}}'"),
                    timeout_ms: None,
                }],
            }],
            ..Default::default()
        };
        Arc::new(HookRunner::new(cfg))
    }

    #[tokio::test]
    async fn pre_tool_use_ask_approved_runs_the_tool() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(BigOutput));
        r.set_lifecycle_hooks(hook_runner_asking("big", "needs review"));

        // ctx() uses the AllowAll gate, which approves any ask.
        let res = r.execute("big", serde_json::json!({}), &ctx()).await;
        assert!(res.is_ok(), "AllowAll gate approves the ask, tool runs");
    }

    #[tokio::test]
    async fn pre_tool_use_ask_rejected_blocks_with_the_human_message() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(BigOutput));
        r.set_lifecycle_hooks(hook_runner_asking("big", "needs review"));

        let ctx = ctx_with_gate(Arc::new(RejectingGate {
            message: "reviewer said no",
        }));
        let err = r
            .execute("big", serde_json::json!({}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "reviewer said no");
    }

    #[tokio::test]
    async fn pre_tool_use_deny_blocks_before_the_tool_runs() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(BigOutput));
        r.set_lifecycle_hooks(hook_runner_denying("big", "blocked by policy"));

        let err = r
            .execute("big", serde_json::json!({}), &ctx())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "blocked by policy");
    }

    #[tokio::test]
    async fn pre_tool_use_non_matching_tool_id_is_unaffected() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(BigOutput));
        // matcher only targets "edit", not "big" — the call must succeed.
        r.set_lifecycle_hooks(hook_runner_denying("^edit$", "should never fire"));

        let res = r.execute("big", serde_json::json!({}), &ctx()).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn post_tool_use_folds_system_message_into_metadata() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(OptOut));
        r.set_lifecycle_hooks(hook_runner_with_system_message("fyi"));

        let res = r
            .execute("optout", serde_json::json!({}), &ctx())
            .await
            .unwrap();
        assert_eq!(res.metadata["hookMessage"], "fyi");
    }

    #[tokio::test]
    async fn no_lifecycle_hooks_configured_behaves_exactly_as_before() {
        let mut r = ToolRegistry::new();
        r.register(Arc::new(BigOutput));
        // no set_lifecycle_hooks call at all — must behave identically to
        // the pre-existing (pre-hooks) execute() path.
        let res = r.execute("big", serde_json::json!({}), &ctx()).await;
        assert!(res.is_ok());
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
            if tool_id == "bash"
                && let Some(cmd) = args["command"].as_str()
            {
                args["command"] = Value::String(format!("wrapped {cmd}"));
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
            .execute(
                "bash",
                serde_json::json!({ "command": "git status" }),
                &ctx(),
            )
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
            .execute(
                "bash",
                serde_json::json!({ "command": "git status" }),
                &ctx(),
            )
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
