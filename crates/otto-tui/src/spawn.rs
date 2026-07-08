//! Auto-spawn a `otto-server` in-process so the TUI can attach over HTTP.

use std::net::SocketAddr;
use std::sync::Arc;

use otto_app::Runtime;
use otto_server::{ServeOptions, router};

/// A server bound on an ephemeral local port, serving until dropped.
#[derive(Debug)]
pub struct LocalServer {
    pub base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Bind `127.0.0.1:0`, serve the otto router over `runtime` on a background
/// task, and return the resulting base URL. No auth, no CORS.
///
/// # Errors
/// Returns the bind error if the ephemeral port cannot be acquired.
pub fn serve_runtime(runtime: Arc<Runtime>) -> std::io::Result<LocalServer> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let addr: SocketAddr = listener.local_addr()?;
    let app = router(runtime, ServeOptions::default());
    let handle = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).expect("from_std");
        let _ = axum::serve(listener, app).await;
    });
    Ok(LocalServer {
        base_url: format!("http://{addr}"),
        handle,
    })
}

/// Convenience: `Runtime::load(cwd)` then serve. Async because loading is.
///
/// # Errors
/// Propagates runtime-load and bind failures.
pub async fn spawn_local_server(cwd: &std::path::Path) -> anyhow::Result<LocalServer> {
    let runtime = Arc::new(Runtime::load(cwd).await?);
    Ok(serve_runtime(runtime)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_config::Config;

    #[tokio::test]
    async fn serve_runtime_answers_app_route() {
        let runtime = Arc::new(Runtime::in_memory(Config::default()).await.unwrap());
        let server = serve_runtime(runtime).unwrap();
        // Give it a beat to bind.
        for _ in 0..50 {
            if reqwest::get(format!("{}/app", server.base_url))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let resp = reqwest::get(format!("{}/app", server.base_url))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
}
