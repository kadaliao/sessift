//! Wire messages. See docs/remote-protocol.md.

use serde::{Deserialize, Serialize};

use crate::model::{
    ContextRequest, Page, ReadRequest, SearchHit, SearchRequest, ServerInfo, SessionSummary,
    SessionsRequest,
};

pub const PROTOCOL: u32 = 1;
/// Written by every server (remote `serve` and the local broker) before its first frame.
pub const MAGIC: &[u8; 9] = b"\0SESSIFT\0";
pub const FEATURE_ZSTD: &str = "zstd";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Op {
    Ping,
    Info,
    Search(SearchRequest),
    Sessions(SessionsRequest),
    Read(ReadRequest),
    /// Records around one record; replies with a `page` in chronological order.
    Context(ContextRequest),
    /// Broker only: report pool state.
    BrokerStatus,
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Op::Ping => "ping",
            Op::Info => "info",
            Op::Search(_) => "search",
            Op::Sessions(_) => "sessions",
            Op::Read(_) => "read",
            Op::Context(_) => "context",
            Op::BrokerStatus => "broker_status",
        }
    }

    pub const KNOWN: &'static [&'static str] = &[
        "ping",
        "info",
        "search",
        "sessions",
        "read",
        "context",
        "broker_status",
    ];
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Pong { server_time_ms: i64 },
    Info(ServerInfo),
    Search { hits: Vec<SearchHit> },
    Sessions { sessions: Vec<SessionSummary> },
    Page(Page),
    BrokerStatus(BrokerStatus),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerStatus {
    pub pid: u32,
    pub build_id: String,
    pub version: String,
    pub uptime_ms: u64,
    pub clients: u32,
    pub requests: u64,
    pub machines: Vec<BrokerMachine>,
    #[serde(default)]
    pub retiring: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerMachine {
    pub id: String,
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_for_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    pub requests: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol: u32,
    pub version: String,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol: u32,
    pub version: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<crate::model::BackendInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerHello {
    pub protocol: u32,
    pub build_id: String,
    #[serde(default)]
    pub pid: u32,
}

/// Messages sent by clients: the CLI or broker to `serve`, or the CLI to the broker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello(ClientHello),
    Req {
        id: u64,
        op: Op,
    },
    Cancel {
        id: u64,
    },
    /// CLI to broker.
    BrokerHello(BrokerHello),
    /// CLI to broker: run `op` on `machine` with a deadline.
    Forward {
        id: u64,
        machine: crate::config::MachineSpec,
        op: Op,
        timeout_ms: u64,
    },
    /// CLI to broker: exit now.
    Shutdown,
    /// CLI to broker: keep serving, but exit as soon as no client is connected. Sent when the
    /// broker comes from another build, so the swap never interrupts anyone's request.
    Retire,
}

/// Messages sent by servers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello(ServerHello),
    BrokerHello(BrokerHello),
    Ok {
        id: u64,
        took_ms: u64,
        /// Compressed size of the remote reply frame, when relayed by the broker.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wire_bytes: Option<u64>,
        reply: Reply,
    },
    Err {
        id: u64,
        code: ErrorCode,
        message: String,
    },
    Fatal {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Unsupported,
    Backend,
    Cancelled,
    Internal,
    Protocol,
    ProtocolMismatch,
    /// Client side: no reply before the deadline.
    Timeout,
    /// Client side: the connection closed while the request was pending.
    Disconnected,
    /// Client side: could not connect to the machine.
    Unavailable,
    /// Client side: the remote command is not installed.
    RemoteMissing,
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Backend => "backend",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Internal => "internal",
            ErrorCode::Protocol => "protocol",
            ErrorCode::ProtocolMismatch => "protocol_mismatch",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Disconnected => "disconnected",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::RemoteMissing => "remote_missing",
            ErrorCode::Unknown => "unknown",
        }
    }
}

/// An error attached to one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallError {
    pub code: ErrorCode,
    pub message: String,
}

impl CallError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CallError {}

/// Outcome of decoding an incoming request on the server side. Short-lived, so the size
/// difference between variants does not matter.
#[allow(clippy::large_enum_variant)]
pub enum Incoming {
    Msg(ClientMsg),
    /// A request whose op could not be decoded; reply with this error.
    BadReq {
        id: u64,
        code: ErrorCode,
        message: String,
    },
    /// An unknown notification from a newer peer.
    Ignored(String),
}

