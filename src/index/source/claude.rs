//! Claude Code (`~/.claude/projects/<encoded cwd>/<session>.jsonl`, subagents under
//! `<session>/subagents/agent-<id>.jsonl`). Format notes: docs/formats.md.
//!
//! Forked sessions start with a verbatim copy of their origin (lines carrying the origin's
//! `sessionId`); fork subagents inherit their parent's context up to a `<fork-boilerplate>`
//! line. Only a file's own lines produce records.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Item, JsonlFormat, Meta, Role, time_ms};

pub struct Claude;

/// Tool-use ids kept to name results; results follow their calls closely.
const TOOL_NAMES_KEPT: usize = 256;

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// Set by `begin`; empty when re-reading a single line, which skips ownership checks.
    #[serde(default)]
    own_sid: String,
    #[serde(default)]
    subagent: bool,
    /// Fork subagent still in its inherited prefix.
    #[serde(default)]
    awaiting_fork: bool,
    #[serde(default)]
    kind_fixed: bool,
    #[serde(default)]
    tools: Vec<(String, String)>,
    #[serde(default)]
    titles: Titles,
}

#[derive(Default, Serialize, Deserialize)]
struct Titles {
    custom: Option<String>,
    description: Option<String>,
    first_prompt: Option<String>,
    ai: Option<String>,
    agent_name: Option<String>,
    summary: Option<String>,
    command: Option<String>,
}

impl Titles {
    /// The user's own naming first, then their first words, then generated names.
    fn best(&self) -> Option<&str> {
        [
            &self.custom,
            &self.description,
            &self.first_prompt,
            &self.ai,
            &self.agent_name,
            &self.summary,
            &self.command,
        ]
        .into_iter()
        .flatten()
        .map(|t| t.trim())
        .find(|t| !t.is_empty())
    }
}

pub fn default_root(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
        .join("projects")
}

/// `<root>/<proj>/<id>.jsonl` and `<root>/<proj>/<id>/subagents/agent-*.jsonl`.
pub fn discover(root: &Path, out: &mut Vec<PathBuf>) {
    use rayon::prelude::*;
    let Ok(projects) = std::fs::read_dir(root) else {
        return;
    };
    let projects: Vec<PathBuf> = projects
        .flatten()
        .filter(|p| super::entry_type(p).is_some_and(|t| t.is_dir()))
        .map(|p| p.path())
        .collect();
    let found: Vec<Vec<PathBuf>> = projects.par_iter().map(|p| project_files(p)).collect();
    out.extend(found.into_iter().flatten());
}

fn project_files(project: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(project) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_type) = super::entry_type(&entry) else {
            continue;
        };
        if file_type.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        } else if file_type.is_dir() {
            let Ok(agents) = std::fs::read_dir(path.join("subagents")) else {
                continue;
            };
            for agent in agents.flatten() {
                let name = agent.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("agent-")
                    && name.ends_with(".jsonl")
                    && super::entry_type(&agent).is_some_and(|t| t.is_file())
                {
                    out.push(agent.path());
                }
            }
        }
    }
    out
}

/// `cd '<cwd>' && claude --resume <id>`; a subagent reopens the session that started it.
pub fn resume_command(session_id: &str, cwd: Option<&str>, parent: Option<&str>) -> Option<String> {
    let id = if session_id.starts_with("agent-") {
        parent?
    } else {
        session_id
    };
    Some(match cwd {
        Some(cwd) => format!("cd {} && claude --resume {id}", super::sh_quote(cwd)),
        None => format!("claude --resume {id}"),
    })
}

impl JsonlFormat for Claude {
    const SOURCE: &'static str = "claude";
    const VERSION: u32 = 1;
    type State = State;

