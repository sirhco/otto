//! `Lsp` service: lazily spawns language-server clients per `(root, server_id)`,
//! opens/updates files, and surfaces merged/deduped diagnostics to callers.
//! Ported from opencode `packages/opencode/src/lsp/index.ts` (`lsp.ts:254-375`).

use crate::client::Client;
use crate::protocol::Diagnostic;
use crate::registry;
use crate::report;
use crate::transport::LspError;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Per-server command/extension/env/init overrides, keyed by server id in
/// `LspConfigResolved::overrides`. An override may point at a built-in server id
/// (replacing its command/extensions/env/initialization) or introduce a brand-new
/// server id entirely (in which case `command` is required).
#[derive(Clone, Debug)]
pub struct ServerOverride {
    pub command: Vec<String>,
    pub extensions: Option<Vec<String>>,
    pub env: HashMap<String, String>,
    pub initialization: Option<Value>,
}

/// The merged LSP configuration the service was built with.
#[derive(Clone, Debug)]
pub struct LspConfigResolved {
    pub enabled: bool,
    pub overrides: HashMap<String, ServerOverride>,
    pub disabled: HashSet<String>,
}

impl LspConfigResolved {
    pub fn enabled_default() -> Self {
        LspConfigResolved {
            enabled: true,
            overrides: HashMap::new(),
            disabled: HashSet::new(),
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct LspStatus {
    pub id: String,
    pub name: String,
    pub root: String,
    pub status: String,
}

struct ClientEntry {
    client: Arc<Client>,
    _child: Option<tokio::process::Child>,
}

/// A resolved server definition (built-in, patched by an override, or wholly
/// introduced by an override) ready to be matched against a file's extension and
/// spawned.
struct ResolvedServer {
    id: String,
    extensions: Vec<String>,
    command: Vec<String>,
    root_markers: Vec<String>,
    env: HashMap<String, String>,
    initialization: Option<Value>,
}

pub struct Lsp {
    directory: PathBuf,
    config: LspConfigResolved,
    // Invariant: the `clients` guard is NEVER held across an `.await` (both
    // `touch_file` lock sites are block-scoped and dropped before `spawn_client`
    // /`open` are awaited), so a plain `std::sync::Mutex` is correct and lets the
    // synchronous readers (`diagnostics_for`, `other_files_with_errors`, `statuses`)
    // lock without a `try_lock()` fallback that could silently return empty
    // diagnostics under contention.
    clients: Mutex<HashMap<(String, String), ClientEntry>>,
}

impl Lsp {
    pub fn new(directory: PathBuf, config: LspConfigResolved) -> Arc<Lsp> {
        Arc::new(Lsp {
            directory,
            config,
            clients: Mutex::new(HashMap::new()),
        })
    }

    /// Merge built-in servers with `config.overrides`, dropping anything in
    /// `config.disabled`. An override on a known id patches that server's fields
    /// (leaving unset fields at their built-in value); an override on an unknown id
    /// introduces a wholly new server (silently skipped if it has no command).
    fn resolved_servers(&self) -> Vec<ResolvedServer> {
        let mut out: HashMap<String, ResolvedServer> = HashMap::new();
        for s in registry::builtin_servers() {
            if self.config.disabled.contains(s.id) {
                continue;
            }
            out.insert(
                s.id.to_string(),
                ResolvedServer {
                    id: s.id.to_string(),
                    extensions: s.extensions.iter().map(|e| e.to_string()).collect(),
                    command: s.command.iter().map(|c| c.to_string()).collect(),
                    root_markers: s.root_markers.iter().map(|m| m.to_string()).collect(),
                    env: HashMap::new(),
                    initialization: None,
                },
            );
        }
        for (id, ov) in &self.config.overrides {
            if self.config.disabled.contains(id.as_str()) {
                continue;
            }
            match out.get_mut(id) {
                Some(existing) => {
                    if !ov.command.is_empty() {
                        existing.command = ov.command.clone();
                    }
                    if let Some(exts) = &ov.extensions {
                        existing.extensions = exts.clone();
                    }
                    existing.env.extend(ov.env.clone());
                    if ov.initialization.is_some() {
                        existing.initialization = ov.initialization.clone();
                    }
                }
                None => {
                    if ov.command.is_empty() {
                        // Nothing to spawn — skip malformed override for an unknown id.
                        continue;
                    }
                    out.insert(
                        id.clone(),
                        ResolvedServer {
                            id: id.clone(),
                            extensions: ov.extensions.clone().unwrap_or_default(),
                            command: ov.command.clone(),
                            root_markers: Vec::new(),
                            env: ov.env.clone(),
                            initialization: ov.initialization.clone(),
                        },
                    );
                }
            }
        }
        out.into_values().collect()
    }

    /// Lazily spawn/reuse clients for every server that matches `path`'s extension,
    /// open (or update) the file on each, and optionally block for a fresh
    /// diagnostics push. Best-effort: a missing binary, spawn failure, or open
    /// failure is swallowed rather than propagated, so one unavailable server never
    /// blocks the edit flow. Mirrors `lsp.ts:344-362`.
    pub async fn touch_file(&self, path: &Path, wait: bool) {
        if !self.config.enabled {
            return;
        }
        let ext = format!(
            ".{}",
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
        );
        let language_id = registry::language_id(path);
        let text = std::fs::read_to_string(path).unwrap_or_default();

        for server in self.resolved_servers() {
            if !server
                .extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&ext))
            {
                continue;
            }
            let markers: Vec<&str> = server.root_markers.iter().map(|s| s.as_str()).collect();
            let root = registry::nearest_root(path, &self.directory, &markers);
            let key = (root.to_string_lossy().into_owned(), server.id.clone());

            let existing = {
                let mut clients = self.clients.lock().unwrap();
                match clients.get(&key) {
                    Some(entry) if entry.client.is_alive() => Some(entry.client.clone()),
                    Some(_) => {
                        clients.remove(&key);
                        None
                    }
                    None => None,
                }
            };

            let client = match existing {
                Some(c) => c,
                None => {
                    let cmd_refs: Vec<&str> = server.command.iter().map(|s| s.as_str()).collect();
                    let Some(resolved_cmd) = registry::resolve_command(&cmd_refs) else {
                        continue;
                    };
                    let Ok((client, child)) = spawn_client(
                        &server.id,
                        &root,
                        resolved_cmd,
                        &server.env,
                        server.initialization.clone(),
                    )
                    .await
                    else {
                        continue;
                    };
                    let mut clients = self.clients.lock().unwrap();
                    clients.insert(
                        key,
                        ClientEntry {
                            client: client.clone(),
                            _child: Some(child),
                        },
                    );
                    client
                }
            };

            let before = Instant::now();
            let Ok(version) = client.open(path, language_id, &text).await else {
                continue;
            };
            if wait {
                client
                    .wait_for_diagnostics(path, version, before, Duration::from_secs(5))
                    .await;
            }
        }
    }

    /// Snapshot of every client. The guard is never held across an `.await`
    /// anywhere, so this plain `lock()` cannot deadlock.
    fn snapshot_clients(&self) -> Vec<Arc<Client>> {
        self.clients
            .lock()
            .unwrap()
            .values()
            .map(|e| e.client.clone())
            .collect()
    }

    /// Merged, deduped diagnostics for `path` across every client whose root is an
    /// ancestor of (or equal to) the file. Dedupe key ports `client.ts:91
    /// dedupeDiagnostics`: the JSON string of `{code, severity, message, source,
    /// range}`.
    pub fn diagnostics_for(&self, path: &Path) -> Vec<Diagnostic> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for client in self.snapshot_clients() {
            if !path.starts_with(client.root()) {
                continue;
            }
            for d in client.diagnostics(path) {
                let key = dedupe_key(&d);
                if seen.insert(key) {
                    out.push(d);
                }
            }
        }
        out
    }

