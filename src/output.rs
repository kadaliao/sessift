//! Rendering results as text (terminals), JSONL (pipes) or JSON (one document).

use std::io::{IsTerminal, Write};

use clap::ValueEnum;
use serde_json::{Value, json};

use crate::fanout::{Located, Outcome};
use crate::model::{Page, RecordView, SearchHit, SessionSummary};
use crate::proto::Reply;
use crate::text::{self, display_width, fit_width, pad, pad_left};
use crate::timefmt::{human_bytes, local_minute, local_second, rfc3339};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Text,
    Jsonl,
    Json,
}

impl Format {
    /// Text for terminals, JSONL for pipes.
    pub fn resolve(requested: Option<Format>) -> Format {
        requested.unwrap_or_else(|| {
            if std::io::stdout().is_terminal() {
                Format::Text
            } else {
                Format::Jsonl
            }
        })
    }
}

#[derive(Clone, Copy)]
pub struct Style {
    on: bool,
}

impl Style {
    pub fn stdout() -> Self {
        Self {
            on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }
    pub fn stderr() -> Self {
        Self {
            on: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on && !s.is_empty() {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    pub fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    pub fn yellow(&self, s: &str) -> String {
        self.paint("33", s)
    }
    pub fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
    pub fn green(&self, s: &str) -> String {
        self.paint("32", s)
    }
    pub fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
    pub fn highlight(&self, s: &str) -> String {
        self.paint("1;33", s)
    }
}

/// Write to stdout; a closed pipe (`| head`) ends the program quietly.
pub fn emit(out: &str) {
    let mut stdout = std::io::stdout().lock();
    let result = stdout
        .write_all(out.as_bytes())
        .and_then(|_| stdout.flush());
    if let Err(err) = result {
        if err.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("sessift: cannot write output: {err}");
        std::process::exit(1);
    }
}

pub fn terminal_width() -> usize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ fills the struct we pass for a terminal descriptor.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col >= 40 {
        return size.ws_col as usize;
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .filter(|c: &usize| *c >= 40)
        .unwrap_or(120)
}

// ---------- JSON shapes ----------

pub fn hit_json(hit: &Located<SearchHit>) -> Value {
    let h = &hit.item;
    // The ids `read` needs come first, so a line cut short still has them.
    json!({
        "machine": hit.machine,
        "session_id": h.session_id,
        "record_id": h.record_id,
        "score": (h.score as f64 * 1000.0).round() / 1000.0,
        "ts": rfc3339(h.ts_ms),
        "source": h.source,
        "project": h.project,
        "role": h.role,
        "snippet": h.snippet,
        "source_path": h.source_path,
    })
}

pub fn session_json(session: &Located<SessionSummary>) -> Value {
    let s = &session.item;
    json!({
        "machine": session.machine,
        "last_at": rfc3339(s.last_ms),
        "started_at": s.started_ms.map(rfc3339),
        "source": s.source,
        "session_id": s.session_id,
        "project": s.project,
        "title": s.title,
        "message_count": s.message_count,
        "cwd": s.cwd,
        "kind": s.kind,
        "source_path": s.source_path,
        "resume_cmd": s.resume_cmd,
    })
}

pub fn record_json(record: &RecordView) -> Value {
    json!({
        "record_id": record.record_id,
        "ts": rfc3339(record.ts_ms),
        "role": record.role,
        "tool_name": record.tool_name,
        "text": record.text,
        "total_chars": record.total_chars,
        "truncated": record.truncated,
    })
}

pub fn machine_json(outcome: &Outcome) -> Value {
    let mut value = json!({
        "id": outcome.machine,
        "ok": outcome.result.is_ok(),
        "elapsed_ms": outcome.elapsed.as_millis() as u64,
        "via": outcome.via.as_str(),
    });
    match &outcome.result {
        Ok(response) => {
            value["remote_ms"] = json!(response.took_ms);
            value["bytes"] = json!(response.wire_bytes);
        }
        Err(err) => {
            value["error"] = json!({"code": err.code.as_str(), "message": err.message});
        }
    }
    value
}

pub fn jsonl(values: &[Value]) -> String {
    let mut out = String::new();
    for value in values {
        out.push_str(&value.to_string());
        out.push('\n');
    }
    out
}

// ---------- warnings and timing (stderr) ----------

pub fn print_warnings(outcomes: &[Outcome]) {
    let style = Style::stderr();
    for outcome in outcomes {
        if let Err(err) = &outcome.result {
            let clean = text::sanitize_line(&err.message);
            let message = if clean.starts_with(&format!("{}:", outcome.machine)) {
                clean
            } else {
                format!("{}: {clean}", outcome.machine)
            };
            eprintln!("{} {message}", style.yellow("warning:"));
        }
    }
}

pub fn print_timing(outcomes: &[Outcome], note: Option<&str>) {
    let style = Style::stderr();
    let width = outcomes
        .iter()
        .map(|o| display_width(&o.machine))
        .max()
        .unwrap_or(5);
    for outcome in outcomes {
        let status = match &outcome.result {
            Ok(_) => style.green("ok "),
            Err(err) => style.red(err.code.as_str()),
        };
        let mut detail = Vec::new();
        if let Ok(response) = &outcome.result {
            match outcome.via {
                crate::fanout::Via::Local => detail.push(format!("backend {}ms", response.took_ms)),
                _ => {
                    detail.push(format!("remote {}ms", response.took_ms));
                    detail.push(human_bytes(response.wire_bytes));
                }
            }
        }
        detail.push(format!("via {}", outcome.via.as_str()));
        eprintln!(
            "{} {} {} {}",
            style.dim("timing"),
            pad(&outcome.machine, width),
            status,
            style.dim(&format!(
                "{:>6}ms  {}",
                outcome.elapsed.as_millis(),
                detail.join(" · ")
            ))
        );
    }
    if let Some(note) = note {
        eprintln!("{} {note}", style.dim("timing"));
    }
}

// ---------- text rendering ----------

// Everything shown in text mode came from transcripts or remote machines; clean it first.
fn clean_hit(h: &SearchHit) -> SearchHit {
    SearchHit {
        source: text::sanitize_line(&h.source),
        session_id: text::sanitize_line(&h.session_id),
        source_path: text::sanitize_line(&h.source_path),
        record_id: text::sanitize_line(&h.record_id),
        project: text::sanitize_line(&h.project),
        role: text::sanitize_line(&h.role),
        snippet: text::sanitize_line(&h.snippet),
        ..h.clone()
    }
}

fn clean_session(s: &SessionSummary) -> SessionSummary {
    SessionSummary {
        source: text::sanitize_line(&s.source),
        session_id: text::sanitize_line(&s.session_id),
        source_path: text::sanitize_line(&s.source_path),
        project: text::sanitize_line(&s.project),
        title: s.title.as_deref().map(text::sanitize_line),
        ..s.clone()
    }
}

fn clean_record(r: &RecordView) -> RecordView {
    RecordView {
        role: text::sanitize_line(&r.role),
        tool_name: r.tool_name.as_deref().map(text::sanitize_line),
        text: text::sanitize_block(&r.text),
        ..r.clone()
    }
}

fn highlight_terms(line: &str, terms: &[String], style: &Style) -> String {
    if terms.is_empty() || !style.on {
        return line.to_string();
    }
    let chars: Vec<char> = line.chars().collect();
    let lower: Vec<char> = line.to_lowercase().chars().collect();
    if lower.len() != chars.len() {
        return line.to_string();
    }
    let mut marked = vec![false; chars.len()];
    for term in terms {
        let needle: Vec<char> = term.chars().collect();
        if needle.is_empty() || needle.len() > lower.len() {
            continue;
        }
        for start in 0..=lower.len() - needle.len() {
            if lower[start..start + needle.len()] == needle[..] {
                marked[start..start + needle.len()]
                    .iter_mut()
                    .for_each(|m| *m = true);
            }
        }
    }
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let j = (i..chars.len())
            .find(|&k| marked[k] != marked[i])
            .unwrap_or(chars.len());
        let piece: String = chars[i..j].iter().collect();
        out.push_str(&if marked[i] {
            style.highlight(&piece)
        } else {
            piece
        });
        i = j;
    }
    out
}

/// Wrap by display width into lines of at most `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(10);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for ch in paragraph.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(ch);
            used += w;
        }
        lines.push(line);
    }
    lines
}

