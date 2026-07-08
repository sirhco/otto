//! [`SessionSubagentSpawner`] — the real implementation of
//! [`otto_tools::SubagentSpawner`], a port of the inline child-session drive in
//! opencode `packages/opencode/src/tool/task.ts` (TaskTool, task.ts:116-333).
//!
//! opencode's `TaskTool` resolves the subagent (`agent.get`, task.ts:116),
//! derives the child session permission (`deriveSubagentSessionPermission`,
//! task.ts:125-158), creates the child session (task.ts:142-158), seeds it with
//! the prompt, and runs a child prompt loop, returning the last text part
//! (task.ts:186-200). This type does the same over otto's `Store` + `run_loop`,
//! and re-injects itself (with the child's derived ruleset as the new parent)
//! into the child [`RunConfig`] so nested `task` calls recurse.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use otto_agent::{AgentInfo, config as agent_config, derive_subagent_permission};
use otto_llm::{Model, Route};
use otto_permission::{Permission, Ruleset, SessionGate, merge};
use otto_storage::model::{
    Info, InfoBody, Part, PartKind, User, UserModel, UserTime, new_message_id, new_part_id,
};
use otto_storage::{Session, SessionTokens, Store};
use otto_tools::{PermissionGate, SubagentRequest, SubagentSpawner, ToolError, ToolRegistry};
use serde_json::json;

use crate::run::{RunConfig, run_loop};
use crate::warm::{WarmCache, WarmKey, compute_warm};

/// A factory that maps a resolved subagent to the [`Route`] + [`Model`] its
/// child loop should generate with — the otto analogue of task.ts:167-170
/// (`next.model ?? { modelID, providerID }`). The caller decides whether to
/// honor `subagent.model` or fall back to the parent's model; the spawner just
/// asks for `(route, model)` per child.
pub type RouteFor = Arc<dyn Fn(&AgentInfo) -> (Arc<dyn Route>, Model) + Send + Sync>;

/// Current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The session-backed [`SubagentSpawner`]. Holds everything needed to run a
/// child agent loop; cloned (with a new `parent_ruleset`) to re-inject into
/// nested child runs.
#[derive(Clone)]
pub struct SessionSubagentSpawner {
    store: Store,
    tools: Arc<ToolRegistry>,
    permission: Arc<Permission>,
    /// The parent session's permission ruleset — the `parentSessionPermission`
    /// fed to `deriveSubagentSessionPermission` (task.ts:125-128). For a nested
    /// spawn this is the enclosing child's derived ruleset.
    parent_ruleset: Ruleset,
    /// The resolved `config.agent` object used to look up subagents
    /// (`agent.get`, task.ts:116).
    config_agents: serde_json::Value,
    route_for: RouteFor,
    directory: PathBuf,
    project_id: String,
    version: String,
    /// Warm-boot system-prompt cache, memoized per `(provider, model, agent,
    /// directory, user_system)` and shared across nested spawns (see
    /// [`crate::warm`]).
    warm: Arc<Mutex<HashMap<WarmKey, Arc<WarmCache>>>>,
    /// The tersemode brevity directive (resolved from `config.tersemode` by
    /// `otto-app`), baked into every child's warm cache so subagent output is
    /// terse too. `None` disables it. Carried across nested spawns via
    /// [`Self::with_parent_ruleset`]'s clone.
    tersemode_directive: Option<String>,
}

