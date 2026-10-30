//! OpenCode (`~/.local/share/opencode/opencode.db`, tables `session`, `message`, `part`).
//! Format notes: docs/formats.md.
//!
//! The database is opened read-only and never checkpointed. Each run compares a cheap
//! per-session signature with the last one and re-reads changed sessions completely. A
//! record's locator is its part's position in the session (messages by time, parts by id),
//! and a tool part yields two records: the call (part 0) and its result (part 1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{FileParse, Item, Meta, Rec, Role, SessionUpdate, time_ms};

pub const SOURCE: &str = "opencode";
pub const VERSION: u32 = 1;

pub fn default_roots(home: &Path) -> Vec<PathBuf> {
    match std::env::var("OPENCODE_DATA_DIR") {
        Ok(dirs) if !dirs.trim().is_empty() => dirs
            .split(',')
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => vec![home.join(".local/share/opencode")],
    }
}

/// `<root>/opencode*.db` (not backups, not WAL/SHM side files).
pub fn discover(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("opencode")
            && name.ends_with(".db")
            && super::entry_type(&entry).is_some_and(|t| t.is_file())
        {
            out.push(entry.path());
        }
    }
}

/// The write-ahead log beside a database; it changes before the database file does.
pub fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Session id → signature of what was indexed.
    #[serde(default)]
    sessions: HashMap<String, String>,
}

fn open(db: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(1))?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

struct SessionRow {
    id: String,
    parent: Option<String>,
    title: String,
    directory: String,
    created: i64,
    updated: i64,
    archived: Option<i64>,
}

pub fn parse(db: &Path, state: Option<&str>) -> Result<FileParse> {
    let prev: State = state
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let conn = open(db)?;
    let mut rows = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, title, directory, time_created, time_updated, time_archived
             FROM session",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                parent: r.get(1)?,
                title: r.get(2)?,
                directory: r.get(3)?,
                created: r.get(4)?,
                updated: r.get(5)?,
                archived: r.get(6)?,
            })
        })?;
        for row in mapped {
            rows.push(row?);
        }
    }
    let stats = |table: &str| -> Result<HashMap<String, (i64, i64)>> {
        let mut stmt = conn.prepare(&format!(
            "SELECT session_id, COUNT(*), MAX(time_updated) FROM {table} GROUP BY session_id"
        ))?;
        let mapped = stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                (r.get(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0)),
            ))
        })?;
        Ok(mapped.collect::<rusqlite::Result<_>>()?)
    };
    let messages = stats("message")?;
    let parts = stats("part")?;

    let mut next = State::default();
    let mut sessions = Vec::new();
    let mut bad_lines = 0;
    for row in &rows {
        let m = messages.get(&row.id).copied().unwrap_or_default();
        let p = parts.get(&row.id).copied().unwrap_or_default();
        let signature = format!(
            "{}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}",
            row.title,
            row.directory,
            row.updated,
            row.parent.as_deref().unwrap_or(""),
            row.archived,
            m,
            p,
            VERSION
        );
        let changed = prev.sessions.get(&row.id) != Some(&signature);
        next.sessions.insert(row.id.clone(), signature);
        if !changed {
            continue;
        }
        let (update, bad) = read_session(&conn, row, m.1)?;
        bad_lines += bad;
        sessions.push(update);
    }
    let gone = prev
        .sessions
        .keys()
        .filter(|id| !next.sessions.contains_key(*id))
        .cloned()
        .collect();
    Ok(FileParse {
        sessions,
        gone,
        full: state.is_none(),
        state: serde_json::to_value(&next)?,
        end: 0,
        bad_lines,
    })
}

/// One part row with its message, in reading order.
struct PartRow {
    role: String,
    part: Value,
    ts: i64,
}

fn session_parts(conn: &Connection, session_id: &str) -> Result<(Vec<PartRow>, u32)> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.time_created, m.data, p.time_created, p.data
         FROM message m JOIN part p ON p.message_id = m.id
         WHERE m.session_id = ?1
         ORDER BY m.time_created, m.id, p.id",
    )?;
    let mut out = Vec::new();
    let mut bad = 0;
    let mut rows = stmt.query([session_id])?;
    while let Some(r) = rows.next()? {
        let message: String = r.get(1)?;
        let part: String = r.get(3)?;
        let (Ok(message), Ok(part)) = (
            serde_json::from_str::<Value>(&message),
            serde_json::from_str::<Value>(&part),
        ) else {
            bad += 1;
            continue;
        };
        out.push(PartRow {
            role: message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            ts: r
                .get::<_, Option<i64>>(2)?
                .or(r.get::<_, Option<i64>>(0)?)
                .unwrap_or(0),
            part,
        });
    }
    Ok((out, bad))
}

