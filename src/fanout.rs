//! Run one operation on several machines at once and merge the results.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use tokio::sync::OnceCell;

use crate::backend::Backend;
use crate::broker::BrokerClient;
use crate::client::{ConnectOptions, Connection};
use crate::config::{Config, LOCAL, Paths};
use crate::model::{SearchHit, ServerInfo, SessionSummary};
use crate::proto::{CallError, ErrorCode, Op, PROTOCOL, Reply};
use crate::rpc::Response;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Local,
    Broker,
    Direct,
}

impl Via {
    pub fn as_str(&self) -> &'static str {
        match self {
            Via::Local => "local",
            Via::Broker => "broker",
            Via::Direct => "direct",
        }
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub machine: String,
    pub result: Result<Response, CallError>,
    pub elapsed: Duration,
    pub via: Via,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Located<T> {
    pub machine: String,
    pub item: T,
}

pub struct Runner {
    paths: Paths,
    config: Config,
    use_broker: bool,
    backend: OnceCell<Result<Arc<Backend>, String>>,
    broker: OnceCell<Option<BrokerClient>>,
    /// Why the broker was skipped, for `--timing`.
    pub broker_note: Mutex<Option<String>>,
}

impl Runner {
    pub fn new(paths: Paths, config: Config, use_broker: bool) -> Self {
        let use_broker = use_broker && config.broker.enabled;
        Self {
            paths,
            config,
            use_broker,
            backend: OnceCell::new(),
            broker: OnceCell::new(),
            broker_note: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub async fn local_backend(&self) -> Result<Arc<Backend>, CallError> {
        self.backend
            .get_or_init(|| async {
                Backend::from_config(&self.config.backend)
                    .map(Arc::new)
                    .map_err(|err| format!("{err:#}"))
            })
            .await
            .clone()
            .map_err(|message| CallError::new(ErrorCode::Backend, message))
    }

    async fn broker(&self) -> Option<&BrokerClient> {
        if !self.use_broker {
            return None;
        }
        self.broker
            .get_or_init(|| async {
                match BrokerClient::connect_or_spawn(&self.paths).await {
                    Ok(client) => Some(client),
                    Err(err) => {
                        *self.broker_note.lock().unwrap() =
                            Some(format!("broker unavailable, connecting directly: {err:#}"));
                        None
                    }
                }
            })
            .await
            .as_ref()
    }

    /// Run `op` (built per machine) on every machine in `machines` concurrently. Each machine
    /// gets `timeout`; a slow machine never delays the others' results.
    pub async fn run(
        &self,
        machines: &[String],
        timeout: Duration,
        op_for: impl Fn(&str) -> Op,
    ) -> Vec<Outcome> {
        let calls = machines.iter().map(|id| {
            let op = op_for(id);
            async move { self.one(id, op, timeout).await }
        });
        join_all(calls).await
    }

    async fn one(&self, id: &str, op: Op, timeout: Duration) -> Outcome {
        let started = Instant::now();
        if id == LOCAL {
            let result = tokio::time::timeout(timeout, self.local(op))
                .await
                .unwrap_or_else(|_| {
                    Err(CallError::new(
                        ErrorCode::Timeout,
                        format!(
                            "no result within {}",
                            crate::timefmt::human_duration(timeout)
                        ),
                    ))
                });
            return Outcome {
                machine: id.to_string(),
                result,
                elapsed: started.elapsed(),
                via: Via::Local,
            };
        }
        let spec = match self.config.spec(id) {
            Ok(spec) => spec,
            Err(err) => {
                return Outcome {
                    machine: id.to_string(),
                    result: Err(CallError::new(ErrorCode::BadRequest, err.to_string())),
                    elapsed: started.elapsed(),
                    via: Via::Direct,
                };
            }
        };
        // Starting or replacing the broker counts against this machine's deadline.
        let broker = match tokio::time::timeout(timeout, self.broker()).await {
            Ok(broker) => broker,
            Err(_) => {
                return Outcome {
                    machine: id.to_string(),
                    result: Err(CallError::new(
                        ErrorCode::Timeout,
                        "the background broker did not answer in time",
                    )),
                    elapsed: started.elapsed(),
                    via: Via::Broker,
                };
            }
        };
        let (via, result) = match broker {
            Some(broker) => {
                let remaining = timeout.saturating_sub(started.elapsed());
                (Via::Broker, broker.forward(&spec, op, remaining).await)
            }
            None => {
                let direct = async {
                    let opts = ConnectOptions {
                        timeout: self.config.connect_timeout().min(timeout),
                        control_master: true,
                    };
                    let conn = Connection::connect(&spec, &self.paths, &opts).await?;
                    let remaining = timeout.saturating_sub(started.elapsed());
                    conn.call(op, remaining).await
                };
                let result = tokio::time::timeout(timeout, direct)
                    .await
                    .unwrap_or_else(|_| {
                        Err(CallError::new(
                            ErrorCode::Timeout,
                            format!(
                                "no result within {}",
                                crate::timefmt::human_duration(timeout)
                            ),
                        ))
                    });
                (Via::Direct, result)
            }
        };
        Outcome {
            machine: id.to_string(),
            result,
            elapsed: started.elapsed(),
            via,
        }
    }

    async fn local(&self, op: Op) -> Result<Response, CallError> {
        let started = Instant::now();
        let reply = match op {
            Op::Ping => Reply::Pong {
                server_time_ms: crate::timefmt::now_ms(),
            },
            Op::Info => {
                let backend = self.local_backend().await?;
                Reply::Info(ServerInfo {
                    host: crate::server::hostname(),
                    version: crate::VERSION.into(),
                    protocol: PROTOCOL,
                    backend: backend.info().await,
                    uptime_ms: 0,
                    active_requests: 0,
                })
            }
            op => {
                let backend = self.local_backend().await?;
                backend.execute(&op).await?
            }
        };
        Ok(Response {
            reply,
            took_ms: started.elapsed().as_millis() as u64,
            wire_bytes: 0,
        })
    }
}

/// Search hits from every machine, best score first (newest first on ties).
pub fn merge_hits(outcomes: &[Outcome], limit: usize) -> Vec<Located<SearchHit>> {
    let mut hits: Vec<Located<SearchHit>> = outcomes
        .iter()
        .filter_map(|o| match &o.result {
            Ok(Response {
                reply: Reply::Search { hits },
                ..
            }) => Some(hits.iter().map(|hit| Located {
                machine: o.machine.clone(),
                item: hit.clone(),
            })),
            _ => None,
        })
        .flatten()
        .collect();
    hits.sort_by(|a, b| {
        b.item
            .score
            .total_cmp(&a.item.score)
            .then_with(|| b.item.ts_ms.cmp(&a.item.ts_ms))
            .then_with(|| a.machine.cmp(&b.machine))
    });
    hits.truncate(limit);
    hits
}

/// Sessions from every machine, most recently active first, after skipping `offset`.
pub fn merge_sessions(
    outcomes: &[Outcome],
    offset: usize,
    limit: usize,
) -> Vec<Located<SessionSummary>> {
    let mut sessions: Vec<Located<SessionSummary>> = outcomes
        .iter()
        .filter_map(|o| match &o.result {
            Ok(Response {
                reply: Reply::Sessions { sessions },
                ..
            }) => Some(sessions.iter().map(|s| Located {
                machine: o.machine.clone(),
                item: s.clone(),
            })),
            _ => None,
        })
        .flatten()
        .collect();
    sessions.sort_by(|a, b| {
        b.item
            .last_ms
            .cmp(&a.item.last_ms)
            .then_with(|| a.machine.cmp(&b.machine))
            .then_with(|| a.item.session_id.cmp(&b.item.session_id))
    });
    sessions.into_iter().skip(offset).take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(machine: &str, reply: Result<Reply, CallError>) -> Outcome {
        Outcome {
            machine: machine.into(),
            result: reply.map(|reply| Response {
                reply,
                took_ms: 1,
                wire_bytes: 10,
            }),
            elapsed: Duration::from_millis(5),
            via: Via::Local,
        }
    }

    fn hit(score: f32, ts: i64, id: &str) -> SearchHit {
        SearchHit {
            score,
            ts_ms: ts,
            source: "claude".into(),
            session_id: id.into(),
            source_path: String::new(),
            record_id: String::new(),
            project: String::new(),
            role: "user".into(),
            snippet: String::new(),
        }
    }

    fn session(last: i64, id: &str) -> SessionSummary {
        SessionSummary {
            source: "codex".into(),
            session_id: id.into(),
            source_path: String::new(),
            project: String::new(),
            cwd: None,
            title: None,
            started_ms: None,
            last_ms: last,
            message_count: None,
            kind: None,
            resume_cmd: None,
        }
    }

    #[test]
    fn hits_merge_by_score_then_time_and_skip_failures() {
        let outcomes = vec![
            outcome(
                "local",
                Ok(Reply::Search {
                    hits: vec![hit(5.0, 1, "a"), hit(1.0, 9, "b")],
                }),
            ),
            outcome(
                "mm",
                Ok(Reply::Search {
                    hits: vec![hit(5.0, 2, "c"), hit(3.0, 1, "d")],
                }),
            ),
            outcome("down", Err(CallError::new(ErrorCode::Timeout, "slow"))),
        ];
        let merged = merge_hits(&outcomes, 3);
        let order: Vec<_> = merged
            .iter()
            .map(|h| (h.machine.as_str(), h.item.session_id.as_str()))
            .collect();
        assert_eq!(order, vec![("mm", "c"), ("local", "a"), ("mm", "d")]);
    }

    #[test]
    fn sessions_merge_by_recency_with_offset() {
        let outcomes = vec![
            outcome(
                "local",
                Ok(Reply::Sessions {
                    sessions: vec![session(30, "a"), session(10, "b")],
                }),
            ),
            outcome(
                "mm",
                Ok(Reply::Sessions {
                    sessions: vec![session(20, "c"), session(5, "d")],
                }),
            ),
        ];
        let ids: Vec<_> = merge_sessions(&outcomes, 1, 2)
            .into_iter()
            .map(|s| s.item.session_id)
            .collect();
        assert_eq!(ids, vec!["c", "b"]);
    }
}
