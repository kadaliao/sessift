//! Queries against the native index. Before answering, the index catches up with the
//! transcripts (at most once per `refresh_every`; never waiting for another writer).

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::fulltext::{FullText, SearchSpec};
use super::ingest::{self, INDEX_MESSAGE_BYTES, Report};
use super::source::{self, Kind, Role, Roots};
use super::store::{RecordRow, SessionRow, Store};
use super::{IndexPaths, tokenizer};
use crate::model::{
    BackendInfo, ContextRequest, Page, ReadRequest, RecordView, SearchHit, SearchRequest,
    SessionSummary, SessionsRequest,
};
use crate::text;

pub struct NativeIndex {
    paths: IndexPaths,
    roots: Roots,
    /// How stale the index may get before a query refreshes it; `None` never refreshes.
    refresh_every: Option<Duration>,
    open: Mutex<Option<Open>>,
    refresh: Mutex<Option<Instant>>,
    background: Option<Background>,
}

/// Large updates (a first build) run in a separate `sessift index` process instead of
/// inside a query.
pub struct Background {
    pub paths: crate::config::Paths,
    /// Bytes a query may parse itself before handing the work off.
    pub budget: u64,
}

struct Open {
    store: Store,
    ft: FullText,
    /// Identity of the database file, to notice a rebuild by another process.
    db_inode: u64,
}

impl NativeIndex {
    pub fn new(paths: IndexPaths, roots: Roots, refresh_every: Option<Duration>) -> Self {
        Self {
            paths,
            roots,
            refresh_every,
            open: Mutex::new(None),
            refresh: Mutex::new(None),
            background: None,
        }
    }

    pub fn with_background(mut self, background: Background) -> Self {
        self.background = Some(background);
        self
    }

    pub fn paths(&self) -> &IndexPaths {
        &self.paths
    }

    pub fn roots(&self) -> &Roots {
        &self.roots
    }

    /// Update the index now.
    pub fn refresh(&self, opts: &ingest::Options) -> Result<Option<Report>> {
        let report = ingest::refresh(&self.paths, &self.roots, opts)?;
        *self.refresh.lock().unwrap() = Some(Instant::now());
        Ok(report)
    }

