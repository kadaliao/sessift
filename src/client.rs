//! A connection to one remote `sessift serve --stdio`, over SSH or a local command.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::codec::{encode_msg, preview, read_frame, read_preamble};
use crate::config::{MachineSpec, Paths, TransportSpec, ensure_private_dir};
use crate::proto::{
    CallError, ClientHello, ClientMsg, ErrorCode, FEATURE_ZSTD, Op, PROTOCOL, ServerHello,
    ServerMsg,
};
use crate::rpc::{Response, RpcCore};

const STDERR_KEEP: usize = 4096;

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub timeout: Duration,
    /// Share one SSH connection between invocations (OpenSSH ControlMaster).
    pub control_master: bool,
}

pub struct Connection {
    core: Arc<RpcCore>,
    pub hello: ServerHello,
    pub connect_time: Duration,
    pub connected_at: Instant,
    // Held so the transport process dies with the connection.
    _child: Child,
}

impl Connection {
    pub async fn connect(
        spec: &MachineSpec,
        paths: &Paths,
        opts: &ConnectOptions,
    ) -> Result<Self, CallError> {
        let started = Instant::now();
        let mut command = transport_command(spec, paths, opts);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|err| {
            CallError::new(
                ErrorCode::Unavailable,
                format!("{}: cannot start {}: {err}", spec.id, spec.describe()),
            )
        })?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stderr_tail = collect_stderr(stderr);

        // Pipelined: the hello goes out before the server is even up, saving a round trip.
        let hello = ClientMsg::Hello(ClientHello {
            protocol: PROTOCOL,
            version: crate::VERSION.into(),
            features: vec![FEATURE_ZSTD.into()],
        });
        let _ = stdin.write_all(&encode_msg(&hello, false)).await;
        let _ = stdin.flush().await;

        let mut reader = BufReader::new(stdout);
        let handshake = tokio::time::timeout(opts.timeout, async {
            read_preamble(&mut reader)
                .await
                .map_err(Handshake::Preamble)?;
            match read_frame(&mut reader).await {
                Ok(Some(frame)) => serde_json::from_slice::<ServerMsg>(&frame.body)
                    .map_err(|err| Handshake::Other(format!("invalid hello: {err}"))),
                Ok(None) => Err(Handshake::Other(
                    "connection closed during handshake".into(),
                )),
                Err(err) => Err(Handshake::Other(err.to_string())),
            }
        })
        .await;

        let failure = match handshake {
            Ok(Ok(ServerMsg::Hello(hello))) if hello.protocol == PROTOCOL => {
                let compress = hello.features.iter().any(|f| f == FEATURE_ZSTD);
                let tail = stderr_tail.clone();
                let core = RpcCore::start(
                    reader,
                    stdin,
                    compress,
                    Some(Arc::new(move || stderr_summary(&tail))),
                );
                return Ok(Self {
                    core,
                    hello,
                    connect_time: started.elapsed(),
                    connected_at: Instant::now(),
                    _child: child,
                });
            }
            Ok(Ok(ServerMsg::Hello(hello))) => CallError::new(
                ErrorCode::ProtocolMismatch,
                format!(
                    "{}: remote sessift {} speaks protocol {}, this sessift speaks {PROTOCOL}; run `sessift remote install {}`",
                    spec.id, hello.version, hello.protocol, spec.id
                ),
            ),
            Ok(Ok(ServerMsg::Fatal {
                code: ErrorCode::ProtocolMismatch,
                message,
            })) => CallError::new(
                ErrorCode::ProtocolMismatch,
                format!(
                    "{}: {message}; run `sessift remote install {}`",
                    spec.id, spec.id
                ),
            ),
            Ok(Ok(other)) => CallError::new(
                ErrorCode::Protocol,
                format!("{}: unexpected handshake reply {other:?}", spec.id),
            ),
            Ok(Err(Handshake::Preamble(err))) => {
                classify_startup_failure(
                    spec,
                    &mut child,
                    &stderr_tail,
                    err.noise(),
                    &err.to_string(),
                )
                .await
            }
            Ok(Err(Handshake::Other(message))) => {
                classify_startup_failure(spec, &mut child, &stderr_tail, &[], &message).await
            }
            Err(_) => CallError::new(
                ErrorCode::Unavailable,
                format!(
                    "{}: no answer from {} within {}{}",
                    spec.id,
                    spec.describe(),
                    crate::timefmt::human_duration(opts.timeout),
                    stderr_summary(&stderr_tail)
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default()
                ),
            ),
        };
        let _ = child.start_kill();
        Err(failure)
    }

    pub async fn call(&self, op: Op, timeout: Duration) -> Result<Response, CallError> {
        self.core
            .call_with(|id| ClientMsg::Req { id, op }, timeout)
            .await
    }

    pub fn is_closed(&self) -> bool {
        self.core.is_closed()
    }

    /// When the server last sent anything.
    pub fn last_rx(&self) -> Instant {
        self.core.last_rx()
    }
}