    fn begin(&self, path: &Path, state: &mut State, meta: &mut Meta) {
        if !state.own_sid.is_empty() {
            return;
        }
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".jsonl"))
            .unwrap_or_default();
        let parent_dir = path.parent();
        if parent_dir
            .and_then(Path::file_name)
            .is_some_and(|n| n == "subagents")
        {
            state.subagent = true;
            let sid = parent_dir
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            state.own_sid = sid.clone();
            meta.kind = Some("subagent".into());
            meta.parent = Some(sid);
            let sidecar = path.with_file_name(format!("{stem}.meta.json"));
            if let Ok(text) = std::fs::read_to_string(sidecar)
                && let Ok(v) = serde_json::from_str::<Value>(&text)
            {
                state.titles.description = v
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                state.awaiting_fork = v.get("isFork").and_then(Value::as_bool) == Some(true);
            }
        } else {
            state.own_sid = stem.to_string();
            meta.kind = Some("main".into());
        }
        update_title(state, meta);
    }

    fn line(&self, v: &Value, state: &mut State, meta: &mut Meta, out: &mut Vec<Item>) {
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let checking = !state.own_sid.is_empty();
        if checking {
            if kind == "fork-context-ref" && state.subagent {
                state.awaiting_fork = true;
                return;
            }
            let foreign = v
                .get("sessionId")
                .and_then(Value::as_str)
                .is_some_and(|sid| sid != state.own_sid);
            if foreign && !state.subagent {
                meta.extra
                    .entry("forked_from")
                    .or_insert_with(|| v["sessionId"].clone());
            }
            if state.awaiting_fork && kind == "user" && has_fork_boilerplate(v) {
                state.awaiting_fork = false;
            }
            if foreign || state.awaiting_fork {
                // Inherited history: learn tool names only, so results here can be named.
                remember_tools(v, state);
                return;
            }
        }
        if matches!(kind, "user" | "assistant" | "attachment" | "system") {
            observe(v, state, meta);
        }
        match kind {
            "custom-title" => set(&mut state.titles.custom, v.get("customTitle")),
            "ai-title" => set(&mut state.titles.ai, v.get("aiTitle")),
            "agent-name" => set(&mut state.titles.agent_name, v.get("agentName")),
            "summary" => set(&mut state.titles.summary, v.get("summary")),
            "assistant" => assistant(v, state, meta, out),
            "user" => user(v, state, out),
            "attachment" => queued_prompt(v, state, out),
            _ => return,
        }
        update_title(state, meta);
    }
}

fn set(slot: &mut Option<String>, value: Option<&Value>) {
    if let Some(text) = value.and_then(Value::as_str) {
        *slot = Some(text.to_string());
    }
}

fn update_title(state: &State, meta: &mut Meta) {
    meta.title = state
        .titles
        .best()
        .map(|t| crate::text::truncate_chars(&crate::text::collapse_ws(t), 80).0);
}

/// Session fields carried on conversation lines.
fn observe(v: &Value, state: &mut State, meta: &mut Meta) {
    if meta.cwd.is_none()
        && let Some(cwd) = v
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
    {
        meta.cwd = Some(cwd.to_string());
    }
    for (key, field) in [
        ("git_branch", "gitBranch"),
        ("version", "version"),
        ("entrypoint", "entrypoint"),
    ] {
        if let Some(value) = v
            .get(field)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            meta.extra.insert(key.into(), Value::from(value));
        }
    }
    if !state.kind_fixed && !state.subagent && v.get("parentUuid").is_some() {
        state.kind_fixed = true;
        if matches!(
            v.get("sessionKind").and_then(Value::as_str),
            Some("bg" | "daemon" | "daemon-worker")
        ) {
            meta.kind = Some("background".into());
        }
    }
}

fn remember_tools(v: &Value, state: &mut State) {
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    for block in content_blocks(v) {
        if block.get("type").and_then(Value::as_str) == Some("tool_use")
            && let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            )
        {
            state.tools.push((id.to_string(), name.to_string()));
            if state.tools.len() > TOOL_NAMES_KEPT {
                state.tools.remove(0);
            }
        }
    }
}

fn content_blocks(v: &Value) -> Vec<Value> {
    match v.pointer("/message/content") {
        Some(Value::String(s)) => vec![serde_json::json!({"type": "text", "text": s})],
        Some(Value::Array(blocks)) => blocks.clone(),
        _ => Vec::new(),
    }
}

