//! Deterministic backend over a JSON file, for tests and demos.
//!
//! `SESSIFT_FIXTURE_DELAY_MS` delays every data operation (cancellation and timeout tests).
//! A search for `__fail__` returns a backend error.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::model::{
    BackendInfo, ContextRequest, Filters, Page, ReadRequest, RecordView, SearchHit, SearchRequest,
    SessionSummary, SessionsRequest,
};
use crate::text;

#[derive(Debug, Deserialize)]
pub struct FixtureData {
    #[serde(default)]
    pub sessions: Vec<SessionSummary>,
    /// Records keyed by session id, in order.
    #[serde(default)]
    pub records: HashMap<String, Vec<RecordView>>,
}

pub struct FixtureBackend {
    data: FixtureData,
    delay: Duration,
}

impl FixtureBackend {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read fixture {}", path.display()))?;
        let data: FixtureData = serde_json::from_str(&text)
            .with_context(|| format!("parse fixture {}", path.display()))?;
        let delay = std::env::var("SESSIFT_FIXTURE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or_default();
        Ok(Self { data, delay })
    }

    pub fn from_data(data: FixtureData, delay: Duration) -> Self {
        Self { data, delay }
    }

    pub fn info(&self) -> BackendInfo {
        BackendInfo {
            kind: "fixture".into(),
            version: format!("{} sessions", self.data.sessions.len()),
        }
    }

    async fn pause(&self) {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
    }

    pub async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        self.pause().await;
        if req.query.trim() == "__fail__" {
            bail!("injected failure");
        }
        let terms = text::query_terms(&req.query);
        let mut hits = Vec::new();
        for session in self
            .data
            .sessions
            .iter()
            .filter(|s| session_matches(s, &req.filters))
        {
            let Some(records) = self.data.records.get(&session.session_id) else {
                continue;
            };
            for record in records {
                if !in_range(record.ts_ms, &req.filters)
                    || req
                        .filters
                        .role
                        .as_ref()
                        .is_some_and(|role| *role != record.role)
                {
                    continue;
                }
                let lower = record.text.to_lowercase();
                if !terms.iter().all(|term| lower.contains(term.as_str())) {
                    continue;
                }
                let occurrences: usize = terms
                    .iter()
                    .map(|t| lower.matches(t.as_str()).count())
                    .sum();
                hits.push(SearchHit {
                    score: occurrences as f32,
                    ts_ms: record.ts_ms,
                    source: session.source.clone(),
                    session_id: session.session_id.clone(),
                    source_path: session.source_path.clone(),
                    record_id: record.record_id.clone(),
                    project: session.project.clone(),
                    role: record.role.clone(),
                    snippet: text::snippet_around(&record.text, &terms, req.snippet_chars as usize),
                });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.ts_ms.cmp(&a.ts_ms))
        });
        hits.truncate(req.limit as usize);
        Ok(hits)
    }

    pub async fn sessions(&self, req: &SessionsRequest) -> Result<Vec<SessionSummary>> {
        self.pause().await;
        let mut sessions: Vec<SessionSummary> = self
            .data
            .sessions
            .iter()
            .filter(|s| session_matches(s, &req.filters) && in_range(s.last_ms, &req.filters))
            .cloned()
            .collect();
        sessions.sort_by(|a, b| b.last_ms.cmp(&a.last_ms));
        Ok(sessions
            .into_iter()
            .skip(req.offset as usize)
            .take(req.limit as usize)
            .collect())
    }

    pub async fn context(&self, req: &ContextRequest) -> Result<Page> {
        self.pause().await;
        for session in &self.data.sessions {
            if req
                .session_id
                .as_ref()
                .is_some_and(|id| *id != session.session_id)
            {
                continue;
            }
            let Some(records) = self.data.records.get(&session.session_id) else {
                continue;
            };
            if let Some(i) = records.iter().position(|r| r.record_id == req.record_id) {
                let start = i.saturating_sub(req.before as usize);
                let end = (i + 1 + req.after as usize).min(records.len());
                return Ok(Page {
                    session_id: session.session_id.clone(),
                    source_path: session.source_path.clone(),
                    offset: start as u32,
                    total: Some(records.len() as u32),
                    next_offset: None,
                    records: records[start..end].to_vec(),
                });
            }
        }
        bail!("record {} not found", req.record_id)
    }

    pub async fn read(&self, req: &ReadRequest) -> Result<Page> {
        self.pause().await;
        let session = self.data.sessions.iter().find(|s| {
            s.session_id == req.session_id
                && req
                    .source_path
                    .as_ref()
                    .is_none_or(|path| *path == s.source_path)
        });
        let records = session
            .and_then(|s| self.data.records.get(&s.session_id))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let total = records.len() as u32;
        let start = (req.offset as usize).min(records.len());
        let end = (start + req.limit as usize).min(records.len());
        Ok(Page {
            session_id: req.session_id.clone(),
            source_path: session.map(|s| s.source_path.clone()).unwrap_or_default(),
            offset: req.offset,
            total: Some(total),
            next_offset: (end < records.len()).then_some(end as u32),
            records: records[start..end].to_vec(),
        })
    }
}

fn session_matches(session: &SessionSummary, filters: &Filters) -> bool {
    let eq = |want: &Option<String>, have: &str| want.as_ref().is_none_or(|w| w == have);
    eq(&filters.source, &session.source)
        && eq(&filters.project, &session.project)
        && eq(&filters.session_id, &session.session_id)
        && filters.cwd.as_ref().is_none_or(|cwd| {
            session
                .cwd
                .as_ref()
                .is_some_and(|have| have == cwd || have.starts_with(&format!("{cwd}/")))
        })
}

fn in_range(ts: i64, filters: &Filters) -> bool {
    filters.since_ms.is_none_or(|since| ts >= since)
        && filters.until_ms.is_none_or(|until| ts <= until)
}
