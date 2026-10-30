//! Tantivy side of the index: one document per record. Only the text is indexed, plus the
//! record's session, sequence number, role and time as fast fields; bodies are read back
//! from the transcripts.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Bound;
use std::path::Path;

use anyhow::{Context, Result};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::query::{
    BooleanQuery, ConstScoreQuery, Occur, PhraseQuery, Query, RangeQuery, RegexQuery, TermQuery,
    TermSetQuery,
};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{
    DocId, Index, IndexReader, IndexWriter, ReloadPolicy, Score, SegmentOrdinal, SegmentReader,
    TantivyDocument, Term,
};

use super::tokenizer;

/// Bump when the Tantivy schema or the tokenizer changes; the whole index is rebuilt.
pub const FULLTEXT_VERSION: u32 = 2;

#[derive(Clone, Copy)]
pub struct Fields {
    pub body: Field,
    /// The indexed text again, zstd-compressed, for search snippets: reading them from the
    /// transcripts would mean decompressing old Codex rollouts from the start.
    pub text: Field,
    pub sess: Field,
    pub seq: Field,
    pub role: Field,
    pub ts: Field,
}

pub fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(tokenizer::NAME)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let body = builder.add_text_field(
        "body",
        TextOptions::default().set_indexing_options(indexing),
    );
    let text = builder.add_text_field("text", STORED);
    let sess = builder.add_u64_field("sess", INDEXED | FAST);
    let seq = builder.add_u64_field("seq", FAST);
    let role = builder.add_u64_field("role", INDEXED | FAST);
    let ts = builder.add_i64_field("ts", INDEXED | FAST);
    (
        builder.build(),
        Fields {
            body,
            text,
            sess,
            seq,
            role,
            ts,
        },
    )
}

pub struct FullText {
    pub index: Index,
    pub reader: IndexReader,
    pub fields: Fields,
}

