//! `/experimental/worktree*` route tests against a real temp git repo.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use otto_app::Runtime;
use otto_config::Config;
use otto_server::{ServeOptions, serve};
use serde_json::Value;

/// Bind the server on an ephemeral port; return its base URL.
async fn spawn(runtime: Arc<Runtime>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    drop(listener);
    tokio::spawn(async move {
        let _ = serve(runtime, addr, ServeOptions::default()).await;
    });
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    format!("http://{addr}")
}

/// A temp git repo with one commit; returns the TempDir (keep it alive).
async fn temp_git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "t@t.t"],
        vec!["config", "user.name", "t"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        otto_vcs::git::run_git(p, &args).await.unwrap();
    }
    std::fs::write(p.join("f.txt"), "x").unwrap();
    otto_vcs::git::run_git(p, &["add", "."]).await.unwrap();
    otto_vcs::git::run_git(p, &["commit", "-q", "-m", "init"])
        .await
        .unwrap();
    dir
}

#[tokio::test]
async fn worktree_routes_list_create_remove() {
    if !otto_vcs::git::git_available().await {
        return;
    }
    let repo = temp_git_repo().await;
    let runtime = Arc::new(
        Runtime::in_memory(Config::default())
            .await
            .unwrap()
            .with_directory(repo.path().to_path_buf()),
    );
    let base = spawn(runtime).await;
    let http = reqwest::Client::new();

    // Empty list to start.
    let list: Value = http
        .get(format!("{base}/experimental/worktree"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list, serde_json::json!([]));

    // Create one.
    let created: Value = http
        .post(format!("{base}/experimental/worktree"))
        .json(&serde_json::json!({ "name": "feat" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["name"], "feat");
    assert_eq!(created["branch"], "otto/feat");
    let dir = created["directory"].as_str().unwrap().to_string();

    // List now has it.
    let list: Value = http
        .get(format!("{base}/experimental/worktree"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Remove it → true, list empty again.
    let removed: Value = http
        .request(
            reqwest::Method::DELETE,
            format!("{base}/experimental/worktree"),
        )
        .json(&serde_json::json!({ "directory": dir }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(removed, serde_json::json!(true));
    let list: Value = http
        .get(format!("{base}/experimental/worktree"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list, serde_json::json!([]));
}
