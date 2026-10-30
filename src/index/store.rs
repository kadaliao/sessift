//! SQLite side of the index: transcript files (for incremental parsing), sessions, and the
//! location of every record in its transcript. Full text lives in Tantivy (`fulltext.rs`).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::model::Filters;

/// Bump when the tables change shape; the whole index is rebuilt.
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    inode INTEGER NOT NULL,
    parsed INTEGER NOT NULL,
    parser INTEGER NOT NULL,
    state TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL,
    source TEXT NOT NULL,
    session_id TEXT NOT NULL,
    path TEXT NOT NULL,
    cwd TEXT,
    project TEXT NOT NULL DEFAULT '',
    title TEXT,
    started_ms INTEGER,
    last_ms INTEGER NOT NULL,
    messages INTEGER NOT NULL DEFAULT 0,
    records INTEGER NOT NULL DEFAULT 0,
    kind TEXT,
    parent TEXT,
    extra TEXT
);
CREATE INDEX IF NOT EXISTS sessions_last ON sessions(last_ms DESC);
CREATE INDEX IF NOT EXISTS sessions_sid ON sessions(session_id);
CREATE INDEX IF NOT EXISTS sessions_file ON sessions(file_id);
CREATE TABLE IF NOT EXISTS records (
    sess INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    pos INTEGER NOT NULL,
    len INTEGER NOT NULL,
    part INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    role INTEGER NOT NULL,
    tool TEXT,
    chars INTEGER NOT NULL,
    PRIMARY KEY (sess, seq)
) WITHOUT ROWID;
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub source: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub inode: u64,
    /// Bytes consumed so far (of the decompressed stream for compressed files).
    pub parsed: u64,
    pub parser: u32,
    /// Parser state carried between incremental runs (JSON).
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionRow {
    /// Stable id derived from the session's identity (see `ingest::session_key`).
    pub id: i64,
    pub file_id: i64,
    pub source: String,
    pub session_id: String,
    pub path: String,
    pub cwd: Option<String>,
    pub project: String,
    pub title: Option<String>,
    pub started_ms: Option<i64>,
    pub last_ms: i64,
    /// User and assistant messages.
    pub messages: u64,
    pub records: u64,
    pub kind: Option<String>,
    pub parent: Option<String>,
    /// Source-specific extras (JSON), e.g. what the resume command needs.
    pub extra: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRow {
    pub seq: u32,
    /// Locator in the transcript: byte offset of the line for JSONL sources.
    pub pos: u64,
    pub len: u32,
    /// Index of the record among those its line yields.
    pub part: u16,
    pub ts: i64,
    pub role: u8,
    pub tool: Option<String>,
    /// Characters of the record's full text.
    pub chars: u64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed). Returns `None` when the file holds an older schema; the
    /// caller then rebuilds the index from scratch.
    pub fn open(path: &Path) -> Result<Option<Store>> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        let store = Store { conn };
        match store.meta("schema")? {
            None => store.set_meta("schema", &SCHEMA_VERSION.to_string())?,
            Some(v) if v == SCHEMA_VERSION.to_string() => {}
            Some(_) => return Ok(None),
        }
        Ok(Some(store))
    }

    /// Read-only handle for queries. Fails when the database does not exist yet.
    pub fn open_readonly(path: &Path) -> Result<Store> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Store { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback(&self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }

    // ----- files -----

    pub fn files(&self) -> Result<HashMap<String, FileRow>> {
        // Without the parser state: only files that changed need it (`file_state`).
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, source, size, mtime_ns, inode, parsed, parser, NULL FROM files",
        )?;
        let rows = stmt.query_map([], file_row)?;
        let mut out = HashMap::new();
        for row in rows {
            let row = row?;
            out.insert(row.path.clone(), row);
        }
        Ok(out)
    }

    pub fn file_state(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .prepare_cached("SELECT state FROM files WHERE id = ?1")?
            .query_row([id], |r| r.get(0))
            .optional()?
            .flatten())
    }

    /// Insert or update by path; returns the file id.
    pub fn put_file(&self, file: &FileRow) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, source, size, mtime_ns, inode, parsed, parser, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET source = excluded.source, size = excluded.size,
                mtime_ns = excluded.mtime_ns, inode = excluded.inode, parsed = excluded.parsed,
                parser = excluded.parser, state = excluded.state",
            params![
                file.path,
                file.source,
                file.size as i64,
                file.mtime_ns,
                file.inode as i64,
                file.parsed as i64,
                file.parser,
                file.state
            ],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM files WHERE path = ?1", [&file.path], |r| {
                r.get(0)
            })?)
    }

    /// Point a file row (and its sessions) at a new path, e.g. after compression.
    pub fn move_file(&self, id: i64, new: &FileRow) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET path = ?2, size = ?3, mtime_ns = ?4, inode = ?5 WHERE id = ?1",
            params![
                id,
                new.path,
                new.size as i64,
                new.mtime_ns,
                new.inode as i64
            ],
        )?;
        self.conn.execute(
            "UPDATE sessions SET path = ?2 WHERE file_id = ?1",
            params![id, new.path],
        )?;
        Ok(())
    }

    pub fn delete_file(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
        Ok(())
    }

    // ----- sessions -----

    pub fn put_session(&self, s: &SessionRow) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO sessions (id, file_id, source, session_id, path, cwd, project, title,
                    started_ms, last_ms, messages, records, kind, parent, extra)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(id) DO UPDATE SET file_id = excluded.file_id, source = excluded.source,
                    session_id = excluded.session_id, path = excluded.path, cwd = excluded.cwd,
                    project = excluded.project, title = excluded.title,
                    started_ms = excluded.started_ms, last_ms = excluded.last_ms,
                    messages = excluded.messages, records = excluded.records,
                    kind = excluded.kind, parent = excluded.parent, extra = excluded.extra",
            )?
            .execute(params![
                s.id,
                s.file_id,
                s.source,
                s.session_id,
                s.path,
                s.cwd,
                s.project,
                s.title,
                s.started_ms,
                s.last_ms,
                s.messages as i64,
                s.records as i64,
                s.kind,
                s.parent,
                s.extra
            ])?;
        Ok(())
    }

    pub fn session(&self, id: i64) -> Result<Option<SessionRow>> {
        Ok(self
            .conn
            .prepare_cached(&format!(
                "SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"
            ))?
            .query_row([id], session_row)
            .optional()?)
    }

    pub fn sessions_of_file(&self, file_id: i64) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE file_id = ?1"
        ))?;
        let rows = stmt.query_map([file_id], session_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Sessions with this id, optionally narrowed to one transcript path.
    pub fn sessions_by_sid(&self, sid: &str, path: Option<&str>) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE session_id = ?1
             AND (?2 IS NULL OR path = ?2) ORDER BY last_ms DESC"
        ))?;
        let rows = stmt.query_map(params![sid, path], session_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_session(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM records WHERE sess = ?1", [id])?;
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Sessions matching the filters, most recently active first. `since`/`until` apply to
    /// the last activity.
    pub fn list_sessions(
        &self,
        filters: &Filters,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SessionRow>> {
        let (clause, mut args) = session_where(filters, true);
        args.push(rusqlite::types::Value::Integer(limit as i64));
        args.push(rusqlite::types::Value::Integer(offset as i64));
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions {clause}
             ORDER BY last_ms DESC, id LIMIT ? OFFSET ?"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), session_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Ids of the sessions a search may return, or `None` when no session-level filter is set.
    pub fn session_ids(&self, filters: &Filters) -> Result<Option<Vec<i64>>> {
        let (clause, args) = session_where(filters, false);
        if clause.is_empty() {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare_cached(&format!("SELECT id FROM sessions {clause}"))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| r.get(0))?;
        Ok(Some(rows.collect::<rusqlite::Result<_>>()?))
    }

    pub fn counts_by_source(&self) -> Result<Vec<(String, u64, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, COUNT(*), COALESCE(SUM(records), 0) FROM sessions
             GROUP BY source ORDER BY source",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ----- records -----

    pub fn put_records(&self, sess: i64, records: &[RecordRow]) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR REPLACE INTO records (sess, seq, pos, len, part, ts, role, tool, chars)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for r in records {
            stmt.execute(params![
                sess,
                r.seq,
                r.pos as i64,
                r.len,
                r.part,
                r.ts,
                r.role,
                r.tool,
                r.chars as i64
            ])?;
        }
        Ok(())
    }

    /// Drop records from `seq` on (a re-parse is about to add them again).
    pub fn delete_records_from(&self, sess: i64, seq: u32) -> Result<()> {
        self.conn.execute(
            "DELETE FROM records WHERE sess = ?1 AND seq >= ?2",
            params![sess, seq],
        )?;
        Ok(())
    }

    pub fn records(&self, sess: i64, from: u32, limit: u32) -> Result<Vec<RecordRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT seq, pos, len, part, ts, role, tool, chars FROM records
             WHERE sess = ?1 AND seq >= ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![sess, from, limit], record_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn record(&self, sess: i64, seq: u32) -> Result<Option<RecordRow>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT seq, pos, len, part, ts, role, tool, chars FROM records
                 WHERE sess = ?1 AND seq = ?2",
            )?
            .query_row(params![sess, seq], record_row)
            .optional()?)
    }

    /// The record at a transcript location (what a record id names).
    pub fn record_at(&self, sess: i64, pos: u64, part: u16) -> Result<Option<RecordRow>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT seq, pos, len, part, ts, role, tool, chars FROM records
                 WHERE sess = ?1 AND pos = ?2 AND part = ?3",
            )?
            .query_row(params![sess, pos as i64, part], record_row)
            .optional()?)
    }
}