impl FullText {
    pub fn open_or_create(dir: &Path) -> Result<FullText> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let (schema, fields) = schema();
        let index = if dir.join("meta.json").exists() {
            Index::open_in_dir(dir).with_context(|| format!("open index {}", dir.display()))?
        } else {
            let settings = tantivy::IndexSettings {
                docstore_compression: tantivy::store::Compressor::Zstd(
                    tantivy::store::ZstdCompressor::default(),
                ),
                ..tantivy::IndexSettings::default()
            };
            Index::builder()
                .schema(schema)
                .settings(settings)
                .create_in_dir(dir)
                .with_context(|| format!("create index {}", dir.display()))?
        };
        Self::from_index(index, fields)
    }

    pub fn open(dir: &Path) -> Result<FullText> {
        let (_, fields) = schema();
        let index =
            Index::open_in_dir(dir).with_context(|| format!("open index {}", dir.display()))?;
        Self::from_index(index, fields)
    }

    fn from_index(index: Index, fields: Fields) -> Result<FullText> {
        index.tokenizers().register(
            tokenizer::NAME,
            TextAnalyzer::from(tokenizer::MixedTokenizer::default()),
        );
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(FullText {
            index,
            reader,
            fields,
        })
    }

    pub fn writer(&self, memory_bytes: usize, threads: usize) -> Result<IndexWriter> {
        let writer = if threads > 0 {
            self.index.writer_with_num_threads(threads, memory_bytes)?
        } else {
            self.index.writer(memory_bytes)?
        };
        Ok(writer)
    }

    pub fn doc(
        &self,
        sess: i64,
        seq: u32,
        role: u8,
        ts: i64,
        body: &str,
        stored: Option<&str>,
    ) -> TantivyDocument {
        let f = &self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_text(f.body, body);
        if let Some(text) = stored {
            doc.add_text(f.text, text);
        }
        doc.add_u64(f.sess, sess as u64);
        doc.add_u64(f.seq, seq as u64);
        doc.add_u64(f.role, role as u64);
        doc.add_i64(f.ts, ts);
        doc
    }

    pub fn delete_session(&self, writer: &IndexWriter, sess: i64) {
        writer.delete_term(Term::from_field_u64(self.fields.sess, sess as u64));
    }

    /// Delete a session's records from `seq` on.
    pub fn delete_from(&self, writer: &IndexWriter, sess: i64, seq: u32) -> Result<()> {
        if seq == 0 {
            self.delete_session(writer, sess);
            return Ok(());
        }
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.fields.sess, sess as u64),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(RangeQuery::new(
                    Bound::Included(Term::from_field_u64(self.fields.seq, seq as u64)),
                    Bound::Unbounded,
                )),
            ),
        ]);
        writer.delete_query(Box::new(query))?;
        Ok(())
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Pick up commits made by this or another process.
    pub fn refresh_reader(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    pub fn search(&self, spec: &SearchSpec) -> Result<Vec<Hit>> {
        let Some(query) = self.build_query(spec)? else {
            return Ok(Vec::new());
        };
        let searcher = self.reader.searcher();
        let collector = TopHits {
            limit: spec.limit,
            unique: spec.unique_sessions,
        };
        Ok(searcher.search(&query, &collector)?)
    }

    /// Stored texts of hits, in order.
    pub fn stored_texts(&self, hits: &[Hit]) -> Vec<Option<String>> {
        use tantivy::schema::Value;
        let searcher = self.reader.searcher();
        hits.iter()
            .map(|hit| {
                let doc: TantivyDocument = searcher.doc(hit.addr).ok()?;
                doc.get_first(self.fields.text)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect()
    }

    fn build_query(&self, spec: &SearchSpec) -> Result<Option<Box<dyn Query>>> {
        let f = &self.fields;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for term in query_terms(spec.query) {
            let tokens = tokenizer::tokens(&term);
            let query: Box<dyn Query> = match tokens.as_slice() {
                [] => continue,
                [one] if tokenizer::is_single_cjk(one) => {
                    // The index holds lone CJK characters only inside bigrams.
                    Box::new(RegexQuery::from_pattern(&format!(".?{one}.?"), f.body)?)
                }
                [one] => Box::new(TermQuery::new(
                    Term::from_field_text(f.body, one),
                    IndexRecordOption::WithFreqs,
                )),
                many => Box::new(PhraseQuery::new(
                    many.iter()
                        .map(|t| Term::from_field_text(f.body, t))
                        .collect(),
                )),
            };
            clauses.push((Occur::Must, query));
        }
        if clauses.is_empty() {
            return Ok(None);
        }
        let filter = |q: Box<dyn Query>| -> (Occur, Box<dyn Query>) {
            (Occur::Must, Box::new(ConstScoreQuery::new(q, 0.0)))
        };
        if let Some(sessions) = spec.sessions {
            if sessions.is_empty() {
                return Ok(None);
            }
            clauses.push(filter(Box::new(TermSetQuery::new(
                sessions
                    .iter()
                    .map(|id| Term::from_field_u64(f.sess, *id as u64)),
            ))));
        }
        if let Some(role) = spec.role {
            clauses.push(filter(Box::new(TermQuery::new(
                Term::from_field_u64(f.role, role as u64),
                IndexRecordOption::Basic,
            ))));
        }
        if spec.since_ms.is_some() || spec.until_ms.is_some() {
            let bound = |v: Option<i64>| match v {
                Some(v) => Bound::Included(Term::from_field_i64(f.ts, v)),
                None => Bound::Unbounded,
            };
            clauses.push(filter(Box::new(RangeQuery::new(
                bound(spec.since_ms),
                bound(spec.until_ms),
            ))));
        }
        Ok(Some(Box::new(BooleanQuery::new(clauses))))
    }
}

/// Split a query into terms: whitespace-separated words, or "quoted phrases".
pub fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut rest = query;
    while let Some(start) = rest.find(|c: char| !c.is_whitespace()) {
        rest = &rest[start..];
        if let Some(stripped) = rest.strip_prefix('"') {
            let end = stripped.find('"').unwrap_or(stripped.len());
            terms.push(stripped[..end].to_string());
            rest = stripped.get(end + 1..).unwrap_or("");
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            terms.push(rest[..end].to_string());
            rest = &rest[end..];
        }
    }
    terms
}

pub struct SearchSpec<'a> {
    pub query: &'a str,
    /// Restrict to these sessions (session-level filters resolved in SQLite).
    pub sessions: Option<&'a [i64]>,
    pub role: Option<u8>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub limit: usize,
    pub unique_sessions: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub addr: tantivy::DocAddress,
    pub sess: i64,
    pub seq: u32,
    pub score: f32,
    pub ts: i64,
    pub role: u8,
}