    fn maybe_refresh(&self) -> Result<()> {
        let Some(every) = self.refresh_every else {
            return Ok(());
        };
        // Another thread is refreshing: answer from what is there.
        let Ok(mut last) = self.refresh.try_lock() else {
            return Ok(());
        };
        if last.is_some_and(|t| t.elapsed() < every) {
            return Ok(());
        }
        let opts = ingest::Options {
            budget: self.background.as_ref().map(|b| b.budget),
            ..ingest::Options::default()
        };
        let result = ingest::refresh(&self.paths, &self.roots, &opts);
        *last = Some(Instant::now());
        match result {
            Ok(Some(report)) if report.deferred => {
                if let Some(background) = &self.background
                    && let Err(err) = ingest::spawn_indexer(&background.paths)
                {
                    eprintln!("sessift: {err:#}");
                }
                Ok(())
            }
            Ok(_) => Ok(()),
            // A stale index still answers; only fail when there is none.
            Err(err) if self.paths.db().exists() => {
                eprintln!("sessift: index refresh failed: {err:#}");
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn prepare(&self) -> Result<MutexGuard<'_, Option<Open>>> {
        self.maybe_refresh()?;
        let mut guard = self.open.lock().unwrap();
        let inode = std::fs::metadata(self.paths.db()).map(|m| m.ino()).ok();
        let stale = match (&*guard, inode) {
            (Some(open), Some(inode)) => open.db_inode != inode,
            (None, _) => true,
            (Some(_), None) => true,
        };
        if stale {
            *guard = None;
            let Some(inode) = inode else {
                bail!("no index yet; run `sessift index` to build it");
            };
            let store = Store::open_readonly(&self.paths.db())?;
            if store.meta("refreshed_ms").ok().flatten().is_none() {
                bail!(
                    "the index is still being built (the first build takes a little while); \
                     try again shortly or run `sessift index` to watch it"
                );
            }
            let ft = FullText::open(&self.paths.fulltext())?;
            *guard = Some(Open {
                store,
                ft,
                db_inode: inode,
            });
        }
        guard.as_ref().expect("opened above").ft.refresh_reader()?;
        Ok(guard)
    }

    pub fn info(&self) -> BackendInfo {
        let version =
            match Store::open_readonly(&self.paths.db()).and_then(|s| s.counts_by_source()) {
                Ok(counts) => {
                    let sessions: u64 = counts.iter().map(|c| c.1).sum();
                    format!("{} {sessions} sessions", crate::VERSION)
                }
                Err(_) => format!("{} (no index yet)", crate::VERSION),
            };
        BackendInfo {
            kind: "native".into(),
            version,
        }
    }

    pub fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        let guard = self.prepare()?;
        let open = guard.as_ref().expect("prepared");
        let role = match req.filters.role.as_deref() {
            None => None,
            Some(name) => match Role::parse(name) {
                Some(role) => Some(role as u8),
                None => return Ok(Vec::new()),
            },
        };
        let sessions = open.store.session_ids(&req.filters)?;
        let hits = open.ft.search(&SearchSpec {
            query: &req.query,
            sessions: sessions.as_deref(),
            role,
            since_ms: req.filters.since_ms,
            until_ms: req.filters.until_ms,
            limit: req.limit as usize,
            unique_sessions: req.unique_sessions,
        })?;
        let stored = open.ft.stored_texts(&hits);
        let mut sessions: HashMap<i64, Option<SessionRow>> = HashMap::new();
        let mut found = Vec::with_capacity(hits.len());
        for (hit, text) in hits.into_iter().zip(stored) {
            let session = sessions
                .entry(hit.sess)
                .or_insert_with(|| open.store.session(hit.sess).ok().flatten());
            let (Some(session), Some(record)) =
                (session.clone(), open.store.record(hit.sess, hit.seq)?)
            else {
                continue;
            };
            found.push((hit.score, session, record, text));
        }
        // Without a stored copy, read snippets from the transcripts.
        let mut texts: HashMap<(i64, u32), String> = HashMap::new();
        let mut by_session: HashMap<i64, (&SessionRow, Vec<&RecordRow>)> = HashMap::new();
        for (_, session, record, text) in &found {
            match text {
                Some(text) => {
                    texts.insert((session.id, record.seq), text.clone());
                }
                None => by_session
                    .entry(session.id)
                    .or_insert_with(|| (session, Vec::new()))
                    .1
                    .push(record),
            }
        }
        for (sess, (session, records)) in by_session {
            let Some(kind) = Kind::parse(&session.source) else {
                continue;
            };
            let spans: Vec<_> = records.iter().map(|r| (r.pos, r.len, r.part)).collect();
            let read = source::read_texts(
                kind,
                std::path::Path::new(&session.path),
                &session.session_id,
                &spans,
            )
            .unwrap_or_else(|_| vec![None; spans.len()]);
            for (record, text) in records.iter().zip(read) {
                texts.insert((sess, record.seq), text.unwrap_or_default());
            }
        }
        Ok(found
            .into_iter()
            .map(|(score, session, record, _)| {
                let body = texts
                    .get(&(session.id, record.seq))
                    .map_or("", String::as_str);
                SearchHit {
                    score,
                    ts_ms: record.ts,
                    record_id: record_id(session.id, &record),
                    role: Role::from_u8(record.role).as_str().into(),
                    snippet: snippet(body, &req.query, req.snippet_chars as usize),
                    source: session.source,
                    session_id: session.session_id,
                    source_path: session.path,
                    project: session.project,
                }
            })
            .collect())
    }

    pub fn sessions(&self, req: &SessionsRequest) -> Result<Vec<SessionSummary>> {
        let guard = self.prepare()?;
        let open = guard.as_ref().expect("prepared");
        Ok(open
            .store
            .list_sessions(&req.filters, req.limit, req.offset)?
            .into_iter()
            .map(summary)
            .collect())
    }

    pub fn read(&self, req: &ReadRequest) -> Result<Page> {
        let guard = self.prepare()?;
        let open = guard.as_ref().expect("prepared");
        let sessions = open
            .store
            .sessions_by_sid(&req.session_id, req.source_path.as_deref())?;
        let Some(session) = sessions.into_iter().next() else {
            return Ok(Page {
                session_id: req.session_id.clone(),
                source_path: req.source_path.clone().unwrap_or_default(),
                offset: req.offset,
                total: Some(0),
                next_offset: None,
                records: Vec::new(),
            });
        };
        let rows = open.store.records(session.id, req.offset, req.limit)?;
        let end = req.offset as u64 + rows.len() as u64;
        let records = views(&session, &rows, req.max_chars as usize)?;
        Ok(Page {
            session_id: session.session_id.clone(),
            offset: req.offset,
            total: Some(session.records as u32),
            next_offset: (end < session.records).then_some(end as u32),
            source_path: session.path,
            records,
        })
    }

    pub fn context(&self, req: &ContextRequest) -> Result<Page> {
        let guard = self.prepare()?;
        let open = guard.as_ref().expect("prepared");
        let (sess, pos, part) = parse_record_id(&req.record_id)
            .ok_or_else(|| anyhow!("record {} not found", req.record_id))?;
        let session = open
            .store
            .session(sess)?
            .ok_or_else(|| anyhow!("record {} not found", req.record_id))?;
        let anchor = open
            .store
            .record_at(sess, pos, part)?
            .ok_or_else(|| anyhow!("record {} not found", req.record_id))?;
        let from = anchor.seq.saturating_sub(req.before);
        let count = anchor.seq - from + 1 + req.after;
        let rows = open.store.records(sess, from, count)?;
        let records = views(&session, &rows, req.max_chars as usize)?;
        Ok(Page {
            session_id: session.session_id.clone(),
            offset: from,
            total: Some(session.records as u32),
            next_offset: None,
            source_path: session.path,
            records,
        })
    }
}

/// Record views for a page, each text cut to at most `max_chars` (the page budget is applied
/// later across records).
fn views(session: &SessionRow, rows: &[RecordRow], max_chars: usize) -> Result<Vec<RecordView>> {
    let kind = Kind::parse(&session.source)
        .with_context(|| format!("unknown source {}", session.source))?;
    let spans: Vec<_> = rows.iter().map(|r| (r.pos, r.len, r.part)).collect();
    let texts = source::read_texts(
        kind,
        std::path::Path::new(&session.path),
        &session.session_id,
        &spans,
    )
    .unwrap_or_else(|_| vec![None; spans.len()]);
    Ok(rows
        .iter()
        .zip(texts)
        .map(|(row, text)| {
            let (text, cut) = match text {
                Some(text) => text::truncate_chars(&text, max_chars),
                // The transcript changed or disappeared since it was indexed.
                None => (String::new(), row.chars > 0),
            };
            RecordView {
                record_id: record_id(session.id, row),
                ts_ms: row.ts,
                role: Role::from_u8(row.role).as_str().into(),
                tool_name: row.tool.clone(),
                text,
                total_chars: row.chars,
                truncated: cut,
            }
        })
        .collect())
}

fn summary(s: SessionRow) -> SessionSummary {
    let resume_cmd = Kind::parse(&s.source).and_then(|kind| {
        source::resume_command(
            kind,
            &s.session_id,
            s.kind.as_deref(),
            s.cwd.as_deref(),
            &s.path,
            s.parent.as_deref(),
        )
    });
    SessionSummary {
        source: s.source,
        session_id: s.session_id,
        source_path: s.path,
        project: s.project,
        cwd: s.cwd,
        title: s.title,
        started_ms: s.started_ms,
        last_ms: s.last_ms,
        message_count: Some(s.messages),
        kind: s.kind,
        resume_cmd,
    }
}

/// `<session key hex>-<line offset hex>-<part>`: names a record by its place in the
/// transcript, so it stays valid across re-indexing.
pub fn record_id(sess: i64, row: &RecordRow) -> String {
    format!("{:x}-{:x}-{}", sess, row.pos, row.part)
}

pub fn parse_record_id(id: &str) -> Option<(i64, u64, u16)> {
    let mut parts = id.split('-');
    let sess = i64::from_str_radix(parts.next()?, 16).ok()?;
    let pos = u64::from_str_radix(parts.next()?, 16).ok()?;
    let part = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((sess, pos, part))
}

/// About `max_chars` characters around the first place the query matches (by the same
/// tokenizer the index uses, so stems and Chinese words line up).
pub fn snippet(text: &str, query: &str, max_chars: usize) -> String {
    // A match can only lie in the indexed prefix.
    let head = &text[..text.floor_char_boundary(INDEX_MESSAGE_BYTES)];
    let flat = text::collapse_ws(head);
    let wanted = tokenizer::tokens(query);
    let singles: Vec<char> = wanted
        .iter()
        .filter(|t| tokenizer::is_single_cjk(t))
        .filter_map(|t| t.chars().next())
        .collect();
    let mut first = None;
    tokenizer::walk(&flat, |token, from, _| {
        if wanted.iter().any(|w| w == token) || singles.iter().any(|c| token.contains(*c)) {
            first = Some(from);
            return false;
        }
        true
    });
    let chars: Vec<char> = flat.chars().collect();
    let at = first.map_or(0, |byte| flat[..byte].chars().count());
    let start = at.saturating_sub(max_chars / 4);
    let end = (start + max_chars).min(chars.len());
    let start = end.saturating_sub(max_chars).min(start);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() || head.len() < text.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_ids_round_trip() {
        let row = RecordRow {
            seq: 3,
            pos: 0x1a2b,
            len: 9,
            part: 2,
            ts: 0,
            role: 0,
            tool: None,
            chars: 0,
        };
        let id = record_id(0x3fa9, &row);
        assert_eq!(id, "3fa9-1a2b-2");
        assert_eq!(parse_record_id(&id), Some((0x3fa9, 0x1a2b, 2)));
        assert_eq!(parse_record_id("rid1_a"), None);
        assert_eq!(parse_record_id("1-2-3-4"), None);
    }

    #[test]
    fn snippets_centre_on_stemmed_and_chinese_matches() {
        let text = format!(
            "{}在那之前回滚还来得及{}",
            "前".repeat(100),
            "后".repeat(100)
        );
        let s = snippet(&text, "回滚", 20);
        assert!(s.contains("回滚"), "{s}");
        let s = snippet(
            &format!("{} we deployed it", "x ".repeat(200)),
            "deploying",
            30,
        );
        assert!(s.contains("deployed"), "{s}");
        assert_eq!(snippet("short", "nothing", 30), "short");
    }
}