impl SessionSubagentSpawner {
    /// Build a spawner.
    ///
    /// * `store` / `tools` / `permission` — the ambient services the child loop
    ///   reuses (the child gets its own [`SessionGate`] over the shared
    ///   [`Permission`]).
    /// * `parent_ruleset` — the parent session's permission, narrowed into the
    ///   child via [`derive_subagent_permission`].
    /// * `config_agents` — the resolved `config.agent` object for agent lookup.
    /// * `route_for` — the per-subagent route/model factory.
    /// * `directory` / `project_id` / `version` — child session scaffolding.
    /// * `tersemode_directive` — the resolved `config.tersemode` brevity directive
    ///   (or `None`), baked into every child's warm cache.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Store,
        tools: Arc<ToolRegistry>,
        permission: Arc<Permission>,
        parent_ruleset: Ruleset,
        config_agents: serde_json::Value,
        route_for: RouteFor,
        directory: PathBuf,
        project_id: impl Into<String>,
        version: impl Into<String>,
        tersemode_directive: Option<String>,
    ) -> Self {
        Self {
            store,
            tools,
            permission,
            parent_ruleset,
            config_agents,
            route_for,
            directory,
            project_id: project_id.into(),
            version: version.into(),
            warm: Arc::new(Mutex::new(HashMap::new())),
            tersemode_directive,
        }
    }

    /// Clone this spawner with a new `parent_ruleset` — used to re-inject into a
    /// child run so a nested `task` derives from the child's (already narrowed)
    /// ruleset rather than the original parent's.
    fn with_parent_ruleset(&self, parent_ruleset: Ruleset) -> Self {
        let mut next = self.clone();
        next.parent_ruleset = parent_ruleset;
        next
    }

    /// The names of the agents available for `task`, for the unknown-agent
    /// error (task.ts:118 lists the invalid type; otto additionally enumerates
    /// the valid ones to help the model retry).
    fn available_agents(&self) -> String {
        let mut names: Vec<String> = agent_config::list(&self.config_agents)
            .into_iter()
            .filter(|a| !a.hidden)
            .map(|a| a.name)
            .collect();
        names.dedup();
        names.join(", ")
    }
}

/// Map a storage error into a model-facing [`ToolError`].
fn storage_err(e: otto_storage::StorageError) -> ToolError {
    ToolError::Execution(format!("subagent storage error: {e}"))
}