/// Relative weight of a match by role: tool chatter is plentiful and rarely what one means.
pub fn role_weight(role: u8) -> f32 {
    match role {
        0 => 1.2, // user
        1 => 1.0, // assistant
        2 => 0.6, // tool_use
        _ => 0.4, // tool_result
    }
}

fn better(a: &Hit, b: &Hit) -> Ordering {
    a.score
        .total_cmp(&b.score)
        .then_with(|| a.ts.cmp(&b.ts))
        .then_with(|| b.sess.cmp(&a.sess))
        .then_with(|| b.seq.cmp(&a.seq))
}

/// Max-heap entry ordered so that the *worst* hit is on top (to evict it).
struct Worst(Hit);

impl PartialEq for Worst {
    fn eq(&self, other: &Self) -> bool {
        better(&self.0, &other.0) == Ordering::Equal
    }
}
impl Eq for Worst {}
impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        better(&other.0, &self.0)
    }
}

/// Best `limit` hits by weighted score, optionally only the best one per session.
struct TopHits {
    limit: usize,
    unique: bool,
}

enum Best {
    Top(BinaryHeap<Worst>),
    PerSession(HashMap<i64, Hit>),
}

impl Best {
    fn push(&mut self, hit: Hit, limit: usize) {
        match self {
            Best::Top(heap) => {
                if heap.len() < limit {
                    heap.push(Worst(hit));
                } else if let Some(worst) = heap.peek()
                    && better(&hit, &worst.0) == Ordering::Greater
                {
                    heap.pop();
                    heap.push(Worst(hit));
                }
            }
            Best::PerSession(map) => match map.get_mut(&hit.sess) {
                Some(have) if better(&hit, have) != Ordering::Greater => {}
                Some(have) => *have = hit,
                None => {
                    map.insert(hit.sess, hit);
                }
            },
        }
    }

    fn into_vec(self) -> Vec<Hit> {
        match self {
            Best::Top(heap) => heap.into_iter().map(|w| w.0).collect(),
            Best::PerSession(map) => map.into_values().collect(),
        }
    }
}

struct SegmentTop {
    segment: SegmentOrdinal,
    best: Best,
    limit: usize,
    sess: Column<u64>,
    seq: Column<u64>,
    role: Column<u64>,
    ts: Column<i64>,
}

impl SegmentCollector for SegmentTop {
    type Fruit = Vec<Hit>;

    fn collect(&mut self, doc: DocId, score: Score) {
        let role = self.role.first(doc).unwrap_or(3) as u8;
        let hit = Hit {
            addr: tantivy::DocAddress::new(self.segment, doc),
            sess: self.sess.first(doc).unwrap_or(0) as i64,
            seq: self.seq.first(doc).unwrap_or(0) as u32,
            score: score * role_weight(role),
            ts: self.ts.first(doc).unwrap_or(0),
            role,
        };
        self.best.push(hit, self.limit);
    }

    fn harvest(self) -> Vec<Hit> {
        self.best.into_vec()
    }
}

impl Collector for TopHits {
    type Fruit = Vec<Hit>;
    type Child = SegmentTop;

