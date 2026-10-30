//! The broker: a per-user background process that keeps one connection open per remote
//! machine, so each CLI invocation pays a Unix-socket hop instead of an SSH handshake.
//!
//! It starts on demand, holds an exclusive lock so only one runs, pre-connects the default
//! machines, reconnects after drops, exits when idle, and is replaced when a CLI from a
//! different build talks to it.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, watch};

use crate::client::{ConnectOptions, Connection};
use crate::codec::{encode_msg, read_frame, read_preamble};
use crate::config::{Config, LOCAL, MachineSpec, Paths, build_id, ensure_private_dir};
use crate::proto::{
    BrokerHello, BrokerMachine, BrokerStatus, CallError, ClientMsg, ErrorCode, Incoming, MAGIC, Op,
    PROTOCOL, Reply, ServerMsg, decode_client,
};
use crate::rpc::{HandlerResult, Inflight, Response, RpcCore, spawn_writer};

/// After a failed connect, requests within this window fail fast instead of retrying.
const RETRY_AFTER: Duration = Duration::from_secs(1);
const MAX_FORWARD_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a liveness probe on an idle connection may take before it is presumed dead.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2000);
const PROBE_MIN: Duration = Duration::from_millis(1000);
/// Probe after a request saw no bytes at all; generous, since nothing waits on it.
const LATE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", jiff::Timestamp::now(), format_args!($($arg)*))
    };
}

struct Broker {
    paths: Paths,
    build_id: String,
    started: Instant,
    connect_timeout: Duration,
    probe_after_idle: Duration,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    clients: AtomicU32,
    requests: AtomicU64,
    last_activity: Mutex<Instant>,
    shutdown: Notify,
    retiring: AtomicBool,
}

enum ConnState {
    Idle,
    Connecting,
    Ready(Arc<Connection>),
    Failed { at: Instant, error: CallError },
}

struct Slot {
    spec: MachineSpec,
    state: watch::Sender<ConnState>,
    requests: AtomicU64,
    last_used: Mutex<Instant>,
}

