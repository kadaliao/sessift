//! Request multiplexing shared by every connection type.
//!
//! Client side, [`RpcCore`] owns one byte stream: callers get request ids, replies are matched
//! by id, and a request that times out or whose future is dropped sends `cancel`.
//! Server side, [`Inflight`] runs requests concurrently and guarantees exactly one reply per id
//! even when a cancel races with completion.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::codec::{encode_msg, read_frame};
use crate::proto::{CallError, ClientMsg, ErrorCode, Reply, ServerMsg};

#[derive(Debug, Clone)]
pub struct Response {
    pub reply: Reply,
    /// Time the far end spent on the request.
    pub took_ms: u64,
    /// Size of the reply frame on the slowest hop.
    pub wire_bytes: u64,
}

type Waiter = oneshot::Sender<Result<Response, CallError>>;
type CloseReason = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// State shared by a connection's reader task, writer task and callers.
struct Shared {
    pending: Mutex<HashMap<u64, Waiter>>,
    closed: AtomicBool,
    close_detail: Mutex<Option<String>>,
    last_rx: Mutex<Instant>,
    extra_detail: Option<CloseReason>,
}

impl Shared {
    /// Mark the stream dead and fail every pending request. Idempotent.
    fn close(&self, code: ErrorCode, message: String) {
        let message = match self.extra_detail.as_ref().and_then(|f| f()) {
            Some(extra) => format!("{message}: {extra}"),
            None => message,
        };
        {
            let mut detail = self.close_detail.lock().unwrap();
            if detail.is_none() {
                *detail = Some(message.clone());
            }
        }
        // Set before draining: a caller that registers after the drain sees the flag.
        self.closed.store(true, Ordering::SeqCst);
        let waiters: Vec<Waiter> = self
            .pending
            .lock()
            .unwrap()
            .drain()
            .map(|(_, w)| w)
            .collect();
        for waiter in waiters {
            let _ = waiter.send(Err(CallError::new(code, message.clone())));
        }
    }
}

pub struct RpcCore {
    // Unbounded so a cancel can always be queued, even on a stalled link.
    out: mpsc::UnboundedSender<Vec<u8>>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    compress: bool,
}

impl RpcCore {
    /// Take over a stream whose handshake is done. `extra_detail` adds context (such as
    /// recent stderr) to the error reported when the stream closes.
    pub fn start<R, W>(
        mut reader: R,
        mut writer: W,
        compress: bool,
        extra_detail: Option<CloseReason>,
    ) -> Arc<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let shared = Arc::new(Shared {
            pending: Mutex::default(),
            closed: AtomicBool::new(false),
            close_detail: Mutex::default(),
            last_rx: Mutex::new(Instant::now()),
            extra_detail,
        });

        {
            let shared = shared.clone();
            tokio::spawn(async move {
                while let Some(frame) = out_rx.recv().await {
                    if let Err(err) = async {
                        writer.write_all(&frame).await?;
                        writer.flush().await
                    }
                    .await
                    {
                        shared.close(ErrorCode::Disconnected, format!("write failed: {err}"));
                        return;
                    }
                }
                let _ = writer.shutdown().await;
            });
        }

        {
            let shared = shared.clone();
            tokio::spawn(async move {
                let (code, message) = loop {
                    let frame = match read_frame(&mut reader).await {
                        Ok(Some(frame)) => frame,
                        Ok(None) => {
                            break (ErrorCode::Disconnected, "connection closed".to_string());
                        }
                        Err(err) => break (ErrorCode::Protocol, err.to_string()),
                    };
                    *shared.last_rx.lock().unwrap() = Instant::now();
                    let msg: ServerMsg = match serde_json::from_slice(&frame.body) {
                        Ok(msg) => msg,
                        Err(err) => break (ErrorCode::Protocol, format!("invalid reply: {err}")),
                    };
                    let (id, result) = match msg {
                        ServerMsg::Ok {
                            id,
                            took_ms,
                            wire_bytes,
                            reply,
                        } => (
                            id,
                            Ok(Response {
                                reply,
                                took_ms,
                                wire_bytes: wire_bytes.unwrap_or(frame.wire_len as u64),
                            }),
                        ),
                        ServerMsg::Err { id, code, message } => {
                            (id, Err(CallError::new(code, message)))
                        }
                        ServerMsg::Fatal { code, message } => break (code, message),
                        ServerMsg::Hello(_) | ServerMsg::BrokerHello(_) => continue,
                    };
                    let waiter = shared.pending.lock().unwrap().remove(&id);
                    if let Some(waiter) = waiter {
                        let _ = waiter.send(result);
                    }
                };
                shared.close(code, message);
            });
        }

