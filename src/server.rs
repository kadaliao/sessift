//! `sessift serve --stdio`: the remote end of the protocol.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Semaphore, mpsc};

use crate::backend::Backend;
use crate::codec::{encode_msg, read_frame};
use crate::config::Config;
use crate::model::{BackendInfo, ServerInfo};
use crate::proto::{
    CallError, ClientMsg, ErrorCode, FEATURE_ZSTD, Incoming, MAGIC, Op, PROTOCOL, Reply,
    ServerHello, ServerMsg, decode_client,
};
use crate::rpc::{Inflight, spawn_writer};
use crate::timefmt::now_ms;

pub struct ServerOptions {
    pub max_concurrency: usize,
    /// Hard ceiling for one request, independent of client deadlines.
    pub request_timeout: Duration,
    pub host: String,
}

impl ServerOptions {
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_concurrency: config.backend.max_concurrency.unwrap_or(4).max(1),
            request_timeout: Duration::from_secs(60),
            host: hostname(),
        }
    }
}

pub async fn serve_stdio(config: &Config) -> Result<()> {
    let backend = Arc::new(Backend::from_config(&config.backend)?);
    let mut stdout = tokio::io::stdout();
    if std::env::var_os("SESSIFT_TEST_NOISE").is_some() {
        // Simulates a login shell that greets on stdout.
        stdout
            .write_all(b"Welcome to fish, the friendly interactive shell\n")
            .await?;
    }
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    run(stdin, stdout, backend, ServerOptions::from_config(config)).await
}

/// Serve one connection until the client closes it.
pub async fn run<R, W>(
    mut reader: R,
    mut writer: W,
    backend: Arc<Backend>,
    opts: ServerOptions,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    writer.write_all(MAGIC).await?;
    writer.flush().await?;

    let hello = match read_frame(&mut reader).await {
        Ok(Some(frame)) => decode_client(&frame.body),
        Ok(None) => return Ok(()),
        Err(err) => Err(err.to_string()),
    };
    let hello = match hello {
        Ok(Incoming::Msg(ClientMsg::Hello(hello))) => hello,
        Ok(_) => {
            return fatal(
                &mut writer,
                ErrorCode::Protocol,
                "first message must be hello",
            )
            .await;
        }
        Err(message) => return fatal(&mut writer, ErrorCode::Protocol, &message).await,
    };
    if hello.protocol != PROTOCOL {
        let message = format!(
            "remote sessift {} speaks protocol {PROTOCOL}; client {} speaks protocol {}",
            crate::VERSION,
            hello.version,
            hello.protocol
        );
        return fatal(&mut writer, ErrorCode::ProtocolMismatch, &message).await;
    }
    let compress = hello.features.iter().any(|f| f == FEATURE_ZSTD);
    let backend_info = backend.info().await;
    let reply = ServerMsg::Hello(ServerHello {
        protocol: PROTOCOL,
        version: crate::VERSION.into(),
        host: opts.host.clone(),
        features: vec![FEATURE_ZSTD.into()],
        backend: Some(backend_info.clone()),
    });
    writer.write_all(&encode_msg(&reply, false)).await?;
    writer.flush().await?;

    let (tx, writer_task) = spawn_writer(writer, compress);
    let inflight = Inflight::new(tx.clone());
    let ctx = Arc::new(Context {
        backend,
        backend_info,
        permits: Arc::new(Semaphore::new(opts.max_concurrency)),
        active: Arc::new(AtomicU32::new(0)),
        started: Instant::now(),
        opts,
    });

    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(err) => {
                let _ = tx
                    .send(ServerMsg::Fatal {
                        code: ErrorCode::Protocol,
                        message: err.to_string(),
                    })
                    .await;
                break;
            }
        };
        match decode_client(&frame.body) {
            Ok(Incoming::Msg(ClientMsg::Req { id, op })) => {
                dispatch(&ctx, &inflight, &tx, id, op).await;
            }
            Ok(Incoming::Msg(ClientMsg::Cancel { id })) => inflight.cancel(id).await,
            Ok(Incoming::BadReq { id, code, message }) => {
                let _ = tx.send(ServerMsg::Err { id, code, message }).await;
            }
            Ok(Incoming::Ignored(_)) => {}
            Ok(Incoming::Msg(other)) => {
                let _ = tx
                    .send(ServerMsg::Fatal {
                        code: ErrorCode::Protocol,
                        message: format!("unexpected message for a sessift server: {other:?}"),
                    })
                    .await;
                break;
            }
            Err(message) => {
                let _ = tx
                    .send(ServerMsg::Fatal {
                        code: ErrorCode::Protocol,
                        message,
                    })
                    .await;
                break;
            }
        }
    }

    inflight.abort_all();
    drop(inflight);
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

struct Context {
    backend: Arc<Backend>,
    backend_info: BackendInfo,
    permits: Arc<Semaphore>,
    active: Arc<AtomicU32>,
    started: Instant,
    opts: ServerOptions,
}

async fn dispatch(
    ctx: &Arc<Context>,
    inflight: &Inflight,
    tx: &mpsc::Sender<ServerMsg>,
    id: u64,
    op: Op,
) {
    let immediate = match &op {
        Op::Ping => Some(Ok(Reply::Pong {
            server_time_ms: now_ms(),
        })),
        Op::Info => Some(Ok(Reply::Info(ServerInfo {
            host: ctx.opts.host.clone(),
            version: crate::VERSION.into(),
            protocol: PROTOCOL,
            backend: ctx.backend_info.clone(),
            uptime_ms: ctx.started.elapsed().as_millis() as u64,
            active_requests: ctx.active.load(Ordering::Relaxed),
        }))),
        Op::BrokerStatus => Some(Err(CallError::new(
            ErrorCode::Unsupported,
            "broker_status is only served by the broker",
        ))),
        Op::Search(_) | Op::Sessions(_) | Op::Read(_) | Op::Context(_) => None,
    };
    if let Some(result) = immediate {
        let msg = match result {
            Ok(reply) => ServerMsg::Ok {
                id,
                took_ms: 0,
                wire_bytes: None,
                reply,
            },
            Err(err) => ServerMsg::Err {
                id,
                code: err.code,
                message: err.message,
            },
        };
        let _ = tx.send(msg).await;
        return;
    }

    let ctx = ctx.clone();
    inflight
        .spawn(id, async move {
            let received = Instant::now();
            let _permit = ctx
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| CallError::new(ErrorCode::Internal, "server is shutting down"))?;
            ctx.active.fetch_add(1, Ordering::Relaxed);
            let _active = ActiveGuard(ctx.active.clone());
            let limit = ctx.opts.request_timeout;
            let reply = tokio::time::timeout(limit, ctx.backend.execute(&op))
                .await
                .map_err(|_| {
                    CallError::new(
                        ErrorCode::Backend,
                        format!(
                            "request exceeded the server limit of {}",
                            crate::timefmt::human_duration(limit)
                        ),
                    )
                })??;
            Ok((reply, received.elapsed().as_millis() as u64, None))
        })
        .await;
}

struct ActiveGuard(Arc<AtomicU32>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn fatal<W: AsyncWrite + Unpin>(
    writer: &mut W,
    code: ErrorCode,
    message: &str,
) -> Result<()> {
    let msg = ServerMsg::Fatal {
        code,
        message: message.to_string(),
    };
    writer.write_all(&encode_msg(&msg, false)).await?;
    writer.flush().await?;
    Ok(())
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
