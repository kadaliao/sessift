//! Pi (`~/.pi/agent/sessions/<encoded cwd>/<time>_<id>.jsonl`). One session per file; the
//! first line is a `session` header. Format notes: docs/formats.md.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Item, JsonlFormat, Meta, Role, str_at, text_blocks, time_ms};

pub struct Pi;

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    /// File-name id (after the `_`); differs from the header id for subagent files.
    #[serde(default)]
    stem_id: String,
    /// Fork files copy their parent's entries; entries older than the header are skipped.
    #[serde(default)]
    fork_since_ms: Option<i64>,
    /// The user named the session (`/name`); that beats the first message.
    #[serde(default)]
    named: bool,
}

/// `<root>/<encoded cwd>/*.jsonl`, exactly two levels deep.
pub fn discover(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(dirs) = std::fs::read_dir(root) else {
        return;
    };
    for dir in dirs.flatten() {
        if !super::entry_type(&dir).is_some_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && super::entry_type(&file).is_some_and(|t| t.is_file())
            {
                out.push(path);
            }
        }
    }
}

pub fn default_root(home: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let agent = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/agent"));
    agent.join("sessions")
}

/// The part of `<time>_<id>.jsonl` after the first `_`.
fn stem_id(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    Some(stem.split_once('_').map_or(stem, |(_, id)| id).to_string())
}

/// A subagent's parent, from the extension's `artifacts/<parent id>/subagent-scripts/*-<id>.sh`.
fn subagent_parent(path: &Path, subagent_id: &str) -> Option<String> {
    let artifacts = path.parent()?.join("artifacts");
    for parent in std::fs::read_dir(artifacts).ok()?.flatten() {
        let Ok(scripts) = std::fs::read_dir(parent.path().join("subagent-scripts")) else {
            continue;
        };
        let suffix = format!("-{subagent_id}.sh");
        if scripts
            .flatten()
            .any(|s| s.file_name().to_string_lossy().ends_with(&suffix))
        {
            return Some(parent.file_name().to_string_lossy().into_owned());
        }
    }
    None
}

impl JsonlFormat for Pi {
    const SOURCE: &'static str = "pi";
    const VERSION: u32 = 2;
    type State = State;

    fn session_id_from_path(&self, path: &Path) -> Option<String> {
        stem_id(path)
    }

    fn begin(&self, path: &Path, state: &mut State, meta: &mut Meta) {
        if state.stem_id.is_empty() {
            state.stem_id = stem_id(path).unwrap_or_default();
        }
        // Subagent files are named `<subagent uuid>-<8hex>-<8hex>-<4hex>`.
        if state.stem_id.len() > 36 && meta.parent.is_none() {
            meta.parent = subagent_parent(path, &state.stem_id[..36]);
        }
    }

    fn line(&self, v: &Value, state: &mut State, meta: &mut Meta, out: &mut Vec<Item>) {
        let entry_ts = time_ms(v.get("timestamp"));
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "session" => {
                header(v, state, meta);
                return;
            }
            "session_info" => {
                if let Some(name) = v.get("name").and_then(Value::as_str) {
                    let name = name.trim();
                    state.named = !name.is_empty();
                    meta.title = (!name.is_empty()).then(|| {
                        crate::text::truncate_chars(&crate::text::collapse_ws(name), 80).0
                    });
                }
                return;
            }
            _ => {}
        }
        if let (Some(since), Some(ts)) = (state.fork_since_ms, entry_ts)
            && ts < since
        {
            return;
        }
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message" => {
                let Some(m) = v.get("message") else { return };
                let ts = time_ms(m.get("timestamp")).or(entry_ts);
                message(m, ts, state, meta, out);
            }
            "custom_message" => {
                let text = v.get("content").map(text_blocks).unwrap_or_default();
                let name = v
                    .get("customType")
                    .and_then(Value::as_str)
                    .map(|t| format!("custom:{t}"));
                if !text.is_empty() {
                    out.push(Item::new(Role::ToolResult, text).tool(name).at(entry_ts));
                }
            }
            "compaction" | "branch_summary" => {
                if let Some(summary) = v.get("summary").and_then(Value::as_str)
                    && !summary.trim().is_empty()
                {
                    out.push(Item::new(Role::Assistant, summary).at(entry_ts));
                }
            }
            _ => {}
        }
    }
}