fn assistant(v: &Value, state: &mut State, meta: &mut Meta, out: &mut Vec<Item>) {
    let model = v.pointer("/message/model").and_then(Value::as_str);
    if model == Some("<synthetic>")
        || v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
    {
        return;
    }
    if let Some(model) = model {
        meta.extra.insert("model".into(), Value::from(model));
    }
    let ts = time_ms(v.get("timestamp"));
    remember_tools(v, state);
    for block in content_blocks(v) {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    out.push(Item::new(Role::Assistant, text).at(ts));
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                out.push(
                    Item::new(Role::ToolUse, render_tool_input(name, &input))
                        .tool(Some(name.to_string()))
                        .at(ts),
                );
            }
            _ => {}
        }
    }
}

fn user(v: &Value, state: &mut State, out: &mut Vec<Item>) {
    if flag(v, "isCompactSummary") || flag(v, "isMeta") {
        return;
    }
    let ts = time_ms(v.get("timestamp"));
    let human = match v.pointer("/origin/kind").and_then(Value::as_str) {
        None | Some("human") => true,
        Some(_) => false,
    };
    let blocks = content_blocks(v);
    let has_result = blocks
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
    for block in &blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => {
                let id = block.get("tool_use_id").and_then(Value::as_str);
                let name = id.and_then(|id| {
                    state
                        .tools
                        .iter()
                        .rev()
                        .find(|(t, _)| t == id)
                        .map(|(_, n)| n.clone())
                });
                let text = render_tool_result(block);
                if !text.trim().is_empty() {
                    out.push(Item::new(Role::ToolResult, text).tool(name).at(ts));
                }
            }
            Some("text") if human => {
                let raw = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let record = user_text(raw);
                if state.titles.first_prompt.is_none() {
                    if state.subagent {
                        state.titles.first_prompt = record.clone();
                    } else if !has_result {
                        state.titles.first_prompt = title_candidate(raw, &mut state.titles.command);
                    }
                }
                if let Some(text) = record {
                    out.push(Item::new(Role::User, text).at(ts));
                }
            }
            _ => {}
        }
    }
}

/// A prompt typed while the agent was busy exists only as a `queued_command` attachment.
fn queued_prompt(v: &Value, state: &mut State, out: &mut Vec<Item>) {
    let Some(a) = v.get("attachment") else { return };
    if a.get("type").and_then(Value::as_str) != Some("queued_command")
        || flag(a, "isMeta")
        || !matches!(
            a.pointer("/origin/kind").and_then(Value::as_str),
            None | Some("human")
        )
        || a.get("commandMode")
            .and_then(Value::as_str)
            .unwrap_or("prompt")
            != "prompt"
    {
        return;
    }
    let raw = match a.get("prompt") {
        Some(p) => super::text_blocks(p),
        None => return,
    };
    if let Some(text) = user_text(&raw) {
        if state.titles.first_prompt.is_none() && !state.subagent {
            state.titles.first_prompt = title_candidate(&raw, &mut state.titles.command);
        }
        out.push(Item::new(Role::User, text).at(time_ms(v.get("timestamp"))));
    }
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

fn has_fork_boilerplate(v: &Value) -> bool {
    content_blocks(v).iter().any(|b| {
        b.get("type").and_then(Value::as_str) == Some("text")
            && b.get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| t.trim_start().starts_with("<fork-boilerplate>"))
    })
}

/// Body of the first `<tag>…</tag>`.
fn tag_body<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(&s[start..end])
}

/// Replace `<pasted_content id="…">\nbody\n</pasted_content id="…">` with its body.
fn unwrap_paste(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("<pasted_content") {
        let Some(open_end) = rest[open..].find('>').map(|i| open + i + 1) else {
            break;
        };
        let Some(close) = rest[open_end..]
            .find("</pasted_content")
            .map(|i| open_end + i)
        else {
            break;
        };
        let Some(close_end) = rest[close..].find('>').map(|i| close + i + 1) else {
            break;
        };
        out.push_str(&rest[..open]);
        let body = &rest[open_end..close];
        let body = body.strip_prefix('\n').unwrap_or(body);
        out.push_str(body.strip_suffix('\n').unwrap_or(body));
        rest = &rest[close_end..];
    }
    out.push_str(rest);
    out
}

