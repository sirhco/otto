use crate::framing::{FrameReader, encode};
use crate::protocol::{IncomingNotification, RpcMessage};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, oneshot};

pub type NotifyFn = Arc<dyn Fn(IncomingNotification) + Send + Sync>;
pub type RequestFn = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport: {0}")]
    Transport(String),
    #[error("timeout")]
    Timeout,
    #[error("shutdown")]
    Shutdown,
}

/// Shared state guarded by a single mutex so that aliveness and the set of
/// outstanding responders are read/written in one critical section. This is
/// what closes the shutdown/registration race: the reader flips `alive=false`
/// and drains `map` atomically, so a concurrent `request()` either inserts
/// before the drain (and gets failed by it) or observes `alive=false` under
/// the same lock and never inserts. There is no window where an entry is
/// inserted after the drain and then orphaned.
struct PendingState {
    alive: bool,
    map: HashMap<i64, oneshot::Sender<Result<Value, LspError>>>,
}

type Pending = Arc<Mutex<PendingState>>;

pub struct Transport {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    next_id: AtomicI64,
    pending: Pending,
    // Cheap lock-free view of aliveness for the `notify()` early-out. The
    // authoritative flag for the `request()` insert path lives inside
    // `PendingState.alive` (guarded by the pending mutex).
    alive: Arc<AtomicBool>,
}

impl Transport {
    pub fn new<R, W>(
        reader: R,
        writer: W,
        on_notification: NotifyFn,
        on_server_request: RequestFn,
    ) -> Self
    where
        R: AsyncBufRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>> =
            Arc::new(Mutex::new(Box::new(writer)));
        let pending: Pending = Arc::new(Mutex::new(PendingState {
            alive: true,
            map: HashMap::new(),
        }));
        let alive = Arc::new(AtomicBool::new(true));

        let r_pending = pending.clone();
        let r_alive = alive.clone();
        let r_writer = writer.clone();
        tokio::spawn(async move {
            let mut fr = FrameReader::new(reader);
            loop {
                match fr.next_frame().await {
                    Ok(Some(body)) => {
                        let msg: RpcMessage = match serde_json::from_slice(&body) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };
                        match msg {
                            RpcMessage::Response(resp) => {
                                if let Some(id) = resp.id.as_i64()
                                    && let Some(tx) = r_pending.lock().await.map.remove(&id)
                                {
                                    let out = if resp.error.is_null() {
                                        Ok(resp.result)
                                    } else {
                                        Err(LspError::Transport(resp.error.to_string()))
                                    };
                                    let _ = tx.send(out);
                                }
                            }
                            RpcMessage::Request(req) => {
                                let result = on_server_request(&req.method, &req.params);
                                let reply = json!({"jsonrpc":"2.0","id":req.id,"result":result});
                                if let Ok(bytes) = serde_json::to_vec(&reply) {
                                    let mut w = r_writer.lock().await;
                                    let _ = w.write_all(&encode(&bytes)).await;
                                    let _ = w.flush().await;
                                }
                            }
                            RpcMessage::Notification(n) => on_notification(n),
                        }
                    }
                    Ok(None) | Err(_) => {
                        // Flip aliveness and fail all pending in one critical
                        // section so no `request()` can slip an entry in after
                        // the drain (see `PendingState` docs).
                        let mut p = r_pending.lock().await;
                        p.alive = false;
                        for (_, tx) in p.map.drain() {
                            let _ = tx.send(Err(LspError::Shutdown));
                        }
                        drop(p);
                        r_alive.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });

        Self {
            writer,
            next_id: AtomicI64::new(1),
            pending,
            alive,
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let bytes = serde_json::to_vec(&msg).map_err(|e| LspError::Transport(e.to_string()))?;

        // Register the responder BEFORE writing so a fast reply can't race.
        // The aliveness check and the insert happen under one lock, so either
        // this insert happens-before the reader's drain (which then fails it)
        // or we observe `alive=false` and never insert — no orphaned entry.
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().await;
            if !p.alive {
                return Err(LspError::Shutdown);
            }
            p.map.insert(id, tx);
        }

        // On write failure, remove our own entry before returning so it does
        // not leak until the next reader drain.
        let write_res = async {
            let mut w = self.writer.lock().await;
            w.write_all(&encode(&bytes)).await?;
            w.flush().await
        }
        .await;
        if let Err(e) = write_res {
            self.pending.lock().await.map.remove(&id);
            return Err(LspError::Io(e));
        }

        rx.await.map_err(|_| LspError::Shutdown)?
    }

    #[cfg(test)]
    async fn pending_len(&self) -> usize {
        self.pending.lock().await.map.len()
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        if !self.is_alive() {
            return Err(LspError::Shutdown);
        }
        let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
        let bytes = serde_json::to_vec(&msg).map_err(|e| LspError::Transport(e.to_string()))?;
        let mut w = self.writer.lock().await;
        w.write_all(&encode(&bytes)).await?;
        w.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::io::{BufReader, duplex};

    // Writer whose sink always errors, to exercise the write-failure path.
    struct FailWriter;
    impl AsyncWrite for FailWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fail",
            )))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    // Minimal fake server: reads one request, replies with {ok:true}.
    #[tokio::test]
    async fn request_response_correlates() {
        let (client_side, server_side) = duplex(8192);
        let (crx, cwx) = tokio::io::split(client_side);
        let (srx, swx) = tokio::io::split(server_side);

        // fake server task
        tokio::spawn(async move {
            let mut fr = crate::framing::FrameReader::new(BufReader::new(srx));
            let mut swx = swx;
            if let Some(body) = fr.next_frame().await.unwrap() {
                let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let id = req["id"].clone();
                let resp = json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}});
                let bytes = serde_json::to_vec(&resp).unwrap();
                use tokio::io::AsyncWriteExt;
                swx.write_all(&crate::framing::encode(&bytes))
                    .await
                    .unwrap();
                swx.flush().await.unwrap();
            }
        });

        let noop_notify: NotifyFn = Arc::new(|_n| {});
        let noop_req: RequestFn = Arc::new(|_m, _p| serde_json::Value::Null);
        let t = Transport::new(BufReader::new(crx), cwx, noop_notify, noop_req);
        let out = t.request("initialize", json!({})).await.unwrap();
        assert_eq!(out["ok"], true);
    }

    // Regression: a request whose write fails must not leak its pending entry.
    #[tokio::test]
    async fn write_failure_cleans_pending() {
        // Hold `_server_side` so the reader half never hits EOF and the
        // transport stays alive; only the writer fails.
        let (client_side, _server_side) = duplex(8192);
        let (crx, _cwx) = tokio::io::split(client_side);

        let noop_notify: NotifyFn = Arc::new(|_n| {});
        let noop_req: RequestFn = Arc::new(|_m, _p| Value::Null);
        let t = Transport::new(BufReader::new(crx), FailWriter, noop_notify, noop_req);

        let res = t.request("initialize", json!({})).await;
        assert!(matches!(res, Err(LspError::Io(_))), "expected io error");
        assert!(t.is_alive(), "transport should still be alive");
        assert_eq!(t.pending_len().await, 0, "pending entry must be cleaned up");
    }
}
