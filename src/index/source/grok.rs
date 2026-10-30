//! Grok Build (`~/.grok/sessions/<URL-encoded cwd>/<session id>/chat_history.jsonl`). The
//! session directory also holds `summary.json` (cwd, times, title, kind) and `updates.jsonl`
//! (the ACP update stream; read here only for when each prompt was sent). Format notes:
//! docs/formats.md.
//!
//! `chat_history.jsonl` is what the model saw: compaction and `/rewind` rewrite it, so the
//! history before a compaction is gone from it (the driver notices the rewrite and parses
//! the file again).

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Item, JsonlFormat, Meta, Role, str_at, text_blocks, time_ms};

pub struct Grok;

/// Tool-call ids kept to name results; results follow their calls closely.
const TOOL_NAMES_KEPT: usize = 256;

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// Bytes of `updates.jsonl` read so far.
    #[serde(default)]
    updates_pos: u64,
    /// When each prompt was sent, by `prompt_index`.
    #[serde(default)]
    prompts: BTreeMap<u32, i64>,
    /// When the history was last compacted.
    #[serde(default)]
    compacted_ms: Option<i64>,
    /// Time given to records: when the prompt of their turn was sent.
    #[serde(default)]
    turn_ms: Option<i64>,
    #[serde(default)]
    tools: Vec<(String, String)>,
    /// Grok's own title (`summary.json`).
    #[serde(default)]
    named: Option<String>,
    #[serde(default)]
    first_prompt: Option<String>,
}

/// The Grok home: `$GROK_HOME` or `~/.grok`.
pub fn default_home(home: &Path) -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".grok"))
}

/// `<home>/sessions/<encoded cwd>/<session id>/chat_history.jsonl`.
pub fn discover(grok_home: &Path, out: &mut Vec<PathBuf>) {
    let Ok(groups) = std::fs::read_dir(grok_home.join("sessions")) else {
        return;
    };
    for group in groups.flatten() {
        if !super::entry_type(&group).is_some_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(group.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            if !super::entry_type(&session).is_some_and(|t| t.is_dir()) {
                continue;
            }
            let path = session.path().join("chat_history.jsonl");
            if std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                out.push(path);
            }
        }
    }
}

/// `summary.json` next to a transcript. Grok often rewrites it after the transcript (title,
/// last active time), so a change to it counts as a change to the session.
pub fn summary_path(transcript: &Path) -> PathBuf {
    transcript.with_file_name("summary.json")
}

/// `cd '<cwd>' && grok --resume <id>` (`--resume` also takes titles, matched within the
/// current directory); a subagent reopens the session that started it.
pub fn resume_command(
    session_id: &str,
    kind: Option<&str>,
    cwd: Option<&str>,
    parent: Option<&str>,
) -> Option<String> {
    let id = if kind == Some("subagent") {
        parent?
    } else {
        session_id
    };
    Some(match cwd {
        Some(cwd) => format!("cd {} && grok --resume {id}", super::sh_quote(cwd)),
        None => format!("grok --resume {id}"),
    })
}

impl JsonlFormat for Grok {
    const SOURCE: &'static str = "grok";
    const VERSION: u32 = 1;
    type State = State;

    fn skip_raw(&self, line: &[u8]) -> bool {
        line.starts_with(br#"{"type":"reasoning""#) || line.starts_with(br#"{"type":"system""#)
    }

    /// The session directory's name.
    fn session_id_from_path(&self, path: &Path) -> Option<String> {
        let dir = path.parent()?.file_name()?.to_str()?;
        (!dir.is_empty()).then(|| dir.to_string())
    }

    fn begin(&self, path: &Path, state: &mut State, meta: &mut Meta) {
        let Some(dir) = path.parent() else { return };
        if let Some(summary) = std::fs::read(summary_path(path))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        {
            apply_summary(&summary, state, meta);
        }
        if meta.cwd.is_none() {
            meta.cwd = cwd_from_dir(dir);
        }
        if meta.kind.is_none() {
            meta.kind = Some("main".into());
        }
        read_updates(&dir.join("updates.jsonl"), state);
        if state.turn_ms.is_none() {
            state.turn_ms = meta.started_ms.or_else(|| {
                let modified = std::fs::metadata(path).ok()?.modified().ok()?;
                let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
                Some(since.as_millis() as i64)
            });
        }
        update_title(state, meta);
    }

    fn line(&self, v: &Value, state: &mut State, meta: &mut Meta, out: &mut Vec<Item>) {
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "user" => user(v, state, meta, out),
            "assistant" => assistant(v, state, out),
            "tool_result" => tool_result(v, state, out),
            "backend_tool_call" => backend_tool_call(v, state, out),
            // `system` (the system prompt), `reasoning` and anything new.
            _ => {}
        }
    }
}