const SESSION_COLS: &str = "id, file_id, source, session_id, path, cwd, project, title, \
    started_ms, last_ms, messages, records, kind, parent, extra";

fn file_row(r: &rusqlite::Row) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: r.get(0)?,
        path: r.get(1)?,
        source: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        mtime_ns: r.get(4)?,
        inode: r.get::<_, i64>(5)? as u64,
        parsed: r.get::<_, i64>(6)? as u64,
        parser: r.get(7)?,
        state: r.get(8)?,
    })
}

fn session_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        file_id: r.get(1)?,
        source: r.get(2)?,
        session_id: r.get(3)?,
        path: r.get(4)?,
        cwd: r.get(5)?,
        project: r.get(6)?,
        title: r.get(7)?,
        started_ms: r.get(8)?,
        last_ms: r.get(9)?,
        messages: r.get::<_, i64>(10)? as u64,
        records: r.get::<_, i64>(11)? as u64,
        kind: r.get(12)?,
        parent: r.get(13)?,
        extra: r.get(14)?,
    })
}

fn record_row(r: &rusqlite::Row) -> rusqlite::Result<RecordRow> {
    Ok(RecordRow {
        seq: r.get(0)?,
        pos: r.get::<_, i64>(1)? as u64,
        len: r.get(2)?,
        part: r.get(3)?,
        ts: r.get(4)?,
        role: r.get(5)?,
        tool: r.get(6)?,
        chars: r.get::<_, i64>(7)? as u64,
    })
}

