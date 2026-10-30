//! Transcript formats. Each JSONL source turns one line into zero or more records; a shared
//! driver handles files, incremental offsets and parser state.
//!
//! Contract for [`JsonlFormat::line`]: which records a kept line yields, their order and
//! their text must depend on the line alone. State may drop a whole line or fill in metadata
//! (tool names, session fields), nothing else. Reading a record back re-runs `line` on that
//! one line with fresh state and takes the record at the stored part index.

use std::io::BufRead;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::reader;

pub mod claude;
pub mod codex;
pub mod grok;
pub mod opencode;
pub mod pi;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Role {
    User = 0,
    Assistant = 1,
    ToolUse = 2,
    ToolResult = 3,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::ToolUse => "tool_use",
            Role::ToolResult => "tool_result",
        }
    }

    pub fn from_u8(v: u8) -> Role {
        match v {
            0 => Role::User,
            1 => Role::Assistant,
            2 => Role::ToolUse,
            _ => Role::ToolResult,
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool_use" | "tool_call" => Role::ToolUse,
            "tool_result" | "tool_output" => Role::ToolResult,
            _ => return None,
        })
    }

    pub fn is_message(self) -> bool {
        matches!(self, Role::User | Role::Assistant)
    }
}

/// One record as a format produces it.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub role: Role,
    pub tool: Option<String>,
    pub text: String,
    pub ts_ms: Option<i64>,
}

impl Item {
    pub fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            tool: None,
            text: text.into(),
            ts_ms: None,
        }
    }

    pub fn tool(mut self, name: Option<String>) -> Self {
        self.tool = name;
        self
    }

    pub fn at(mut self, ts: Option<i64>) -> Self {
        self.ts_ms = ts;
        self
    }
}

/// Session metadata accumulated while parsing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Source-specific values (model, branch, ...), kept for the resume command and display.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, Value>,
}

impl Meta {
    pub fn saw_time(&mut self, ts: i64) {
        if ts <= 0 {
            return;
        }
        self.started_ms = Some(self.started_ms.map_or(ts, |s| s.min(ts)));
        self.last_ms = Some(self.last_ms.map_or(ts, |l| l.max(ts)));
    }

    pub fn set_title(&mut self, text: &str) {
        if self.title.is_none() {
            let flat = crate::text::collapse_ws(text);
            if !flat.is_empty() {
                self.title = Some(crate::text::truncate_chars(&flat, 80).0);
            }
        }
    }
}

pub trait JsonlFormat: Sync {
    const SOURCE: &'static str;
    /// Bump to re-parse every file of this source.
    const VERSION: u32;
    type State: Default + Serialize + DeserializeOwned;

    /// Lines that can be dropped from their raw bytes alone, before JSON parsing (a cheap
    /// filter for large files; must never drop a line that yields records).
    fn skip_raw(&self, _line: &[u8]) -> bool {
        false
    }

    /// Called before each parse run with the file being parsed.
    fn begin(&self, _path: &Path, _state: &mut Self::State, _meta: &mut Meta) {}

    /// Records of one line, appended to `out`; also updates state and metadata.
    fn line(&self, v: &Value, state: &mut Self::State, meta: &mut Meta, out: &mut Vec<Item>);

    /// Session id when the content does not name one.
    fn session_id_from_path(&self, path: &Path) -> Option<String> {
        let name = path.file_name()?.to_str()?;
        let stem = name.split('.').next()?;
        (!stem.is_empty()).then(|| stem.to_string())
    }
}

/// Everything needed to continue parsing a file where the last run stopped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileState {
    pub meta: Meta,
    #[serde(default)]
    pub fmt: Value,
    #[serde(default)]
    pub next_seq: u32,
    #[serde(default)]
    pub last_ts: i64,
    #[serde(default)]
    pub messages: u64,
    /// Fingerprint of the bytes before the parsed offset (plain files), to notice rewrites.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<u64>,
}

/// A record with its place in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Rec {
    pub seq: u32,
    pub pos: u64,
    pub len: u32,
    pub part: u16,
    pub ts: i64,
    pub role: Role,
    pub tool: Option<String>,
    pub text: String,
}

pub struct Parsed {
    pub records: Vec<Rec>,
    pub state: FileState,
    /// Bytes consumed; the next run starts here.
    pub end: u64,
    pub bad_lines: u32,
}