fn apply_summary(s: &Value, state: &mut State, meta: &mut Meta) {
    if let Some(cwd) = str_at(s, &["info", "cwd"]).filter(|c| !c.is_empty()) {
        meta.cwd = Some(cwd.to_string());
    }
    for key in ["created_at", "updated_at", "last_active_at"] {
        if let Some(ts) = time_ms(s.get(key)) {
            meta.saw_time(ts);
        }
    }
    // `session_summary` is the title Grok shows (a `/rename` or the generated one).
    state.named = ["session_summary", "generated_title"]
        .iter()
        .filter_map(|k| s.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|t| !t.is_empty())
        .map(str::to_string)
        .or(state.named.take());
    let raw_kind = s
        .get("session_kind")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty());
    let parent = s
        .get("parent_session_id")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty());
    meta.parent = parent.map(str::to_string);
    let kind = match (raw_kind, parent) {
        (Some(k), _) if k.contains("subagent") => "subagent",
        (_, Some(_)) => "fork",
        // `grok -p`: a scripted single run, like `codex exec`.
        (Some("headless"), None) => "exec",
        _ => "main",
    };
    meta.kind = Some(kind.into());
    if let Some(raw) = raw_kind {
        meta.extra.insert("session_kind".into(), Value::from(raw));
    }
    for (key, field) in [("model", "current_model_id"), ("git_branch", "head_branch")] {
        if let Some(value) = s
            .get(field)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            meta.extra.insert(key.into(), Value::from(value));
        }
    }
}