pub fn render_hits(hits: &[Located<SearchHit>], query: &str) -> String {
    let style = Style::stdout();
    let width = terminal_width();
    let terms = text::query_terms(query);
    let machine_w = hits
        .iter()
        .map(|h| display_width(&h.machine))
        .max()
        .unwrap_or(5);
    let source_w = hits
        .iter()
        .map(|h| display_width(&h.item.source))
        .max()
        .unwrap_or(6);
    let mut out = String::new();
    for hit in hits {
        let h = &clean_hit(&hit.item);
        let project = if h.project.is_empty() {
            "-"
        } else {
            &h.project
        };
        let header = format!(
            "{}  {}  {}  {}  {}  {}  {}",
            style.bold(&pad_left(&format!("{:.1}", h.score), 5)),
            style.dim(&local_minute(h.ts_ms)),
            style.cyan(&pad(&hit.machine, machine_w)),
            pad(&h.source, source_w),
            fit_width(project, 28),
            style.dim(&h.role),
            style.dim(&h.session_id),
        );
        out.push_str(&header);
        out.push('\n');
        for line in wrap(&h.snippet, width.saturating_sub(7))
            .into_iter()
            .take(4)
        {
            out.push_str("       ");
            out.push_str(&highlight_terms(&line, &terms, &style));
            out.push('\n');
        }
    }
    out
}

