//! LSP client: handshake, didOpen/didChange, publishDiagnostics intake,
//! wait-for-fresh-push. Ported from opencode `packages/opencode/src/lsp/client.ts`.
//!
//! MVP simplifications (acceptable parity gaps for the 4 target servers):
//! - Always full-text sync (`textDocument/didChange` with `[{text}]`); no incremental sync.
//! - Push diagnostics only (no pull diagnostics support).
//! - No TypeScript first-push seeding.
//! - server→client `workspace/configuration` is simplified to `null` (config pull-through gap).

use crate::protocol::{Diagnostic, IncomingNotification, PublishDiagnosticsParams};
use crate::transport::{LspError, NotifyFn, RequestFn, Transport};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncWrite};

/// Per-file diagnostics + freshness bookkeeping.
#[derive(Default)]
struct DiagState {
    by_path: HashMap<PathBuf, Vec<Diagnostic>>,
    /// last time a push arrived for this path
    published_at: HashMap<PathBuf, Instant>,
    /// open document versions
    versions: HashMap<PathBuf, i64>,
}

pub struct Client {
    server_id: String,
    root: PathBuf,
    transport: Transport,
    state: Arc<Mutex<DiagState>>,
    notify: Arc<tokio::sync::Notify>,
}

/// Upper bound on the `initialize` handshake. A server that spawns and keeps
/// stdout open but never answers `initialize` would otherwise wedge the caller
/// (edit/write/apply_patch → `touch_file` → `report_for`) forever. Matches
/// opencode's 45s `withTimeout` guard (client.ts:211).
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);

fn file_url(p: &Path) -> String {
    // Minimal file:// URL. Assumes absolute path (servers require it).
    format!("file://{}", p.to_string_lossy())
}

