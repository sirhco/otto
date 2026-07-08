use otto_lsp::client::Client;
use otto_lsp::framing::{FrameReader, encode};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader, duplex, split};

// A scripted fake language server driven over an in-memory duplex.
// Responds to `initialize`, swallows `initialized`/didOpen, then pushes one error diagnostic.
#[tokio::test]
async fn handshake_then_receives_pushed_diagnostic() {
    let (client_side, server_side) = duplex(1 << 16);
    let (crx, cwx) = split(client_side);
    let (srx, swx) = split(server_side);

    tokio::spawn(async move {
        let mut fr = FrameReader::new(BufReader::new(srx));
        let mut swx = swx;
        while let Some(body) = fr.next_frame().await.unwrap() {
            let msg: Value = serde_json::from_slice(&body).unwrap();
            let method = msg["method"].as_str().unwrap_or("");
            if method == "initialize" {
                let resp = json!({"jsonrpc":"2.0","id":msg["id"],
                    "result":{"capabilities":{}}});
                swx.write_all(&encode(&serde_json::to_vec(&resp).unwrap()))
                    .await
                    .unwrap();
                swx.flush().await.unwrap();
            } else if method == "textDocument/didOpen" {
                // push a diagnostic for the opened file
                let uri = msg["params"]["textDocument"]["uri"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let note = json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics",
                    "params":{"uri":uri,"version":0,"diagnostics":[
                        {"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":5}},
                         "severity":1,"message":"boom"}]}});
                swx.write_all(&encode(&serde_json::to_vec(&note).unwrap()))
                    .await
                    .unwrap();
                swx.flush().await.unwrap();
            }
        }
    });

    let client = Client::connect(
        "rust".into(),
        PathBuf::from("/tmp/project"),
        BufReader::new(crx),
        cwx,
        None,
    )
    .await
    .unwrap();

    let path = PathBuf::from("/tmp/project/src/main.rs");
    let after = Instant::now();
    let version = client.open(&path, "rust", "fn main() {}\n").await.unwrap();
    assert_eq!(version, 0);
    client
        .wait_for_diagnostics(&path, version, after, Duration::from_secs(2))
        .await;
    let diags = client.diagnostics(&path);
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].message, "boom");
    assert_eq!(diags[0].severity, Some(1));
}

// Regression (hang-class guard): if the server answers `initialize` but NEVER pushes
// diagnostics, `wait_for_diagnostics` must return by the timeout deadline — not hang,
// and not return instantly. Locks in that a silent/crashed server can't stall an edit.
#[tokio::test]
async fn wait_for_diagnostics_returns_on_timeout_when_no_push() {
    let (client_side, server_side) = duplex(1 << 16);
    let (crx, cwx) = split(client_side);
    let (srx, swx) = split(server_side);

    tokio::spawn(async move {
        let mut fr = FrameReader::new(BufReader::new(srx));
        let mut swx = swx;
        while let Some(body) = fr.next_frame().await.unwrap() {
            let msg: Value = serde_json::from_slice(&body).unwrap();
            let method = msg["method"].as_str().unwrap_or("");
            if method == "initialize" {
                let resp = json!({"jsonrpc":"2.0","id":msg["id"],
                    "result":{"capabilities":{}}});
                swx.write_all(&encode(&serde_json::to_vec(&resp).unwrap()))
                    .await
                    .unwrap();
                swx.flush().await.unwrap();
            }
            // NOTE: deliberately never emits textDocument/publishDiagnostics.
        }
    });

    let client = Client::connect(
        "rust".into(),
        PathBuf::from("/tmp/project"),
        BufReader::new(crx),
        cwx,
        None,
    )
    .await
    .unwrap();

    let path = PathBuf::from("/tmp/project/src/main.rs");
    let version = client.open(&path, "rust", "fn main() {}\n").await.unwrap();
    assert_eq!(version, 0);

    let timeout = Duration::from_millis(200);
    let start = Instant::now();
    client
        .wait_for_diagnostics(&path, version, Instant::now(), timeout)
        .await;
    let elapsed = start.elapsed();

    assert!(
        elapsed >= timeout,
        "returned too early ({elapsed:?}) — did not wait for the deadline"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "took too long ({elapsed:?}) — likely hung instead of timing out"
    );

    // No push ever arrived, so there are no diagnostics.
    assert!(client.diagnostics(&path).is_empty());
}
