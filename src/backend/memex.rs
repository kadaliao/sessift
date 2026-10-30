//! Interim backend: run the installed memex CLI against the local index and translate its
//! JSONL output. Always passes `--machine local` so a remote memex never fans out on its own.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::model::{
    BackendInfo, ContextRequest, Filters, Page, ReadRequest, RecordView, SearchHit, SearchRequest,
    SessionSummary, SessionsRequest,
};
use crate::text;

const SEARCH_FIELDS: &str = "score,ts,record_id,project,role,session_id,source,source_path,snippet";
/// Characters of memex stderr kept in error messages.
const STDERR_TAIL: usize = 600;
/// Upper bound on sessions fetched from memex to satisfy an `until` filter.
const SESSIONS_FETCH_MAX: usize = 5000;

pub struct MemexBackend {
    bin: Option<PathBuf>,
    root: Option<String>,
    version: OnceCell<String>,
}

impl MemexBackend {
    pub fn new(configured: Option<&str>, root: Option<String>) -> Self {
        Self {
            bin: discover(configured),
            root,
            version: OnceCell::new(),
        }
    }

    pub fn binary(&self) -> Option<&Path> {
        self.bin.as_deref()
    }

    pub async fn info(&self) -> BackendInfo {
        let version = match (&self.bin, self.version.get()) {
            (None, _) => "not found".to_string(),
            (Some(_), Some(cached)) => cached.clone(),
            // Bounded and only cached on success: this runs inside the server handshake.
            (Some(_), None) => {
                let probe = self.run(vec!["--version".into()], false);
                match tokio::time::timeout(Duration::from_secs(3), probe).await {
                    Ok(Ok(out)) => {
                        let text = String::from_utf8_lossy(&out).trim().to_string();
                        // `memex 0.31.1` -> `0.31.1`; the kind is reported separately.
                        let version = text.strip_prefix("memex ").unwrap_or(&text).to_string();
                        let _ = self.version.set(version.clone());
                        version
                    }
                    Ok(Err(err)) => format!("unavailable: {err:#}"),
                    Err(_) => "unavailable: `memex --version` did not finish within 3s".into(),
                }
            }
        };
        BackendInfo {
            kind: "memex".into(),
            version,
        }
    }

    pub async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        let out = self
            .run(search_args(req, self.root.as_deref()), true)
            .await?;
        parse_search(&out)
    }

    pub async fn sessions(&self, req: &SessionsRequest) -> Result<Vec<SessionSummary>> {
        // memex has no `--until` for sessions, so newer sessions are dropped here; fetch more
        // until enough remain or memex runs out.
        let wanted = (req.offset + req.limit) as usize;
        let mut fetch = wanted;
        loop {
            let out = self
                .run(sessions_args(req, fetch, self.root.as_deref()), true)
                .await?;
            let mut sessions = parse_sessions(&out)?;
            let exhausted = sessions.len() < fetch;
            if let Some(until) = req.filters.until_ms {
                sessions.retain(|s| s.last_ms <= until);
            }
            if sessions.len() >= wanted
                || exhausted
                || fetch >= SESSIONS_FETCH_MAX
                || req.filters.until_ms.is_none()
            {
                sessions.sort_by(|a, b| b.last_ms.cmp(&a.last_ms));
                return Ok(sessions
                    .into_iter()
                    .skip(req.offset as usize)
                    .take(req.limit as usize)
                    .collect());
            }
            fetch = (fetch * 4).min(SESSIONS_FETCH_MAX);
        }
    }

    pub async fn context(&self, req: &ContextRequest) -> Result<Page> {
        let out = self
            .run(context_args(req, self.root.as_deref()), true)
            .await?;
        parse_context(&out, req)
    }

    pub async fn read(&self, req: &ReadRequest) -> Result<Page> {
        let out = self.run(read_args(req, self.root.as_deref()), true).await?;
        parse_page(&out, req)
    }

    async fn run(&self, args: Vec<String>, global_flags: bool) -> Result<Vec<u8>> {
        let bin = self.bin.as_ref().ok_or_else(|| {
            anyhow!("memex not found; install it or set backend.memex in ~/.sessift/config.toml")
        })?;
        let mut command = tokio::process::Command::new(bin);
        if global_flags {
            command.args(["--no-update-check", "--non-interactive"]);
        }
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Cancelling a request drops this future, which must stop memex too.
            .kill_on_drop(true);
        let output = command
            .output()
            .await
            .with_context(|| format!("run {}", bin.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let tail: String = {
                let chars: Vec<char> = stderr.chars().collect();
                chars[chars.len().saturating_sub(STDERR_TAIL)..]
                    .iter()
                    .collect()
            };
            bail!("memex exited with {}: {}", output.status, tail);
        }
        Ok(output.stdout)
    }
}