enum Handshake {
    Preamble(crate::codec::PreambleError),
    Other(String),
}

/// The command that starts a server for `spec`.
pub fn transport_command(spec: &MachineSpec, paths: &Paths, opts: &ConnectOptions) -> Command {
    match &spec.transport {
        TransportSpec::Ssh {
            target,
            remote_command,
            options,
        } => {
            let mut command = ssh_base(paths, opts.timeout, opts.control_master);
            command
                .args(options)
                .arg("--")
                .arg(target)
                .arg(format!("{remote_command} serve --stdio"));
            command
        }
        TransportSpec::Command { argv } => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            command
        }
    }
}

/// `ssh` with non-interactive, keep-alive and (optionally) connection-sharing options.
pub fn ssh_base(paths: &Paths, connect_timeout: Duration, control_master: bool) -> Command {
    let mut command = Command::new("ssh");
    let secs = connect_timeout.as_secs().clamp(1, 120);
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        &format!("ConnectTimeout={secs}"),
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
    ]);
    match control_path(paths).filter(|_| control_master) {
        Some(path) => {
            command.args([
                "-o",
                "ControlMaster=auto",
                "-o",
                &format!("ControlPath={path}"),
                "-o",
                "ControlPersist=600",
            ]);
        }
        None => {
            command.args(["-o", "ControlMaster=no", "-o", "ControlPath=none"]);
        }
    }
    command
}

fn control_path(paths: &Paths) -> Option<String> {
    let path = paths.ssh_control_path()?;
    ensure_private_dir(&paths.run_dir()).ok()?;
    Some(path)
}

type StderrTail = Arc<Mutex<Vec<u8>>>;

fn collect_stderr(mut stderr: tokio::process::ChildStderr) -> StderrTail {
    let tail: StderrTail = Arc::default();
    let sink = tail.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut tail = sink.lock().unwrap();
                    tail.extend_from_slice(&buf[..n]);
                    let excess = tail.len().saturating_sub(STDERR_KEEP);
                    tail.drain(..excess);
                }
            }
        }
    });
    tail
}

fn stderr_summary(tail: &StderrTail) -> Option<String> {
    let bytes = tail.lock().unwrap().clone();
    let text = preview(&bytes);
    (!text.is_empty()).then_some(text)
}

async fn classify_startup_failure(
    spec: &MachineSpec,
    child: &mut Child,
    stderr_tail: &StderrTail,
    stdout_noise: &[u8],
    detail: &str,
) -> CallError {
    // Give the process a moment to exit so its status and last stderr lines are available.
    let mut status = None;
    for _ in 0..30 {
        if let Ok(Some(exit)) = child.try_wait() {
            status = Some(exit);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let stderr = stderr_summary(stderr_tail).unwrap_or_default();
    let noise = String::from_utf8_lossy(stdout_noise);
    let combined = format!("{stderr}\n{noise}");
    let code = status.and_then(|s| s.code());
    let is_ssh = matches!(spec.transport, TransportSpec::Ssh { .. });
    // 127 is "command not found" in every common shell (fish included), but a dynamic loader
    // that cannot start the binary also exits 127; reinstalling would not help there.
    let loader_markers = [
        "error while loading shared libraries",
        "GLIBC_",
        "Library not loaded",
        "dyld",
        "Exec format error",
        "cannot execute binary file",
        "bad CPU type",
    ];
    let loader_problem = loader_markers.iter().any(|m| combined.contains(m));
    if loader_problem {
        return CallError::new(
            ErrorCode::Unavailable,
            format!(
                "{}: the remote sessift cannot start (built for another platform?): {}",
                spec.id,
                if stderr.is_empty() {
                    detail
                } else {
                    stderr.as_str()
                }
            ),
        );
    }
    if code == Some(127) {
        let path = match &spec.transport {
            TransportSpec::Ssh { remote_command, .. } => remote_command.as_str(),
            TransportSpec::Command { argv } => argv[0].as_str(),
        };
        return CallError::new(
            ErrorCode::RemoteMissing,
            format!(
                "{}: sessift is not installed at {path}; run `sessift remote install {}`",
                spec.id, spec.id
            ),
        );
    }
    if is_ssh && code == Some(255) {
        let reason = if stderr.is_empty() {
            detail.to_string()
        } else {
            stderr
        };
        return CallError::new(
            ErrorCode::Unavailable,
            format!("{}: ssh failed: {reason}", spec.id),
        );
    }
    let mut message = format!("{}: {detail}", spec.id);
    if let Some(code) = code {
        message.push_str(&format!(" (exit status {code})"));
    }
    if !stderr.is_empty() {
        message.push_str(&format!("; stderr: {stderr}"));
    }
    CallError::new(ErrorCode::Unavailable, message)
}