        Arc::new(Self {
            out,
            shared,
            next_id: AtomicU64::new(1),
            compress,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// When the last frame arrived (or the stream started).
    pub fn last_rx(&self) -> Instant {
        *self.shared.last_rx.lock().unwrap()
    }

    fn disconnected(&self) -> CallError {
        let detail = self
            .shared
            .close_detail
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "connection closed".into());
        CallError::new(ErrorCode::Disconnected, detail)
    }

    /// Send one request built from a fresh id and wait for its reply.
    pub async fn call_with(
        &self,
        build: impl FnOnce(u64) -> ClientMsg,
        timeout: Duration,
    ) -> Result<Response, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(id, tx);
        // Checked after registering, so a close that drained `pending` first is not missed.
        if self.is_closed() {
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(self.disconnected());
        }
        let mut guard = CancelGuard {
            core: self,
            id,
            armed: true,
        };
        let frame = encode_msg(&build(id), self.compress);
        if self.out.send(frame).is_err() {
            guard.armed = false;
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(self.disconnected());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => {
                guard.armed = false;
                result
            }
            Ok(Err(_)) => {
                guard.armed = false;
                Err(self.disconnected())
            }
            // The guard sends `cancel` as it drops.
            Err(_) => Err(CallError::new(
                ErrorCode::Timeout,
                format!(
                    "no reply within {}",
                    crate::timefmt::human_duration(timeout)
                ),
            )),
        }
    }

    /// Send a message that has no reply.
    pub async fn notify(&self, msg: &ClientMsg) -> bool {
        self.out.send(encode_msg(msg, self.compress)).is_ok()
    }
}

struct CancelGuard<'a> {
    core: &'a RpcCore,
    id: u64,
    armed: bool,
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.core.shared.pending.lock().unwrap().remove(&self.id);
        let _ = self
            .core
            .out
            .send(encode_msg(&ClientMsg::Cancel { id: self.id }, false));
    }
}

/// What a request handler produces: the reply plus the far end's processing time and,
/// when relaying, the size of the upstream frame.
pub type HandlerResult = Result<(Reply, u64, Option<u64>), CallError>;

/// Server-side bookkeeping for concurrent requests on one connection.
#[derive(Clone)]
pub struct Inflight {
    map: Arc<Mutex<HashMap<u64, AbortHandle>>>,
    tx: mpsc::Sender<ServerMsg>,
}

impl Inflight {
    pub fn new(tx: mpsc::Sender<ServerMsg>) -> Self {
        Self {
            map: Arc::default(),
            tx,
        }
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Run `work` for request `id`. Replies `bad_request` if the id is already in flight.
    pub async fn spawn<F>(&self, id: u64, work: F)
    where
        F: Future<Output = HandlerResult> + Send + 'static,
    {
        if self.map.lock().unwrap().contains_key(&id) {
            let _ = self
                .tx
                .send(ServerMsg::Err {
                    id,
                    code: ErrorCode::BadRequest,
                    message: format!("request id {id} is already in flight"),
                })
                .await;
            return;
        }
        let task = tokio::spawn(work);
        self.map.lock().unwrap().insert(id, task.abort_handle());
        let map = self.map.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = task.await;
            // Whoever removes the entry owns the reply; a cancel may have beaten us to it.
            if map.lock().unwrap().remove(&id).is_none() {
                return;
            }
            let msg = match result {
                Ok(Ok((reply, took_ms, wire_bytes))) => ServerMsg::Ok {
                    id,
                    took_ms,
                    wire_bytes,
                    reply,
                },
                Ok(Err(err)) => ServerMsg::Err {
                    id,
                    code: err.code,
                    message: err.message,
                },
                Err(join) if join.is_panic() => ServerMsg::Err {
                    id,
                    code: ErrorCode::Internal,
                    message: "request handler panicked".into(),
                },
                Err(_) => return,
            };
            let _ = tx.send(msg).await;
        });
    }

    pub async fn cancel(&self, id: u64) {
        let handle = self.map.lock().unwrap().remove(&id);
        if let Some(handle) = handle {
            handle.abort();
            let _ = self
                .tx
                .send(ServerMsg::Err {
                    id,
                    code: ErrorCode::Cancelled,
                    message: "cancelled".into(),
                })
                .await;
        }
    }

    pub fn abort_all(&self) {
        for (_, handle) in self.map.lock().unwrap().drain() {
            handle.abort();
        }
    }
}

/// Write `ServerMsg`s from a channel to `writer` until the channel closes.
pub fn spawn_writer<W>(
    mut writer: W,
    compress: bool,
) -> (mpsc::Sender<ServerMsg>, tokio::task::JoinHandle<()>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(64);
    let task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let frame = encode_msg(&msg, compress);
            if writer.write_all(&frame).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
        let _ = writer.flush().await;
    });
    (tx, task)
}