    fn for_segment(
        &self,
        ord: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<SegmentTop> {
        let fast = segment.fast_fields();
        Ok(SegmentTop {
            segment: ord,
            best: if self.unique {
                Best::PerSession(HashMap::new())
            } else {
                Best::Top(BinaryHeap::new())
            },
            limit: self.limit,
            sess: fast.u64("sess")?,
            seq: fast.u64("seq")?,
            role: fast.u64("role")?,
            ts: fast.i64("ts")?,
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, fruits: Vec<Vec<Hit>>) -> tantivy::Result<Vec<Hit>> {
        let mut best = if self.unique {
            Best::PerSession(HashMap::new())
        } else {
            Best::Top(BinaryHeap::new())
        };
        for hit in fruits.into_iter().flatten() {
            best.push(hit, self.limit);
        }
        let mut hits = best.into_vec();
        hits.sort_by(|a, b| better(b, a));
        hits.truncate(self.limit);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with(docs: &[(i64, u32, u8, i64, &str)]) -> (tempfile::TempDir, FullText) {
        let dir = tempfile::tempdir().unwrap();
        let ft = FullText::open_or_create(dir.path()).unwrap();
        let mut writer = ft.writer(15_000_000, 1).unwrap();
        for (sess, seq, role, ts, text) in docs {
            writer
                .add_document(ft.doc(*sess, *seq, *role, *ts, text, None))
                .unwrap();
        }
        writer.commit().unwrap();
        ft.refresh_reader().unwrap();
        (dir, ft)
    }

    fn spec(query: &str) -> SearchSpec<'_> {
        SearchSpec {
            query,
            sessions: None,
            role: None,
            since_ms: None,
            until_ms: None,
            limit: 10,
            unique_sessions: false,
        }
    }

    fn found(ft: &FullText, spec: &SearchSpec) -> Vec<(i64, u32)> {
        ft.search(spec)
            .unwrap()
            .iter()
            .map(|h| (h.sess, h.seq))
            .collect()
    }

    #[test]
    fn chinese_word_inside_a_sentence_is_found() {
        let (_dir, ft) = index_with(&[
            (1, 0, 1, 100, "在那之前回滚还来得及"),
            (1, 1, 0, 101, "滚动更新"),
            (2, 0, 0, 102, "rollback the deploy"),
        ]);
        assert_eq!(found(&ft, &spec("回滚")), vec![(1, 0)]);
        assert_eq!(found(&ft, &spec("回滚还来")), vec![(1, 0)]);
        assert!(found(&ft, &spec("回来")).is_empty());
        // A single character matches wherever it appears in a bigram.
        let mut single = found(&ft, &spec("滚"));
        single.sort();
        assert_eq!(single, vec![(1, 0), (1, 1)]);
        assert_eq!(found(&ft, &spec("Rollbacks")), vec![(2, 0)]);
        assert_eq!(found(&ft, &spec("deploy 回滚")), Vec::<(i64, u32)>::new());
    }

    #[test]
    fn filters_and_unique_sessions() {
        let (_dir, ft) = index_with(&[
            (1, 0, 0, 100, "deploy deploy"),
            (1, 1, 1, 200, "deploy"),
            (2, 0, 3, 300, "deploy log"),
            (3, 0, 1, 400, "deploy"),
        ]);
        let mut s = spec("deploy");
        s.unique_sessions = true;
        let sessions: Vec<i64> = ft.search(&s).unwrap().iter().map(|h| h.sess).collect();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0], 1, "user message with two matches ranks first");
        let only = [2i64, 3];
        let mut s = spec("deploy");
        s.sessions = Some(&only);
        s.role = Some(1);
        assert_eq!(found(&ft, &s), vec![(3, 0)]);
        let mut s = spec("deploy");
        s.since_ms = Some(150);
        s.until_ms = Some(300);
        let mut hits = found(&ft, &s);
        hits.sort();
        assert_eq!(hits, vec![(1, 1), (2, 0)]);
        let empty: [i64; 0] = [];
        let mut s = spec("deploy");
        s.sessions = Some(&empty);
        assert!(found(&ft, &s).is_empty());
    }

    #[test]
    fn deletes_from_a_sequence_number() {
        let (_dir, ft) = index_with(&[
            (1, 0, 0, 1, "alpha"),
            (1, 1, 0, 2, "alpha"),
            (1, 2, 0, 3, "alpha"),
            (2, 0, 0, 4, "alpha"),
        ]);
        let mut writer = ft.writer(15_000_000, 1).unwrap();
        ft.delete_from(&writer, 1, 1).unwrap();
        writer.commit().unwrap();
        ft.refresh_reader().unwrap();
        let mut hits = found(&ft, &spec("alpha"));
        hits.sort();
        assert_eq!(hits, vec![(1, 0), (2, 0)]);
    }

    #[test]
    fn query_terms_keep_quoted_phrases() {
        assert_eq!(
            query_terms(r#" deploy  "roll back" 回滚 "unterminated"#),
            vec!["deploy", "roll back", "回滚", "unterminated"]
        );
    }
}
