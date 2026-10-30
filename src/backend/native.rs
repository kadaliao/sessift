//! Backend over our own index (`crate::index`). Index work is blocking, so each call runs on
//! the blocking thread pool.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::config::{BackendConfig, Paths};
use crate::index::IndexPaths;
use crate::index::query::{Background, NativeIndex};
use crate::index::source::Roots;
use crate::model::{
    BackendInfo, ContextRequest, Page, ReadRequest, SearchHit, SearchRequest, SessionSummary,
    SessionsRequest,
};

/// How stale the index may get before a query brings it up to date.
pub const DEFAULT_REFRESH_EVERY: Duration = Duration::from_secs(2);
/// A query parses up to this much itself (well under a second); more goes to a background
/// `sessift index`.
pub const BACKGROUND_ABOVE_BYTES: u64 = 256 << 20;

pub struct NativeBackend {
    index: Arc<NativeIndex>,
}

impl NativeBackend {
    pub fn from_config(config: &BackendConfig) -> Result<Self> {
        let paths = Paths::from_env()?;
        let refresh = match config.refresh_every {
            Some(d) if d.is_zero() => None,
            Some(d) => Some(d),
            None => Some(DEFAULT_REFRESH_EVERY),
        };
        let index = NativeIndex::new(
            IndexPaths::new(paths.index_dir()),
            Roots::from_env(),
            refresh,
        )
        .with_background(Background {
            paths,
            budget: BACKGROUND_ABOVE_BYTES,
        });
        Ok(Self::new(index))
    }

    pub fn new(index: NativeIndex) -> Self {
        Self {
            index: Arc::new(index),
        }
    }

    pub fn index(&self) -> &Arc<NativeIndex> {
        &self.index
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&NativeIndex) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let index = self.index.clone();
        tokio::task::spawn_blocking(move || f(&index))
            .await
            .map_err(|err| anyhow!("index task failed: {err}"))?
    }

    pub async fn info(&self) -> BackendInfo {
        let index = self.index.clone();
        tokio::task::spawn_blocking(move || index.info())
            .await
            .unwrap_or_else(|err| BackendInfo {
                kind: "native".into(),
                version: format!("unavailable: {err}"),
            })
    }

    pub async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        let req = req.clone();
        self.run(move |index| index.search(&req)).await
    }

    pub async fn sessions(&self, req: &SessionsRequest) -> Result<Vec<SessionSummary>> {
        let req = req.clone();
        self.run(move |index| index.sessions(&req)).await
    }

    pub async fn read(&self, req: &ReadRequest) -> Result<Page> {
        let req = req.clone();
        self.run(move |index| index.read(&req)).await
    }

    pub async fn context(&self, req: &ContextRequest) -> Result<Page> {
        let req = req.clone();
        self.run(move |index| index.context(&req)).await
    }
}