fn discover(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return Some(expand_home(path));
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("memex"))
                .collect()
        })
        .unwrap_or_default();
    // Non-interactive SSH sessions often lack Homebrew and Cargo on PATH.
    candidates.extend(
        ["/opt/homebrew/bin/memex", "/usr/local/bin/memex"]
            .into_iter()
            .map(PathBuf::from),
    );
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".cargo/bin/memex"));
    }
    candidates.into_iter().find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn push_root(args: &mut Vec<String>, root: Option<&str>) {
    if let Some(root) = root {
        args.push("--root".into());
        args.push(expand_home(root).to_string_lossy().into_owned());
    }
}

fn push_filters(args: &mut Vec<String>, filters: &Filters, search: bool) {
    let mut push = |flag: &str, value: &Option<String>| {
        if let Some(value) = value {
            args.push(flag.into());
            args.push(value.clone());
        }
    };
    push("--source", &filters.source);
    push("--project", &filters.project);
    push("--cwd", &filters.cwd);
    if search {
        push("--role", &filters.role);
        push("--session", &filters.session_id);
    } else {
        push("--session-id", &filters.session_id);
    }
    if let Some(since) = filters.since_ms {
        args.push("--since".into());
        args.push(since.to_string());
    }
    if search && let Some(until) = filters.until_ms {
        args.push("--until".into());
        args.push(until.to_string());
    }
}

pub fn search_args(req: &SearchRequest, root: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "search".into(),
        "--machine".into(),
        "local".into(),
        "--format".into(),
        "jsonl".into(),
        "--fields".into(),
        SEARCH_FIELDS.into(),
        "--limit".into(),
        req.limit.to_string(),
    ];
    push_root(&mut args, root);
    push_filters(&mut args, &req.filters, true);
    if req.unique_sessions {
        args.push("--unique-session".into());
    }
    args.push("--".into());
    args.push(req.query.clone());
    args
}

pub fn context_args(req: &ContextRequest, root: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "context".into(),
        "--machine".into(),
        "local".into(),
        "--format".into(),
        "json".into(),
        "--record-id".into(),
        req.record_id.clone(),
        "--before".into(),
        req.before.to_string(),
        "--after".into(),
        req.after.to_string(),
        "--max-chars".into(),
        req.max_chars.to_string(),
    ];
    push_root(&mut args, root);
    args
}

pub fn sessions_args(req: &SessionsRequest, fetch: usize, root: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "sessions".into(),
        "--machine".into(),
        "local".into(),
        "--format".into(),
        "jsonl".into(),
        "--limit".into(),
        fetch.to_string(),
    ];
    push_root(&mut args, root);
    push_filters(&mut args, &req.filters, false);
    args
}

pub fn read_args(req: &ReadRequest, root: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "session".into(),
        "--machine".into(),
        "local".into(),
        "--format".into(),
        "jsonl".into(),
        "--offset".into(),
        req.offset.to_string(),
        "--limit".into(),
        req.limit.to_string(),
        "--max-chars".into(),
        req.max_chars.to_string(),
    ];
    if let Some(path) = &req.source_path {
        args.push("--source-path".into());
        args.push(path.clone());
    }
    push_root(&mut args, root);
    args.push("--".into());
    args.push(req.session_id.clone());
    args
}

