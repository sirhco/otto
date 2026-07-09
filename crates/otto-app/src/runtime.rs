//! The [`Runtime`]: the assembled, ready-to-drive session runtime.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use otto_agent::{AgentInfo, ModelRef, resolve_agents};
use otto_auth::AuthStore;
use otto_config::{Config, TersemodeLevel};
use otto_events::LLMEvent;
use otto_llm::HttpTransport;
use otto_mcp::{McpClient, McpServerConfig};
use otto_permission::{Permission, PermissionMode, Ruleset, SessionGate};
use otto_session::run::{
    DEFAULT_COMPACTION_RESERVED, DEFAULT_MAX_RETRIES, DEFAULT_MAX_TOTAL_RETRIES,
    DEFAULT_PRESERVE_RECENT_TOKENS,
};
use otto_session::{RouteFor, RunConfig, SessionSubagentSpawner, run_loop};
use otto_storage::model::{
    Info, InfoBody, Part, PartKind, User, UserModel, UserTime, new_message_id, new_part_id,
};
use otto_storage::{Session, SessionTokens, Store};
use otto_tools::{PermissionGate, SubagentSpawner, ToolRegistry};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::route_factory::{AuthRouteFactory, ProviderOverride, RouteFactory};
use crate::{Result, RunError};

/// Reported as the session `version` and the MCP client version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build the [`AuthRouteFactory`] provider-override map from
/// `config.provider.<id>`, keeping entries that supply a non-empty `baseURL`
/// and/or per-model `limits` — an entry with neither supplies nothing any
/// route arm can use, so it's dropped rather than stored as a no-op override.
fn provider_overrides(config: &Config) -> HashMap<String, ProviderOverride> {
    config
        .provider_overrides()
        .into_iter()
        .filter_map(|(id, entry)| {
            let base_url = entry
                .options
                .base_url
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_default();
            let model_limits: HashMap<String, crate::route_factory::ModelLimitsOverride> = entry
                .models
                .into_iter()
                .filter_map(|(model, m)| {
                    m.limits.map(|l| {
                        (
                            model,
                            crate::route_factory::ModelLimitsOverride {
                                context: l.context,
                                output: l.output,
                            },
                        )
                    })
                })
                .collect();
            if base_url.is_empty() && model_limits.is_empty() {
                return None;
            }
            Some((
                id,
                ProviderOverride {
                    base_url,
                    api_key: entry.options.api_key,
                    model_limits,
                },
            ))
        })
        .collect()
}

/// The result of [`Runtime::run`]: the streaming events and the join handle for
/// the final assistant message.
pub struct RunHandle {
    /// Live [`LLMEvent`]s tapped from the run as they stream (closes when the
    /// run finishes). Port of the streaming seam the CLI/server render from.
    pub events: mpsc::UnboundedReceiver<LLMEvent>,
    /// The spawned agent loop; resolves to the final [`Info`] or a [`RunError`].
    pub join: JoinHandle<std::result::Result<Info, RunError>>,
}

/// The assembled session runtime shared by the CLI and the server.
///
/// Built by [`Runtime::load`] (real config/auth/storage/MCP) or
/// [`Runtime::in_memory`] (headless, for tests), then driven with
/// [`Runtime::create_session`] + [`Runtime::run`]. The route seam is injectable
/// via [`Runtime::with_route_factory`] and the toolset via
/// [`Runtime::with_tools`].
pub struct Runtime {
    store: Store,
    tools: Arc<ToolRegistry>,
    permission: Arc<Permission>,
    agents: Vec<AgentInfo>,
    config: Config,
    auth: AuthStore,
    transport: Arc<HttpTransport>,
    directory: PathBuf,
    route_factory: Arc<dyn RouteFactory>,
    project_id: String,
    version: String,
    lsp: Arc<otto_lsp::Lsp>,
}

