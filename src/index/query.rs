//! Queries against the native index. Before answering, the index catches up with the
//! transcripts (at most once per `refresh_every`; never waiting for another writer).

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::fulltext::{FullText, Hit, SearchSpec, prefix_term, query_terms};
use super::ingest::{self, INDEX_MESSAGE_BYTES, Report};
use super::rank::{self, Rank};
use super::source::{self, Kind, Role, Roots};
use super::store::{RecordRow, SessionRow, Store};
use super::{IndexPaths, tokenizer};
use crate::model::{
    BackendInfo, ContextRequest, Page, ReadRequest, RecordView, SearchHit, SearchRequest,
    SessionSummary, SessionsRequest,
};
use crate::text;

/// Extra hits a per-session search asks for, to make up for sessions split over several
/// files that collapse into one.
const SPLIT_SLACK: usize = 16;

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
            in_query: true,
            ..ingest::Options::default()
        };
        let result = ingest::refresh(&self.paths, &self.roots, &opts);
        *last = Some(Instant::now());
        match result {
            Ok(Some(report)) if report.deferred || report.merges_due => {
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
        let rank = Rank::current();
        let sessions = open.store.session_ids(&req.filters)?;
        let weights: HashMap<i64, f32> = open
            .store
            .sessions_of_kinds(&["guardian", "exec", "subagent"])?
            .into_iter()
            .map(|(id, kind)| (id, rank.kind_weight(Some(&kind))))
            .collect();
        let prefix = req.prefix.then(|| prefix_term(&req.query)).flatten();
        let spec = SearchSpec {
            prefix: prefix.as_deref(),
            query: &req.query,
            sessions: sessions.as_deref(),
            role,
            since_ms: req.filters.since_ms,
            until_ms: req.filters.until_ms,
            limit: req.limit as usize,
            weights: Some(Arc::new(weights)),
        };
        let hits = if req.unique_sessions {
            // A session split over several files may come back once per file.
            rank_sessions(open, &spec, rank, req.limit as usize + SPLIT_SLACK)?
        } else {
            open.ft.search(&spec)?
        };
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
        if req.unique_sessions {
            let mut seen = std::collections::HashSet::new();
            found.retain(|(_, session, _, _)| {
                seen.insert((session.source.clone(), session.session_id.clone()))
            });
        }
        found.truncate(req.limit as usize);
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
                    snippet: snippet(
                        body,
                        &req.query,
                        prefix.as_deref(),
                        req.snippet_chars as usize,
                    ),
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
        // A session split over several files (Codex) reads as one, file after file.
        let files = open.store.session_files(&session)?;
        let files = if files.len() > 1 {
            files
        } else {
            vec![session.clone()]
        };
        let total: u64 = files.iter().map(|f| f.records).sum();
        let mut skip = req.offset as u64;
        let mut want = req.limit;
        let mut records = Vec::new();
        for file in &files {
            if want == 0 {
                break;
            }
            if skip >= file.records {
                skip -= file.records;
                continue;
            }
            let rows = open.store.records(file.id, skip as u32, want)?;
            skip = 0;
            want = want.saturating_sub(rows.len() as u32);
            records.extend(views(file, &rows, req.max_chars as usize)?);
        }
        let end = req.offset as u64 + records.len() as u64;
        Ok(Page {
            session_id: session.session_id.clone(),
            offset: req.offset,
            total: Some(total as u32),
            next_offset: (end < total).then_some(end as u32),
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

/// The best `limit` sessions for a search, each as its best record. A session matches when
/// one record holds every term or, scored lower, when the terms are spread over its records.
fn rank_sessions(open: &Open, spec: &SearchSpec, rank: &Rank, limit: usize) -> Result<Vec<Hit>> {
    let terms: Vec<String> = query_terms(spec.query)
        .into_iter()
        .filter(|t| !tokenizer::tokens(t).is_empty())
        .collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    // Session -> (record to show, score before title and recency).
    let mut found: HashMap<i64, (Hit, f32)> = open
        .ft
        .session_matches(spec, &terms)?
        .into_iter()
        .map(|(sess, m)| (sess, (m.best, rank.base(m.best.score, m.records))))
        .collect();
    if terms.len() > 1 && rank.cross > 0.0 {
        let per_term = terms
            .iter()
            .map(|t| open.ft.session_matches(spec, std::slice::from_ref(t)))
            .collect::<Result<Vec<_>>>()?;
        let (first, rest) = per_term.split_first().expect("two or more terms");
        'session: for (sess, m) in first {
            if found.contains_key(sess) {
                continue;
            }
            let (mut sum, mut best, mut records) = (m.best.score, m.best, m.records);
            for other in rest {
                let Some(o) = other.get(sess) else {
                    continue 'session;
                };
                sum += o.best.score;
                if o.best.score > best.score {
                    best = o.best;
                }
                records = records.min(o.records);
            }
            found.insert(*sess, (best, rank.base(rank.cross * sum, records)));
        }
    }
    let mut ranked: Vec<(Hit, f32)> = found.into_values().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.0.ts.cmp(&a.0.ts)));
    // Title and recency only reorder the front of the list.
    ranked.truncate((limit * 4).max(100));
    let ids: Vec<i64> = ranked.iter().map(|(hit, _)| hit.sess).collect();
    let info = open.store.titles_and_times(&ids)?;
    let now = crate::timefmt::now_ms();
    let mut hits: Vec<Hit> = ranked
        .into_iter()
        .map(|(mut hit, base)| {
            let (share, last) = match info.get(&hit.sess) {
                Some((title, last)) => (
                    title
                        .as_deref()
                        .map_or(0.0, |t| rank::title_share(&terms, t)),
                    *last,
                ),
                None => (0.0, hit.ts),
            };
            hit.score = rank.adjust(base, share, now - last);
            hit
        })
        .collect();
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| b.ts.cmp(&a.ts)));
    hits.truncate(limit);
    Ok(hits)
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
pub fn snippet(text: &str, query: &str, prefix: Option<&str>, max_chars: usize) -> String {
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
        if wanted.iter().any(|w| w == token)
            || singles.iter().any(|c| token.contains(*c))
            || prefix.is_some_and(|p| token.starts_with(p))
        {
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
        let s = snippet(&text, "回滚", None, 20);
        assert!(s.contains("回滚"), "{s}");
        let s = snippet(
            &format!("{} we deployed it", "x ".repeat(200)),
            "deploying",
            None,
            30,
        );
        assert!(s.contains("deployed"), "{s}");
        assert_eq!(snippet("short", "nothing", None, 30), "short");
        let s = snippet(
            &format!("{} the tantivy index", "x ".repeat(200)),
            "tanti",
            Some("tanti"),
            30,
        );
        assert!(s.contains("tantivy"), "{s}");
    }
}
