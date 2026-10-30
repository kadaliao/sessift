//! Query backends. The wire protocol and CLI only see `crate::model` types; a backend turns
//! requests into results however it likes: our own index (`native`), or a deterministic
//! fixture backend for tests.

pub mod fixture;
pub mod native;

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::config::{BackendConfig, BackendKind};
use crate::model::BackendInfo;
use crate::proto::{CallError, ErrorCode, Op, Reply};
use crate::text;

pub enum Backend {
    Fixture(fixture::FixtureBackend),
    Native(native::NativeBackend),
}

impl Backend {
    /// `SESSIFT_FIXTURE` overrides the configured backend; tests use it to give each fake
    /// machine its own data.
    pub fn from_config(config: &BackendConfig) -> Result<Self> {
        if let Some(path) = std::env::var_os("SESSIFT_FIXTURE").filter(|v| !v.is_empty()) {
            return Ok(Backend::Fixture(fixture::FixtureBackend::load(
                &PathBuf::from(path),
            )?));
        }
        match config.kind {
            BackendKind::Memex => bail!(
                "the memex backend has been removed; set `kind = \"native\"` under [backend] \
                 in the config (or delete the line)"
            ),
            BackendKind::Fixture => {
                let Some(path) = &config.fixture else {
                    bail!("backend.kind = \"fixture\" needs backend.fixture");
                };
                Ok(Backend::Fixture(fixture::FixtureBackend::load(path)?))
            }
            BackendKind::Native => Ok(Backend::Native(native::NativeBackend::from_config(config)?)),
        }
    }

    pub async fn info(&self) -> BackendInfo {
        match self {
            Backend::Fixture(backend) => backend.info(),
            Backend::Native(backend) => backend.info().await,
        }
    }

    /// Run a data operation. Validates the request and enforces response bounds no matter
    /// what the backend returned, so a misbehaving backend cannot blow up a slow link.
    pub async fn execute(&self, op: &Op) -> Result<Reply, CallError> {
        let bad = |message: String| CallError::new(ErrorCode::BadRequest, message);
        let failed = |err: anyhow::Error| CallError::new(ErrorCode::Backend, format!("{err:#}"));
        match op {
            Op::Search(req) => {
                req.validate().map_err(bad)?;
                let mut hits = match self {
                    Backend::Fixture(b) => b.search(req).await,
                    Backend::Native(b) => b.search(req).await,
                }
                .map_err(failed)?;
                if req.unique_sessions {
                    // Backends return hits best first; keep each session's first.
                    let mut seen = std::collections::HashSet::new();
                    hits.retain(|h| seen.insert((h.source.clone(), h.session_id.clone())));
                }
                hits.truncate(req.limit as usize);
                for hit in &mut hits {
                    let (snippet, cut) =
                        text::truncate_chars(&hit.snippet, req.snippet_chars as usize);
                    hit.snippet = if cut {
                        format!("{snippet}…")
                    } else {
                        snippet
                    };
                }
                Ok(Reply::Search { hits })
            }
            Op::Sessions(req) => {
                req.validate().map_err(bad)?;
                let mut sessions = match self {
                    Backend::Fixture(b) => b.sessions(req).await,
                    Backend::Native(b) => b.sessions(req).await,
                }
                .map_err(failed)?;
                sessions.truncate(req.limit as usize);
                for session in &mut sessions {
                    if let Some(title) = &session.title {
                        session.title = Some(text::truncate_chars(title, 200).0);
                    }
                    if req.brief {
                        session.cwd = None;
                        session.resume_cmd = None;
                        session.kind = None;
                        session.started_ms = None;
                    }
                }
                Ok(Reply::Sessions { sessions })
            }
            Op::Read(req) => {
                req.validate().map_err(bad)?;
                let mut page = match self {
                    Backend::Fixture(b) => b.read(req).await,
                    Backend::Native(b) => b.read(req).await,
                }
                .map_err(failed)?;
                page.records.truncate(req.limit as usize);
                text::apply_page_budget(&mut page.records, req.max_chars as usize);
                Ok(Reply::Page(page))
            }
            Op::Context(req) => {
                req.validate().map_err(bad)?;
                let mut page = match self {
                    Backend::Fixture(b) => b.context(req).await,
                    Backend::Native(b) => b.context(req).await,
                }
                .map_err(failed)?;
                let side = (req.before + req.after + 1) as usize;
                page.records.truncate(side);
                // Spend the budget on the anchor first so it is never crowded out.
                let anchor = page
                    .records
                    .iter()
                    .position(|r| r.record_id == req.record_id)
                    .unwrap_or(0);
                if !page.records.is_empty() {
                    page.records.rotate_left(anchor);
                    text::apply_page_budget(&mut page.records, req.max_chars as usize);
                    page.records.rotate_right(anchor);
                }
                Ok(Reply::Page(page))
            }
            Op::Ping | Op::Info | Op::BrokerStatus => Err(CallError::new(
                ErrorCode::Internal,
                format!("{} is not a backend operation", op.name()),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_memex_config_says_what_to_change() {
        let config = BackendConfig {
            kind: BackendKind::Memex,
            ..BackendConfig::default()
        };
        let Err(err) = Backend::from_config(&config) else {
            panic!("the memex backend is gone");
        };
        assert!(err.to_string().contains("kind = \"native\""), "{err}");
    }
}