impl Runtime {
    /// Boot a runtime rooted at `cwd`: load config, open the persistent store at
    /// `global_data_dir()/otto.db`, resolve agents, build the permission
    /// service from `config.permission`, register built-in tools plus the tools
    /// of any (best-effort) connected `config.mcp` servers, and install the
    /// default credential-backed [`AuthRouteFactory`].
    ///
    /// # Errors
    /// Returns [`crate::Error`] on config, auth, storage, or filesystem failure.
    pub async fn load(cwd: impl Into<PathBuf>) -> Result<Runtime> {
        let directory = cwd.into();
        let config = otto_config::load(&directory)?;

        let data_dir = otto_config::paths::global_data_dir();
        std::fs::create_dir_all(&data_dir)?;
        let store = Store::open(data_dir.join("otto.db")).await?;

        let auth = AuthStore::new()?;
        let transport = Arc::new(HttpTransport::new());

        // Best-effort models.dev registry refresh: fresh disk cache, else
        // network fetch, else stale cache, else the embedded snapshot. `load`
        // never panics and always returns a usable `Registry`, so a network
        // failure here must never fail runtime boot. Installed before the
        // `AuthRouteFactory` is built so the first `p.model(...)` lookup sees
        // live data.
        let cache = otto_config::paths::global_cache_dir().join("models.json");
        let opts = otto_llm::models_dev::LoadOptions::from_env(cache);
        let registry = otto_llm::models_dev::load(&opts).await;
        otto_llm::registry::install(registry);

        let agents = resolve_agents(&agent_config(&config));
        let permission = Arc::new(Permission::with_mode(
            permission_ruleset(&config),
            permission_mode(&config),
        ));

        // Construct the LSP service and inject it into the diagnostics-aware
        // tools (edit/write/apply_patch). Retained on the runtime so the server
        // can surface `/lsp` status.
        let lsp: Arc<otto_lsp::Lsp> = otto_lsp::Lsp::new(
            directory.clone(),
            otto_lsp::config::resolve(config.lsp.as_ref()),
        );
        let lsp_handle: Option<Arc<dyn otto_tools::LspHandle>> =
            Some(lsp.clone() as Arc<dyn otto_tools::LspHandle>);
        let mut registry = ToolRegistry::with_builtins(lsp_handle);
        registry.register_hook(Arc::new(otto_tools::RtkHook::new(rtk_enabled(&config))));
        // Best-effort MCP: connect each configured server and register its
        // namespaced tools. A server that fails to connect is skipped, never
        // fatal (mirrors opencode tolerating an unreachable MCP server).
        if let Some(mcp) = &config.mcp {
            let servers: BTreeMap<String, McpServerConfig> =
                serde_json::from_value(mcp.clone()).unwrap_or_default();
            if !servers.is_empty() {
                let client = McpClient::new(VERSION);
                for (name, server) in &servers {
                    let _ = client.connect(name.clone(), server).await;
                }
                for tool in client.tools() {
                    registry.register(tool);
                }
            }
        }
        let tools = Arc::new(registry);

        let route_factory: Arc<dyn RouteFactory> = Arc::new(AuthRouteFactory::new(
            auth.all().unwrap_or_default(),
            transport.clone(),
            provider_overrides(&config),
        ));

        let project_id = project_id_for(&directory);
        Ok(Runtime {
            store,
            tools,
            permission,
            agents,
            config,
            auth,
            transport,
            directory,
            route_factory,
            project_id,
            version: VERSION.to_string(),
            lsp,
        })
    }

    /// Assemble a headless runtime over an in-memory store and an empty
    /// credential store — for tests. Combine with [`Runtime::with_route_factory`]
    /// (a scripted factory) and [`Runtime::with_tools`] to run with no network.
    ///
    /// # Errors
    /// Returns [`crate::Error`] on storage failure.
    pub async fn in_memory(config: Config) -> Result<Runtime> {
        let directory = std::env::temp_dir();
        let store = Store::open_in_memory().await?;
        let auth = AuthStore::with_content("{}");
        let transport = Arc::new(HttpTransport::new());

        // Headless twin: never touch the network. Install the embedded
        // models.dev snapshot explicitly (rather than relying on
        // `registry::current`'s lazy default) so tests can assert the
        // registry is populated deterministically.
        otto_llm::registry::install(otto_llm::models_dev::Registry::embedded());

        let agents = resolve_agents(&agent_config(&config));
        let permission = Arc::new(Permission::with_mode(
            permission_ruleset(&config),
            permission_mode(&config),
        ));
        let lsp: Arc<otto_lsp::Lsp> = otto_lsp::Lsp::new(
            directory.clone(),
            otto_lsp::LspConfigResolved::enabled_default(),
        );
        let lsp_handle: Option<Arc<dyn otto_tools::LspHandle>> =
            Some(lsp.clone() as Arc<dyn otto_tools::LspHandle>);
        let mut registry = ToolRegistry::with_builtins(lsp_handle);
        registry.register_hook(Arc::new(otto_tools::RtkHook::new(rtk_enabled(&config))));
        let tools = Arc::new(registry);

        let route_factory: Arc<dyn RouteFactory> = Arc::new(AuthRouteFactory::new(
            auth.all().unwrap_or_default(),
            transport.clone(),
            provider_overrides(&config),
        ));

        let project_id = project_id_for(&directory);
        Ok(Runtime {
            store,
            tools,
            permission,
            agents,
            config,
            auth,
            transport,
            directory,
            route_factory,
            project_id,
            version: VERSION.to_string(),
            lsp,
        })
    }

