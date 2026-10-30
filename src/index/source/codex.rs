//! Codex CLI rollouts (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, compressed to
//! `.jsonl.zst` after about a week; `archived_sessions/`). Format notes: docs/formats.md.
//!
//! Records come only from `response_item` lines; `event_msg` lines re-render the same
//! content. Spawned threads (subagent, guardian, fork) start with a copy of the parent's
//! history, which is skipped up to the thread's first own turn.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Item, JsonlFormat, Meta, Role, time_ms};

pub struct Codex;

const CALLS_KEPT: usize = 256;

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    meta_seen: bool,
    /// In a spawned thread's copied prefix.
    #[serde(default)]
    copied: bool,
    #[serde(default)]
    calls: Vec<(String, String)>,
    #[serde(default)]
    titled: bool,
    #[serde(default)]
    agent_path: Option<String>,
}

pub fn default_home(home: &Path) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
}

/// `rollout-*.jsonl[.zst]` under `sessions/` (any depth) and `archived_sessions/`. When both
/// forms of a name exist (mid-compression), the plain one wins.
pub fn discover(codex_home: &Path, out: &mut Vec<PathBuf>) {
    let mut found = walk(&codex_home.join("sessions"), 4);
    found.extend(walk(&codex_home.join("archived_sessions"), 0));
    let plain: std::collections::HashSet<PathBuf> = found
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .cloned()
        .collect();
    out.extend(found.into_iter().filter(|p| {
        !(p.extension().is_some_and(|e| e == "zst") && plain.contains(&p.with_extension("")))
    }));
}

/// Rollout files under `dir`, descending `depth` levels (the `YYYY/MM/DD` tree), in parallel.
fn walk(dir: &Path, depth: usize) -> Vec<PathBuf> {
    use rayon::prelude::*;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let Some(file_type) = super::entry_type(&entry) else {
            continue;
        };
        if file_type.is_dir() {
            if depth > 0 {
                dirs.push(entry.path());
            }
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_file()
            && name.starts_with("rollout-")
            && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
        {
            files.push(entry.path());
        }
    }
    let nested: Vec<Vec<PathBuf>> = dirs.par_iter().map(|d| walk(d, depth - 1)).collect();
    files.extend(nested.into_iter().flatten());
    files
}

/// `cd '<cwd>' && codex resume <id>`; spawned helper threads reopen their parent.
pub fn resume_command(
    session_id: &str,
    kind: Option<&str>,
    cwd: Option<&str>,
    parent: Option<&str>,
) -> Option<String> {
    let id = match kind {
        Some("subagent" | "guardian") => parent?,
        _ => session_id,
    };
    Some(match cwd {
        Some(cwd) => format!("cd {} && codex resume {id}", super::sh_quote(cwd)),
        None => format!("codex resume {id}"),
    })
}

impl JsonlFormat for Codex {
    const SOURCE: &'static str = "codex";
    const VERSION: u32 = 2;
    type State = State;