/// The record text of one user text, or `None` for machine-generated text.
pub fn user_text(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with("<fork-boilerplate>") {
        let rest = s
            .find("</fork-boilerplate>")
            .map_or("", |i| &s[i + "</fork-boilerplate>".len()..])
            .trim();
        let rest = rest.strip_prefix("Your directive:").unwrap_or(rest).trim();
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    const MACHINE: [&str; 4] = [
        "<task-notification>",
        "<system-reminder>",
        "<local-command-caveat>",
        "[Request interrupted by user",
    ];
    if MACHINE.iter().any(|p| s.starts_with(p)) {
        return None;
    }
    let joined = |parts: [Option<&str>; 2]| {
        let text = parts
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    };
    if (s.starts_with("<command-name>") || s.starts_with("<command-message>"))
        && let Some(name) = tag_body(s, "command-name")
    {
        let args = tag_body(s, "command-args").unwrap_or_default();
        return Some(
            format!("{} {}", name.trim(), args.trim())
                .trim()
                .to_string(),
        );
    }
    if s.starts_with("<local-command-stdout>") || s.starts_with("<local-command-stderr>") {
        return joined([
            tag_body(s, "local-command-stdout"),
            tag_body(s, "local-command-stderr"),
        ]);
    }
    if s.starts_with("<bash-input>")
        && let Some(command) = tag_body(s, "bash-input")
    {
        return Some(format!("! {}", command.trim()));
    }
    if s.starts_with("<bash-stdout>") || s.starts_with("<bash-stderr>") {
        return joined([tag_body(s, "bash-stdout"), tag_body(s, "bash-stderr")]);
    }
    let text = unwrap_paste(s).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Claude Code's own first-prompt rule: skip tagged and interrupted messages, remember the
/// first slash command as a fallback, show `!` commands as such.
fn title_candidate(raw: &str, command: &mut Option<String>) -> Option<String> {
    let n = unwrap_paste(raw).replace('\n', " ");
    let n = n.trim();
    if n.is_empty() {
        return None;
    }
    if let Some(name) = tag_body(n, "command-name") {
        command.get_or_insert_with(|| name.trim().to_string());
        return None;
    }
    if let Some(input) = tag_body(n, "bash-input") {
        return Some(format!("! {}", input.trim()));
    }
    if starts_with_tag(n) || n.starts_with("[Request interrupted by user") {
        return None;
    }
    let text = crate::text::collapse_ws(&strip_image_refs(n));
    (!text.is_empty()).then_some(text)
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

/// Remove `[Image #N]` placeholders.
fn strip_image_refs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("[Image #") {
        let after = &rest[i + "[Image #".len()..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with(']') {
            out.push_str(&rest[..i]);
            rest = &after[digits + 1..];
        } else {
            out.push_str(&rest[..i + 1]);
            rest = &rest[i + 1..];
        }
    }
    out.push_str(rest);
    out
}

fn render_tool_result(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                Some("tool_reference") => Some(format!(
                    "[tool {}]",
                    b.get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                )),
                Some(t @ ("image" | "document")) => Some(format!("[{t}]")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A readable rendering of a tool call's input.
pub fn render_tool_input(name: &str, input: &Value) -> String {
    let Some(obj) = input.as_object() else {
        return match input {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        };
    };
    let s = |key: &str| obj.get(key).and_then(Value::as_str).unwrap_or_default();
    let has = |key: &str| {
        obj.get(key)
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty())
    };
    let base = if name.starts_with("mcp__") {
        name.rsplit("__").next().unwrap_or(name)
    } else {
        name
    };
    match base {
        "Bash" | "Monitor" if has("command") => {
            if has("description") {
                format!("# {}\n{}", s("description"), s("command"))
            } else {
                s("command").to_string()
            }
        }
        "Read" | "NotebookRead" if has("file_path") => {
            let mut out = s("file_path").to_string();
            for key in ["offset", "limit"] {
                if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
                    out.push_str(&format!(" {key}={}", plain(v)));
                }
            }
            out
        }
        "Write" if has("file_path") => format!("{}\n{}", s("file_path"), s("content")),
        "Edit" if has("file_path") => format!(
            "{}\n- {}\n+ {}",
            s("file_path"),
            s("old_string"),
            s("new_string")
        ),
        "MultiEdit" if has("file_path") => {
            let mut out = s("file_path").to_string();
            for edit in obj
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let e = |k: &str| edit.get(k).and_then(Value::as_str).unwrap_or_default();
                out.push_str(&format!("\n- {}\n+ {}", e("old_string"), e("new_string")));
            }
            out
        }
        "NotebookEdit" => format!("{}\n{}", s("notebook_path"), s("new_source")),
        "Glob" | "Grep" => ["pattern", "path", "glob"]
            .iter()
            .filter_map(|k| obj.get(*k).filter(|v| !v.is_null()).map(plain))
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        "WebSearch" | "ToolSearch" if has("query") => s("query").to_string(),
        "WebFetch" => format!("{}\n{}", s("url"), s("prompt")).trim().to_string(),
        "Agent" | "Task" => format!(
            "{} [{}]\n{}",
            s("description"),
            s("subagent_type"),
            s("prompt")
        )
        .trim()
        .to_string(),
        "TodoWrite" => obj
            .get("todos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|t| {
                let f = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or_default();
                format!("[{}] {}", f("status"), f("content"))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "Skill" => format!("{} {}", s("skill"), s("args")).trim().to_string(),
        "AskUserQuestion" => {
            let mut lines = Vec::new();
            for q in obj
                .get("questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
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
        "ExitPlanMode" if has("plan") => s("plan").to_string(),
        "SendMessage" => {
            let to = if has("to") { s("to") } else { s("recipient") };
            let body = if has("message") {
                s("message")
            } else {
                s("content")
            };
            format!("to {to}: {body}")
        }
        _ => obj
            .iter()
            .map(|(k, v)| format!("{k}: {}", plain(v)))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Strings as themselves, anything else as compact JSON.
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::source::testutil::parse_and_check;

    fn projects() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/home/.claude/projects")
    }

    #[test]
    fn main_session_records_and_title() {
        let path = projects()
            .join("-Users-alice-work-shop-api/5f0c6a1e-8b2d-4e7f-a913-2c4d6e8f0a1b.jsonl");
        let parsed = parse_and_check(&Claude, &path);
        let meta = &parsed.state.meta;
        assert_eq!(meta.kind.as_deref(), Some("main"));
        assert_eq!(meta.cwd.as_deref(), Some("/Users/alice/work/shop-api"));
        assert_eq!(parsed.records.len(), 19, "{:#?}", parsed.records);
        let rollback: Vec<_> = parsed
            .records
            .iter()
            .filter(|r| r.text.contains("在那之前回滚还来得及"))
            .collect();
        assert_eq!(
            rollback.len(),
            1,
            "the compaction summary copy is not indexed"
        );
        assert_eq!(rollback[0].role, Role::Assistant);
        for r in &parsed.records {
            assert!(!r.text.contains("<system-reminder>"), "{}", r.text);
            assert!(!r.text.contains("<synthetic>"));
        }
        assert!(
            parsed
                .records
                .iter()
                .all(|r| r.role != Role::ToolResult || r.tool.is_some())
        );
        // The trailing partial line is left for the next run.
        assert!(parsed.end < std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn fork_skips_copied_history_and_subagent_links_parent() {
        let fork = projects()
            .join("-Users-alice-work-notes-app/c3d2e1f0-a9b8-4c7d-9e6f-5a4b3c2d1e0f.jsonl");
        let parsed = parse_and_check(&Claude, &fork);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(
            parsed.state.meta.title.as_deref(),
            Some("Dark mode toggle (persist)")
        );
        assert_eq!(
            parsed
                .state
                .meta
                .extra
                .get("forked_from")
                .and_then(Value::as_str),
            Some("9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d")
        );

        let agent = projects().join(
            "-Users-alice-work-shop-api/5f0c6a1e-8b2d-4e7f-a913-2c4d6e8f0a1b/subagents/agent-a3f9c1d7e2b48a605.jsonl",
        );
        let parsed = parse_and_check(&Claude, &agent);
        let meta = &parsed.state.meta;
        assert_eq!(meta.kind.as_deref(), Some("subagent"));
        assert_eq!(meta.session_id.as_deref(), Some("agent-a3f9c1d7e2b48a605"));
        assert_eq!(
            meta.parent.as_deref(),
            Some("5f0c6a1e-8b2d-4e7f-a913-2c4d6e8f0a1b")
        );
        assert_eq!(meta.title.as_deref(), Some("Review retry logic"));
        assert_eq!(
            resume_command(
                "agent-a3f9c1d7e2b48a605",
                Some("/w/x"),
                meta.parent.as_deref()
            )
            .unwrap(),
            "cd /w/x && claude --resume 5f0c6a1e-8b2d-4e7f-a913-2c4d6e8f0a1b"
        );
    }

    /// Titles, record roles and texts match the reference parser's output.
    #[test]
    fn matches_the_reference_parser() {
        let expected: Value = serde_json::from_str(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/expected/claude.json"),
            )
            .unwrap(),
        )
        .unwrap();
        for session in expected.as_array().unwrap() {
            let rel = session["source_path"].as_str().unwrap();
            let parsed = parse_and_check(&Claude, &projects().join(rel));
            assert_eq!(
                parsed.state.meta.title.as_deref(),
                session["title"].as_str().map(|t| t.trim_end_matches('…')),
                "{rel}"
            );
            let want: Vec<(&str, &str)> = session["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (r["role"].as_str().unwrap(), r["text"].as_str().unwrap()))
                .collect();
            let got: Vec<(&str, &str)> = parsed
                .records
                .iter()
                .map(|r| (r.role.as_str(), r.text.as_str()))
                .collect();
            assert_eq!(got, want, "{rel}");
            assert_eq!(
                parsed.state.meta.cwd.as_deref(),
                session["cwd"].as_str(),
                "{rel}"
            );
        }
    }

    #[test]
    fn user_text_normalisation() {
        assert_eq!(user_text("<system-reminder>x</system-reminder>"), None);
        assert_eq!(
            user_text("<command-message>model</command-message>\n<command-name>/model</command-name>\n<command-args>opus</command-args>").as_deref(),
            Some("/model opus")
        );
        assert_eq!(
            user_text("<bash-input>ls -la</bash-input>").as_deref(),
            Some("! ls -la")
        );
        assert_eq!(
            user_text("<bash-stdout></bash-stdout><bash-stderr></bash-stderr>"),
            None
        );
        assert_eq!(
            user_text(
                "看这个 <pasted_content id=\"ab\">\nline\n</pasted_content id=\"ab\"> 怎么办"
            )
            .as_deref(),
            Some("看这个 line 怎么办")
        );
        assert_eq!(
            user_text("<fork-boilerplate>rules</fork-boilerplate>\n\nYour directive: check logs")
                .as_deref(),
            Some("check logs")
        );
        let mut command = None;
        assert_eq!(
            title_candidate("<command-name>/clear</command-name>", &mut command),
            None
        );
        assert_eq!(command.as_deref(), Some("/clear"));
        assert_eq!(
            title_candidate("[Image #1] 这个按钮 [Image #2]", &mut command).as_deref(),
            Some("这个按钮")
        );
    }

    #[test]
    fn tool_input_rendering() {
        let bash = serde_json::json!({"command": "make test", "description": "Run tests"});
        assert_eq!(render_tool_input("Bash", &bash), "# Run tests\nmake test");
        let read = serde_json::json!({"file_path": "/a.rs", "offset": 10});
        assert_eq!(render_tool_input("Read", &read), "/a.rs offset=10");
        let mcp = serde_json::json!({"url": "https://example.com", "n": 2});
        assert_eq!(
            render_tool_input("mcp__browser__open", &mcp),
            "url: https://example.com\nn: 2"
        );
    }
}