    /// Replace the route factory (dependency injection for tests / custom
    /// providers). The subagent spawner reuses whatever factory is installed.
    #[must_use]
    pub fn with_route_factory(mut self, factory: Arc<dyn RouteFactory>) -> Self {
        self.route_factory = factory;
        self
    }

    /// Replace the tool registry (e.g. to inject a scripted tool in tests).
    #[must_use]
    pub fn with_tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = tools;
        self
    }

    /// Override the working directory (e.g. to root a headless test runtime at
    /// a specific repository). Does not rebuild directory-derived services
    /// (LSP, project id); intended for tests and callers that only read
    /// [`Runtime::directory`] afterwards.
    #[must_use]
    pub fn with_directory(mut self, dir: std::path::PathBuf) -> Self {
        self.directory = dir;
        self
    }

    /// The default model: `config.model` if set, else `anthropic/claude-sonnet-4-5`.
    #[must_use]
    pub fn default_model(&self) -> ModelRef {
        crate::default_model(self.config.model.as_deref())
    }

    /// The default agent: `config.default_agent` if set and present, else
    /// `build`, else the first resolved agent.
    #[must_use]
    pub fn default_agent(&self) -> &AgentInfo {
        let name = self.config.default_agent.as_deref().unwrap_or("build");
        self.agents
            .iter()
            .find(|a| a.name == name)
            .or_else(|| self.agents.iter().find(|a| a.name == "build"))
            .unwrap_or(&self.agents[0])
    }

    /// The permission service.
    #[must_use]
    pub fn permission(&self) -> &Arc<Permission> {
        &self.permission
    }

    /// The resolved agent set.
    #[must_use]
    pub fn agents(&self) -> &[AgentInfo] {
        &self.agents
    }

    /// The persistence store.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The tool registry.
    #[must_use]
    pub fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// The LSP service (language-server diagnostics), for the server's status
    /// route and diagnostics surfacing.
    #[must_use]
    pub fn lsp(&self) -> &Arc<otto_lsp::Lsp> {
        &self.lsp
    }

    /// The loaded config.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The credential store.
    #[must_use]
    pub fn auth(&self) -> &AuthStore {
        &self.auth
    }

    /// The shared HTTP transport.
    #[must_use]
    pub fn transport(&self) -> &Arc<HttpTransport> {
        &self.transport
    }

    /// The working directory relative tool paths resolve against.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Create a new session for `agent`, optionally as a child of `parent`, and
    /// return its id.
    ///
    /// # Errors
    /// Returns [`crate::Error`] on storage failure.
    pub async fn create_session(
        &self,
        title: impl Into<String>,
        agent: &AgentInfo,
        parent: Option<String>,
    ) -> Result<String> {
        let id = otto_id::ascending(otto_id::Prefix::Session);
        let now = now_ms();
        // Link the child into the permission service's parent chain so it
        // inherits the parent's permission mode live (e.g. a workflow session
        // under a full-auto TUI chat session).
        if let Some(parent_id) = &parent {
            self.permission.link_parent(&id, parent_id);
        }
        // Enforce the agent's own ruleset at the gate (metadata alone is not
        // evaluated): e.g. the plan agent's edit-deny outside `.otto/plans/`
        // holds even in full-auto.
        self.permission
            .set_session_ruleset(&id, agent.permission.clone());
        self.store
            .create_session(&Session {
                id: id.clone(),
                project_id: self.project_id.clone(),
                parent_id: parent,
                directory: self.directory.display().to_string(),
                title: title.into(),
                version: self.version.clone(),
                cost: 0.0,
                tokens: SessionTokens::default(),
                metadata: Some(json!({ "agent": agent.name, "permission": agent.permission })),
                time_created: now,
                time_updated: now,
            })
            .await?;
        Ok(id)
    }

    /// Persist `prompt` as a user message on `session_id`, then spawn the agent
    /// loop for `agent` on `model_ref`. Returns a [`RunHandle`] whose `events`
    /// stream the turn live and whose `join` yields the final assistant message.
    ///
    /// The run is built with a [`SessionGate`] over this runtime's
    /// [`Permission`], the current toolset, a [`SessionSubagentSpawner`] reusing
    /// the same route factory, the configured compaction knobs, and the live
    /// event tap wired to `events`.
    ///
    /// Delegates to [`Runtime::run_with_parts`] with no extra parts.
    #[must_use]
    pub fn run(
        &self,
        session_id: impl Into<String>,
        prompt: impl Into<String>,
        agent: &AgentInfo,
        model_ref: &ModelRef,
        abort: CancellationToken,
    ) -> RunHandle {
        self.run_with_parts(session_id, prompt, Vec::new(), agent, model_ref, abort)
    }

    /// Build a subagent spawner from this runtime's services (the same one
    /// [`Runtime::run_with_parts`] builds inline). `agent` supplies the
    /// parent permission ruleset a spawned child narrows from; `model_ref` is
    /// resolved via the route factory to seed the fallback route/model a
    /// child with no pinned model falls back to. Exposed so drivers above
    /// `run_loop` (e.g. the workflow engine) can dispatch subagents without
    /// going through a full `run`.
    ///
    /// # Errors
    /// Returns [`RunError::Route`] if `model_ref` cannot be resolved.
    pub fn subagent_spawner(
        &self,
        agent: &AgentInfo,
        model_ref: &ModelRef,
    ) -> std::result::Result<Arc<dyn SubagentSpawner>, RunError> {
        let (route, model) = self
            .route_factory
            .route_for(model_ref)
            .map_err(|e| RunError::Route(e.to_string()))?;
        let fallback = (route, model);
        let sub_factory = self.route_factory.clone();
        let route_for: RouteFor = Arc::new(move |a: &AgentInfo| match &a.model {
            Some(m) => sub_factory
                .route_for(m)
                .unwrap_or_else(|_| fallback.clone()),
            None => fallback.clone(),
        });
        Ok(Arc::new(SessionSubagentSpawner::new(
            self.store.clone(),
            self.tools.clone(),
            self.permission.clone(),
            agent.permission.clone(),
            agent_config(&self.config),
            route_for,
            self.directory.clone(),
            self.project_id.clone(),
            self.version.clone(),
            tersemode_directive(&self.config),
        )))
    }

    /// Like [`Runtime::run`], but persists `extra_parts` on the same user
    /// message immediately after the prompt `Text` part — e.g. inlined text
    /// attachments or `File` parts resolved from `files[]` in the prompt
    /// request. Used by the server's attachment-aware prompt route; `run`
    /// itself is `run_with_parts` with an empty `extra_parts`.
    #[must_use]
    pub fn run_with_parts(
        &self,
        session_id: impl Into<String>,
        prompt: impl Into<String>,
        extra_parts: Vec<PartKind>,
        agent: &AgentInfo,
        model_ref: &ModelRef,
        abort: CancellationToken,
    ) -> RunHandle {
        let (event_tx, events) = mpsc::unbounded_channel::<LLMEvent>();

        // Everything the spawned task needs, owned.
        let store = self.store.clone();
        let tools = self.tools.clone();
        let permission = self.permission.clone();
        let factory = self.route_factory.clone();
        let directory = self.directory.clone();
        let agent = agent.clone();
        let model_ref = model_ref.clone();
        let session_id = session_id.into();
        let prompt = prompt.into();
        // Built eagerly (outside the spawned task) so any route-resolution
        // failure surfaces the same way it did inline: as a `RunError::Route`
        // from the `?` below, once the join awaits it.
        let spawner_result = self.subagent_spawner(&agent, &model_ref);

        // Compaction knobs (`config.compaction`), falling back to the session
        // defaults.
        let compaction = self.config.compaction.clone();
        let auto_compact = compaction.as_ref().and_then(|c| c.auto).unwrap_or(true);
        let preserve_recent_tokens = compaction
            .as_ref()
            .and_then(|c| c.preserve_recent_tokens)
            .unwrap_or(DEFAULT_PRESERVE_RECENT_TOKENS);
        let compaction_reserved = compaction
            .as_ref()
            .and_then(|c| c.reserved)
            .unwrap_or(DEFAULT_COMPACTION_RESERVED);
        let prune_protect_tokens = compaction
            .as_ref()
            .and_then(|c| c.prune_protect_tokens)
            .unwrap_or(otto_session::compaction::PRUNE_PROTECT);

        // Retry knobs (`config.retry`), falling back to the session defaults.
        let retry_cfg = self.config.retry.clone();
        let max_retries_cfg = retry_cfg
            .as_ref()
            .and_then(|r| r.max_attempts)
            .unwrap_or(DEFAULT_MAX_RETRIES);
        let max_total_retries_cfg = retry_cfg
            .as_ref()
            .and_then(|r| r.max_total_attempts)
            .unwrap_or(DEFAULT_MAX_TOTAL_RETRIES);

        // Optional wall-clock cap on the whole turn (`retry.turn_timeout_seconds`,
        // default off): a watchdog cancels the run's abort token at the
        // deadline, reusing the graceful-interrupt path (partial work is
        // persisted and the assistant is finalized as aborted).
        if let Some(secs) = retry_cfg
            .as_ref()
            .and_then(|r| r.turn_timeout_seconds)
            .filter(|s| *s > 0)
        {
            let abort = abort.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = tokio::time::sleep(std::time::Duration::from_secs(secs)) => abort.cancel(),
                    // The turn ended (or was interrupted) first — stand down.
                    () = abort.cancelled() => {}
                }
            });
        }

        // Tersemode brevity directive, resolved before the spawn (borrows
        // `self.config`) and moved into the run.
        let tersemode = tersemode_directive(&self.config);

        let join = tokio::spawn(async move {
            // 1. Resolve the run's route/model (also the subagent fallback).
            let (route, model) = factory
                .route_for(&model_ref)
                .map_err(|e| RunError::Route(e.to_string()))?;

            // Auto-name the session from its first prompt: only when it still
            // wears a default title (never clobber a user-set / already-named
            // one) AND has no prior messages (so we summarize the FIRST
            // question). Seeds a background title call (best-effort — never
            // blocks or fails the turn).
            let title_seed = {
                let default_title = store
                    .get_session(&session_id)
                    .await?
                    .is_some_and(|s| crate::title::is_default_session_title(&s.title));
                if default_title && store.list_messages(&session_id).await?.is_empty() {
                    Some(prompt.clone())
                } else {
                    None
                }
            };

            // 2. Persist the user prompt (message + text part).
            let user_id = new_message_id();
            let user = Info {
                id: user_id.clone(),
                session_id: session_id.clone(),
                body: InfoBody::User(User {
                    time: UserTime { created: now_ms() },
                    format: None,
                    summary: None,
                    agent: agent.name.clone(),
                    model: UserModel {
                        provider_id: model.provider.0.clone(),
                        model_id: model.id.0.clone(),
                        variant: agent.variant.clone(),
                    },
                    system: None,
                    tools: None,
                }),
            };
            store.insert_message(&user).await?;
            store
                .insert_part(&Part {
                    id: new_part_id(),
                    session_id: session_id.clone(),
                    message_id: user_id.clone(),
                    kind: PartKind::Text {
                        text: prompt,
                        synthetic: None,
                        ignored: None,
                        time: None,
                        metadata: None,
                    },
                })
                .await?;
            for part in extra_parts {
                store
                    .insert_part(&Part {
                        id: new_part_id(),
                        session_id: session_id.clone(),
                        message_id: user_id.clone(),
                        kind: part,
                    })
                    .await?;
            }

            // Auto-name the session from its first prompt, in the background so
            // it never delays or fails the turn. Clones the route/model before
            // they move into the `RunConfig` below.
            if let Some(seed) = title_seed {
                let route = route.clone();
                let model = model.clone();
                let store = store.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    if let Some(title) =
                        crate::title::generate_session_title(route, model, &seed).await
                    {
                        let _ = store.update_session_title(&sid, &title).await;
                    }
                });
            }

            // 3. Subagent spawner reusing the same route factory (built via
            //    `subagent_spawner`, resolved eagerly above so its own
            //    `route_for` failure surfaces through this same `?`).
            let spawner: Arc<dyn SubagentSpawner> = spawner_result?;

            // 4. Assemble the RunConfig with the live event tap installed.
            let gate: Arc<dyn PermissionGate> =
                Arc::new(SessionGate::new(permission.clone(), session_id.clone()));
            let cfg = RunConfig {
                store: store.clone(),
                route,
                tools: tools.clone(),
                permission: gate,
                model,
                agent: agent.name.clone(),
                agent_prompt: agent.prompt.clone(),
                directory,
                max_steps: agent.steps,
                abort,
                subagent: Some(spawner),
                preserve_recent_tokens,
                compaction_reserved,
                auto_compact,
                prune_protect_tokens,
                max_retries: max_retries_cfg,
                max_total_retries: max_total_retries_cfg,
                event_tx: Some(event_tx),
                system_cache: None,
                tersemode_directive: tersemode,
            };

            // 5. Drive the loop and return the final assistant message.
            let info = run_loop(&cfg, &session_id).await?;
            Ok(info)
        });

        RunHandle { events, join }
    }
}