/// WHERE clause for session-level filters. `with_time` also applies since/until to the last
/// activity (for listing; searches filter record times instead).
fn session_where(filters: &Filters, with_time: bool) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    let mut parts: Vec<&str> = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    if let Some(source) = &filters.source {
        parts.push("source = ?");
        args.push(Value::Text(source.clone()));
    }
    if let Some(project) = &filters.project {
        parts.push("project = ?");
        args.push(Value::Text(project.clone()));
    }
    if let Some(cwd) = &filters.cwd {
        let cwd = cwd.trim_end_matches('/');
        parts.push("(cwd = ? OR substr(cwd, 1, ?) = ?)");
        let prefix = format!("{cwd}/");
        args.push(Value::Text(cwd.to_string()));
        args.push(Value::Integer(prefix.len() as i64));
        args.push(Value::Text(prefix));
    }
    if let Some(sid) = &filters.session_id {
        parts.push("session_id = ?");
        args.push(Value::Text(sid.clone()));
    }
    if with_time {
        if let Some(since) = filters.since_ms {
            parts.push("last_ms >= ?");
            args.push(Value::Integer(since));
        }
        if let Some(until) = filters.until_ms {
            parts.push("last_ms <= ?");
            args.push(Value::Integer(until));
        }
    }
    if parts.is_empty() {
        (String::new(), args)
    } else {
        (format!("WHERE {}", parts.join(" AND ")), args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: i64, sid: &str, last: i64, cwd: &str) -> SessionRow {
        SessionRow {
            id,
            file_id: 1,
            source: "claude".into(),
            session_id: sid.into(),
            path: format!("/t/{sid}.jsonl"),
            cwd: Some(cwd.into()),
            project: "shop".into(),
            last_ms: last,
            ..SessionRow::default()
        }
    }

    #[test]
    fn sessions_filter_and_page() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("x.sqlite")).unwrap().unwrap();
        store.put_session(&session(1, "a", 10, "/w/shop")).unwrap();
        store
            .put_session(&session(2, "b", 30, "/w/shop/api"))
            .unwrap();
        store
            .put_session(&session(3, "c", 20, "/w/shopping"))
            .unwrap();
        let ids = |rows: Vec<SessionRow>| rows.into_iter().map(|s| s.id).collect::<Vec<_>>();
        let all = Filters::default();
        assert_eq!(
            ids(store.list_sessions(&all, 10, 0).unwrap()),
            vec![2, 3, 1]
        );
        assert_eq!(ids(store.list_sessions(&all, 1, 1).unwrap()), vec![3]);
        let cwd = Filters {
            cwd: Some("/w/shop".into()),
            ..Filters::default()
        };
        assert_eq!(ids(store.list_sessions(&cwd, 10, 0).unwrap()), vec![2, 1]);
        let mut ranged = Filters {
            since_ms: Some(15),
            ..Filters::default()
        };
        assert_eq!(
            ids(store.list_sessions(&ranged, 10, 0).unwrap()),
            vec![2, 3]
        );
        // Searches filter record times, not session times.
        assert_eq!(store.session_ids(&ranged).unwrap(), None);
        ranged.session_id = Some("c".into());
        assert_eq!(store.session_ids(&ranged).unwrap(), Some(vec![3]));
    }

    #[test]
    fn records_round_trip_and_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("x.sqlite")).unwrap().unwrap();
        let rec = |seq: u32, pos: u64, part: u16| RecordRow {
            seq,
            pos,
            len: 10,
            part,
            ts: seq as i64,
            role: 0,
            tool: None,
            chars: 3,
        };
        store
            .put_records(
                7,
                &[rec(0, 0, 0), rec(1, 11, 0), rec(2, 11, 1), rec(3, 40, 0)],
            )
            .unwrap();
        assert_eq!(store.records(7, 1, 2).unwrap().len(), 2);
        assert_eq!(store.record_at(7, 11, 1).unwrap().unwrap().seq, 2);
        store.delete_records_from(7, 2).unwrap();
        assert_eq!(store.records(7, 0, 10).unwrap().len(), 2);
        assert!(store.record(7, 3).unwrap().is_none());
    }

    #[test]
    fn schema_mismatch_asks_for_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.sqlite");
        let store = Store::open(&path).unwrap().unwrap();
        store.set_meta("schema", "0").unwrap();
        drop(store);
        assert!(Store::open(&path).unwrap().is_none());
    }
}
