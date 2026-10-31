//! Background requests for the TUI. Every request runs in its own task and reports back with
//! a [`Msg`]; aborting the task cancels the request on the remote end too.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::fanout::{Outcome, Runner};
use crate::model::{ContextRequest, Page, ReadRequest, RecordView};
use crate::pick::{self, Chosen, ShellKind, Target};
use crate::proto::{ErrorCode, Op, Reply};

/// Identifies a session on a machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub machine: String,
    pub session_id: String,
    pub source_path: String,
}

/// A preview is per session, and per hit for search results.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    pub key: Key,
    pub record_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Preview {
    pub records: Vec<RecordView>,
    /// For long sessions: records omitted after index `.0`.
    pub gap: Option<(usize, u32)>,
    pub total: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Run,
    Edit,
    Copy,
}

pub enum Msg {
    Rows {
        generation: u64,
        outcome: Outcome,
    },
    Preview {
        key: PreviewKey,
        result: Result<Preview, String>,
    },
    Page {
        key: Key,
        offset: u32,
        result: Result<Page, String>,
    },
    Command {
        purpose: Purpose,
        chosen: Chosen,
    },
}

pub const PAGE_SIZE: u32 = 300;

#[derive(Clone)]
pub struct Loader {
    pub runner: Arc<Runner>,
    pub tx: UnboundedSender<Msg>,
    pub timeout: Duration,
    pub shell: ShellKind,
}

impl Loader {
    /// One task per machine, so each machine's results arrive as soon as they exist.
    pub fn rows(&self, generation: u64, machines: &[String], op: &Op) -> Vec<JoinHandle<()>> {
        machines
            .iter()
            .map(|machine| {
                let (loader, machine, op) = (self.clone(), machine.clone(), op.clone());
                tokio::spawn(async move {
                    let mut outcomes = loader
                        .runner
                        .run(std::slice::from_ref(&machine), loader.timeout, |_| {
                            op.clone()
                        })
                        .await;
                    if let Some(outcome) = outcomes.pop() {
                        let _ = loader.tx.send(Msg::Rows {
                            generation,
                            outcome,
                        });
                    }
                })
            })
            .collect()
    }

    async fn one(&self, machine: &str, op: Op) -> Result<Reply, (Option<ErrorCode>, String)> {
        let outcome = self
            .runner
            .run(&[machine.to_string()], self.timeout, |_| op.clone())
            .await
            .pop()
            .expect("one outcome");
        match outcome.result {
            Ok(response) => Ok(response.reply),
            Err(err) => Err((Some(err.code), err.message)),
        }
    }

    async fn page(
        &self,
        key: &Key,
        offset: u32,
        limit: u32,
        max_chars: u32,
    ) -> Result<Page, (Option<ErrorCode>, String)> {
        let request = ReadRequest {
            session_id: key.session_id.clone(),
            source_path: (!key.source_path.is_empty()).then(|| key.source_path.clone()),
            offset,
            limit,
            max_chars,
            // The reader folds tool output and shows it whole on demand.
            tool_chars: None,
        };
        match self.one(&key.machine, Op::Read(request)).await? {
            Reply::Page(page) => Ok(page),
            _ => Err((None, "unexpected reply".into())),
        }
    }

    async fn build_preview(&self, key: &PreviewKey) -> Result<Preview, String> {
        if let Some(record_id) = &key.record_id {
            let request = ContextRequest {
                record_id: record_id.clone(),
                session_id: Some(key.key.session_id.clone()),
                // Keep the hit near the top of the preview.
                before: 1,
                after: 10,
                max_chars: 8000,
                tool_chars: None,
            };
            match self.one(&key.key.machine, Op::Context(request)).await {
                Ok(Reply::Page(page)) => {
                    return Ok(Preview {
                        records: page.records,
                        gap: None,
                        total: page.total,
                    });
                }
                // An older remote without `context`: show the session instead.
                Err((Some(ErrorCode::Unsupported), _)) => {}
                Err((_, message)) => return Err(message),
                Ok(_) => return Err("unexpected reply".into()),
            }
        }
        let head = self.page(&key.key, 0, 6, 3000).await.map_err(|e| e.1)?;
        let total = head.total.unwrap_or(0);
        let shown = head.records.len() as u32;
        let mut records = head.records;
        let mut gap = None;
        let tail_len = 12;
        if total > shown {
            let offset = total.saturating_sub(tail_len).max(shown);
            if offset > shown {
                gap = Some((records.len(), offset - shown));
            }
            if let Ok(tail) = self.page(&key.key, offset, tail_len, 6000).await {
                records.extend(tail.records);
            }
        }
        Ok(Preview {
            records,
            gap,
            total: head.total,
        })
    }

    pub fn preview(&self, key: PreviewKey) -> JoinHandle<()> {
        let loader = self.clone();
        tokio::spawn(async move {
            let result = loader.build_preview(&key).await;
            let _ = loader.tx.send(Msg::Preview { key, result });
        })
    }

    pub fn reader_page(&self, key: Key, offset: u32) -> JoinHandle<()> {
        let loader = self.clone();
        tokio::spawn(async move {
            let result = loader
                .page(&key, offset, PAGE_SIZE, 150_000)
                .await
                .map_err(|e| e.1);
            let _ = loader.tx.send(Msg::Page {
                key,
                offset,
                result,
            });
        })
    }

    pub fn command(&self, key: Key, purpose: Purpose) -> JoinHandle<()> {
        let loader = self.clone();
        tokio::spawn(async move {
            let target = Target {
                machine: key.machine,
                session_id: key.session_id,
                source_path: key.source_path,
            };
            let chosen = pick::resume_command(&loader.runner, &target, loader.shell).await;
            let _ = loader.tx.send(Msg::Command { purpose, chosen });
        })
    }
}