impl Broker {
    fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_activity.lock().unwrap().elapsed()
    }

    fn slot(&self, spec: &MachineSpec) -> Arc<Slot> {
        self.slots
            .lock()
            .unwrap()
            .entry(spec.key())
            .or_insert_with(|| {
                Arc::new(Slot {
                    spec: spec.clone(),
                    state: watch::channel(ConnState::Idle).0,
                    requests: AtomicU64::new(0),
                    last_used: Mutex::new(Instant::now()),
                })
            })
            .clone()
    }

    async fn forward(
        self: &Arc<Self>,
        spec: MachineSpec,
        op: Op,
        timeout: Duration,
    ) -> HandlerResult {
        if matches!(op, Op::BrokerStatus) {
            return Err(CallError::new(
                ErrorCode::BadRequest,
                "broker_status cannot be forwarded",
            ));
        }
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.touch();
        let deadline = tokio::time::Instant::now() + timeout.min(MAX_FORWARD_TIMEOUT);
        let slot = self.slot(&spec);
        slot.requests.fetch_add(1, Ordering::Relaxed);
        *slot.last_used.lock().unwrap() = Instant::now();
        // Every op is a read, so one retry on a fresh connection is always safe.
        let conn = slot.connection(self, deadline).await?;
        let result = match self.attempt(&slot, &conn, op.clone(), deadline).await {
            Attempt::Done(result) => result,
            Attempt::Reconnect => {
                let conn = slot.connection(self, deadline).await?;
                conn.call(op, remaining(deadline)).await
            }
        };
        let Response {
            reply,
            took_ms,
            wire_bytes,
        } = result?;
        Ok((reply, took_ms, Some(wire_bytes)))
    }

    /// One try on `conn`. A connection can die without closing (a laptop that slept, a
    /// network change); keep-alives take up to 45 s to notice, so idle connections get a
    /// short probe raced against the request, and a request that saw no bytes at all for its
    /// whole deadline drops the connection.
    async fn attempt(
        &self,
        slot: &Arc<Slot>,
        conn: &Arc<Connection>,
        op: Op,
        deadline: tokio::time::Instant,
    ) -> Attempt {
        let sent_at = Instant::now();
        let idle = sent_at.saturating_duration_since(conn.last_rx());
        let budget = remaining(deadline);
        let request = conn.call(op, budget);
        tokio::pin!(request);
        let result = if idle >= self.probe_after_idle {
            // Generous enough for a slow, jittery link: a false alarm costs a reconnect.
            let probe = conn.call(Op::Ping, PROBE_TIMEOUT.min(budget / 2).max(PROBE_MIN));
            tokio::select! {
                result = &mut request => result,
                alive = probe => match alive {
                    Err(err) if matches!(err.code, ErrorCode::Timeout | ErrorCode::Disconnected) => {
                        log!(
                            "{}: idle {} and no answer to a probe; reconnecting",
                            slot.spec.id,
                            crate::timefmt::human_duration(idle)
                        );
                        slot.invalidate(conn);
                        return Attempt::Reconnect;
                    }
                    _ => request.await,
                },
            }
        } else {
            request.await
        };
        match &result {
            Err(err) if err.code == ErrorCode::Disconnected && !remaining(deadline).is_zero() => {
                log!(
                    "{}: connection lost mid-request ({}); retrying",
                    slot.spec.id,
                    err.message
                );
                slot.invalidate(conn);
                Attempt::Reconnect
            }
            // Silence for a whole request means dead or merely slow. Find out in the background
            // instead of dropping a connection that a slow link would struggle to replace.
            Err(err) if err.code == ErrorCode::Timeout && conn.last_rx() < sent_at => {
                let (slot, conn) = (slot.clone(), conn.clone());
                tokio::spawn(async move {
                    let started = Instant::now();
                    let alive = conn.call(Op::Ping, LATE_PROBE_TIMEOUT).await.is_ok();
                    if !alive && conn.last_rx() < started {
                        log!(
                            "{}: no answer to a probe after a silent request; dropping the connection",
                            slot.spec.id
                        );
                        slot.invalidate(&conn);
                    }
                });
                Attempt::Done(result)
            }
            _ => Attempt::Done(result),
        }
    }

    fn status(&self) -> BrokerStatus {
        let mut machines: Vec<BrokerMachine> = self
            .slots
            .lock()
            .unwrap()
            .values()
            .map(|slot| {
                let state = slot.state.borrow();
                let (connected, remote_version, connected_for_ms, last_error) = match &*state {
                    ConnState::Ready(conn) if !conn.is_closed() => (
                        true,
                        Some(conn.hello.version.clone()),
                        Some(conn.connected_at.elapsed().as_millis() as u64),
                        None,
                    ),
                    ConnState::Failed { error, .. } => {
                        (false, None, None, Some(error.message.clone()))
                    }
                    _ => (false, None, None, None),
                };
                BrokerMachine {
                    id: slot.spec.id.clone(),
                    connected,
                    remote_version,
                    connected_for_ms,
                    idle_ms: Some(slot.last_used.lock().unwrap().elapsed().as_millis() as u64),
                    requests: slot.requests.load(Ordering::Relaxed),
                    last_error,
                }
            })
            .collect();
        machines.sort_by(|a, b| a.id.cmp(&b.id));
        BrokerStatus {
            pid: std::process::id(),
            build_id: self.build_id.clone(),
            version: crate::VERSION.into(),
            uptime_ms: self.started.elapsed().as_millis() as u64,
            clients: self.clients.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            machines,
            retiring: self.retiring.load(Ordering::Relaxed),
        }
    }
}

enum Attempt {
    Done(Result<Response, CallError>),
    Reconnect,
}

