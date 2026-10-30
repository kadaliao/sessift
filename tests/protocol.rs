//! Protocol-level tests against the server over in-memory pipes and child processes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use sessift::backend::Backend;
use sessift::backend::fixture::{FixtureBackend, FixtureData};
use sessift::client::{ConnectOptions, Connection};
use sessift::codec::{encode_msg, read_frame, read_preamble};
use sessift::config::{MachineSpec, Paths, TransportSpec};
use sessift::model::{SearchRequest, SessionsRequest};
use sessift::proto::{
    ClientHello, ClientMsg, ErrorCode, FEATURE_ZSTD, Op, PROTOCOL, Reply, ServerMsg,
};
use sessift::rpc::RpcCore;
use sessift::server::{self, ServerOptions};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

const T0: i64 = 1_793_000_000_000;

fn data(records: usize, text: &str) -> FixtureData {
    let records: Vec<_> = (0..records)
        .map(|i| {
            json!({"record_id": format!("r{i}"), "ts_ms": T0 + i as i64, "role": "assistant",
                   "text": text, "total_chars": text.chars().count(), "truncated": false})
        })
        .collect();
    serde_json::from_value(json!({
        "sessions": [{"source": "claude", "session_id": "s1", "source_path": "/s1.jsonl",
                      "project": "p", "last_ms": T0}],
        "records": {"s1": records}
    }))
    .unwrap()
}

fn backend(delay_ms: u64) -> Backend {
    Backend::Fixture(FixtureBackend::from_data(
        data(3, "deploy the broker"),
        Duration::from_millis(delay_ms),
    ))
}

type Client = (BufReader<ReadHalf<DuplexStream>>, WriteHalf<DuplexStream>);

fn start(backend: Backend, max_concurrency: usize) -> Client {
    let (client, server_side) = tokio::io::duplex(1 << 20);
    let (r, w) = tokio::io::split(server_side);
    let opts = ServerOptions {
        max_concurrency,
        request_timeout: Duration::from_secs(10),
        host: "test".into(),
    };
    tokio::spawn(server::run(BufReader::new(r), w, Arc::new(backend), opts));
    let (r, w) = tokio::io::split(client);
    (BufReader::new(r), w)
}

async fn send(client: &mut Client, msg: &ClientMsg) {
    client.1.write_all(&encode_msg(msg, false)).await.unwrap();
}

async fn recv(client: &mut Client) -> ServerMsg {
    let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut client.0))
        .await
        .expect("reply in time")
        .unwrap()
        .expect("a frame");
    serde_json::from_slice(&frame.body).unwrap()
}

fn hello(protocol: u32) -> ClientMsg {
    ClientMsg::Hello(ClientHello {
        protocol,
        version: "test".into(),
        features: vec![FEATURE_ZSTD.into()],
    })
}

async fn handshake(client: &mut Client) {
    read_preamble(&mut client.0).await.unwrap();
    send(client, &hello(PROTOCOL)).await;
    match recv(client).await {
        ServerMsg::Hello(h) => assert_eq!(h.protocol, PROTOCOL),
        other => panic!("expected hello, got {other:?}"),
    }
}

#[tokio::test]
async fn protocol_mismatch_is_fatal() {
    let mut client = start(backend(0), 4);
    read_preamble(&mut client.0).await.unwrap();
    send(&mut client, &hello(PROTOCOL + 1)).await;
    match recv(&mut client).await {
        ServerMsg::Fatal { code, message } => {
            assert_eq!(code, ErrorCode::ProtocolMismatch);
            assert!(message.contains("speaks protocol"), "{message}");
        }
        other => panic!("expected fatal, got {other:?}"),
    }
}

#[tokio::test]
async fn first_message_must_be_hello() {
    let mut client = start(backend(0), 4);
    read_preamble(&mut client.0).await.unwrap();
    send(
        &mut client,
        &ClientMsg::Req {
            id: 1,
            op: Op::Ping,
        },
    )
    .await;
    assert!(matches!(
        recv(&mut client).await,
        ServerMsg::Fatal {
            code: ErrorCode::Protocol,
            ..
        }
    ));
}