    pub fn report_for(&self, path: &Path) -> String {
        report::report(&path.display().to_string(), &self.diagnostics_for(path))
    }

    /// Up to `max` OTHER files (≠ `exclude`) carrying at least one error-severity
    /// diagnostic, across all live clients. Mirrors write.ts's "surface nearby
    /// errors" parity (≤5 others).
    pub fn other_files_with_errors(&self, exclude: &Path, max: usize) -> Vec<(PathBuf, String)> {
        let mut seen_paths: HashSet<PathBuf> = HashSet::new();
        let mut out = Vec::new();
        'outer: for client in self.snapshot_clients() {
            for (path, diags) in client.all_diagnostics() {
                if path == exclude || seen_paths.contains(&path) {
                    continue;
                }
                if !diags.iter().any(|d| d.severity == Some(1)) {
                    continue;
                }
                seen_paths.insert(path.clone());
                let rendered = report::report(&path.display().to_string(), &diags);
                if rendered.is_empty() {
                    continue;
                }
                out.push((path, rendered));
                if out.len() >= max {
                    break 'outer;
                }
            }
        }
        out
    }

    /// One status per live client.
    pub fn statuses(&self) -> Vec<LspStatus> {
        self.snapshot_clients()
            .into_iter()
            .filter(|c| c.is_alive())
            .map(|c| {
                let root = c.root();
                let display = root
                    .strip_prefix(&self.directory)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| root.display().to_string());
                LspStatus {
                    id: c.server_id().to_string(),
                    name: c.server_id().to_string(),
                    root: display,
                    status: "connected".to_string(),
                }
            })
            .collect()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_client_for_test(&self, server_id: &str, client: Arc<Client>) {
        let root = client.root().to_path_buf();
        let key = (root.to_string_lossy().into_owned(), server_id.to_string());
        let mut clients = self.clients.lock().unwrap();
        clients.insert(
            key,
            ClientEntry {
                client,
                _child: None,
            },
        );
    }
}