fn remaining(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

impl Slot {
    /// Forget `conn` if it is still the current connection, so the next request reconnects.
    fn invalidate(&self, conn: &Arc<Connection>) {
        self.state.send_if_modified(|state| match state {
            ConnState::Ready(current) if Arc::ptr_eq(current, conn) => {
                *state = ConnState::Idle;
                true
            }
            _ => false,
        });
    }

    /// A live connection, connecting at most once at a time. The connect runs in its own task,
    /// so a request that gives up does not abort it; the next request reuses the result.
    async fn connection(
        self: &Arc<Self>,
        broker: &Arc<Broker>,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<Connection>, CallError> {
        let mut rx = self.state.subscribe();
        loop {
            let mut stale = false;
            {
                let current = rx.borrow_and_update();
                match &*current {
                    ConnState::Ready(conn) if !conn.is_closed() => return Ok(conn.clone()),
                    ConnState::Failed { at, error } if at.elapsed() < RETRY_AFTER => {
                        return Err(error.clone());
                    }
                    ConnState::Connecting => {}
                    _ => stale = true,
                }
            }
            if stale {
                let claimed = self.state.send_if_modified(|state| {
                    let stale = match state {
                        ConnState::Idle => true,
                        ConnState::Ready(conn) => conn.is_closed(),
                        ConnState::Failed { at, .. } => at.elapsed() >= RETRY_AFTER,
                        ConnState::Connecting => false,
                    };
                    if stale {
                        *state = ConnState::Connecting;
                    }
                    stale
                });
                if claimed {
                    self.spawn_connect(broker.clone());
                }
                continue;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    return Err(CallError::new(ErrorCode::Internal, "connection state lost"));
                }
                Err(_) => {
                    return Err(CallError::new(
                        ErrorCode::Timeout,
                        format!(
                            "{}: still connecting ({})",
                            self.spec.id,
                            self.spec.describe()
                        ),
                    ));
                }
            }
        }
    }

    fn spawn_connect(self: &Arc<Self>, broker: Arc<Broker>) {
        let slot = self.clone();
        tokio::spawn(async move {
            log!("connecting to {} ({})", slot.spec.id, slot.spec.describe());
            let opts = ConnectOptions {
                timeout: broker.connect_timeout,
                control_master: true,
            };
            let next = match Connection::connect(&slot.spec, &broker.paths, &opts).await {
                Ok(conn) => {
                    log!(
                        "connected to {} in {}ms (remote sessift {}, backend {})",
                        slot.spec.id,
                        conn.connect_time.as_millis(),
                        conn.hello.version,
                        conn.hello
                            .backend
                            .as_ref()
                            .map(|b| b.version.as_str())
                            .unwrap_or("?")
                    );
                    ConnState::Ready(Arc::new(conn))
                }
                Err(error) => {
                    log!("connect to {} failed: {}", slot.spec.id, error.message);
                    ConnState::Failed {
                        at: Instant::now(),
                        error,
                    }
                }
            };
            slot.state.send_replace(next);
        });
    }
}

/// Run the broker until shutdown, idle timeout or a signal.
pub async fn run(paths: Paths, config: Config) -> Result<()> {
    ensure_private_dir(&paths.run_dir())?;
    ensure_private_dir(&paths.log_dir())?;
    if !paths.broker_socket_usable() {
        bail!(
            "broker socket path is too long: {}",
            paths.broker_socket().display()
        );
    }
    let Some(_lock) = acquire_lock(&paths.broker_lock(), Duration::from_secs(3)).await? else {
        log!(
            "another broker holds {}; exiting",
            paths.broker_lock().display()
        );
        return Ok(());
    };
    let socket = paths.broker_socket();
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("remove {}", socket.display())),
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;

    let idle = config.broker_idle_timeout();
    let broker = Arc::new(Broker {
        paths: paths.clone(),
        build_id: build_id(),
        started: Instant::now(),
        connect_timeout: config.connect_timeout(),
        probe_after_idle: config.broker_probe_after_idle(),
        slots: Mutex::default(),
        clients: AtomicU32::new(0),
        requests: AtomicU64::new(0),
        last_activity: Mutex::new(Instant::now()),
        shutdown: Notify::new(),
        retiring: AtomicBool::new(false),
    });
    log!(
        "broker {} started (pid {}), idle timeout {}",
        broker.build_id,
        std::process::id(),
        crate::timefmt::human_duration(idle)
    );

    // Pre-connect the default machines so the first query does not pay the handshake.
    for id in config.select(&[]).unwrap_or_default() {
        if id == LOCAL {
            continue;
        }
        if let Ok(spec) = config.spec(&id) {
            let slot = broker.slot(&spec);
            let broker = broker.clone();
            let deadline = tokio::time::Instant::now() + broker.connect_timeout;
            tokio::spawn(async move {
                let _ = slot.connection(&broker, deadline).await;
            });
        }
    }

    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let tick_every = (idle / 4).clamp(Duration::from_millis(100), Duration::from_secs(5));
    let mut tick = tokio::time::interval(tick_every);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(handle_client(broker.clone(), stream));
                }
                Err(err) => {
                    log!("accept failed: {err}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = broker.shutdown.notified() => {
                log!("shutdown requested");
                break;
            }
            _ = sigterm.recv() => { log!("SIGTERM"); break; }
            _ = sigint.recv() => { log!("SIGINT"); break; }
            _ = sighup.recv() => { log!("SIGHUP ignored"); }
            _ = tick.tick() => {
                let no_clients = broker.clients.load(Ordering::Relaxed) == 0;
                if no_clients && broker.retiring.load(Ordering::Relaxed) {
                    log!("retired, exiting");
                    break;
                }
                if no_clients && broker.idle_for() >= idle {
                    log!("idle for {}, exiting", crate::timefmt::human_duration(idle));
                    break;
                }
            }
        }
    }
    // Remove the socket while still holding the lock, so a successor never loses its socket.
    let _ = std::fs::remove_file(&socket);
    broker.slots.lock().unwrap().clear();
    Ok(())
}

