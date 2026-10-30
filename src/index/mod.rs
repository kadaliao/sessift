//! Native index: our own transcript parsers, an SQLite session table and a Tantivy full-text
//! index. Transcripts are not copied; record bodies are read back from them by location.
//!
//! Layout of the index directory (`~/.sessift/index`):
//! - `meta.sqlite`: files (for incremental parsing), sessions, record locations.
//! - `tantivy/`: one document per record; the text is indexed, and stored zstd-compressed
//!   for search snippets.
//! - `write.lock`: held by whichever process is updating the index.

mod directory;
pub mod fulltext;
pub mod ingest;
pub mod project;
pub mod query;
pub mod rank;
pub mod reader;
pub mod source;
pub mod store;
pub mod tokenizer;

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct IndexPaths {
    pub dir: PathBuf,
}

impl IndexPaths {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn db(&self) -> PathBuf {
        self.dir.join("meta.sqlite")
    }

    pub fn fulltext(&self) -> PathBuf {
        self.dir.join("tantivy")
    }

    pub fn lock(&self) -> PathBuf {
        self.dir.join("write.lock")
    }
}