#[tokio::test]
async fn unknown_ops_and_duplicate_ids_get_request_errors() {
    let mut client = start(backend(300), 4);
    handshake(&mut client).await;
    let raw = br#"{"t":"req","id":5,"op":{"kind":"teleport"}}"#;
    client
        .1
        .write_all(&sessift::codec::encode_frame(raw, false))
        .await
        .unwrap();
    match recv(&mut client).await {
        ServerMsg::Err { id, code, .. } => {
            assert_eq!((id, code), (5, ErrorCode::Unsupported));
        }
        other => panic!("{other:?}"),
    }
    let search = Op::Search(SearchRequest::new("deploy"));
    send(
        &mut client,
        &ClientMsg::Req {
            id: 7,
            op: search.clone(),
        },
    )
    .await;
    send(&mut client, &ClientMsg::Req { id: 7, op: search }).await;
    match recv(&mut client).await {
        ServerMsg::Err { id, code, .. } => assert_eq!((id, code), (7, ErrorCode::BadRequest)),
        other => panic!("{other:?}"),
    }
    match recv(&mut client).await {
        ServerMsg::Ok {
            id,
            reply: Reply::Search { hits },
            ..
        } => {
            assert_eq!(id, 7);
            assert_eq!(hits.len(), 3);
        }
        other => panic!("{other:?}"),
    }
    let mut bad = SearchRequest::new("deploy");
    bad.limit = 0;
    send(
        &mut client,
        &ClientMsg::Req {
            id: 8,
            op: Op::Search(bad),
        },
    )
    .await;
    match recv(&mut client).await {
        ServerMsg::Err { id, code, message } => {
            assert_eq!((id, code), (8, ErrorCode::BadRequest));
            assert!(message.contains("limit"));
        }
        other => panic!("{other:?}"),
    }
}

/// With one execution slot, a timed-out request must give its slot back immediately;
/// otherwise the next request waits for the abandoned one to finish.
#[tokio::test]
async fn cancelled_requests_release_capacity() {
    let mut client = start(backend(1000), 1);
    handshake(&mut client).await;
    let core = RpcCore::start(client.0, client.1, true, None);
    let started = Instant::now();
    let err = core
        .call_with(
            |id| ClientMsg::Req {
                id,
                op: Op::Search(SearchRequest::new("deploy")),
            },
            Duration::from_millis(150),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Timeout);
    let response = core
        .call_with(
            |id| ClientMsg::Req {
                id,
                op: Op::Sessions(SessionsRequest::default()),
            },
            Duration::from_millis(1600),
        )
        .await
        .expect("the slot was released by the cancel");
    assert!(matches!(response.reply, Reply::Sessions { .. }));
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[tokio::test]
async fn large_replies_are_compressed() {
    let text = "deploy the broker before the release window closes. ".repeat(40);
    let mut client = start(
        Backend::Fixture(FixtureBackend::from_data(data(200, &text), Duration::ZERO)),
        4,
    );
    handshake(&mut client).await;
    let core = RpcCore::start(client.0, client.1, true, None);
    let mut request = SearchRequest::new("deploy");
    request.limit = 200;
    request.snippet_chars = 2000;
    let response = core
        .call_with(
            |id| ClientMsg::Req {
                id,
                op: Op::Search(request),
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    let Reply::Search { hits } = &response.reply else {
        panic!("unexpected reply");
    };
    assert_eq!(hits.len(), 200);
    let raw = serde_json::to_vec(&response.reply).unwrap().len() as u64;
    assert!(
        response.wire_bytes * 10 < raw,
        "wire {} vs raw {raw}",
        response.wire_bytes
    );
}

/// The server is killed while a 5 s request is pending; the request must fail as soon as
/// the connection drops, not when its 10 s deadline expires. (The first launch of a freshly
/// built binary can take ~0.6 s on macOS, so the server gets 1.5 s before it is killed.)
#[tokio::test]
async fn a_server_that_dies_fails_pending_requests_fast() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("f.json");
    std::fs::write(
        &fixture,
        json!({"sessions": [{"source": "claude", "session_id": "s", "last_ms": T0}]}).to_string(),
    )
    .unwrap();
    let script = format!(
        "exec 3<&0; SESSIFT_FIXTURE='{}' SESSIFT_FIXTURE_DELAY_MS=5000 '{}' serve --stdio <&3 & sleep 1.5; kill $!; echo 'server crashed' >&2",
        fixture.display(),
        env!("CARGO_BIN_EXE_sessift")
    );
    let spec = MachineSpec {
        id: "flaky".into(),
        transport: TransportSpec::Command {
            argv: vec!["sh".into(), "-c".into(), script],
        },
    };
    let paths = Paths {
        home: dir.path().to_path_buf(),
    };
    let opts = ConnectOptions {
        timeout: Duration::from_secs(5),
        control_master: false,
    };
    let conn = Connection::connect(&spec, &paths, &opts).await.unwrap();
    let started = Instant::now();
    let err = conn
        .call(
            Op::Sessions(SessionsRequest::default()),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Disconnected, "{err:?}");
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert!(conn.is_closed());
}