/// Parse `path` from byte `start` (of the decompressed stream), continuing `state`.
pub fn parse_jsonl<F: JsonlFormat>(
    fmt: &F,
    path: &Path,
    start: u64,
    state: FileState,
) -> Result<Parsed> {
    let compressed = reader::is_compressed(path);
    let mut stream = reader::open_stream(path)?;
    if start > 0 {
        let skipped = std::io::copy(
            &mut std::io::Read::take(&mut stream, start),
            &mut std::io::sink(),
        )?;
        if skipped < start {
            anyhow::bail!("{} is shorter than the parsed offset", path.display());
        }
    }
    let mut fstate: F::State = serde_json::from_value(state.fmt.clone()).unwrap_or_default();
    let mut state = state;
    if state.meta.session_id.is_none() {
        state.meta.session_id = fmt.session_id_from_path(path);
    }
    fmt.begin(path, &mut fstate, &mut state.meta);
    let mut records = Vec::new();
    let mut items = Vec::new();
    let mut pos = start;
    let mut bad_lines = 0;
    let mut line = Vec::with_capacity(64 * 1024);
    loop {
        line.clear();
        let n = stream.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        // A plain file's unterminated last line may still be being written: leave it for
        // the next run. Compressed files are finished, so their last line counts.
        if line.last() != Some(&b'\n') && !compressed {
            break;
        }
        let body = trim_line(&line);
        let value = if body.is_empty() || fmt.skip_raw(body) {
            None
        } else {
            match serde_json::from_slice::<Value>(body) {
                Ok(v) => Some(v),
                Err(_) => {
                    bad_lines += 1;
                    None
                }
            }
        };
        if let Some(v) = value {
            items.clear();
            fmt.line(&v, &mut fstate, &mut state.meta, &mut items);
            for (part, item) in items.drain(..).enumerate() {
                let ts = item.ts_ms.filter(|t| *t > 0).unwrap_or(state.last_ts);
                state.last_ts = state.last_ts.max(ts);
                state.meta.saw_time(ts);
                if item.role.is_message() {
                    state.messages += 1;
                }
                records.push(Rec {
                    seq: state.next_seq,
                    pos,
                    len: body.len() as u32,
                    part: part.min(u16::MAX as usize) as u16,
                    ts,
                    role: item.role,
                    tool: item.tool,
                    text: item.text,
                });
                state.next_seq += 1;
            }
        }
        pos += n as u64;
    }
    state.fmt = serde_json::to_value(&fstate)?;
    Ok(Parsed {
        records,
        state,
        end: pos,
        bad_lines,
    })
}

/// Line bytes without the trailing newline (and `\r`), as stored in a record's span.
fn trim_line(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && matches!(line[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &line[..end]
}

/// Re-extract the records of one stored line (see the module contract).
pub fn items_of_line<F: JsonlFormat>(fmt: &F, bytes: &[u8]) -> Vec<Item> {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    fmt.line(&v, &mut F::State::default(), &mut Meta::default(), &mut out);
    out
}

// ----- registry -----

/// A transcript source we can parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Claude,
    Codex,
    Pi,
    OpenCode,
    Grok,
}

impl Kind {
    pub const ALL: &'static [Kind] = &[
        Kind::Claude,
        Kind::Codex,
        Kind::Pi,
        Kind::OpenCode,
        Kind::Grok,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Claude => claude::Claude::SOURCE,
            Kind::Codex => codex::Codex::SOURCE,
            Kind::Pi => pi::Pi::SOURCE,
            Kind::OpenCode => opencode::SOURCE,
            Kind::Grok => grok::Grok::SOURCE,
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|k| k.name() == name)
    }

    /// Line-oriented transcripts, parsed incrementally by byte offset.
    pub fn is_jsonl(self) -> bool {
        self != Kind::OpenCode
    }

    /// Parser version; files parsed by another version are parsed again.
    pub fn version(self) -> u32 {
        match self {
            Kind::Claude => claude::Claude::VERSION,
            Kind::Codex => codex::Codex::VERSION,
            Kind::Pi => pi::Pi::VERSION,
            Kind::OpenCode => opencode::VERSION,
            Kind::Grok => grok::Grok::VERSION,
        }
    }
}

/// Where each source keeps its transcripts.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    pub claude: Vec<std::path::PathBuf>,
    /// Codex homes (`~/.codex`), holding `sessions/` and `archived_sessions/`.
    pub codex: Vec<std::path::PathBuf>,
    pub pi: Vec<std::path::PathBuf>,
    /// OpenCode data directories, holding `opencode*.db`.
    pub opencode: Vec<std::path::PathBuf>,
    /// Grok homes (`~/.grok`), holding `sessions/`.
    pub grok: Vec<std::path::PathBuf>,
}