/// The cwd a session directory stands for: the group's `.cwd` file (Grok writes one when
/// the encoded name would be too long), else the URL-decoded group name.
fn cwd_from_dir(session_dir: &Path) -> Option<String> {
    let group = session_dir.parent()?;
    if let Ok(text) = std::fs::read_to_string(group.join(".cwd")) {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    let name = group.file_name()?.to_str()?;
    name.starts_with("%2F").then(|| percent_decode(name))
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Prompt times (and the last compaction) from `updates.jsonl`, continuing where the last
/// run stopped. Only two kinds of update are parsed; the rest is skipped by its bytes.
fn read_updates(path: &Path, state: &mut State) {
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let len = file.metadata().map_or(0, |m| m.len());
    if len < state.updates_pos {
        state.updates_pos = 0;
        state.prompts.clear();
        state.compacted_ms = None;
    }
    let mut buf = Vec::new();
    if file.seek(SeekFrom::Start(state.updates_pos)).is_err() || file.read_to_end(&mut buf).is_err()
    {
        return;
    }
    // A last line without its newline may still be being written.
    let Some(end) = memchr::memrchr(b'\n', &buf) else {
        return;
    };
    let prompt = memchr::memmem::Finder::new(br#""user_message_chunk""#);
    let compaction = memchr::memmem::Finder::new(br#""compaction_checkpoint""#);
    for line in buf[..end].split(|&b| b == b'\n') {
        if prompt.find(line).is_none() && compaction.find(line).is_none() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(ts) = time_ms(v.pointer("/params/_meta/agentTimestampMs"))
            .or_else(|| time_ms(v.get("timestamp")))
        else {
            continue;
        };
        match v
            .pointer("/params/update/sessionUpdate")
            .and_then(Value::as_str)
        {
            Some("user_message_chunk") => {
                if let Some(index) = v
                    .pointer("/params/update/_meta/promptIndex")
                    .and_then(Value::as_u64)
                {
                    // A later prompt with the same index (after `/rewind`) replaces the old.
                    state.prompts.insert(index as u32, ts);
                }
            }
            Some("compaction_checkpoint") => state.compacted_ms = Some(ts),
            _ => {}
        }
    }
    state.updates_pos += end as u64 + 1;
}

fn update_title(state: &State, meta: &mut Meta) {
    meta.title = state
        .named
        .as_deref()
        .or(state.first_prompt.as_deref())
        .map(|t| crate::text::truncate_chars(&crate::text::collapse_ws(t), 80).0)
        .filter(|t| !t.is_empty());
}

fn user(v: &Value, state: &mut State, meta: &mut Meta, out: &mut Vec<Item>) {
    if let Some(index) = v.get("prompt_index").and_then(Value::as_u64)
        && let Some(&ts) = state.prompts.get(&(index as u32))
    {
        state.turn_ms = Some(ts);
    }
    let raw = v.get("content").map(text_blocks).unwrap_or_default();
    match v.get("synthetic_reason").and_then(Value::as_str) {
        None => {
            if let Some(text) = user_text(&raw) {
                if state.first_prompt.is_none() {
                    state.first_prompt = Some(text.clone());
                    update_title(state, meta);
                }
                out.push(Item::new(Role::User, text).at(state.turn_ms));
            }
        }
        // A compacted file starts with these: the re-sent `<user_info>` and the summary that
        // stands in for the history before the compaction, which is no longer in the file.
        Some("compaction_meta") => {
            if state.compacted_ms.is_some() {
                state.turn_ms = state.compacted_ms;
            }
            let text = raw.trim();
            if !text.is_empty() && !starts_with_tag(text) {
                out.push(Item::new(Role::Assistant, text).at(state.turn_ms));
            }
        }
        // `system_reminder`, `task_completed` and other injected text.
        Some(_) => {}
    }
}

/// The record text of a user message: the body of `<user_query>` (Grok wraps every prompt
/// in one, sometimes with an explanation or skill details around it); `None` for injected
/// context such as `<user_info>`.
pub fn user_text(raw: &str) -> Option<String> {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let s = raw.trim();
    if s.contains(OPEN) {
        let mut parts = Vec::new();
        let mut rest = s;
        while let Some(start) = rest.find(OPEN) {
            let body = &rest[start + OPEN.len()..];
            let (inner, after) = match body.find(CLOSE) {
                Some(end) => (&body[..end], &body[end + CLOSE.len()..]),
                None => (body, ""),
            };
            let inner = inner.trim();
            if !inner.is_empty() {
                parts.push(inner);
            }
            rest = after;
        }
        let text = parts.join("\n");
        return (!text.is_empty()).then_some(text);
    }
    (!s.is_empty() && !starts_with_tag(s)).then(|| s.to_string())
}

/// `<x…` followed by whitespace or `>`, where `x` is a lowercase letter.
fn starts_with_tag(s: &str) -> bool {
    let Some(rest) = s.trim_start().strip_prefix('<') else {
        return false;
    };
    let mut chars = rest.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return false;
    }
    for c in chars {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            continue;
        }
        return c.is_whitespace() || c == '>';
    }
    false
}

fn assistant(v: &Value, state: &mut State, out: &mut Vec<Item>) {
    let text = v.get("content").map(text_blocks).unwrap_or_default();
    if !text.trim().is_empty() {
        out.push(Item::new(Role::Assistant, text).at(state.turn_ms));
    }
    for call in v
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            state.tools.push((id.to_string(), name.to_string()));
            if state.tools.len() > TOOL_NAMES_KEPT {
                state.tools.remove(0);
            }
        }
        let args = match call.get("arguments") {
            Some(Value::String(s)) => {
                serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::from(s.as_str()))
            }
            Some(other) => other.clone(),
            None => Value::Null,
        };
        out.push(
            Item::new(Role::ToolUse, render_call(name, &args))
                .tool((!name.is_empty()).then(|| name.to_string()))
                .at(state.turn_ms),
        );
    }
}

