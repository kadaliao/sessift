//! Background requests for the TUI. Every request runs in its own task and reports back with
//! a [`Msg`]; aborting the task cancels the request on the remote end too.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::fanout::{Outcome, Runner};
use crate::model::{ContextRequest, MAX_CHARS_MIN, Page, ReadRequest, RecordView};
use crate::pick::{self, Chosen, ShellKind, Target};
use crate::proto::{ErrorCode, Op, Reply};

/// Identifies a session on a machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub machine: String,
    pub session_id: String,
    pub source_path: String,
}

/// A preview is per session, and per hit and order for search results.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    pub key: Key,
    pub record_id: Option<String>,
    /// Only set for hits: newest first, the context reaches further back from the hit.
    pub newest_first: bool,
}

/// Consecutive records of a session; `offset` is the first one's place in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub offset: u32,
    pub records: Vec<RecordView>,
}

impl Part {
    pub fn end(&self) -> u32 {
        self.offset + self.records.len() as u32
    }
}

/// What a preview shows of a session: the hit with what follows it, or the first records,
/// and then the last ones. The view puts the newest first or last.
#[derive(Debug, Clone)]
pub struct Preview {
    /// In session order, not overlapping.
    pub parts: Vec<Part>,
    pub total: Option<u32>,
}

/// Which records a reader page holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// From this offset on.
    From(u32),
    /// The records just before this offset; `u32::MAX` for the end of the session.
    Before(u32),
    /// Around a search hit, with up to this many records on each side.
    Around {
        record_id: String,
        before: u32,
        after: u32,
    },
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
        /// The reader's generation when it asked; it starts over when the order changes.
        generation: u64,
        result: Result<Page, String>,
    },
    Command {
        purpose: Purpose,
        chosen: Chosen,
    },
}