impl Roots {
    /// The default locations under `home`, honouring each tool's own environment variables.
    pub fn standard(home: &Path) -> Roots {
        Roots {
            claude: vec![claude::default_root(home)],
            codex: vec![codex::default_home(home)],
            pi: vec![pi::default_root(home)],
            opencode: opencode::default_roots(home),
            grok: vec![grok::default_home(home)],
        }
    }

    pub fn from_env() -> Roots {
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        Roots::standard(&home)
    }
}

/// The type of a directory entry, following symlinks.
pub(crate) fn entry_type(entry: &std::fs::DirEntry) -> Option<std::fs::FileType> {
    let t = entry.file_type().ok()?;
    if t.is_symlink() {
        std::fs::metadata(entry.path()).ok().map(|m| m.file_type())
    } else {
        Some(t)
    }
}

/// Every transcript file under the roots (sources are walked in parallel).
pub fn discover(roots: &Roots) -> Vec<(Kind, std::path::PathBuf)> {
    use rayon::prelude::*;
    type Walk = fn(&Path, &mut Vec<std::path::PathBuf>);
    let mut jobs: Vec<(Kind, &std::path::PathBuf, Walk)> = Vec::new();
    jobs.extend(
        roots
            .claude
            .iter()
            .map(|r| (Kind::Claude, r, claude::discover as Walk)),
    );
    jobs.extend(
        roots
            .codex
            .iter()
            .map(|r| (Kind::Codex, r, codex::discover as Walk)),
    );
    jobs.extend(roots.pi.iter().map(|r| (Kind::Pi, r, pi::discover as Walk)));
    jobs.extend(
        roots
            .opencode
            .iter()
            .map(|r| (Kind::OpenCode, r, opencode::discover as Walk)),
    );
    jobs.extend(
        roots
            .grok
            .iter()
            .map(|r| (Kind::Grok, r, grok::discover as Walk)),
    );
    jobs.par_iter()
        .flat_map_iter(|(kind, root, walk)| {
            let mut paths = Vec::new();
            walk(root, &mut paths);
            paths.into_iter().map(move |p| (*kind, p))
        })
        .collect()
}

/// What parsing one transcript file produced.
pub struct FileParse {
    pub sessions: Vec<SessionUpdate>,
    /// Session ids that are no longer in the file.
    pub gone: Vec<String>,
    /// Replace every session of the file: those not in `sessions` are dropped.
    pub full: bool,
    /// Kept in the index and handed back on the next run.
    pub state: Value,
    /// Bytes consumed (JSONL sources); the next run starts here.
    pub end: u64,
    pub bad_lines: u32,
}

/// New records for one session.
pub struct SessionUpdate {
    pub meta: Meta,
    /// Records from this sequence number on are replaced (0: the whole session).
    pub from_seq: u32,
    pub records: Vec<Rec>,
    /// Records in the session after the update.
    pub total: u32,
    pub messages: u64,
}

/// Parse a transcript of `kind`. JSONL sources continue from byte `start` with the state
/// the previous run left (`None` or `start == 0`: from scratch).
pub fn parse_file(kind: Kind, path: &Path, start: u64, state: Option<&str>) -> Result<FileParse> {
    if kind == Kind::OpenCode {
        return opencode::parse(path, state);
    }
    let prev: FileState = match state {
        Some(s) if start > 0 => serde_json::from_str(s).unwrap_or_default(),
        _ => FileState::default(),
    };
    let from_seq = prev.next_seq;
    let parsed = match kind {
        Kind::Claude => parse_jsonl(&claude::Claude, path, start, prev),
        Kind::Codex => parse_jsonl(&codex::Codex, path, start, prev),
        Kind::Pi => parse_jsonl(&pi::Pi, path, start, prev),
        Kind::Grok => parse_jsonl(&grok::Grok, path, start, prev),
        Kind::OpenCode => unreachable!("handled above"),
    }?;
    Ok(FileParse {
        sessions: vec![SessionUpdate {
            meta: parsed.state.meta.clone(),
            from_seq,
            records: parsed.records,
            total: parsed.state.next_seq,
            messages: parsed.state.messages,
        }],
        gone: Vec::new(),
        full: start == 0,
        state: serde_json::to_value(&parsed.state)?,
        end: parsed.end,
        bad_lines: parsed.bad_lines,
    })
}