pub fn render_sessions(sessions: &[Located<SessionSummary>]) -> String {
    let style = Style::stdout();
    let width = terminal_width();
    let machine_w = sessions
        .iter()
        .map(|s| display_width(&s.machine))
        .chain([7])
        .max()
        .unwrap_or(7);
    let source_w = sessions
        .iter()
        .map(|s| display_width(&s.item.source))
        .chain([6])
        .max()
        .unwrap_or(6);
    let project_w = sessions
        .iter()
        .map(|s| display_width(&s.item.project))
        .chain([7])
        .max()
        .unwrap_or(7)
        .min(24);
    let id_w = sessions
        .iter()
        .map(|s| display_width(&s.item.session_id))
        .chain([7])
        .max()
        .unwrap_or(7);
    let fixed = 16 + 2 + machine_w + 2 + source_w + 2 + 5 + 2 + project_w + 2 + 2 + id_w;
    let title_w = width.saturating_sub(fixed).clamp(12, 60);
    let mut out = String::new();
    out.push_str(&style.dim(&format!(
        "{}  {}  {}  {}  {}  {}  {}",
        pad("LAST ACTIVE", 16),
        pad("MACHINE", machine_w),
        pad("SOURCE", source_w),
        pad_left("MSGS", 5),
        pad("PROJECT", project_w),
        pad("TITLE", title_w),
        "SESSION"
    )));
    out.push('\n');
    for session in sessions {
        let s = &clean_session(&session.item);
        let title = s.title.as_deref().unwrap_or("");
        let msgs = s
            .message_count
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "{}  {}  {}  {}  {}  {}  {}\n",
            pad(&local_minute(s.last_ms), 16),
            style.cyan(&pad(&session.machine, machine_w)),
            pad(&s.source, source_w),
            pad_left(&msgs, 5),
            pad(
                if s.project.is_empty() {
                    "-"
                } else {
                    &s.project
                },
                project_w
            ),
            pad(&text::collapse_ws(title), title_w),
            style.dim(&s.session_id),
        ));
    }
    out
}

