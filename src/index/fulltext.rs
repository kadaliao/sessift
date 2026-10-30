//! Tantivy side of the index: one document per record. Only the text is indexed, plus the
//! record's session, sequence number, role and time as fast fields; bodies are read back
//! from the transcripts.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::merge_policy::{LogMergePolicy, MergeCandidate, MergePolicy};
use tantivy::query::{
    BooleanQuery, ConstScoreQuery, Occur, PhraseQuery, Query, RangeQuery, RegexQuery, TermQuery,
    TermSetQuery,
};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{
    DocId, Index, IndexReader, IndexWriter, ReloadPolicy, Score, SegmentMeta, SegmentOrdinal,
    SegmentReader, TantivyDocument, Term,
};

use super::directory::IndexDirectory;
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

/// Which segment merges a writer runs once it commits. Every commit adds a segment, and
/// the writer waits for the merges before it is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merges {
    /// Tantivy's default (`LogMergePolicy`). Keeps the segment count low, but every few
    /// tens of thousands of new records it rewrites a large part of the index: seconds.
    All,
    /// Only segments below `SMALL_SEGMENT_DOCS`, so a query never waits long; the rest is
    /// left to `Merges::All` in `sessift index`.
    Small,
}

/// A segment this small is merged in a query's refresh, as soon as there are
/// `SMALL_MERGE_COUNT` of them: at most 16k records, a few tens of milliseconds.
const SMALL_SEGMENT_DOCS: u32 = 2_000;
const SMALL_MERGE_COUNT: usize = 8;

#[derive(Debug)]
struct SmallMerges;