fn tool_result(v: &Value, state: &State, out: &mut Vec<Item>) {
    let name = v
        .get("tool_call_id")
        .and_then(Value::as_str)
        .and_then(|id| state.tools.iter().rev().find(|(t, _)| t == id))
        .map(|(_, n)| n.clone());
    let mut text = v.get("content").map(text_blocks).unwrap_or_default();
    // Images arrive as base64 data URLs; only a marker is kept.
    let images = v
        .get("images")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    for _ in 0..images {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[image]");
    }
    if !text.trim().is_empty() {
        out.push(
            Item::new(Role::ToolResult, text)
                .tool(name)
                .at(state.turn_ms),
        );
    }
}

/// A tool the server ran itself (web search): the call, and the sources it returned.
fn backend_tool_call(v: &Value, state: &State, out: &mut Vec<Item>) {
    let Some(kind) = v.get("kind") else { return };
    let name = kind
        .get("tool_type")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or("backend_tool")
        .to_string();
    let action = kind.get("action").cloned().unwrap_or(Value::Null);
    let text = match action.get("query").and_then(Value::as_str) {
        Some(query) => query.to_string(),
        None => {
            let mut rest = action.clone();
            if let Some(obj) = rest.as_object_mut() {
                obj.remove("sources");
            }
            key_values(&rest)
        }
    };
    if !text.trim().is_empty() {
        out.push(
            Item::new(Role::ToolUse, text)
                .tool(Some(name.clone()))
                .at(state.turn_ms),
        );
    }
    let sources: Vec<&str> = action
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("url").and_then(Value::as_str))
        .collect();
    if !sources.is_empty() {
        out.push(
            Item::new(Role::ToolResult, sources.join("\n"))
                .tool(Some(name))
                .at(state.turn_ms),
        );
    }
}

/// A readable rendering of a tool call's arguments.
pub fn render_call(name: &str, args: &Value) -> String {
    let Some(obj) = args.as_object() else {
        return plain(args);
    };
    let s = |key: &str| obj.get(key).and_then(Value::as_str).unwrap_or_default();
    let has = |key: &str| !s(key).is_empty();
    let list = |key: &str| obj.get(key).and_then(Value::as_array).into_iter().flatten();
    match name {
        "run_terminal_command" | "monitor" if has("command") => {
            if has("description") {
                format!("# {}\n{}", s("description"), s("command"))
            } else {
                s("command").to_string()
            }
        }
        "read_file" if has("target_file") => {
            let mut out = s("target_file").to_string();
            for key in ["offset", "limit", "pages"] {
                if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
                    out.push_str(&format!(" {key}={}", plain(v)));
                }
            }
            out
        }
        "write" if has("file_path") => format!("{}\n{}", s("file_path"), s("content")),
        "search_replace" if has("file_path") => format!(
            "{}\n- {}\n+ {}",
            s("file_path"),
            s("old_string"),
            s("new_string")
        ),
        "grep" => ["pattern", "path", "glob"]
            .iter()
            .map(|k| s(k))
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        "list_dir" if has("target_directory") => s("target_directory").to_string(),
        "web_fetch" if has("url") => s("url").to_string(),
        "search_tool" if has("query") => s("query").to_string(),
        "spawn_subagent" => format!("{}\n{}", s("description"), s("prompt"))
            .trim()
            .to_string(),
        // An MCP tool called through the gateway: its name, then its arguments.
        "use_tool" if has("tool_name") => {
            let input = obj
                .get("tool_input")
                .map(key_values)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| s("file").to_string());
            format!("{}\n{input}", s("tool_name")).trim().to_string()
        }
        "todo_write" => list("todos")
            .map(|t| {
                let f = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or_default();
                format!("[{}] {}", f("status"), f("content"))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "ask_user_question" => {
            let mut lines = Vec::new();
            for q in list("questions") {
                lines.push(
                    q.get("question")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                );
                for o in q
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    lines.push(format!(
                        "- {}",
                        o.get("label").and_then(Value::as_str).unwrap_or_default()
                    ));
                }
            }
            lines.join("\n")
        }
        _ => key_values(args),
    }
}