fn path_from_uri(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

impl Client {
    pub async fn connect<R, W>(
        server_id: String,
        root: PathBuf,
        reader: R,
        writer: W,
        initialization: Option<Value>,
    ) -> Result<Arc<Client>, LspError>
    where
        R: AsyncBufRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::connect_with_timeout(
            server_id,
            root,
            reader,
            writer,
            initialization,
            INITIALIZE_TIMEOUT,
        )
        .await
    }

    /// Internal seam for [`Client::connect`] that makes the `initialize` timeout
    /// injectable so tests can drive the unresponsive-server path fast. Public
    /// callers go through `connect` (fixed 45s bound).
    async fn connect_with_timeout<R, W>(
        server_id: String,
        root: PathBuf,
        reader: R,
        writer: W,
        initialization: Option<Value>,
        init_timeout: Duration,
    ) -> Result<Arc<Client>, LspError>
    where
        R: AsyncBufRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let state = Arc::new(Mutex::new(DiagState::default()));
        let notify = Arc::new(tokio::sync::Notify::new());

        let n_state = state.clone();
        let n_notify = notify.clone();
        let on_notification: NotifyFn = Arc::new(move |n: IncomingNotification| {
            if n.method == "textDocument/publishDiagnostics"
                && let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(n.params)
            {
                let path = path_from_uri(&p.uri);
                let mut s = n_state.lock().unwrap();
                s.published_at.insert(path.clone(), Instant::now());
                s.by_path.insert(path, p.diagnostics);
                drop(s);
                n_notify.notify_waiters();
            }
        });

        let root_url = file_url(&root);
        let on_server_request: RequestFn = Arc::new(move |method: &str, _params: &Value| {
            match method {
                "workspace/workspaceFolders" => {
                    json!([{"name":"workspace","uri":root_url}])
                }
                // configuration / progress / capability registration → null (simplified)
                _ => Value::Null,
            }
        });

        let transport = Transport::new(reader, writer, on_notification, on_server_request);

        // initialize — capabilities ported from client.ts:211-255
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": file_url(&root),
            "workspaceFolders": [{"name":"workspace","uri":file_url(&root)}],
            "initializationOptions": initialization.clone().unwrap_or(Value::Null),
            "capabilities": {
                "window": { "workDoneProgress": true },
                "workspace": {
                    "configuration": true,
                    "didChangeWatchedFiles": { "dynamicRegistration": true }
                },
                "textDocument": {
                    "synchronization": { "didOpen": true, "didChange": true },
                    "publishDiagnostics": { "versionSupport": false }
                }
            }
        });
        // Bound ONLY the `initialize` request: it is the sole blocking
        // request-response in the handshake. On elapse we return early; the
        // caller drops the child (`kill_on_drop`) so no orphan lingers. The
        // `initialized`/`didChangeConfiguration` calls are fire-and-forget
        // notifications, so they need no bound.
        match tokio::time::timeout(init_timeout, transport.request("initialize", init_params)).await
        {
            Ok(res) => res?,
            Err(_) => return Err(LspError::Timeout),
        };
        transport.notify("initialized", json!({})).await?;
        if let Some(init) = initialization {
            transport
                .notify(
                    "workspace/didChangeConfiguration",
                    json!({ "settings": init }),
                )
                .await?;
        }

        Ok(Arc::new(Client {
            server_id,
            root,
            transport,
            state,
            notify,
        }))
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }

    /// didOpen (first time) or didChange (subsequent). Full-text sync (MVP).
    ///
    /// # Concurrency
    /// NOT safe against concurrent calls for the SAME `path`: the read of the current
    /// version and the write-back of `version + 1` straddle the `.await` on the notify,
    /// so two overlapping opens on one file can race and desync the version counter.
    /// Callers MUST serialize `open` per file. (The `Lsp` service, a later task, drives
    /// `open` serially per edit, so this invariant holds in practice.) Per-path locking
    /// is intentionally out of scope for the MVP.
    pub async fn open(&self, path: &Path, language_id: &str, text: &str) -> Result<i64, LspError> {
        let existing = self.state.lock().unwrap().versions.get(path).copied();
        let uri = file_url(path);
        match existing {
            None => {
                self.transport
                    .notify(
                        "textDocument/didOpen",
                        json!({"textDocument":{
                            "uri": uri, "languageId": language_id,
                            "version": 0, "text": text }}),
                    )
                    .await?;
                let mut s = self.state.lock().unwrap();
                s.versions.insert(path.to_path_buf(), 0);
                Ok(0)
            }
            Some(prev) => {
                let version = prev + 1;
                self.transport
                    .notify(
                        "textDocument/didChange",
                        json!({
                            "textDocument": {"uri": uri, "version": version},
                            "contentChanges": [{"text": text}]
                        }),
                    )
                    .await?;
                self.state
                    .lock()
                    .unwrap()
                    .versions
                    .insert(path.to_path_buf(), version);
                Ok(version)
            }
        }
    }

    /// Wait for a push newer than `after`, with a 150ms debounce, bounded by `timeout`.
    /// Mirrors client.ts waitForFreshPush (document mode, push-only).
    pub async fn wait_for_diagnostics(
        &self,
        path: &Path,
        _version: i64,
        after: Instant,
        timeout: Duration,
    ) {
        const DEBOUNCE: Duration = Duration::from_millis(150);
        let deadline = Instant::now() + timeout;
        loop {
            let fresh = {
                let s = self.state.lock().unwrap();
                s.published_at.get(path).is_some_and(|t| *t > after)
            };
            if fresh {
                // debounce: let a burst settle
                tokio::time::sleep(DEBOUNCE).await;
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            let _ = tokio::time::timeout(remaining.min(DEBOUNCE), self.notify.notified()).await;
        }
    }

    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.state
            .lock()
            .unwrap()
            .by_path
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    pub fn all_diagnostics(&self) -> HashMap<PathBuf, Vec<Diagnostic>> {
        self.state.lock().unwrap().by_path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, BufReader, duplex};

    /// A server that spawns, keeps stdout open, reads the `initialize` request
    /// but NEVER replies must not wedge `connect` forever: the handshake bound
    /// fires and yields `LspError::Timeout` within the (small, injected) window.
    #[tokio::test]
    async fn connect_times_out_on_unresponsive_server() {
        let (client_side, server_side) = duplex(8192);
        let (crx, cwx) = tokio::io::split(client_side);
        let (mut srx, _swx) = tokio::io::split(server_side);

        // Fake server: drain whatever the client writes so its `write_all`
        // never blocks, but never send a reply. Holding `_swx` keeps the
        // client's reader half from hitting EOF, so the only thing that can
        // unblock `connect` is the timeout.
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = srx.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        });

        let bound = Duration::from_millis(200);
        let start = Instant::now();
        let res = Client::connect_with_timeout(
            "test-server".to_string(),
            PathBuf::from("/tmp/root"),
            BufReader::new(crx),
            cwx,
            None,
            bound,
        )
        .await;

        // Map away the non-Debug `Arc<Client>` so we can assert on the error.
        let outcome = res.map(|_| ());
        assert!(
            matches!(outcome, Err(LspError::Timeout)),
            "expected LspError::Timeout, got {outcome:?}"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "connect must return promptly after the bound, took {elapsed:?}"
        );
    }
}