/// The `config.agent` object, or an empty object when unset.
fn agent_config(config: &Config) -> Value {
    config
        .agent
        .clone()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
}

/// Whether RTK shell-command wrapping is enabled in config (default off).
fn rtk_enabled(config: &Config) -> bool {
    config.rtk.as_ref().map(|r| r.enabled).unwrap_or(false)
}

/// The tersemode brevity directive from `config.tersemode`, or `None` when the block
/// is absent or disabled. Threaded into every run's `RunConfig` (main session +
/// subagent warm cache) so it is appended last in the system prompt.
fn tersemode_directive(config: &Config) -> Option<String> {
    let c = config.tersemode.as_ref()?;
    if !c.enabled {
        return None;
    }
    Some(tersemode_text(c.level))
}

/// The system-prompt directive text for a tersemode intensity level. Every level
/// carries the same byte-exact-preservation clause; they differ only in how far
/// prose is compressed.
fn tersemode_text(level: TersemodeLevel) -> String {
    const PRESERVE: &str = "Never alter, abbreviate, reword, or reformat anything \
inside code blocks, inline code, file paths, shell commands, identifiers, URLs, or \
error/log strings — reproduce them byte-for-byte. Do not compress commit messages, \
PRs, or code you write. When a security warning, an irreversible-action \
confirmation, or a multi-step instruction risks being misread if compressed, answer \
in full prose instead.";
    let head = match level {
        TersemodeLevel::Lite => {
            "Answer tersely. Drop filler, hedging, and pleasantries (no \"sure\", \
\"of course\", \"I'd be happy to\"). Keep normal grammar and full sentences."
        }
        TersemodeLevel::Full => {
            "Answer in a tight, telegraphic style: drop articles (a/an/the), filler \
(just/really/basically), hedging, and pleasantries. Sentence fragments are fine. \
Prefer short synonyms (big not extensive, fix not implement-a-solution-for). \
Pattern: [thing] [action] [reason]. [next step]."
        }
        TersemodeLevel::Ultra => {
            "Answer with maximum compression: telegraphic fragments, one word \
where one word does, heavy abbreviation of common prose words \
(config/req/res/fn/impl/DB/auth), arrows for causality (X -> Y), strip \
conjunctions. Never abbreviate technical names, APIs, or error strings."
        }
        TersemodeLevel::Wenyan => {
            "Answer in a classical-Chinese brevity register (文言文): maximum \
terseness, classical sentence patterns, particles (之/乃/為/其), subjects often \
omitted. Keep all code, paths, commands, and identifiers in their original form."
        }
    };
    format!("<output_style>\n{head}\n\n{PRESERVE}\n</output_style>")
}