/// The records one stored line yields (JSONL sources).
pub fn items_of(kind: Kind, line: &[u8]) -> Vec<Item> {
    match kind {
        Kind::Claude => items_of_line(&claude::Claude, line),
        Kind::Codex => items_of_line(&codex::Codex, line),
        Kind::Pi => items_of_line(&pi::Pi, line),
        Kind::Grok => items_of_line(&grok::Grok, line),
        Kind::OpenCode => Vec::new(),
    }
}

/// Texts of stored records, read back from their transcript. One entry per
/// `(pos, len, part)`; `None` when the transcript no longer has it.
pub fn read_texts(
    kind: Kind,
    path: &Path,
    session_id: &str,
    spans: &[(u64, u32, u16)],
) -> Result<Vec<Option<String>>> {
    if kind == Kind::OpenCode {
        return opencode::read_texts(path, session_id, spans);
    }
    let lines = reader::read_spans(
        path,
        &spans
            .iter()
            .map(|&(pos, len, _)| (pos, len))
            .collect::<Vec<_>>(),
    )?;
    // Several records can share a line; parse each line once.
    let mut parsed: std::collections::HashMap<u64, Vec<Item>> = std::collections::HashMap::new();
    Ok(spans
        .iter()
        .zip(lines)
        .map(|(&(pos, _, part), line)| {
            let items = parsed
                .entry(pos)
                .or_insert_with(|| line.map(|l| items_of(kind, &l)).unwrap_or_default());
            items.get(part as usize).map(|item| item.text.clone())
        })
        .collect())
}

/// Shell command that resumes a session, if its tool can.
pub fn resume_command(
    kind: Kind,
    session_id: &str,
    session_kind: Option<&str>,
    cwd: Option<&str>,
    path: &str,
    parent: Option<&str>,
) -> Option<String> {
    match kind {
        Kind::Claude => claude::resume_command(session_id, cwd, parent),
        Kind::Codex => codex::resume_command(session_id, session_kind, cwd, parent),
        Kind::Pi => Some(format!("pi --session {}", sh_quote(path))),
        Kind::OpenCode => Some(match cwd {
            Some(cwd) => format!("cd {} && opencode --session {session_id}", sh_quote(cwd)),
            None => format!("opencode --session {session_id}"),
        }),
        Kind::Grok => grok::resume_command(session_id, session_kind, cwd, parent),
    }
}