/// `next_flags` follow `--offset` in the suggested command for the next page.
pub fn render_page(machine: &str, page: &Page, next_flags: &str) -> String {
    let style = Style::stdout();
    let width = terminal_width();
    let mut out = String::new();
    let rule = style.dim(&"─".repeat(width.min(100)));
    out.push_str(&style.dim(&format!(
        "{} · {}{}",
        machine,
        text::sanitize_line(&page.session_id),
        if page.source_path.is_empty() {
            String::new()
        } else {
            format!(" · {}", text::sanitize_line(&page.source_path))
        }
    )));
    out.push('\n');
    out.push_str(&rule);
    out.push('\n');
    for record in page.records.iter().map(clean_record) {
        let record = &record;
        let who = match &record.tool_name {
            Some(tool) => format!("{} {tool}", record.role),
            None => record.role.clone(),
        };
        let who = match record.role.as_str() {
            "user" => style.green(&who),
            "assistant" => style.cyan(&who),
            _ => style.yellow(&who),
        };
        out.push_str(&format!(
            "● {}  {}\n",
            style.bold(&who),
            style.dim(&local_second(record.ts_ms))
        ));
        for line in record.text.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        if record.truncated {
            out.push_str(&style.dim(&format!(
                "  … {} of {} chars shown\n",
                text::char_len(&record.text),
                record.total_chars
            )));
        }
        out.push('\n');
    }
    out.push_str(&rule);
    out.push('\n');
    let shown_from = page.offset + 1;
    let shown_to = page.offset + page.records.len() as u32;
    let total = page
        .total
        .map(|t| t.to_string())
        .unwrap_or_else(|| "?".into());
    let mut footer = if page.records.is_empty() {
        format!("no records at offset {} (total {total})", page.offset)
    } else {
        format!("records {shown_from}–{shown_to} of {total}")
    };
    if let Some(next) = page.next_offset {
        footer.push_str(&format!(
            " · next: sessift read {} -m {machine} --offset {next}{next_flags}",
            text::sanitize_line(&page.session_id),
        ));
    }
    out.push_str(&style.dim(&footer));
    out.push('\n');
    out
}

pub fn render_machines(outcomes: &[Outcome], describe: impl Fn(&str) -> String) -> String {
    let style = Style::stdout();
    let id_w = outcomes
        .iter()
        .map(|o| display_width(&o.machine))
        .chain([7])
        .max()
        .unwrap_or(7);
    let mut out = style.dim(&format!(
        "{}  {}  {}  {}  {}  {}\n",
        pad("MACHINE", id_w),
        pad("STATUS", 8),
        pad_left("LATENCY", 8),
        pad("VERSION", 8),
        pad("BACKEND", 16),
        "TRANSPORT"
    ));
    for outcome in outcomes {
        let (status, version, backend) = match &outcome.result {
            Ok(response) => match &response.reply {
                Reply::Info(info) => (
                    style.green(&pad("ok", 8)),
                    text::sanitize_line(&info.version),
                    text::sanitize_line(&format!("{} {}", info.backend.kind, info.backend.version)),
                ),
                _ => (style.green(&pad("ok", 8)), "-".into(), "-".into()),
            },
            Err(err) => (
                style.red(&pad(err.code.as_str(), 8)),
                "-".into(),
                "-".into(),
            ),
        };
        out.push_str(&format!(
            "{}  {}  {}  {}  {}  {}\n",
            style.cyan(&pad(&outcome.machine, id_w)),
            status,
            pad_left(&format!("{}ms", outcome.elapsed.as_millis()), 8),
            pad(&version, 8),
            pad(&backend, 16),
            describe(&outcome.machine),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_wide_characters() {
        let lines = wrap("回滚回滚回滚", 10);
        assert_eq!(lines, vec!["回滚回滚回", "滚"]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
    }

    #[test]
    fn highlighting_is_case_insensitive() {
        let style = Style { on: true };
        let out = highlight_terms("Deploy 部署", &["deploy".into(), "部署".into()], &style);
        assert_eq!(out, "\x1b[1;33mDeploy\x1b[0m \x1b[1;33m部署\x1b[0m");
        let plain = Style { on: false };
        assert_eq!(
            highlight_terms("Deploy", &["deploy".into()], &plain),
            "Deploy"
        );
    }
}