#[async_trait::async_trait]
impl SubagentSpawner for SessionSubagentSpawner {
    async fn spawn(&self, req: SubagentRequest) -> Result<String, ToolError> {
        // 1. Resolve the subagent (task.ts:116-119).
        let subagent =
            agent_config::get(&self.config_agents, &req.subagent_type).ok_or_else(|| {
                ToolError::Execution(format!(
                    "Unknown agent type: {} is not a valid agent type. Available agents: {}",
                    req.subagent_type,
                    self.available_agents()
                ))
            })?;

        // 2. Derive the child session permission (task.ts:125-158). The child
        //    session ruleset = the subagent's own permission overlaid with the
        //    parent-derived denies/directory scoping. Persisted on the child
        //    session record (otto's `Session` has no `permission` column, so it
        //    lives in `metadata`, mirroring opencode storing it on the session).
        let derived = derive_subagent_permission(&self.parent_ruleset, &subagent);
        let child_ruleset = merge(&[&subagent.permission, &derived]);

        // 3. Create the child session (task.ts:142-158). `task_id` resume
        //    (task.ts:121-123) reuses an existing session id when present.
        let (route, model) = (self.route_for)(&subagent);
        let child_session_id = match &req.task_id {
            Some(id)
                if self
                    .store
                    .get_session(id)
                    .await
                    .map_err(storage_err)?
                    .is_some() =>
            {
                id.clone()
            }
            _ => {
                let id = otto_id::ascending(otto_id::Prefix::Session);
                let now = now_ms();
                self.store
                    .create_session(&Session {
                        id: id.clone(),
                        project_id: self.project_id.clone(),
                        parent_id: Some(req.parent_session_id.clone()),
                        directory: self.directory.display().to_string(),
                        title: format!("{} (@{} subagent)", req.description, subagent.name),
                        version: self.version.clone(),
                        cost: 0.0,
                        tokens: SessionTokens::default(),
                        metadata: Some(json!({ "permission": child_ruleset })),
                        time_created: now,
                        time_updated: now,
                    })
                    .await
                    .map_err(storage_err)?;
                id
            }
        };

        // 4. Seed the child session with the prompt as a user message
        //    (task.ts:186-198).
        let user_id = new_message_id();
        let user = Info {
            id: user_id.clone(),
            session_id: child_session_id.clone(),
            body: InfoBody::User(User {
                time: UserTime { created: now_ms() },
                format: None,
                summary: None,
                agent: subagent.name.clone(),
                model: UserModel {
                    provider_id: model.provider.0.clone(),
                    model_id: model.id.0.clone(),
                    variant: subagent.variant.clone(),
                },
                system: None,
                tools: None,
            }),
        };
        self.store
            .insert_message(&user)
            .await
            .map_err(storage_err)?;
        self.store
            .insert_part(&Part {
                id: new_part_id(),
                session_id: child_session_id.clone(),
                message_id: user_id,
                kind: PartKind::Text {
                    text: req.prompt.clone(),
                    synthetic: None,
                    ignored: None,
                    time: None,
                    metadata: None,
                },
            })
            .await
            .map_err(storage_err)?;

        // 5. Build the child RunConfig (task.ts:186-200). The child runs under
        //    its own SessionGate (over the shared Permission service) and
        //    re-injects this spawner with the child ruleset as the new parent so
        //    a nested `task` recurses correctly.
        let child_gate: Arc<dyn PermissionGate> = Arc::new(SessionGate::new(
            self.permission.clone(),
            child_session_id.clone(),
        ));
        let child_spawner: Arc<dyn SubagentSpawner> =
            Arc::new(self.with_parent_ruleset(child_ruleset));
        let system_cache = Some(compute_warm(
            &self.warm,
            &self.directory,
            &model,
            &subagent.name,
            subagent.prompt.as_deref(),
            self.tersemode_directive.as_deref(),
        ));
        let cfg = RunConfig {
            store: self.store.clone(),
            route,
            tools: self.tools.clone(),
            permission: child_gate,
            model,
            agent: subagent.name.clone(),
            agent_prompt: subagent.prompt.clone(),
            directory: self.directory.clone(),
            max_steps: subagent.steps,
            abort: req.abort.clone(),
            subagent: Some(child_spawner),
            preserve_recent_tokens: crate::run::DEFAULT_PRESERVE_RECENT_TOKENS,
            compaction_reserved: crate::run::DEFAULT_COMPACTION_RESERVED,
            auto_compact: true,
            max_retries: crate::run::DEFAULT_MAX_RETRIES,
            // Forward the request's optional event tap into the child run. `None`
            // (the default for the `task` tool + tests) leaves the child
            // untapped, byte-identical to the prior hard-coded behavior; `Some`
            // surfaces the child's live event stream to the requester.
            event_tx: req.event_tx,
            system_cache,
            // Unused on the cached path (the directive is already baked into
            // `system_cache` above), but carried so a nested spawn that rebuilds
            // still has it.
            tersemode_directive: self.tersemode_directive.clone(),
        };

        // 6. Run the child loop and 7. return its last assistant text
        //    (task.ts:199).
        let last = run_loop(&cfg, &child_session_id)
            .await
            .map_err(|e| ToolError::Execution(format!("subagent run failed: {e}")))?;

        let parts = self
            .store
            .list_parts(last.id())
            .await
            .map_err(storage_err)?;
        let text: String = parts
            .iter()
            .filter_map(|p| match &p.kind {
                PartKind::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        Ok(text)
    }

    /// Parallel batch dispatch: each child builds its own session id and
    /// `SessionGate` inside `spawn`, so there is no shared mutable state to
    /// race — `join_all` runs them concurrently. Preserves input order and
    /// isolates per-request errors (each future's `Result` is collected
    /// independently), matching the trait contract.
    async fn spawn_many(&self, reqs: Vec<SubagentRequest>) -> Vec<Result<String, ToolError>> {
        futures::future::join_all(reqs.into_iter().map(|r| self.spawn(r))).await
    }
}