fn header(v: &Value, state: &mut State, meta: &mut Meta) {
    let id = v.get("id").and_then(Value::as_str).unwrap_or_default();
    if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
        meta.cwd = Some(cwd.to_string());
    }
    let started = time_ms(v.get("timestamp"));
    if let Some(ts) = started {
        meta.saw_time(ts);
    }
    if !id.is_empty() {
        meta.session_id = Some(id.to_string());
    }
    if let Some(parent_file) = v.get("parentSession").and_then(Value::as_str) {
        meta.kind = Some("fork".into());
        meta.parent = stem_id(Path::new(parent_file));
        state.fork_since_ms = started;
    } else if !state.stem_id.is_empty() && !id.is_empty() && state.stem_id != id {
        // `<subagent id>-<8hex>-<8hex>-<4hex>`: the id is the leading UUID.
        meta.kind = Some("subagent".into());
        meta.extra.insert(
            "subagent_id".into(),
            Value::from(&state.stem_id[..state.stem_id.len().min(36)]),
        );
    } else {
        meta.kind = Some("main".into());
    }
}

fn message(m: &Value, ts: Option<i64>, state: &State, meta: &mut Meta, out: &mut Vec<Item>) {
    match m.get("role").and_then(Value::as_str).unwrap_or_default() {
        "user" => {
            let text = m.get("content").map(text_blocks).unwrap_or_default();
            if text.trim().is_empty() {
                return;
            }
            // The skill file Pi pastes in is not what the user wrote.
            let text = skill_call(&text);
            if !state.named && meta.title.is_none() {
                let title = clean_title(&text);
                if !title.is_empty() {
                    meta.set_title(&title);
                }
            }
            out.push(Item::new(Role::User, text).at(ts));
        }
        "assistant" => {
            let blocks = m.get("content").and_then(Value::as_array);
            let text = m.get("content").map(text_blocks).unwrap_or_default();
            if !text.trim().is_empty() {
                out.push(Item::new(Role::Assistant, text).at(ts));
            }
            for block in blocks.into_iter().flatten() {
                if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let args = block.get("arguments").map(render_args).unwrap_or_default();
                    out.push(Item::new(Role::ToolUse, args).tool(name).at(ts));
                }
            }
        }
        "toolResult" => {
            let mut text = m.get("content").map(text_blocks).unwrap_or_default();
            if m.get("isError").and_then(Value::as_bool) == Some(true) {
                text = format!("Error: {text}");
            }
            if text.trim().is_empty() {
                return;
            }
            let name = m
                .get("toolName")
                .and_then(Value::as_str)
                .map(str::to_string);
            out.push(Item::new(Role::ToolResult, text).tool(name).at(ts));
        }
        "bashExecution" => {
            let command = str_at(m, &["command"]).unwrap_or_default();
            let output = str_at(m, &["output"]).unwrap_or_default();
            out.push(
                Item::new(Role::ToolResult, format!("$ {command}\n{output}"))
                    .tool(Some("bash".into()))
                    .at(ts),
            );
        }
        "branchSummary" | "compactionSummary" => {
            if let Some(summary) = m.get("summary").and_then(Value::as_str) {
                out.push(Item::new(Role::Assistant, summary).at(ts));
            }
        }
        _ => {}
    }
}