async fn handle_client(broker: Arc<Broker>, stream: UnixStream) {
    // The socket directory is private, but check the peer anyway.
    let uid = unsafe { libc::getuid() };
    if stream
        .peer_cred()
        .map(|cred| cred.uid() != uid)
        .unwrap_or(true)
    {
        log!("rejected a connection from another user");
        return;
    }
    broker.clients.fetch_add(1, Ordering::Relaxed);
    broker.touch();
    if let Err(err) = serve_client(&broker, stream).await {
        log!("client error: {err:#}");
    }
    let remaining_clients = broker.clients.fetch_sub(1, Ordering::Relaxed) - 1;
    broker.touch();
    if remaining_clients == 0 && broker.retiring.load(Ordering::Relaxed) {
        broker.shutdown.notify_one();
    }
}

async fn serve_client(broker: &Arc<Broker>, stream: UnixStream) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    write_half.write_all(MAGIC).await?;
    // A client that never says hello must not keep the broker from going idle.
    let hello = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader))
        .await
        .map_err(|_| anyhow!("client sent no hello"))??
        .ok_or_else(|| anyhow!("client closed before hello"))?;
    match decode_client(&hello.body) {
        Ok(Incoming::Msg(ClientMsg::BrokerHello(hello))) if hello.protocol == PROTOCOL => {}
        Ok(Incoming::Msg(ClientMsg::BrokerHello(hello))) => {
            // Still answer, so the client can see the build differs and replace us.
            log!("client speaks protocol {}", hello.protocol);
        }
        _ => {
            let fatal = ServerMsg::Fatal {
                code: ErrorCode::Protocol,
                message: "first message must be broker_hello".into(),
            };
            write_half.write_all(&encode_msg(&fatal, false)).await?;
            return Ok(());
        }
    }
    let reply = ServerMsg::BrokerHello(BrokerHello {
        protocol: PROTOCOL,
        build_id: broker.build_id.clone(),
        pid: std::process::id(),
    });
    write_half.write_all(&encode_msg(&reply, false)).await?;
    write_half.flush().await?;

    let (tx, writer_task) = spawn_writer(write_half, false);
    let inflight = Inflight::new(tx.clone());
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(err) => {
                log!("bad frame from client: {err}");
                break;
            }
        };
        match decode_client(&frame.body) {
            Ok(Incoming::Msg(ClientMsg::Forward {
                id,
                machine,
                op,
                timeout_ms,
            })) => {
                let broker = broker.clone();
                inflight
                    .spawn(id, async move {
                        broker
                            .forward(machine, op, Duration::from_millis(timeout_ms))
                            .await
                    })
                    .await;
            }
            Ok(Incoming::Msg(ClientMsg::Req { id, op })) => {
                let msg = match op {
                    Op::BrokerStatus => ServerMsg::Ok {
                        id,
                        took_ms: 0,
                        wire_bytes: None,
                        reply: Reply::BrokerStatus(broker.status()),
                    },
                    Op::Ping => ServerMsg::Ok {
                        id,
                        took_ms: 0,
                        wire_bytes: None,
                        reply: Reply::Pong {
                            server_time_ms: crate::timefmt::now_ms(),
                        },
                    },
                    other => ServerMsg::Err {
                        id,
                        code: ErrorCode::Unsupported,
                        message: format!(
                            "the broker forwards '{}'; send it with forward",
                            other.name()
                        ),
                    },
                };
                let _ = tx.send(msg).await;
            }
            Ok(Incoming::Msg(ClientMsg::Cancel { id })) => inflight.cancel(id).await,
            Ok(Incoming::Msg(ClientMsg::Retire)) => {
                if !broker.retiring.swap(true, Ordering::Relaxed) {
                    log!("retiring: a client from another build connected");
                }
            }
            Ok(Incoming::Msg(ClientMsg::Shutdown)) => {
                broker.shutdown.notify_one();
                break;
            }
            Ok(Incoming::BadReq { id, code, message }) => {
                let _ = tx.send(ServerMsg::Err { id, code, message }).await;
            }
            Ok(Incoming::Ignored(tag)) => log!("ignored unknown client message '{tag}'"),
            Ok(Incoming::Msg(other)) => {
                log!("unexpected client message: {other:?}");
                break;
            }
            Err(message) => {
                log!("undecodable client message: {message}");
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

/// Take the broker lock, retrying while a previous broker finishes exiting.
async fn acquire_lock(path: &Path, wait: Duration) -> Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let deadline = Instant::now() + wait;
    loop {
        // SAFETY: the descriptor is valid for the lifetime of `file`.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(file));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(err).with_context(|| format!("lock {}", path.display()));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// CLI side of the broker socket.
pub struct BrokerClient {
    core: Arc<RpcCore>,
    pub hello: BrokerHello,
}

impl BrokerClient {
    /// Connect to a running broker without starting one.
    pub async fn connect(paths: &Paths) -> Result<Self> {
        let stream = tokio::time::timeout(
            Duration::from_secs(1),
            UnixStream::connect(paths.broker_socket()),
        )
        .await
        .map_err(|_| anyhow!("timed out connecting to the broker"))??;
        let (read_half, mut write_half) = stream.into_split();
        let hello = ClientMsg::BrokerHello(BrokerHello {
            protocol: PROTOCOL,
            build_id: build_id(),
            pid: std::process::id(),
        });
        write_half.write_all(&encode_msg(&hello, false)).await?;
        let mut reader = BufReader::new(read_half);
        let reply = tokio::time::timeout(Duration::from_secs(3), async {
            read_preamble(&mut reader).await?;
            let frame = read_frame(&mut reader)
                .await?
                .ok_or_else(|| anyhow!("broker closed the connection"))?;
            anyhow::Ok(serde_json::from_slice::<ServerMsg>(&frame.body)?)
        })
        .await
        .map_err(|_| anyhow!("broker did not answer"))??;
        let ServerMsg::BrokerHello(hello) = reply else {
            bail!("unexpected broker reply: {reply:?}");
        };
        let core = RpcCore::start(reader, write_half, false, None);
        Ok(Self { core, hello })
    }

    /// Connect to the broker for this build, starting (or replacing) it as needed.
    pub async fn connect_or_spawn(paths: &Paths) -> Result<Self> {
        if !paths.broker_socket_usable() {
            bail!(
                "broker socket path is too long: {}",
                paths.broker_socket().display()
            );
        }
        let mine = build_id();
        if let Ok(client) = Self::connect(paths).await {
            if client.hello.protocol == PROTOCOL {
                // Another build speaks the same protocol: use it now and let it retire once
                // idle; the next invocation starts this build's broker.
                if client.hello.build_id != mine {
                    client.retire().await;
                }
                return Ok(client);
            }
            client.shutdown().await;
            wait_until_gone(paths, Duration::from_secs(3)).await;
        }
        // Spawning waits for exec, which can take most of a second for a freshly built
        // binary on macOS; keep that off the async workers.
        let spawn_paths = paths.clone();
        tokio::task::spawn_blocking(move || spawn_broker(&spawn_paths))
            .await
            .map_err(|err| anyhow!("broker spawn task failed: {err}"))??;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match Self::connect(paths).await {
                Ok(client) => return Ok(client),
                Err(err) if Instant::now() >= deadline => {
                    return Err(err.context(format!(
                        "the broker did not start; see {}",
                        paths.broker_log().display()
                    )));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    pub async fn forward(
        &self,
        spec: &MachineSpec,
        op: Op,
        timeout: Duration,
    ) -> Result<Response, CallError> {
        let timeout_ms = timeout.as_millis() as u64;
        let machine = spec.clone();
        // The broker enforces the deadline; the margin only covers a broker that hangs.
        self.core
            .call_with(
                move |id| ClientMsg::Forward {
                    id,
                    machine,
                    op,
                    timeout_ms,
                },
                timeout + Duration::from_millis(500),
            )
            .await
    }

    pub async fn status(&self) -> Result<BrokerStatus, CallError> {
        let response = self
            .core
            .call_with(
                |id| ClientMsg::Req {
                    id,
                    op: Op::BrokerStatus,
                },
                Duration::from_secs(3),
            )
            .await?;
        match response.reply {
            Reply::BrokerStatus(status) => Ok(status),
            other => Err(CallError::new(
                ErrorCode::Protocol,
                format!("unexpected reply {other:?}"),
            )),
        }
    }

    pub async fn retire(&self) {
        self.core.notify(&ClientMsg::Retire).await;
    }

    pub async fn shutdown(&self) {
        self.core.notify(&ClientMsg::Shutdown).await;
        // Let the writer task flush before the caller drops us.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until nothing answers on the broker socket.
pub async fn wait_until_gone(paths: &Paths, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if UnixStream::connect(paths.broker_socket()).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Start `sessift broker run` fully detached (new session, reparented to launchd/init).
fn spawn_broker(paths: &Paths) -> Result<()> {
    ensure_private_dir(&paths.run_dir())?;
    ensure_private_dir(&paths.log_dir())?;
    let log_path = paths.broker_log();
    if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(&log_path, log_path.with_extension("log.1"));
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("open {}", log_path.display()))?;
    let exe = std::env::current_exe().context("locate the sessift binary")?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["broker", "run"])
        .env("SESSIFT_HOME", &paths.home)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: only async-signal-safe calls between fork and exec. The intermediate child
    // exits at once; the grandchild runs the broker and is never our child, so no zombie.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    let mut child = command.spawn().context("start the broker")?;
    let _ = child.wait();
    Ok(())
}