impl MergePolicy for SmallMerges {
    fn compute_merge_candidates(&self, segments: &[SegmentMeta]) -> Vec<MergeCandidate> {
        let small: Vec<_> = segments
            .iter()
            .filter(|segment| segment.max_doc() < SMALL_SEGMENT_DOCS)
            .map(SegmentMeta::id)
            .collect();
        if small.len() >= SMALL_MERGE_COUNT {
            vec![MergeCandidate(small)]
        } else {
            Vec::new()
        }
    }
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
        let directory = IndexDirectory::open(dir)
            .with_context(|| format!("open index directory {}", dir.display()))?;
        let index = if dir.join("meta.json").exists() {
            Index::open(directory).with_context(|| format!("open index {}", dir.display()))?
        } else {
            let settings = tantivy::IndexSettings {
                docstore_compression: tantivy::store::Compressor::Zstd(
                    tantivy::store::ZstdCompressor::default(),
                ),
                ..tantivy::IndexSettings::default()
            };
            Index::create(directory, schema, settings)
                .with_context(|| format!("create index {}", dir.display()))?
        };
        Self::from_index(index, fields)
    }

    pub fn open(dir: &Path) -> Result<FullText> {
        let (_, fields) = schema();
        let directory = IndexDirectory::open(dir)
            .with_context(|| format!("open index directory {}", dir.display()))?;
        let index =
            Index::open(directory).with_context(|| format!("open index {}", dir.display()))?;
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

    pub fn writer(
        &self,
        memory_bytes: usize,
        threads: usize,
        merges: Merges,
    ) -> Result<IndexWriter> {
        let writer: IndexWriter = if threads > 0 {
            self.index.writer_with_num_threads(threads, memory_bytes)?
        } else {
            self.index.writer(memory_bytes)?
        };
        if merges == Merges::Small {
            writer.set_merge_policy(Box::new(SmallMerges));
        }
        Ok(writer)
    }

    /// Whether Tantivy's default policy has merges waiting, which `Merges::Small` leaves.
    pub fn merges_due(&self) -> Result<bool> {
        let segments = self.index.searchable_segment_metas()?;
        Ok(!LogMergePolicy::default()
            .compute_merge_candidates(&segments)
            .is_empty())
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

    /// Best records matching every term of the query.
    pub fn search(&self, spec: &SearchSpec) -> Result<Vec<Hit>> {
        let terms = query_terms(spec.query);
        let Some(query) = self.build_query(spec, &terms)? else {
            return Ok(Vec::new());
        };
        let searcher = self.reader.searcher();
        let collector = TopHits {
            limit: spec.limit,
            weights: spec.weights.clone(),
        };
        Ok(searcher.search(&query, &collector)?)
    }

    /// For every session with a record matching all of `terms`: its best record and how many
    /// records match.
    pub fn session_matches(
        &self,
        spec: &SearchSpec,
        terms: &[String],
    ) -> Result<HashMap<i64, SessionMatch>> {
        let Some(query) = self.build_query(spec, terms)? else {
            return Ok(HashMap::new());
        };
        let searcher = self.reader.searcher();
        Ok(searcher.search(
            &query,
            &SessionCollector {
                weights: spec.weights.clone(),
            },
        )?)
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

    fn build_query(&self, spec: &SearchSpec, terms: &[String]) -> Result<Option<Box<dyn Query>>> {
        let f = &self.fields;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for term in terms {
            let tokens = tokenizer::tokens(term);
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
    /// Score multipliers of sessions (by kind); sessions not in the map count 1.
    pub weights: Option<Arc<HashMap<i64, f32>>>,
}

/// A session's matches for one query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionMatch {
    pub best: Hit,
    pub records: u32,
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

/// Fast-field columns a collector reads for each matching record.
struct Columns {
    segment: SegmentOrdinal,
    sess: Column<u64>,
    seq: Column<u64>,
    role: Column<u64>,
    ts: Column<i64>,
    weights: Option<Arc<HashMap<i64, f32>>>,
}

impl Columns {
    fn open(
        segment: SegmentOrdinal,
        reader: &SegmentReader,
        weights: Option<Arc<HashMap<i64, f32>>>,
    ) -> tantivy::Result<Columns> {
        let fast = reader.fast_fields();
        Ok(Columns {
            segment,
            sess: fast.u64("sess")?,
            seq: fast.u64("seq")?,
            role: fast.u64("role")?,
            ts: fast.i64("ts")?,
            weights,
        })
    }

    /// The record as a hit, its score weighted by role and session.
    fn hit(&self, doc: DocId, score: Score) -> Hit {
        let role = self.role.first(doc).unwrap_or(3) as u8;
        let sess = self.sess.first(doc).unwrap_or(0) as i64;
        let session_weight = self
            .weights
            .as_ref()
            .and_then(|w| w.get(&sess).copied())
            .unwrap_or(1.0);
        Hit {
            addr: tantivy::DocAddress::new(self.segment, doc),
            sess,
            seq: self.seq.first(doc).unwrap_or(0) as u32,
            score: score * role_weight(role) * session_weight,
            ts: self.ts.first(doc).unwrap_or(0),
            role,
        }
    }
}

/// Best `limit` records by weighted score.
struct TopHits {
    limit: usize,
    weights: Option<Arc<HashMap<i64, f32>>>,
}

struct SegmentTop {
    columns: Columns,
    heap: BinaryHeap<Worst>,
    limit: usize,
}

fn push_top(heap: &mut BinaryHeap<Worst>, hit: Hit, limit: usize) {
    if heap.len() < limit {
        heap.push(Worst(hit));
    } else if let Some(worst) = heap.peek()
        && better(&hit, &worst.0) == Ordering::Greater
    {
        heap.pop();
        heap.push(Worst(hit));
    }
}

impl SegmentCollector for SegmentTop {
    type Fruit = Vec<Hit>;

    fn collect(&mut self, doc: DocId, score: Score) {
        let hit = self.columns.hit(doc, score);
        push_top(&mut self.heap, hit, self.limit);
    }

    fn harvest(self) -> Vec<Hit> {
        self.heap.into_iter().map(|w| w.0).collect()
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
        Ok(SegmentTop {
            columns: Columns::open(ord, segment, self.weights.clone())?,
            heap: BinaryHeap::new(),
            limit: self.limit,
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(&self, fruits: Vec<Vec<Hit>>) -> tantivy::Result<Vec<Hit>> {
        let mut heap = BinaryHeap::new();
        for hit in fruits.into_iter().flatten() {
            push_top(&mut heap, hit, self.limit);
        }
        let mut hits: Vec<Hit> = heap.into_iter().map(|w| w.0).collect();
        hits.sort_by(|a, b| better(b, a));
        Ok(hits)
    }
}

/// Every matching session's best record and number of matching records.
struct SessionCollector {
    weights: Option<Arc<HashMap<i64, f32>>>,
}

struct SegmentSessions {
    columns: Columns,
    found: HashMap<i64, SessionMatch>,
}

fn add_match(found: &mut HashMap<i64, SessionMatch>, hit: Hit, records: u32) {
    match found.get_mut(&hit.sess) {
        Some(have) => {
            have.records += records;
            if better(&hit, &have.best) == Ordering::Greater {
                have.best = hit;
            }
        }
        None => {
            found.insert(hit.sess, SessionMatch { best: hit, records });
        }
    }
}

impl SegmentCollector for SegmentSessions {
    type Fruit = HashMap<i64, SessionMatch>;

    fn collect(&mut self, doc: DocId, score: Score) {
        let hit = self.columns.hit(doc, score);
        add_match(&mut self.found, hit, 1);
    }

    fn harvest(self) -> HashMap<i64, SessionMatch> {
        self.found
    }
}

impl Collector for SessionCollector {
    type Fruit = HashMap<i64, SessionMatch>;
    type Child = SegmentSessions;

    fn for_segment(
        &self,
        ord: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<SegmentSessions> {
        Ok(SegmentSessions {
            columns: Columns::open(ord, segment, self.weights.clone())?,
            found: HashMap::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        true
    }

    fn merge_fruits(
        &self,
        fruits: Vec<HashMap<i64, SessionMatch>>,
    ) -> tantivy::Result<HashMap<i64, SessionMatch>> {
        let mut fruits = fruits.into_iter();
        let mut found = fruits.next().unwrap_or_default();
        for fruit in fruits {
            for (_, m) in fruit {
                add_match(&mut found, m.best, m.records);
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with(docs: &[(i64, u32, u8, i64, &str)]) -> (tempfile::TempDir, FullText) {
        let dir = tempfile::tempdir().unwrap();
        let ft = FullText::open_or_create(dir.path()).unwrap();
        let mut writer = ft.writer(15_000_000, 1, Merges::All).unwrap();
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
            weights: None,
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
    fn filters_session_matches_and_weights() {
        let (_dir, ft) = index_with(&[
            (1, 0, 0, 100, "deploy deploy"),
            (1, 1, 1, 200, "deploy"),
            (2, 0, 3, 300, "deploy log"),
            (3, 0, 1, 400, "deploy"),
        ]);
        let s = spec("deploy");
        let matches = ft.session_matches(&s, &query_terms("deploy")).unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[&1].records, 2);
        assert_eq!(
            matches[&1].best.seq, 0,
            "user message with two matches is the best"
        );
        let best = matches
            .values()
            .max_by(|a, b| better(&a.best, &b.best))
            .unwrap();
        assert_eq!(best.best.sess, 1);
        // Session weights scale every record of a session.
        let mut s = spec("deploy");
        s.weights = Some(Arc::new(HashMap::from([(1, 0.01)])));
        assert_ne!(ft.search(&s).unwrap()[0].sess, 1);
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
        let mut writer = ft.writer(15_000_000, 1, Merges::All).unwrap();
        ft.delete_from(&writer, 1, 1).unwrap();
        writer.commit().unwrap();
        ft.refresh_reader().unwrap();
        let mut hits = found(&ft, &spec("alpha"));
        hits.sort();
        assert_eq!(hits, vec![(1, 0), (2, 0)]);
    }

    fn segment_sizes(ft: &FullText) -> Vec<u32> {
        let mut sizes: Vec<u32> = ft
            .index
            .searchable_segment_metas()
            .unwrap()
            .iter()
            .map(SegmentMeta::max_doc)
            .collect();
        sizes.sort_unstable();
        sizes
    }

    /// One commit (one new segment) per call, the way a query's refresh adds records.
    fn commit_docs(ft: &FullText, merges: Merges, docs: u64) {
        let mut writer = ft.writer(15_000_000, 1, merges).unwrap();
        for seq in 0..docs {
            writer
                .add_document(ft.doc(1, seq as u32, 0, 1, "alpha", None))
                .unwrap();
        }
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
    }

    #[test]
    fn small_merges_leave_big_segments_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ft = FullText::open_or_create(dir.path()).unwrap();
        // Bigger than SMALL_SEGMENT_DOCS: never merged by a query.
        for _ in 0..SMALL_MERGE_COUNT {
            commit_docs(&ft, Merges::Small, SMALL_SEGMENT_DOCS as u64);
        }
        assert_eq!(
            segment_sizes(&ft),
            vec![SMALL_SEGMENT_DOCS; SMALL_MERGE_COUNT]
        );
        assert!(
            ft.merges_due().unwrap(),
            "the default policy would merge them"
        );
        // Small ones are merged once there are enough of them.
        for _ in 0..SMALL_MERGE_COUNT - 1 {
            commit_docs(&ft, Merges::Small, 3);
        }
        assert_eq!(segment_sizes(&ft).len(), 2 * SMALL_MERGE_COUNT - 1);
        commit_docs(&ft, Merges::Small, 3);
        let sizes = segment_sizes(&ft);
        assert_eq!(sizes[0], 3 * SMALL_MERGE_COUNT as u32);
        assert_eq!(sizes.len(), SMALL_MERGE_COUNT + 1);
        // A commit with nothing new still runs the merges that are due.
        commit_docs(&ft, Merges::All, 0);
        assert!(!ft.merges_due().unwrap());
        assert!(segment_sizes(&ft).len() < SMALL_MERGE_COUNT);
        let total: u32 = segment_sizes(&ft).iter().sum();
        assert_eq!(total, (SMALL_SEGMENT_DOCS + 3) * SMALL_MERGE_COUNT as u32);
    }

    #[test]
    fn query_terms_keep_quoted_phrases() {
        assert_eq!(
            query_terms(r#" deploy  "roll back" 回滚 "unterminated"#),
            vec!["deploy", "roll back", "回滚", "unterminated"]
        );
    }
}
