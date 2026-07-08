use otto_lsp::client::Client;
use otto_lsp::framing::{FrameReader, encode};
use otto_lsp::service::{Lsp, LspConfigResolved};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::io::{AsyncWriteExt, BufReader, duplex, split};

async fn fake_client_pushing_error(root: PathBuf, file: PathBuf) -> std::sync::Arc<Client> {
    let (client_side, server_side) = duplex(1 << 16);
    let (crx, cwx) = split(client_side);
    let (srx, swx) = split(server_side);
    tokio::spawn(async move {
        let mut fr = FrameReader::new(BufReader::new(srx));
        let mut swx = swx;
        while let Some(body) = fr.next_frame().await.unwrap() {
            let msg: Value = serde_json::from_slice(&body).unwrap();
            match msg["method"].as_str().unwrap_or("") {
                "initialize" => {
                    let r = json!({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}}});
                    swx.write_all(&encode(&serde_json::to_vec(&r).unwrap()))
                        .await
                        .unwrap();
                    swx.flush().await.unwrap();
                }
                "textDocument/didOpen" => {
                    let uri = msg["params"]["textDocument"]["uri"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    let n = json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics",
                        "params":{"uri":uri,"diagnostics":[
                            {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                             "severity":1,"message":"bad"}]}});
                    swx.write_all(&encode(&serde_json::to_vec(&n).unwrap()))
                        .await
                        .unwrap();
                    swx.flush().await.unwrap();
                }
                _ => {}
            }
        }
    });
    let client = Client::connect("rust".into(), root, BufReader::new(crx), cwx, None)
        .await
        .unwrap();
    client.open(&file, "rust", "fn main(){}").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    client
}

#[tokio::test]
async fn report_for_surfaces_error_block() {
    let dir = PathBuf::from("/tmp/proj");
    let file = dir.join("src/main.rs");
    let lsp = Lsp::new(dir.clone(), LspConfigResolved::enabled_default());
    let client = fake_client_pushing_error(dir.clone(), file.clone()).await;
    lsp.insert_client_for_test("rust", client);
    let block = lsp.report_for(&file);
    assert!(block.contains("<diagnostics"));
    assert!(block.contains("ERROR [1:1] bad"));
}