/// Decode a client message, keeping the request id when only the op is malformed or unknown,
/// so the server can answer that request instead of dropping the connection.
pub fn decode_client(body: &[u8]) -> Result<Incoming, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|err| format!("invalid JSON: {err}"))?;
    match serde_json::from_value::<ClientMsg>(value.clone()) {
        Ok(msg) => Ok(Incoming::Msg(msg)),
        Err(err) => {
            let id = value.get("id").and_then(serde_json::Value::as_u64);
            let tag = value.get("t").and_then(serde_json::Value::as_str);
            match (tag, id) {
                (Some("req" | "forward"), Some(id)) => {
                    let kind = value
                        .get("op")
                        .and_then(|op| op.get("kind"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let code = if Op::KNOWN.contains(&kind) {
                        ErrorCode::BadRequest
                    } else {
                        ErrorCode::Unsupported
                    };
                    Ok(Incoming::BadReq {
                        id,
                        code,
                        message: format!("cannot decode op '{kind}': {err}"),
                    })
                }
                // A message type from a newer peer: answer it if it expects a reply,
                // otherwise ignore it rather than dropping the connection.
                (Some(tag), id) if !CLIENT_TAGS.contains(&tag) => Ok(match id {
                    Some(id) => Incoming::BadReq {
                        id,
                        code: ErrorCode::Unsupported,
                        message: format!("unsupported message '{tag}'"),
                    },
                    None => Incoming::Ignored(tag.to_string()),
                }),
                _ => Err(format!("invalid message: {err}")),
            }
        }
    }
}

/// Every `t` value this version can send as a client.
const CLIENT_TAGS: &[&str] = &[
    "hello",
    "req",
    "cancel",
    "broker_hello",
    "forward",
    "shutdown",
    "retire",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_round_trips_with_flattened_fields() {
        let op = Op::Search(SearchRequest::new("部署"));
        let json = serde_json::to_string(&ClientMsg::Req {
            id: 3,
            op: op.clone(),
        })
        .unwrap();
        assert!(json.contains(r#""t":"req""#));
        assert!(json.contains(r#""kind":"search""#));
        assert!(json.contains(r#""query":"部署""#));
        match decode_client(json.as_bytes()).unwrap() {
            Incoming::Msg(ClientMsg::Req { id, op: decoded }) => {
                assert_eq!(id, 3);
                assert_eq!(decoded, op);
            }
            _ => panic!("unexpected decode"),
        }
    }

    #[test]
    fn unknown_op_keeps_request_id() {
        let body = br#"{"t":"req","id":9,"op":{"kind":"teleport"}}"#;
        match decode_client(body).unwrap() {
            Incoming::BadReq { id, code, .. } => {
                assert_eq!(id, 9);
                assert_eq!(code, ErrorCode::Unsupported);
            }
            _ => panic!("expected BadReq"),
        }
        let body = br#"{"t":"req","id":4,"op":{"kind":"search"}}"#;
        match decode_client(body).unwrap() {
            Incoming::BadReq { id, code, .. } => {
                assert_eq!(id, 4);
                assert_eq!(code, ErrorCode::BadRequest);
            }
            _ => panic!("expected BadReq"),
        }
        assert!(matches!(
            decode_client(br#"{"t":"nope"}"#).unwrap(),
            Incoming::Ignored(tag) if tag == "nope"
        ));
        assert!(matches!(
            decode_client(br#"{"t":"nope","id":3}"#).unwrap(),
            Incoming::BadReq {
                id: 3,
                code: ErrorCode::Unsupported,
                ..
            }
        ));
        assert!(decode_client(br#"{"t":"cancel"}"#).is_err());
        assert!(decode_client(b"not json").is_err());
    }

    #[test]
    fn unknown_error_codes_decode() {
        let msg: ServerMsg =
            serde_json::from_str(r#"{"t":"err","id":1,"code":"from_the_future","message":"x"}"#)
                .unwrap();
        assert!(matches!(
            msg,
            ServerMsg::Err {
                code: ErrorCode::Unknown,
                ..
            }
        ));
    }
}