    /// The first UUID in `rollout-<time>-<uuid>[_<generation>].jsonl`.
    fn session_id_from_path(&self, path: &Path) -> Option<String> {
        let name = path.file_name()?.to_str()?;
        let stem = name.strip_prefix("rollout-")?;
        // `YYYY-MM-DDTHH-MM-SS-` is 20 characters.
        let id = stem.get(20..56)?;
        (id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
            .then(|| id.to_string())
    }

    fn skip_raw(&self, line: &[u8]) -> bool {
        // Event lines duplicate response items; most of them are skipped without parsing.
        // `task_started` marks where a spawned thread's own history begins, so keep it.
        let head = &line[..line.len().min(240)];
        memchr::memmem::find(head, b"\"type\":\"event_msg\"").is_some()
            && memchr::memmem::find(head, b"\"task_started\"").is_none()
    }

    fn line(&self, v: &Value, state: &mut State, meta: &mut Meta, out: &mut Vec<Item>) {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let empty = Value::Null;
        let p = v.get("payload").filter(|p| p.is_object()).unwrap_or(&empty);
        let ts = time_ms(v.get("timestamp"));
        if kind == "session_meta" {
            if !state.meta_seen {
                state.meta_seen = true;
                session_meta(p, ts, state, meta);
            }
            return;
        }
        if state.copied {
            let turn = match kind {
                "turn_context" => p.get("turn_id"),
                "event_msg" if p.get("type").and_then(Value::as_str) == Some("task_started") => {
                    p.get("turn_id")
                }
                _ => None,
            };
            let own = turn.and_then(Value::as_str).is_some_and(|turn| {
                is_uuid_v7(turn) && meta.session_id.as_deref().is_some_and(|sid| turn >= sid)
            });
            if !own {
                return;
            }
            state.copied = false;
        }
        if let Some(ts) = ts {
            meta.saw_time(ts);
        }
        match kind {
            "turn_context" => {
                if let Some(model) = p.get("model").and_then(Value::as_str) {
                    meta.extra.insert("model".into(), Value::from(model));
                }
            }
            "response_item" => {
                if let Some(item) = response_item(p, state) {
                    if item.role == Role::User
                        && !state.titled
                        && let Some(title) = title_from(&item.text)
                    {
                        state.titled = true;
                        meta.title = Some(title);
                    }
                    out.push(item.at(ts));
                }
            }
            _ => {}
        }
    }
}

fn session_meta(p: &Value, ts: Option<i64>, state: &mut State, meta: &mut Meta) {
    if let Some(id) = p.get("id").and_then(Value::as_str) {
        meta.session_id = Some(id.to_string());
    }
    if let Some(cwd) = p.get("cwd").and_then(Value::as_str) {
        meta.cwd = Some(cwd.to_string());
    }
    if let Some(started) = time_ms(p.get("timestamp")).or(ts) {
        meta.saw_time(started);
    }
    for (key, field) in [("originator", "originator"), ("cli_version", "cli_version")] {
        if let Some(value) = p.get(field).and_then(Value::as_str) {
            meta.extra.insert(key.into(), Value::from(value));
        }
    }
    if let Some(branch) = p.pointer("/git/branch").and_then(Value::as_str) {
        meta.extra.insert("git_branch".into(), Value::from(branch));
    }
    let (kind, parent) = classify(p);
    meta.kind = Some(kind.into());
    meta.parent = parent;
    state.copied = matches!(kind, "subagent" | "guardian" | "fork");
    state.agent_path = p
        .get("agent_path")
        .or_else(|| p.pointer("/source/subagent/thread_spawn/agent_path"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if matches!(kind, "subagent" | "guardian")
        && let Some(path) = &state.agent_path
    {
        // Until a user message names it better.
        meta.title = Some(path.clone());
    }
}

fn classify(p: &Value) -> (&'static str, Option<String>) {
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    let source = p.get("source");
    let thread_source = p.get("thread_source").and_then(Value::as_str);
    let sub = source.and_then(|s| s.get("subagent"));
    let parent = s(p.get("parent_thread_id"))
        .or_else(|| s(sub.and_then(|s| s.pointer("/thread_spawn/parent_thread_id"))))
        .or_else(|| s(p.get("forked_from_id")));
    if thread_source == Some("guardian_review")
        || sub.and_then(|s| s.get("other")).and_then(Value::as_str) == Some("guardian")
    {
        return ("guardian", parent);
    }
    if sub.is_some() || thread_source == Some("subagent") {
        return ("subagent", parent);
    }
    if p.get("forked_from_id").and_then(Value::as_str).is_some() {
        return ("fork", parent);
    }
    if source.and_then(Value::as_str) == Some("exec") {
        return ("exec", None);
    }
    ("main", None)
}

fn is_uuid_v7(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 15
        && b[..8].iter().all(u8::is_ascii_hexdigit)
        && b[8] == b'-'
        && b[9..13].iter().all(u8::is_ascii_hexdigit)
        && b[13] == b'-'
        && b[14] == b'7'
}

fn remember(state: &mut State, call_id: Option<&str>, name: &str) {
    if let Some(id) = call_id {
        state.calls.push((id.to_string(), name.to_string()));
        if state.calls.len() > CALLS_KEPT {
            state.calls.remove(0);
        }
    }
}

fn call_name(state: &State, call_id: Option<&str>) -> Option<String> {
    let id = call_id?;
    state
        .calls
        .iter()
        .rev()
        .find(|(c, _)| c == id)
        .map(|(_, n)| n.clone())
}

fn response_item(p: &Value, state: &mut State) -> Option<Item> {
    let call_id = p.get("call_id").and_then(Value::as_str);
    let item = match p.get("type").and_then(Value::as_str)? {
        "message" => match p.get("role").and_then(Value::as_str)? {
            "user" => Item::new(Role::User, user_text(p)?),
            "assistant" => {
                let text = without_citations(joined_text(p.get("content"))?);
                if text.is_empty() {
                    return None;
                }
                Item::new(Role::Assistant, text)
            }
            _ => return None,
        },
        "function_call" => {
            let name = tool_name(p);
            remember(state, call_id, &name);
            Item::new(Role::ToolUse, render_call(&name, p.get("arguments"))).tool(Some(name))
        }
        "custom_tool_call" => {
            let name = p
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            remember(state, call_id, &name);
            let input = match p.get("input") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            Item::new(Role::ToolUse, input).tool(Some(name))
        }
        "local_shell_call" => {
            remember(state, call_id, "local_shell");
            let command = p.pointer("/action/command").cloned().unwrap_or(Value::Null);
            Item::new(Role::ToolUse, shell_command(&command)).tool(Some("local_shell".into()))
        }
        "web_search_call" => {
            remember(state, call_id, "web_search");
            Item::new(Role::ToolUse, search_text(p)).tool(Some("web_search".into()))
        }
        "tool_search_call" => {
            remember(state, call_id, "tool_search");
            let text = match p.get("arguments") {
                Some(Value::Object(a)) => a
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            Item::new(Role::ToolUse, text).tool(Some("tool_search".into()))
        }
        "image_generation_call" => {
            remember(state, call_id, "image_generation");
            let prompt = p
                .get("revised_prompt")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Item::new(Role::ToolUse, prompt).tool(Some("image_generation".into()))
        }
        "function_call_output" | "custom_tool_call_output" => {
            Item::new(Role::ToolResult, render_output(p.get("output")))
                .tool(call_name(state, call_id))
        }
        "tool_search_output" => {
            let names: Vec<&str> = p
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|t| t.get("name").and_then(Value::as_str).unwrap_or("?"))
                .collect();
            Item::new(Role::ToolResult, names.join(", "))
                .tool(call_name(state, call_id).or(Some("tool_search".into())))
        }
        "agent_message" => Item::new(
            Role::ToolResult,
            joined_text(p.get("content")).unwrap_or_default(),
        )
        .tool(Some("agent_message".into())),
        _ => return None,
    };
    (!item.text.trim().is_empty()).then_some(item)
}

fn tool_name(p: &Value) -> String {
    let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
    match p.get("namespace").and_then(Value::as_str) {
        Some(ns) if ns.starts_with("mcp__") && !name.starts_with("mcp__") => {
            format!("{}__{name}", ns.trim_end_matches('_'))
        }
        _ => name.to_string(),
    }
}

/// Text parts joined by blank lines (assistant messages, agent messages).
fn joined_text(content: Option<&Value>) -> Option<String> {
    let text = match content? {
        Value::String(s) => s.trim().to_string(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n")
            .trim()
            .to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

const INJECTED: [&str; 26] = [
    "<environment_context>",
    "<codex_internal_context",
    "<user_instructions>",
    "# AGENTS.md instructions",
    "<INSTRUCTIONS>",
    "<recommended_plugins>",
    "<subagent_notification>",
    "<turn_aborted>",
    "<skill>",
    "<permissions instructions>",
    "<collaboration_mode>",
    "<apps_instructions>",
    "<skills_instructions>",
    "<plugins_instructions>",
    "<app-context>",
    "<multi_agent_mode>",
    "<multi_agent_role>",
    "<image_resize_notice>",
    "<model_switch>",
    "<personality_spec>",
    "<external_codex_apps_",
    "<codex_apps_",
    "<system_instruction>",
    "<system-instruction>",
    "<system-reminder>",
    "Warning: apply_patch was requested via",
];

/// A part that is only an image wrapper: `<image …>`, `</image>` or `[Image #N]`.
fn image_wrapper(text: &str) -> bool {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("[Image #").and_then(|r| r.strip_suffix(']')) {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    let inner = t
        .strip_prefix("</image")
        .or_else(|| t.strip_prefix("<image"));
    matches!(inner, Some(rest) if rest.ends_with('>')
        && !rest[..rest.len() - 1].contains('>')
        && rest.chars().next().is_some_and(|c| c == '>' || c.is_whitespace()))
}

fn injected(text: &str) -> bool {
    let t = text.trim_start();
    INJECTED.iter().any(|p| t.starts_with(p)) || image_wrapper(t)
}

/// The user's own words in a user message, without injected context parts.
fn user_text(p: &Value) -> Option<String> {
    let content = p.get("content")?;
    if let Value::String(s) = content {
        let s = s.trim();
        return (!injected(s) && !s.is_empty()).then(|| s.to_string());
    }
    let parts = content.as_array()?;
    let kinds = p
        .pointer("/internal_chat_message_metadata_passthrough/content_item_kinds")
        .and_then(Value::as_array)
        .filter(|k| k.len() == parts.len());
    let mut keep = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !matches!(
            part.get("type").and_then(Value::as_str),
            Some("input_text" | "text" | "output_text")
        ) {
            continue;
        }
        let real = match kinds {
            Some(kinds) => {
                kinds[i].as_str().is_some_and(|k| k.starts_with("user.")) && !image_wrapper(text)
            }
            None => !injected(text),
        };
        if real {
            let text = request_part(text);
            if !text.is_empty() {
                keep.push(text);
            }
        }
    }
    let text = keep.join("\n\n").trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The request in an IDE or desktop wrapper (`# Context from my IDE setup:`, `# Files
/// mentioned by the user:` … `## My request for Codex:` request); other text as is.
fn request_part(text: &str) -> &str {
    let t = text.trim();
    if !(t.starts_with("# Context from my IDE setup:")
        || t.starts_with("# Files mentioned by the user:"))
    {
        return t;
    }
    for marker in ["## My request for Codex:", "## My request:"] {
        if let Some(i) = t.rfind(marker) {
            return t[i + marker.len()..].trim();
        }
    }
    t
}

/// Assistant text without the memory citations Codex appends.
fn without_citations(text: String) -> String {
    const OPEN: &str = "<oai-mem-citation>";
    const CLOSE: &str = "</oai-mem-citation>";
    if !text.contains(OPEN) {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The title a user message stands for: the request part of IDE/desktop wrappers; heartbeat
/// and delegation messages do not count.
fn title_from(text: &str) -> Option<String> {
    let mut t = text;
    for marker in ["## My request for Codex:", "## My request:"] {
        if let Some(i) = t.rfind(marker) {
            t = &t[i + marker.len()..];
            break;
        }
    }
    let flat = crate::text::collapse_ws(t);
    if flat.is_empty() || flat.starts_with("<heartbeat>") || flat.starts_with("<codex_delegation>")
    {
        return None;
    }
    Some(crate::text::truncate_chars(&flat, 80).0)
}

fn shell_command(command: &Value) -> String {
    let Some(parts) = command.as_array() else {
        return command.as_str().unwrap_or_default().to_string();
    };
    let words: Vec<String> = parts
        .iter()
        .map(|w| w.as_str().map_or_else(|| w.to_string(), str::to_string))
        .collect();
    const SHELLS: [&str; 7] = [
        "bash",
        "zsh",
        "sh",
        "/bin/bash",
        "/bin/zsh",
        "/bin/sh",
        "fish",
    ];
    if words.len() == 3
        && SHELLS.contains(&words[0].as_str())
        && matches!(words[1].as_str(), "-lc" | "-c")
    {
        return words[2].clone();
    }
    words.join(" ")
}

/// A readable rendering of a function call's arguments (a JSON string).
fn render_call(name: &str, args: Option<&Value>) -> String {
    let raw = match args {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let parsed: Value = match args {
        Some(Value::String(s)) => match serde_json::from_str(s) {
            Ok(v) => v,
            Err(_) => return raw,
        },
        Some(other) => other.clone(),
        None => return raw,
    };
    let Some(a) = parsed.as_object() else {
        return raw;
    };
    let base = name.rsplit("__").next().unwrap_or(name);
    let str_of = |key: &str| a.get(key).and_then(Value::as_str);
    match base {
        "exec_command" if str_of("cmd").is_some() => {
            return str_of("cmd").unwrap_or_default().into();
        }
        "shell_command" if str_of("command").is_some() => {
            return str_of("command").unwrap_or_default().into();
        }
        "shell" | "container.exec" | "local_shell"
            if a.get("command").is_some_and(Value::is_array) =>
        {
            return shell_command(&a["command"]);
        }
        "write_stdin" => return str_of("chars").unwrap_or_default().into(),
        "update_plan" => {
            return a
                .get("plan")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|s| {
                    format!(
                        "[{}] {}",
                        s.get("status").and_then(Value::as_str).unwrap_or_default(),
                        s.get("step").and_then(Value::as_str).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        "js" if str_of("code").is_some() => return str_of("code").unwrap_or_default().into(),
        _ => {}
    }
    if let Some(message) = str_of("message") {
        return message.into();
    }
    if base == "view_image" {
        return str_of("path").unwrap_or_default().into();
    }
    parsed.to_string()
}

fn render_output(out: Option<&Value>) -> String {
    match out {
        Some(Value::String(s)) => {
            if s.trim_start().starts_with("{\"output\"")
                && let Ok(Value::Object(o)) = serde_json::from_str::<Value>(s)
                && let Some(inner) = o.get("output").and_then(Value::as_str)
            {
                return inner.to_string();
            }
            s.clone()
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|x| {
                if let Some(text) = x.get("text").and_then(Value::as_str) {
                    Some(text.to_string())
                } else if matches!(
                    x.get("type").and_then(Value::as_str),
                    Some("input_image" | "image")
                ) {
                    Some("[image]".to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(o)) => match o.get("content") {
            Some(Value::String(s)) => s.clone(),
            _ => Value::Object(o.clone()).to_string(),
        },
        _ => String::new(),
    }
}

fn search_text(p: &Value) -> String {
    let empty = Value::Null;
    let a = p.get("action").unwrap_or(&empty);
    for key in ["query", "url", "pattern"] {
        if let Some(v) = a.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            if key == "pattern"
                && let Some(url) = a.get("url").and_then(Value::as_str)
            {
                return format!("{v} ({url})");
            }
            return v.to_string();
        }
    }
    if let Some(queries) = a.get("queries").and_then(Value::as_array) {
        return queries
            .iter()
            .map(|q| q.as_str().map_or_else(|| q.to_string(), str::to_string))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    p.get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_and_citations_are_dropped() {
        assert_eq!(
            request_part(
                "# Context from my IDE setup:\n\n## Open tabs:\n- a.md\n\n## My request for Codex:\n改一下标题"
            ),
            "改一下标题"
        );
        assert_eq!(
            request_part(
                "# Files mentioned by the user:\n\n## x.png: /tmp/x.png\n\n## My request:\n打几分"
            ),
            "打几分"
        );
        assert_eq!(
            request_part("## My request for Codex: 原样"),
            "## My request for Codex: 原样"
        );
        assert_eq!(
            without_citations("完成了。\n<oai-mem-citation>\n<rollout_ids>\nx\n</rollout_ids>\n</oai-mem-citation>".into()),
            "完成了。"
        );
        assert!(injected(
            "<codex_internal_context source=\"goal\">\nContinue"
        ));
    }
    use crate::index::source::testutil::parse_and_check;

    fn home() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/home/.codex")
    }

    /// Sessions and records match the reference parser's output (empty tool texts aside).
    #[test]
    fn matches_the_reference_parser() {
        let text = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/codex.jsonl"),
        )
        .unwrap();
        let mut files: Vec<(Value, Vec<Value>)> = Vec::new();
        for line in text.lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            if let Some(session) = v.get("session") {
                files.push((session.clone(), Vec::new()));
            } else if let Some(record) = v.get("record") {
                files.last_mut().unwrap().1.push(record.clone());
            }
        }
        assert_eq!(files.len(), 3);
        for (session, records) in files {
            let rel = session["source_path"].as_str().unwrap();
            let rel = rel.split("sessions/").last().unwrap();
            let parsed = parse_and_check(&Codex, &home().join("sessions").join(rel));
            let meta = &parsed.state.meta;
            assert_eq!(
                meta.session_id.as_deref(),
                session["session_id"].as_str(),
                "{rel}"
            );
            assert_eq!(meta.kind.as_deref(), session["kind"].as_str(), "{rel}");
            assert_eq!(meta.parent.as_deref(), session["parent"].as_str(), "{rel}");
            assert_eq!(meta.cwd.as_deref(), session["cwd"].as_str(), "{rel}");
            // The reference cuts titles to 79 characters plus `…`; we keep 80.
            match session["title"].as_str() {
                Some(want) => {
                    let got = meta.title.as_deref().unwrap_or_default();
                    assert!(
                        got.starts_with(want.trim_end_matches('…')),
                        "{rel}: {got} vs {want}"
                    );
                }
                None => assert_eq!(meta.title, None, "{rel}"),
            }
            assert_eq!(
                parsed.state.messages,
                session["message_count"].as_u64().unwrap(),
                "{rel}"
            );
            let want: Vec<(&str, Option<&str>, &str)> = records
                .iter()
                .filter(|r| !r["text"].as_str().unwrap().trim().is_empty())
                .map(|r| {
                    (
                        r["role"].as_str().unwrap(),
                        r["tool_name"].as_str(),
                        r["text"].as_str().unwrap(),
                    )
                })
                .collect();
            let got: Vec<(&str, Option<&str>, &str)> = parsed
                .records
                .iter()
                .map(|r| (r.role.as_str(), r.tool.as_deref(), r.text.as_str()))
                .collect();
            assert_eq!(got, want, "{rel}");
        }
    }

    #[test]
    fn compressed_copy_has_the_same_records_at_the_same_offsets() {
        let plain = home().join("sessions/2026/03/14/rollout-2026-03-14T09-30-05-019cf1a2-7b3c-7d10-9e21-4a5b6c7d8e9f.jsonl");
        let packed = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "tests/fixtures/codex-compressed/rollout-2026-03-14T09-30-05-019cf1a2-7b3c-7d10-9e21-4a5b6c7d8e9f.jsonl.zst",
        );
        let a = parse_and_check(&Codex, &plain);
        let b = parse_and_check(&Codex, &packed);
        assert_eq!(a.records, b.records);
        assert!(
            a.records
                .iter()
                .any(|r| r.text.contains("在那之前回滚还来得及"))
        );
    }

    #[test]
    fn discovery_and_ids() {
        let mut found = Vec::new();
        discover(&home(), &mut found);
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(
            Codex
                .session_id_from_path(Path::new(
                    "rollout-2026-03-14T09-30-05-019cf1a2-7b3c-7d10-9e21-4a5b6c7d8e9f_019cf1a2-0000-7000-8000-000000000000.jsonl"
                ))
                .as_deref(),
            Some("019cf1a2-7b3c-7d10-9e21-4a5b6c7d8e9f")
        );
        assert!(image_wrapper("<image name=x>"));
        assert!(image_wrapper("[Image #2]"));
        assert!(!image_wrapper("<image> and more text"));
    }
}