/// Implements the tool-side injection seam. `report_for` opens the file and
/// waits (≤5s) for a fresh diagnostics push before formatting; the inherent
/// [`Lsp::report_for`]/[`Lsp::other_files_with_errors`] are the sync formatters
/// over already-collected diagnostics. Disambiguated via `Lsp::method(self, ..)`
/// so the trait method never recurses into itself.
#[async_trait::async_trait]
impl otto_tools::LspHandle for Lsp {
    async fn report_for(&self, path: &Path) -> String {
        self.touch_file(path, true).await;
        Lsp::report_for(self, path)
    }

    async fn other_files_with_errors(&self, exclude: &Path, max: usize) -> Vec<(PathBuf, String)> {
        Lsp::other_files_with_errors(self, exclude, max)
    }
}

fn dedupe_key(d: &Diagnostic) -> String {
    let obj = json!({
        "code": d.code,
        "severity": d.severity,
        "message": d.message,
        "source": d.source,
        "range": d.range,
    });
    obj.to_string()
}

/// Spawn a language-server child process and complete the LSP handshake over its
/// stdio. The returned `Child` must be held by the caller (in `ClientEntry`) so the
/// process isn't reaped when this function returns.
async fn spawn_client(
    server_id: &str,
    root: &Path,
    resolved_cmd: Vec<String>,
    env: &HashMap<String, String>,
    initialization: Option<Value>,
) -> Result<(Arc<Client>, tokio::process::Child), LspError> {
    let mut cmd = tokio::process::Command::new(&resolved_cmd[0]);
    cmd.args(&resolved_cmd[1..]);
    cmd.current_dir(root);
    cmd.envs(env);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    // Reap the server when its `ClientEntry` (holding the `Child`) is dropped/replaced,
    // so a dead/superseded client never leaves an orphan language-server process.
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| LspError::Transport(format!("spawn {}: {e}", resolved_cmd[0])))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| LspError::Transport("child missing stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LspError::Transport("child missing stdout".into()))?;
    let reader = tokio::io::BufReader::new(stdout);

    let client = Client::connect(
        server_id.to_string(),
        root.to_path_buf(),
        reader,
        stdin,
        initialization,
    )
    .await?;
    Ok((client, child))
}