/// Tool arguments as compact JSON (keys in their original order).
fn render_args(args: &Value) -> String {
    match args {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A skill invocation (`<skill name="x">` + the skill file + `</skill>` + request) as the user
/// typed it, `/skill:x request`; other text as is.
fn skill_call(text: &str) -> String {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("<skill name=\"")
        && let Some((name, _)) = rest.split_once('"')
        && let Some(end) = text.find("</skill>")
    {
        return format!(
            "/skill:{name} {}",
            &text[end + "</skill>".len()..].trim_start()
        )
        .trim_end()
        .to_string();
    }
    text.to_string()
}

/// The title a user message stands for: skill invocations become `/skill:name`, leading
/// `@file` blocks are dropped (subagent tasks keep the first file's body).
pub fn clean_title(text: &str) -> String {
    let text = skill_call(text);
    let mut rest = text.as_str();
    let mut first_file_body: Option<&str> = None;
    while let Some(after) = rest.trim_start().strip_prefix("<file name=\"") {
        let Some(open_end) = after.find('>') else {
            break;
        };
        let Some(close) = after.find("</file>") else {
            break;
        };
        if close > open_end && first_file_body.is_none() {
            first_file_body = Some(&after[open_end + 1..close]);
        }
        rest = &after[close + "</file>".len()..];
    }
    let mut chosen = rest.trim();
    if chosen.is_empty()
        && let Some(body) = first_file_body
    {
        chosen = body.trim();
        chosen = chosen
            .strip_prefix("Complete your task autonomously.")
            .unwrap_or(chosen)
            .trim();
    }
    crate::text::collapse_ws(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::source::testutil::parse_and_check;

    fn fixture(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/home/.pi/agent/sessions/--Users-alice-work-blog--");
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(name))
            .unwrap_or_else(|| panic!("fixture {name} in {}", dir.display()))
    }

    #[test]
    fn main_session() {
        let parsed = parse_and_check(&Pi, &fixture("2026-03-01T09-00-00"));
        let meta = &parsed.state.meta;
        assert_eq!(
            meta.session_id.as_deref(),
            Some("0190f3a2-1c00-7000-8000-00000000a001")
        );
        assert_eq!(meta.cwd.as_deref(), Some("/Users/alice/work/blog"));
        assert_eq!(meta.kind.as_deref(), Some("main"));
        assert_eq!(
            meta.title.as_deref(),
            Some("/skill:blog-deploy 把发布脚本改成先备份再上传")
        );
        let texts: Vec<&str> = parsed.records.iter().map(|r| r.text.as_str()).collect();
        assert!(texts.contains(&"在那之前回滚还来得及"));
        assert!(
            !texts.iter().any(|t| t.contains("iVBORw0KGgo")),
            "image data is not text"
        );
        assert!(
            !texts.iter().any(|t| t.contains("expert coding assistant")),
            "system prompt skipped"
        );
        let tool = parsed
            .records
            .iter()
            .find(|r| r.role == Role::ToolUse)
            .unwrap();
        assert_eq!(tool.tool.as_deref(), Some("bash"));
        assert!(tool.text.contains("cat scripts/deploy.sh"));
        assert!(
            parsed
                .records
                .iter()
                .any(|r| r.tool.as_deref() == Some("custom:subagent_result"))
        );
    }

    #[test]
    fn subagent_and_fork() {
        let sub = parse_and_check(&Pi, &fixture("2026-03-01T09-05-00"));
        assert_eq!(sub.state.meta.kind.as_deref(), Some("subagent"));
        assert_eq!(
            sub.state.meta.parent.as_deref(),
            Some("0190f3a2-1c00-7000-8000-00000000a001")
        );
        assert_eq!(
            sub.state.meta.session_id.as_deref(),
            Some("0190f3a8-7d00-7000-8000-00000000b001")
        );
        let title = sub.state.meta.title.clone().unwrap();
        assert!(!title.contains("<file"), "{title}");
        let fork = parse_and_check(&Pi, &fixture("2026-03-01T10-00-00"));
        assert_eq!(fork.state.meta.kind.as_deref(), Some("fork"));
        assert_eq!(
            fork.state.meta.parent.as_deref(),
            Some("0190f3a2-1c00-7000-8000-00000000a001")
        );
        assert!(
            fork.records
                .iter()
                .all(|r| r.ts >= fork.state.meta.started_ms.unwrap()),
            "copied parent entries are skipped"
        );
    }

    #[test]
    fn title_cleaning() {
        assert_eq!(
            clean_title(
                "<file name=\"/a/b.md\">\nComplete your task autonomously.\n\n检查链接\n</file>"
            ),
            "检查链接"
        );
        assert_eq!(
            clean_title("<file name=\"x\">ctx</file>\n真正的问题"),
            "真正的问题"
        );
        assert_eq!(clean_title("  plain  text "), "plain text");
    }
}