/// One `key: value` line per field of an object; anything else as itself.
fn key_values(v: &Value) -> String {
    match v.as_object() {
        Some(obj) => obj
            .iter()
            .map(|(k, v)| format!("{k}: {}", plain(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        None => plain(v),
    }
}

/// Strings as themselves, null as nothing, anything else as compact JSON.
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::source::testutil::parse_and_check;
    use crate::index::source::{FileState, Rec, parse_jsonl};

    const WEATHER: &str = "%2FUsers%2Falice%2Fwork%2Fweather-cli";
    const GARDEN: &str = "%2FUsers%2Falice%2Fwork%2Fgarden-bot";
    const MAIN: &str = "01a2c3d4-0000-7000-8000-00000000e001";

    fn home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/home/.grok")
    }

    fn transcript(group: &str, id: &str) -> PathBuf {
        home()
            .join("sessions")
            .join(group)
            .join(id)
            .join("chat_history.jsonl")
    }

    fn view(records: &[Rec]) -> Vec<(Role, Option<&str>, &str, i64)> {
        records
            .iter()
            .map(|r| (r.role, r.tool.as_deref(), r.text.as_str(), r.ts))
            .collect()
    }

    #[test]
    fn main_session_records_title_and_times() {
        let parsed = parse_and_check(&Grok, &transcript(WEATHER, MAIN));
        let meta = &parsed.state.meta;
        assert_eq!(meta.session_id.as_deref(), Some(MAIN));
        assert_eq!(meta.cwd.as_deref(), Some("/Users/alice/work/weather-cli"));
        assert_eq!(meta.kind.as_deref(), Some("main"));
        assert_eq!(meta.parent, None);
        assert_eq!(meta.title.as_deref(), Some("Cache forecast responses"));
        assert_eq!(meta.started_ms, Some(1_772_697_600_000), "created_at");
        assert_eq!(meta.last_ms, Some(1_772_698_350_000), "last_active_at");
        assert_eq!(meta.extra["model"], "grok-build");
        assert_eq!(meta.extra["git_branch"], "main");
        assert_eq!(parsed.state.messages, 7, "3 prompts and 4 replies");

        let records = view(&parsed.records);
        assert_eq!(records.len(), 19, "{records:#?}");
        let (p0, p1, p2, p3) = (
            1_772_697_605_000,
            1_772_697_790_000,
            1_772_698_000_000,
            1_772_698_140_000,
        );
        assert_eq!(
            records[0],
            (Role::User, None, "天气接口太慢了，给预报结果加个缓存", p0)
        );
        assert_eq!(
            records[1],
            (
                Role::ToolUse,
                Some("run_terminal_command"),
                "# Find the forecast fetcher\nrg -n fetch_forecast src",
                p0
            )
        );
        assert_eq!(
            records[2].2,
            "/Users/alice/work/weather-cli/src/api.rs limit=80"
        );
        assert_eq!(records[3].1, Some("run_terminal_command"));
        assert_eq!(records[4].1, Some("read_file"));
        assert_eq!(
            records[5],
            (
                Role::ToolUse,
                Some("web_search"),
                "rust cached crate async ttl",
                p0
            )
        );
        assert_eq!(
            records[6],
            (
                Role::ToolResult,
                Some("web_search"),
                "https://docs.rs/cached\nhttps://example.com/blog/rust-caching",
                p0
            )
        );
        // The background task's notice is injected text, but it starts a new turn.
        assert_eq!(
            records[12],
            (
                Role::Assistant,
                None,
                "缓存加好了，`cargo test` 全部通过。",
                p1
            )
        );
        assert_eq!(records[13], (Role::User, None, "上线前要注意什么？", p2));
        assert_eq!(records[14].0, Role::Assistant);
        assert!(records[14].2.contains("在那之前回滚还来得及"));
        assert_eq!(
            records[15],
            (
                Role::User,
                None,
                "@/Users/alice/work/weather-cli/error.png 截图里的报错是什么",
                p3
            )
        );
        assert_eq!(
            records[17],
            (
                Role::ToolResult,
                Some("read_file"),
                "Read image file: /Users/alice/work/weather-cli/error.png\n[image]",
                p3
            )
        );
        for (role, tool, text, _) in &records {
            for injected in [
                "<user_info>",
                "<system-reminder>",
                "<user_query>",
                "You are Grok",
                "Find the forecast fetcher first",
                "iVBORw0KGgo",
            ] {
                assert!(!text.contains(injected), "{injected} in {text}");
            }
            if *role == Role::ToolResult {
                assert!(tool.is_some(), "{text}");
            }
        }
    }

    #[test]
    fn headless_run_is_exec_and_titled_by_its_prompt() {
        let id = "01a2c3d4-0000-7000-8000-00000000e002";
        let parsed = parse_and_check(&Grok, &transcript(WEATHER, id));
        let meta = &parsed.state.meta;
        assert_eq!(meta.kind.as_deref(), Some("exec"));
        assert_eq!(meta.extra["session_kind"], "headless");
        assert_eq!(
            meta.title.as_deref(),
            Some("Summarise CHANGELOG.md in one line")
        );
        assert_eq!(parsed.records.len(), 4);
        assert_eq!(
            resume_command(id, meta.kind.as_deref(), meta.cwd.as_deref(), None).unwrap(),
            format!("cd /Users/alice/work/weather-cli && grok --resume {id}")
        );
        assert_eq!(
            resume_command("child", Some("subagent"), None, Some("root")).unwrap(),
            "grok --resume root"
        );
    }

    #[test]
    fn compacted_session_keeps_the_summary_of_what_was_dropped() {
        let parsed = parse_and_check(
            &Grok,
            &transcript(GARDEN, "01a2c3d4-0000-7000-8000-00000000f001"),
        );
        let meta = &parsed.state.meta;
        assert_eq!(
            meta.title.as_deref(),
            Some("Balcony herb watering schedule")
        );
        assert_eq!(meta.started_ms, Some(1_772_805_600_000));
        let compacted = 1_772_810_400_000;
        let records = view(&parsed.records);
        assert_eq!(records.len(), 7, "{records:#?}");
        assert_eq!(records[0], (Role::User, None, "continue", compacted));
        assert_eq!(records[1].0, Role::Assistant);
        assert!(records[1].2.contains("soil-moisture drops below 30%"));
        assert!(records[..5].iter().all(|r| r.3 == compacted));
        assert_eq!(
            records[5],
            (Role::User, None, "下雨天跳过浇水", 1_772_810_700_000)
        );
        assert!(!records.iter().any(|r| r.2.contains("<rules>")));
    }

    #[test]
    fn session_without_prompts_has_no_records() {
        let parsed = parse_and_check(
            &Grok,
            &transcript(WEATHER, "01a2c3d4-0000-7000-8000-00000000e003"),
        );
        assert!(parsed.records.is_empty());
        assert_eq!(parsed.state.meta.title, None);
    }

    /// Parsing in two runs, the second continuing from the state the first left (as the
    /// index does while a session grows), gives what one run over the whole file gives.
    #[test]
    fn continues_where_the_last_run_stopped() {
        let src = transcript(WEATHER, MAIN).parent().unwrap().to_path_buf();
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("sessions/%2Fw").join(MAIN);
        std::fs::create_dir_all(&session).unwrap();
        std::fs::copy(src.join("summary.json"), session.join("summary.json")).unwrap();
        let chat = std::fs::read_to_string(src.join("chat_history.jsonl")).unwrap();
        let updates = std::fs::read_to_string(src.join("updates.jsonl")).unwrap();
        let head = |text: &str, n: usize| -> String {
            text.split_inclusive('\n').take(n).collect::<String>()
        };
        let path = session.join("chat_history.jsonl");
        // Up to and including the call of `call_a4`, whose result comes in the second run;
        // the updates stop before the later prompts.
        std::fs::write(&path, head(&chat, 12)).unwrap();
        std::fs::write(session.join("updates.jsonl"), head(&updates, 5)).unwrap();
        let first = parse_jsonl(&Grok, &path, 0, FileState::default()).unwrap();
        assert!(first.records.last().unwrap().text.contains("cargo test"));

        std::fs::write(&path, &chat).unwrap();
        std::fs::write(session.join("updates.jsonl"), &updates).unwrap();
        let saved: FileState =
            serde_json::from_str(&serde_json::to_string(&first.state).unwrap()).unwrap();
        let second = parse_jsonl(&Grok, &path, first.end, saved).unwrap();
        let whole = parse_jsonl(&Grok, &path, 0, FileState::default()).unwrap();
        let mut both = first.records.clone();
        both.extend(second.records.iter().cloned());
        assert_eq!(view(&both), view(&whole.records));
        assert_eq!(second.state.meta, whole.state.meta);
    }

    #[test]
    fn discovery_needs_a_transcript() {
        let mut found = Vec::new();
        discover(&home(), &mut found);
        found.sort();
        let ids: Vec<String> = found
            .iter()
            .map(|p| Grok.session_id_from_path(p).unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "01a2c3d4-0000-7000-8000-00000000f001",
                "01a2c3d4-0000-7000-8000-00000000e001",
                "01a2c3d4-0000-7000-8000-00000000e002",
                "01a2c3d4-0000-7000-8000-00000000e003",
            ]
        );
    }

    #[test]
    fn user_text_and_cwd() {
        assert_eq!(user_text("<user_info>\nOS: macos\n</user_info>"), None);
        assert_eq!(
            user_text(
                "<user_query>\n/deploy\n</user_query>\n<skill_information>\nx\n</skill_information>"
            )
            .as_deref(),
            Some("/deploy")
        );
        assert_eq!(user_text("  plain words ").as_deref(), Some("plain words"));
        assert_eq!(user_text("<user_query>\n\n</user_query>"), None);

        let dir = tempfile::tempdir().unwrap();
        let encoded = dir
            .path()
            .join("%2FUsers%2Falice%2F%E8%8A%B1%E5%9B%AD%20notes/s1");
        std::fs::create_dir_all(&encoded).unwrap();
        assert_eq!(
            cwd_from_dir(&encoded).as_deref(),
            Some("/Users/alice/花园 notes")
        );
        // A name too long to encode is a slug, with the path in `.cwd`.
        let slug = dir.path().join("very-long-path-3f2a/s2");
        std::fs::create_dir_all(&slug).unwrap();
        assert_eq!(cwd_from_dir(&slug), None);
        std::fs::write(
            dir.path().join("very-long-path-3f2a/.cwd"),
            "/Users/alice/x\n",
        )
        .unwrap();
        assert_eq!(cwd_from_dir(&slug).as_deref(), Some("/Users/alice/x"));
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn call_rendering() {
        use serde_json::json;
        assert_eq!(
            render_call(
                "search_replace",
                &json!({"file_path": "/a.rs", "old_string": "x", "new_string": "y"})
            ),
            "/a.rs\n- x\n+ y"
        );
        assert_eq!(
            render_call(
                "grep",
                &json!({"pattern": "TODO", "path": "src", "head_limit": 5})
            ),
            "TODO src"
        );
        assert_eq!(
            render_call(
                "use_tool",
                &json!({"tool_name": "browser__navigate", "tool_input": {"url": "https://example.com"}})
            ),
            "browser__navigate\nurl: https://example.com"
        );
        assert_eq!(
            render_call(
                "todo_write",
                &json!({"todos": [{"id": "1", "content": "write tests", "status": "in_progress"}]})
            ),
            "[in_progress] write tests"
        );
        assert_eq!(
            render_call(
                "image_gen",
                &json!({"prompt": "a fox", "aspect_ratio": "1:1"})
            ),
            "prompt: a fox\naspect_ratio: 1:1"
        );
        assert_eq!(render_call("x", &Value::from("raw text")), "raw text");
    }
}