/// Quote for POSIX shells and fish alike: bare when safe, else single quotes.
pub fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+=:,@%".contains(&b));
    if safe {
        s.to_string()
    } else if s.contains(['\'', '\\']) {
        // fish and POSIX disagree on escapes inside single quotes; double quotes with only
        // `"` `$` `\` and backquote escaped mean the same in both for ordinary paths.
        let mut out = String::from("\"");
        for c in s.chars() {
            if matches!(c, '"' | '$' | '\\' | '`') {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        format!("'{s}'")
    }
}

/// Session identity, stable across runs: the source, the session id and the transcript path
/// (without a compression suffix, so compressing a file keeps its sessions).
pub fn session_key(kind: Kind, session_id: &str, path: &Path) -> i64 {
    use sha2::Digest;
    let path = path.to_string_lossy();
    let path = path.strip_suffix(".zst").unwrap_or(&path);
    let mut hasher = sha2::Sha256::new();
    hasher.update(kind.name().as_bytes());
    hasher.update([0]);
    hasher.update(session_id.as_bytes());
    hasher.update([0]);
    hasher.update(path.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    // Positive and non-zero, so it fits SQLite's INTEGER PRIMARY KEY and reads well in hex.
    (u64::from_be_bytes(bytes) >> 1).max(1) as i64
}

// ----- helpers shared by formats -----

/// Text of a message content value: a string, or the text blocks of an array joined by
/// newlines. Images, thinking and tool blocks are left out.
pub fn text_blocks(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut out = String::new();
            for block in blocks {
                let text = match block {
                    Value::String(s) => Some(s.as_str()),
                    Value::Object(o) => match o.get("type").and_then(Value::as_str) {
                        None | Some("text" | "input_text" | "output_text") => {
                            o.get("text").and_then(Value::as_str)
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            out
        }
        _ => String::new(),
    }
}

pub fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str()
}

/// RFC 3339 string or epoch number (seconds or milliseconds) to milliseconds.
pub fn time_ms(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => {
            let x = n.as_f64()?;
            Some(if x > 1e12 {
                x as i64
            } else {
                (x * 1000.0) as i64
            })
        }
        Value::String(s) => s
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|t| t.as_millisecond()),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// Parse a whole file, then check the module contract: every record's text comes back
    /// from its line alone.
    pub fn parse_and_check<F: JsonlFormat>(fmt: &F, path: &Path) -> Parsed {
        let parsed = parse_jsonl(fmt, path, 0, FileState::default()).unwrap();
        let spans: Vec<(u64, u32)> = parsed.records.iter().map(|r| (r.pos, r.len)).collect();
        let lines = reader::read_spans(path, &spans).unwrap();
        for (rec, line) in parsed.records.iter().zip(lines) {
            let items = items_of_line(fmt, &line.expect("line readable"));
            let item = items
                .get(rec.part as usize)
                .expect("part exists on re-read");
            assert_eq!(
                item.text, rec.text,
                "record {} re-reads differently",
                rec.seq
            );
            assert_eq!(item.role, rec.role);
        }
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal format: `{"role": "...", "text": "..."}` per line.
    struct Toy;

    impl JsonlFormat for Toy {
        const SOURCE: &'static str = "toy";
        const VERSION: u32 = 1;
        type State = u32;

        fn line(&self, v: &Value, state: &mut u32, meta: &mut Meta, out: &mut Vec<Item>) {
            *state += 1;
            let Some(role) = v.get("role").and_then(Value::as_str).and_then(Role::parse) else {
                return;
            };
            let text = v.get("text").and_then(Value::as_str).unwrap_or_default();
            if role == Role::User {
                meta.set_title(text);
            }
            for piece in text.split('|') {
                out.push(Item::new(role, piece).at(time_ms(v.get("ts"))));
            }
        }
    }

    #[test]
    fn incremental_parse_resumes_after_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc-123.jsonl");
        let first = "{\"role\":\"user\",\"text\":\"部署 to prod\",\"ts\":1700000000000}\nnot json\n{\"role\":\"assistant\",\"text\":\"a|b\"}\n{\"role\":\"user\",\"te";
        std::fs::write(&path, first).unwrap();
        let p1 = parse_jsonl(&Toy, &path, 0, FileState::default()).unwrap();
        assert_eq!(p1.records.len(), 3);
        assert_eq!(p1.bad_lines, 1);
        assert_eq!(p1.records[2].part, 1);
        assert_eq!(
            p1.records[2].ts, 1_700_000_000_000,
            "missing times inherit the last one"
        );
        assert_eq!(p1.state.meta.session_id.as_deref(), Some("abc-123"));
        assert_eq!(p1.state.meta.title.as_deref(), Some("部署 to prod"));
        assert!(p1.end < first.len() as u64);

        let rest = "xt\":\"again\",\"ts\":1700000005000}\n";
        std::fs::write(&path, format!("{first}{rest}")).unwrap();
        let p2 = parse_jsonl(&Toy, &path, p1.end, p1.state).unwrap();
        assert_eq!(p2.records.len(), 1);
        assert_eq!(p2.records[0].seq, 3);
        assert_eq!(p2.records[0].text, "again");
        assert_eq!(p2.state.messages, 4);
        assert_eq!(p2.state.meta.last_ms, Some(1_700_000_005_000));
        assert_eq!(p2.state.meta.title.as_deref(), Some("部署 to prod"));
        assert_eq!(
            serde_json::from_value::<u32>(p2.state.fmt.clone()).unwrap(),
            3
        );
        testutil::parse_and_check(&Toy, &path);
    }

    #[test]
    fn compressed_files_parse_to_the_same_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let text = "{\"role\":\"user\",\"text\":\"x\"}\n{\"role\":\"assistant\",\"text\":\"y\"}\n";
        let plain = dir.path().join("s.jsonl");
        let packed = dir.path().join("s.jsonl.zst");
        std::fs::write(&plain, text).unwrap();
        std::fs::write(&packed, zstd::encode_all(text.as_bytes(), 3).unwrap()).unwrap();
        let a = parse_jsonl(&Toy, &plain, 0, FileState::default()).unwrap();
        let b = parse_and_check(&Toy, &packed);
        assert_eq!(a.records, b.records);
        assert_eq!(b.state.meta.session_id.as_deref(), Some("s"));
    }

    use testutil::parse_and_check;
}