pub const PAGE_SIZE: u32 = 300;
/// Records per page when reading towards the start: each page is read whole, however many
/// requests that takes, so keep it smaller.
const BACK_PAGE: u32 = 100;
const READER_CHARS: u32 = 150_000;
/// The preview's first and last records. Each record is cut short before the page budget is
/// shared out, so the last ones always come back.
const HEAD: u32 = 6;
const TAIL: u32 = 12;
const PREVIEW_RECORD_CHARS: u32 = 1000;
const PREVIEW_TOOL_CHARS: u32 = 300;

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
        record_chars: Option<u32>,
    ) -> Result<Page, (Option<ErrorCode>, String)> {
        let request = ReadRequest {
            session_id: key.session_id.clone(),
            source_path: (!key.source_path.is_empty()).then(|| key.source_path.clone()),
            offset,
            limit,
            max_chars,
            // The reader folds tool output and shows it whole on demand.
            tool_chars: record_chars.map(|n| n.min(PREVIEW_TOOL_CHARS)),
            record_chars,
        };
        match self.one(&key.machine, Op::Read(request)).await? {
            Reply::Page(page) => Ok(page),
            _ => Err((None, "unexpected reply".into())),
        }
    }

    /// The records from `end - BACK_PAGE` up to `end`, all of them: a page cut short by the
    /// character budget is continued.
    async fn before(&self, key: &Key, end: u32) -> Result<Page, (Option<ErrorCode>, String)> {
        let end = if end == u32::MAX {
            // A page past the end costs nothing and tells how long the session is.
            let probe = self.page(key, u32::MAX, 1, MAX_CHARS_MIN, None).await?;
            probe.total.unwrap_or(0)
        } else {
            end
        };
        let start = end.saturating_sub(BACK_PAGE);
        let mut page = Page {
            session_id: key.session_id.clone(),
            source_path: key.source_path.clone(),
            offset: start,
            total: None,
            next_offset: None,
            records: Vec::new(),
        };
        let mut at = start;
        while at < end {
            let next = self.page(key, at, end - at, READER_CHARS, None).await?;
            page.total = next.total;
            if next.records.is_empty() {
                break;
            }
            at += next.records.len() as u32;
            page.records.extend(next.records);
        }
        page.next_offset = page.total.filter(|t| at < *t).map(|_| at);
        Ok(page)
    }

    async fn read_span(&self, key: &Key, span: &Span) -> Result<Page, String> {
        match span {
            Span::From(offset) => self
                .page(key, *offset, PAGE_SIZE, READER_CHARS, None)
                .await
                .map_err(|e| e.1),
            Span::Before(end) => self.before(key, *end).await.map_err(|e| e.1),
            Span::Around {
                record_id,
                before,
                after,
            } => {
                let request = ContextRequest {
                    record_id: record_id.clone(),
                    session_id: Some(key.session_id.clone()),
                    before: *before,
                    after: *after,
                    max_chars: READER_CHARS,
                    tool_chars: None,
                };
                match self.one(&key.machine, Op::Context(request)).await {
                    Ok(Reply::Page(page)) => Ok(page),
                    // An older remote without `context`, or a hit the index no longer has
                    // (rebuilt since the search): open at the end the order starts from.
                    Err((Some(ErrorCode::Unsupported | ErrorCode::Backend), _))
                        if before > after =>
                    {
                        self.before(key, u32::MAX).await.map_err(|e| e.1)
                    }
                    Err((Some(ErrorCode::Unsupported | ErrorCode::Backend), _)) => self
                        .page(key, 0, PAGE_SIZE, READER_CHARS, None)
                        .await
                        .map_err(|e| e.1),
                    Err((_, message)) => Err(message),
                    Ok(_) => Err("unexpected reply".into()),
                }
            }
        }
    }

    async fn build_preview(&self, key: &PreviewKey) -> Result<Preview, String> {
        let mut parts = Vec::new();
        let mut total = None;
        if let Some(record_id) = &key.record_id {
            // The hit comes right after the gap: oldest first it opens the context and what
            // followed it comes next; newest first, what led to it follows below.
            let (before, after) = if key.newest_first { (8, 2) } else { (1, 10) };
            let request = ContextRequest {
                record_id: record_id.clone(),
                session_id: Some(key.key.session_id.clone()),
                before,
                after,
                max_chars: 8000,
                tool_chars: None,
            };
            match self.one(&key.key.machine, Op::Context(request)).await {
                Ok(Reply::Page(page)) => {
                    total = page.total;
                    parts.push(Part {
                        offset: page.offset,
                        records: page.records,
                    });
                }
                // An older remote without `context`: show the session instead.
                Err((Some(ErrorCode::Unsupported), _)) => {}
                Err((_, message)) => return Err(message),
                Ok(_) => return Err("unexpected reply".into()),
            }
        }
        if parts.is_empty() {
            let head = self
                .page(
                    &key.key,
                    0,
                    HEAD,
                    HEAD * PREVIEW_RECORD_CHARS / 2,
                    Some(PREVIEW_RECORD_CHARS / 2),
                )
                .await
                .map_err(|e| e.1)?;
            total = head.total;
            parts.push(Part {
                offset: 0,
                records: head.records,
            });
        }
        // The newest records, which the preview opens with when the newest come first.
        let covered = parts.last().map_or(0, Part::end);
        let last = total.unwrap_or(0);
        if last > covered {
            let offset = last.saturating_sub(TAIL).max(covered);
            if let Ok(tail) = self
                .page(
                    &key.key,
                    offset,
                    last - offset,
                    TAIL * PREVIEW_RECORD_CHARS,
                    Some(PREVIEW_RECORD_CHARS),
                )
                .await
            {
                parts.push(Part {
                    offset,
                    records: tail.records,
                });
            }
        }
        parts.retain(|p| !p.records.is_empty());
        Ok(Preview { parts, total })
    }

    pub fn preview(&self, key: PreviewKey) -> JoinHandle<()> {
        let loader = self.clone();
        tokio::spawn(async move {
            let result = loader.build_preview(&key).await;
            let _ = loader.tx.send(Msg::Preview { key, result });
        })
    }

    pub fn reader_page(&self, key: Key, generation: u64, span: Span) -> JoinHandle<()> {
        let loader = self.clone();
        tokio::spawn(async move {
            let result = loader.read_span(&key, &span).await;
            let _ = loader.tx.send(Msg::Page {
                key,
                generation,
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