/// Records of one part. Depends on the part and its message only.
fn items(row: &PartRow) -> Vec<Item> {
    let p = &row.part;
    let ts = Some(row.ts);
    match p.get("type").and_then(Value::as_str).unwrap_or_default() {
        "text" => {
            let text = p.get("text").and_then(Value::as_str).unwrap_or_default();
            let flagged = |k: &str| p.get(k).and_then(Value::as_bool) == Some(true);
            if text.trim().is_empty() {
                return Vec::new();
            }
            match row.role.as_str() {
                "user" if !flagged("synthetic") && !flagged("ignored") => {
                    vec![Item::new(Role::User, text).at(ts)]
                }
                "assistant" => vec![Item::new(Role::Assistant, text).at(ts)],
                _ => Vec::new(),
            }
        }
        "tool" => {
            let name = p.get("tool").and_then(Value::as_str).map(str::to_string);
            let state = p.get("state").cloned().unwrap_or(Value::Null);
            let input = render_input(state.get("input").unwrap_or(&Value::Null));
            let mut out = vec![Item::new(Role::ToolUse, input).tool(name.clone()).at(ts)];
            let result = match state.get("status").and_then(Value::as_str) {
                Some("completed") => state
                    .get("output")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Some("error") => state
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|e| format!("Error: {e}")),
                _ => None,
            };
            if let Some(result) = result {
                let end = state
                    .pointer("/time/end")
                    .and_then(|v| time_ms(Some(v)))
                    .or(ts);
                out.push(Item::new(Role::ToolResult, result).tool(name).at(end));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// A tool's input: the shell command for command tools, else compact JSON.
fn render_input(input: &Value) -> String {
    if let Some(command) = input.get("command").and_then(Value::as_str) {
        return match input.get("description").and_then(Value::as_str) {
            Some(d) if !d.is_empty() => format!("# {d}\n{command}"),
            _ => command.to_string(),
        };
    }
    match input {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `New session - 2026-03-01T09:00:00.000Z` and the like: OpenCode's own placeholder.
fn placeholder_title(title: &str) -> bool {
    let rest = title
        .strip_prefix("New session - ")
        .or_else(|| title.strip_prefix("Child session - "));
    rest.is_some_and(|r| r.len() == 24 && r.ends_with('Z') && r.as_bytes()[10] == b'T')
}

fn read_session(
    conn: &Connection,
    row: &SessionRow,
    last_message: i64,
) -> Result<(SessionUpdate, u32)> {
    let (parts, bad) = session_parts(conn, &row.id)?;
    let mut meta = Meta {
        session_id: Some(row.id.clone()),
        cwd: Some(row.directory.clone()),
        kind: Some(
            if row.parent.is_some() {
                "subagent"
            } else {
                "main"
            }
            .into(),
        ),
        parent: row.parent.clone(),
        ..Meta::default()
    };
    if row.archived.is_some() {
        meta.extra.insert("archived".into(), Value::Bool(true));
    }
    meta.saw_time(row.created);
    meta.saw_time(row.updated.max(last_message));
    if !placeholder_title(&row.title) && !row.title.trim().is_empty() {
        meta.set_title(&row.title);
    }
    let mut records = Vec::new();
    let mut messages = 0;
    for (pos, part) in parts.iter().enumerate() {
        for (sub, item) in items(part).into_iter().enumerate() {
            if item.role == Role::User && meta.title.is_none() {
                meta.set_title(&item.text);
            }
            if item.role.is_message() {
                messages += 1;
            }
            let ts = item.ts_ms.unwrap_or(part.ts);
            meta.saw_time(ts);
            records.push(Rec {
                seq: records.len() as u32,
                pos: pos as u64,
                len: 0,
                part: sub as u16,
                ts,
                role: item.role,
                tool: item.tool,
                text: item.text,
            });
        }
    }
    let total = records.len() as u32;
    Ok((
        SessionUpdate {
            meta,
            from_seq: 0,
            records,
            total,
            messages,
        },
        bad,
    ))
}

/// Texts of records by `(pos, _, part)`, re-reading the session.
pub fn read_texts(
    db: &Path,
    session_id: &str,
    spans: &[(u64, u32, u16)],
) -> Result<Vec<Option<String>>> {
    let conn = open(db)?;
    let (parts, _) = session_parts(&conn, session_id)?;
    Ok(spans
        .iter()
        .map(|&(pos, _, sub)| {
            let row = parts.get(pos as usize)?;
            items(row).into_iter().nth(sub as usize).map(|i| i.text)
        })
        .collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build the fixture database at `path` from the SQL in tests/fixtures/opencode.
    pub fn build_fixture_db(path: &Path) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");
        let conn = Connection::open(path).unwrap();
        for file in ["schema.sql", "insert.sql"] {
            conn.execute_batch(&std::fs::read_to_string(dir.join(file)).unwrap())
                .unwrap();
        }
    }

    #[test]
    fn sessions_records_titles_and_incremental_changes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        build_fixture_db(&db);
        let first = parse(&db, None).unwrap();
        assert!(first.full);
        assert_eq!(first.sessions.len(), 3);
        let by_id = |id: &str| {
            first
                .sessions
                .iter()
                .find(|s| s.meta.session_id.as_deref() == Some(id))
        };
        let main = by_id("ses_8a1f0c2b3ffeQwErTyUiOpAsDf").unwrap();
        assert_eq!(main.meta.kind.as_deref(), Some("main"));
        assert_eq!(main.meta.cwd.as_deref(), Some("/Users/alice/work/blog"));
        assert!(
            main.records
                .iter()
                .any(|r| r.text == "在那之前回滚还来得及" && r.role == Role::Assistant)
        );
        let bash = main
            .records
            .iter()
            .find(|r| r.tool.as_deref() == Some("bash"))
            .unwrap();
        assert_eq!(bash.role, Role::ToolUse);
        assert!(
            main.records
                .iter()
                .any(|r| r.role == Role::ToolResult && r.text.starts_with("Error: "))
        );
        let child = by_id("ses_8a1e9d7c6ffeZxCvBnMaSdFgHj").unwrap();
        assert_eq!(child.meta.kind.as_deref(), Some("subagent"));
        assert_eq!(
            child.meta.parent.as_deref(),
            Some("ses_8a1f0c2b3ffeQwErTyUiOpAsDf")
        );
        let placeholder = by_id("ses_8a1e0b4a2ffeLkJhGfDsApOiUy").unwrap();
        assert_eq!(
            placeholder.meta.title.as_deref(),
            Some("根据这份笔记写本周的更新日志")
        );

        // Every record reads back from the database.
        for s in &first.sessions {
            let spans: Vec<_> = s.records.iter().map(|r| (r.pos, r.len, r.part)).collect();
            let texts = read_texts(&db, s.meta.session_id.as_deref().unwrap(), &spans).unwrap();
            for (r, t) in s.records.iter().zip(texts) {
                assert_eq!(t.as_deref(), Some(r.text.as_str()));
            }
        }

        // Nothing changed: nothing to re-read.
        let state = first.state.to_string();
        let again = parse(&db, Some(&state)).unwrap();
        assert!(again.sessions.is_empty() && again.gone.is_empty());

        // A new part in one session re-reads only that session; a deleted session is gone.
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
             SELECT 'prt_zzzz', message_id, session_id, time_created + 1, time_updated + 1,
                    '{\"type\":\"text\",\"text\":\"zebrafish\"}'
             FROM part WHERE session_id = 'ses_8a1f0c2b3ffeQwErTyUiOpAsDf'
             ORDER BY id DESC LIMIT 1;
             DELETE FROM part WHERE session_id = 'ses_8a1e9d7c6ffeZxCvBnMaSdFgHj';
             DELETE FROM message WHERE session_id = 'ses_8a1e9d7c6ffeZxCvBnMaSdFgHj';
             DELETE FROM session WHERE id = 'ses_8a1e9d7c6ffeZxCvBnMaSdFgHj';",
        )
        .unwrap();
        let changed = parse(&db, Some(&state)).unwrap();
        assert_eq!(changed.sessions.len(), 1);
        assert!(
            changed.sessions[0]
                .records
                .iter()
                .any(|r| r.text == "zebrafish")
        );
        assert_eq!(
            changed.gone,
            vec!["ses_8a1e9d7c6ffeZxCvBnMaSdFgHj".to_string()]
        );
        assert!(placeholder_title("New session - 2026-03-01T09:00:00.000Z"));
        assert!(!placeholder_title("New session - plans"));
    }
}