fn lines(out: &[u8]) -> impl Iterator<Item = Result<Value>> + '_ {
    out.split(|b| *b == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
        .map(|line| {
            serde_json::from_slice::<Value>(line).with_context(|| {
                format!("unexpected memex output: {}", crate::codec::preview(line))
            })
        })
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// memex prints RFC 3339 strings in most places and epoch milliseconds in raw records.
fn time_ms(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|ts| ts.as_millisecond()),
        _ => None,
    }
}

pub fn parse_search(out: &[u8]) -> Result<Vec<SearchHit>> {
    let mut hits = Vec::new();
    for line in lines(out) {
        let v = line?;
        // Memory documents have no session; conversations always do.
        let session_id = str_field(&v, "session_id");
        if session_id.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            score: v.get("score").and_then(Value::as_f64).unwrap_or(0.0) as f32,
            ts_ms: time_ms(v.get("ts")).unwrap_or(0),
            source: str_field(&v, "source"),
            session_id,
            source_path: str_field(&v, "source_path"),
            record_id: str_field(&v, "record_id"),
            project: str_field(&v, "project"),
            role: str_field(&v, "role"),
            snippet: text::collapse_ws(&str_field(&v, "snippet")),
        });
    }
    Ok(hits)
}

pub fn parse_sessions(out: &[u8]) -> Result<Vec<SessionSummary>> {
    let mut sessions = Vec::new();
    for line in lines(out) {
        let v = line?;
        let session_id = str_field(&v, "session_id");
        if session_id.is_empty() {
            continue;
        }
        let project = opt_str(&v, "repo_project").unwrap_or_else(|| str_field(&v, "project"));
        sessions.push(SessionSummary {
            source: str_field(&v, "source"),
            session_id,
            source_path: str_field(&v, "source_path"),
            project,
            cwd: opt_str(&v, "cwd"),
            title: opt_str(&v, "label").map(|t| text::collapse_ws(&t)),
            started_ms: time_ms(v.get("started_at")),
            last_ms: time_ms(v.get("last_at")).unwrap_or(0),
            message_count: v.get("message_count").and_then(Value::as_u64),
            kind: opt_str(&v, "conversation_kind"),
            resume_cmd: opt_str(&v, "resume_cmd"),
        });
    }
    Ok(sessions)
}

pub fn parse_page(out: &[u8], req: &ReadRequest) -> Result<Page> {
    let mut page = Page {
        session_id: req.session_id.clone(),
        source_path: req.source_path.clone().unwrap_or_default(),
        offset: req.offset,
        total: None,
        next_offset: None,
        records: Vec::new(),
    };
    for line in lines(out) {
        let v = line?;
        if v.get("type").and_then(Value::as_str) == Some("page") {
            page.total = v.get("total").and_then(Value::as_u64).map(|n| n as u32);
            page.next_offset = v
                .get("next_offset")
                .and_then(Value::as_u64)
                .map(|n| n as u32);
            if let Some(path) = opt_str(&v, "source_path") {
                page.source_path = path;
            }
            continue;
        }
        let Some(record) = record_view(&v) else {
            continue;
        };
        page.records.push(record);
    }
    Ok(page)
}