/// The permission ruleset from `config.permission`, or an empty ruleset.
fn permission_ruleset(config: &Config) -> Ruleset {
    config
        .permission
        .as_ref()
        .map(Ruleset::from_config)
        .unwrap_or_default()
}

/// The configured starting permission mode (default approve-each; unknown → default).
fn permission_mode(config: &Config) -> PermissionMode {
    config
        .permission_mode
        .as_deref()
        .and_then(PermissionMode::from_str_opt)
        .unwrap_or_default()
}

/// A stable project id derived from the working directory.
fn project_id_for(dir: &Path) -> String {
    format!("prj_{}", dir.display())
}

/// Current wall-clock time in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod provider_override_tests {
    use super::provider_overrides;
    use otto_config::Config;

    fn cfg(json: &str) -> Config {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn per_model_limits_map_into_overrides() {
        let ov = provider_overrides(&cfg(r#"{ "provider": { "ollama": {
                "options": { "baseURL": "http://localhost:11434/v1" },
                "models": { "gemma4:26b-mlx": { "limits": { "context": 32768, "output": 8192 } } }
            } } }"#));
        let l = &ov["ollama"].model_limits["gemma4:26b-mlx"];
        assert_eq!(l.context, Some(32_768));
        assert_eq!(l.output, Some(8192));
    }

    #[test]
    fn limits_only_entry_survives_without_base_url() {
        // Declaring limits for a KNOWN provider's model needs no baseURL; the
        // entry must not be dropped by the base-URL gate.
        let ov = provider_overrides(&cfg(r#"{ "provider": { "anthropic": {
                "models": { "my-fine-tune": { "limits": { "context": 100000 } } }
            } } }"#));
        assert_eq!(
            ov["anthropic"].model_limits["my-fine-tune"].context,
            Some(100_000)
        );
        assert!(ov["anthropic"].base_url.is_empty());
    }

    #[test]
    fn empty_entry_is_dropped() {
        let ov = provider_overrides(&cfg(r#"{ "provider": { "noop": {} } }"#));
        assert!(!ov.contains_key("noop"));
    }
}

#[cfg(test)]
mod tersemode_tests {
    use super::{TersemodeLevel, tersemode_directive, tersemode_text};
    use otto_config::Config;

    fn cfg(json: &str) -> Config {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn absent_or_disabled_yields_none() {
        assert_eq!(tersemode_directive(&cfg(r#"{}"#)), None);
        assert_eq!(
            tersemode_directive(&cfg(r#"{ "tersemode": { "enabled": false } }"#)),
            None
        );
    }

    #[test]
    fn enabled_yields_directive_with_preserve_clause() {
        let d = tersemode_directive(&cfg(r#"{ "tersemode": { "enabled": true } }"#)).unwrap();
        // The resolved directive carries the byte-exact-preservation clause and
        // the Full-level (default) prose rule ("drop articles ...").
        assert!(d.contains("byte-for-byte"));
        assert!(d.contains("articles"));
    }

    #[test]
    fn level_selects_distinct_text() {
        // Each level produces a different directive; all keep the preserve clause.
        let lite = tersemode_text(TersemodeLevel::Lite);
        let ultra = tersemode_text(TersemodeLevel::Ultra);
        assert_ne!(lite, ultra);
        assert!(lite.contains("byte-for-byte") && ultra.contains("byte-for-byte"));
        assert!(ultra.contains("telegraphic"));
    }
}

#[cfg(test)]
mod permission_mode_tests {
    use super::permission_mode;
    use otto_config::Config;
    use otto_permission::PermissionMode;

    fn cfg(json: &str) -> Config {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn absent_defaults_to_approve_each() {
        assert_eq!(permission_mode(&cfg("{}")), PermissionMode::ApproveEach);
    }

    #[test]
    fn parses_configured_mode() {
        assert_eq!(
            permission_mode(&cfg(r#"{ "permission_mode": "full-auto" }"#)),
            PermissionMode::FullAuto
        );
    }

    #[test]
    fn unknown_mode_falls_back_to_default() {
        assert_eq!(
            permission_mode(&cfg(r#"{ "permission_mode": "bogus" }"#)),
            PermissionMode::ApproveEach
        );
    }
}