/// One `{record_id, record, content}` entry from `memex session` or `memex context`.
fn record_view(v: &Value) -> Option<RecordView> {
    {
        let record = v.get("record")?;
        // Tool calls carry their input in `text` (duplicated in `tool_input`); tool results
        // carry their output in `text`. Fall back to the dedicated fields when `text` is empty.
        let body = [
            opt_str(record, "text"),
            opt_str(record, "tool_input"),
            opt_str(record, "tool_output"),
        ]
        .into_iter()
        .flatten()
        .next()
        .unwrap_or_default();
        let content = v.get("content");
        let truncated = content
            .and_then(|c| c.get("truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let total_chars = if truncated {
            content
                .and_then(|c| c.get("continuations"))
                .and_then(Value::as_array)
                .and_then(|items| {
                    items
                        .iter()
                        .find(|c| c.get("field").and_then(Value::as_str) == Some("text"))
                })
                .and_then(|c| c.get("total_chars"))
                .and_then(Value::as_u64)
                .or_else(|| {
                    content
                        .and_then(|c| c.get("total_chars"))
                        .and_then(Value::as_u64)
                })
                .unwrap_or(text::char_len(&body) as u64)
        } else {
            text::char_len(&body) as u64
        };
        Some(RecordView {
            record_id: str_field(v, "record_id"),
            ts_ms: time_ms(record.get("ts")).unwrap_or(0),
            role: str_field(record, "role"),
            tool_name: opt_str(record, "tool_name"),
            text: body,
            total_chars,
            truncated,
        })
    }
}

/// `memex context --format json`: the anchor first, then its neighbours. Reordered here into
/// reading order using each record's relation and distance to the anchor.
pub fn parse_context(out: &[u8], req: &ContextRequest) -> Result<Page> {
    let v: Value = serde_json::from_slice(out).with_context(|| {
        format!(
            "unexpected memex context output: {}",
            crate::codec::preview(out)
        )
    })?;
    let mut entries: Vec<(i64, RecordView, String, String)> = Vec::new();
    for entry in v
        .get("records")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let distance = entry.get("distance").and_then(Value::as_i64).unwrap_or(0);
        // memex reports `before` distances as negative already; force the sign either way.
        let position = match entry.get("relation").and_then(Value::as_str) {
            Some("before") => -distance.abs(),
            Some("anchor") => 0,
            _ => distance.abs(),
        };
        let session = entry
            .get("record")
            .map(|r| str_field(r, "session_id"))
            .unwrap_or_default();
        let path = entry
            .get("record")
            .map(|r| str_field(r, "source_path"))
            .unwrap_or_default();
        if let Some(record) = record_view(entry) {
            entries.push((position, record, session, path));
        }
    }
    entries.sort_by_key(|(position, ..)| *position);
    let session_id = entries
        .iter()
        .map(|(_, _, s, _)| s.clone())
        .find(|s| !s.is_empty())
        .or_else(|| req.session_id.clone())
        .unwrap_or_default();
    let source_path = entries
        .iter()
        .map(|(_, _, _, p)| p.clone())
        .find(|p| !p.is_empty())
        .unwrap_or_default();
    Ok(Page {
        session_id,
        source_path,
        offset: 0,
        total: v.get("total").and_then(Value::as_u64).map(|n| n as u32),
        next_offset: None,
        records: entries.into_iter().map(|(_, r, ..)| r).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_args_keep_query_after_separator() {
        let mut req = SearchRequest::new("-rf 部署");
        req.limit = 7;
        req.filters.source = Some("claude".into());
        req.filters.since_ms = Some(1000);
        let args = search_args(&req, None);
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[sep + 1], "-rf 部署");
        assert_eq!(args.len(), sep + 2);
        let joined = args.join(" ");
        assert!(joined.contains("--machine local"));
        assert!(joined.contains("--limit 7"));
        assert!(joined.contains("--source claude"));
        assert!(joined.contains("--since 1000"));
    }

    #[test]
    fn sessions_args_request_offset_plus_limit() {
        let req = SessionsRequest {
            limit: 20,
            offset: 40,
            filters: Filters {
                session_id: Some("abc".into()),
                ..Filters::default()
            },
            brief: false,
        };
        let joined = sessions_args(&req, 60, Some("/tmp/m")).join(" ");
        assert!(joined.contains("--limit 60"));
        assert!(joined.contains("--session-id abc"));
        assert!(joined.contains("--root /tmp/m"));
    }

    #[test]
    fn parses_real_search_output() {
        let out = br#"{"score":28.03,"machine":"local","ts":"2026-07-30T07:40:15Z","doc_id":1,"record_id":"rid1_a","project":"dash","role":"assistant","session_id":"019f","source":"omp","source_path":"/x.jsonl","snippet":"line one\n\nline   two"}
{"kind":"memory","score":3.0,"title":"note"}
"#;
        let hits = parse_search(out).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].ts_ms, 1_785_397_215_000);
        assert_eq!(hits[0].snippet, "line one line two");
        assert_eq!(hits[0].source, "omp");
    }

    #[test]
    fn parses_real_sessions_output() {
        let out = br#"{"source":"claude","session_id":"2f37","source_path":"/p.jsonl","project":"someone-workspace-sessift","cwd":"/w/sessift","started_at":"2026-10-30T07:18:36Z","last_at":"2026-10-30T07:51:07Z","message_count":190,"label":"nicosuave/memex \u9879\u76ee","conversation_kind":"main","machine":"local","resume_cmd":"claude --resume 2f37"}
{"source":"codex","session_id":"019f","project":"flat","repo_project":"myrepo","last_at":"2026-10-29T00:00:00Z"}
"#;
        let sessions = parse_sessions(out).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].title.as_deref(), Some("nicosuave/memex 项目"));
        assert_eq!(sessions[0].message_count, Some(190));
        assert_eq!(sessions[0].kind.as_deref(), Some("main"));
        assert_eq!(sessions[1].project, "myrepo");
        assert!(sessions[0].last_ms > sessions[1].last_ms);
    }

    #[test]
    fn parses_real_page_output() {
        let out = br#"{"record_id":"rid1_u","record":{"source":"claude","ts":1793344716325,"role":"user","text":"\u4f60\u597d"},"content":{"returned_chars":2,"total_chars":2,"truncated":false,"continuations":[]},"machine":"local"}
{"record_id":"rid1_t","record":{"ts":1793344724839,"role":"tool_use","text":"{\"command\":\"ls\"}","tool_name":"Bash","tool_input":"{\"command\":\"ls\"}"},"content":{"returned_chars":30,"total_chars":30,"truncated":false,"continuations":[]}}
{"record_id":"rid1_r","record":{"ts":1793344724900,"role":"tool_result","text":"Cloning","tool_name":"Bash","tool_output":""},"content":{"returned_chars":7,"total_chars":8298,"truncated":true,"continuations":[{"field":"text","offset_chars":7,"total_chars":8298}]}}
{"type":"page","machine":"local","session_id":"2f37","source_path":"","offset":0,"total":219,"next_offset":3}
"#;
        let page = parse_page(out, &ReadRequest::new("2f37")).unwrap();
        assert_eq!(page.total, Some(219));
        assert_eq!(page.next_offset, Some(3));
        assert_eq!(page.records.len(), 3);
        assert_eq!(page.records[0].text, "你好");
        assert_eq!(page.records[1].tool_name.as_deref(), Some("Bash"));
        assert_eq!(page.records[1].total_chars, 16);
        assert!(page.records[2].truncated);
        assert_eq!(page.records[2].total_chars, 8298);
    }

    #[test]
    fn context_is_put_in_reading_order() {
        let out = br#"{"anchor_record_id":"a","order":"anchor_first","records":[
            {"record_id":"a","relation":"anchor","distance":0,"record":{"ts":3,"role":"assistant","text":"anchor","session_id":"s","source_path":"/s.jsonl"},"content":{"truncated":false}},
            {"record_id":"b3","relation":"before","distance":-3,"record":{"ts":1,"role":"user","text":"first"},"content":{"truncated":false}},
            {"record_id":"f1","relation":"after","distance":1,"record":{"ts":4,"role":"user","text":"next"},"content":{"truncated":false}},
            {"record_id":"b1","relation":"before","distance":-1,"record":{"ts":2,"role":"tool_result","text":"second"},"content":{"truncated":false}}
        ],"total":4}"#;
        let page = parse_context(out, &ContextRequest::new("a")).unwrap();
        let ids: Vec<&str> = page.records.iter().map(|r| r.record_id.as_str()).collect();
        assert_eq!(ids, vec!["b3", "b1", "a", "f1"]);
        assert_eq!(page.session_id, "s");
        assert_eq!(page.source_path, "/s.jsonl");
    }

    #[test]
    fn missing_session_is_an_empty_page() {
        let out = br#"{"type":"page","machine":"local","session_id":"nope","source_path":"","offset":0,"total":0,"next_offset":null}"#;
        let page = parse_page(out, &ReadRequest::new("nope")).unwrap();
        assert_eq!(page.total, Some(0));
        assert!(page.records.is_empty());
        assert_eq!(page.next_offset, None);
    }
}
