//! Backend-independent data model shared by the CLI, the wire protocol and every backend.
//!
//! Times are Unix milliseconds. Character counts are Unicode scalar values.

use serde::{Deserialize, Serialize};

pub const SEARCH_LIMIT_DEFAULT: u32 = 20;
pub const SEARCH_LIMIT_MAX: u32 = 200;
pub const SNIPPET_CHARS_DEFAULT: u32 = 240;
pub const SNIPPET_CHARS_MIN: u32 = 16;
pub const SNIPPET_CHARS_MAX: u32 = 2000;
pub const SESSIONS_LIMIT_DEFAULT: u32 = 20;
pub const SESSIONS_LIMIT_MAX: u32 = 500;
pub const READ_LIMIT_DEFAULT: u32 = 50;
pub const READ_LIMIT_MAX: u32 = 500;
pub const MAX_CHARS_DEFAULT: u32 = 16_000;
pub const MAX_CHARS_MIN: u32 = 256;
pub const MAX_CHARS_MAX: u32 = 200_000;
pub const CONTEXT_BEFORE_DEFAULT: u32 = 3;
pub const CONTEXT_AFTER_DEFAULT: u32 = 8;
pub const CONTEXT_SIDE_MAX: u32 = 100;
/// What the command line keeps of each tool call and output when reading (`--tool-chars`).
pub const TOOL_CHARS_DEFAULT: u32 = 1500;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<i64>,
    /// Session kinds, comma-separated: `main`, `subagent`, `fork`, `background`, `exec`,
    /// `guardian`, or the groups `conversation` (what a person talked to, with at least one
    /// message) and `all`. Unset: session lists show `conversation`, searches `all`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Leave these sessions out, together with the sessions spawned or forked from them. The
    /// CLI puts the calling agent's own session here, which otherwise matches every query.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_sessions: Vec<String>,
}

/// Kinds in the `conversation` group; sessions without a kind count as `main`.
pub const CONVERSATION_KINDS: &[&str] = &["main", "fork", "background"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
    #[serde(default = "default_snippet_chars")]
    pub snippet_chars: u32,
    #[serde(default)]
    pub filters: Filters,
    /// Return only the best hit of each session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unique_sessions: bool,
    /// The last word may be unfinished: also match words that start with it (search as you
    /// type). Older servers ignore it and match whole words.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prefix: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub score: f32,
    pub ts_ms: i64,
    pub source: String,
    pub session_id: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub record_id: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionsRequest {
    #[serde(default = "default_sessions_limit")]
    pub limit: u32,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub filters: Filters,
    /// Leave out fields a list does not show (`cwd`, `resume_cmd`, `kind`, `started_ms`), which
    /// make up much of each row on a slow link.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub brief: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub source: String,
    pub session_id: String,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<i64>,
    pub last_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_cmd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default)]
    pub offset: u32,
    #[serde(default = "default_read_limit")]
    pub limit: u32,
    #[serde(default = "default_max_chars")]
    pub max_chars: u32,
    /// Characters kept of each tool call and tool output before the page budget is shared out,
    /// so that long outputs do not crowd out the conversation. Unset keeps them whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_chars: Option<u32>,
}

/// Records around one record, for showing a search hit in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRequest {
    pub record_id: String,
    /// Narrows the lookup; backends that index record ids globally may ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default = "default_context_before")]
    pub before: u32,
    #[serde(default = "default_context_after")]
    pub after: u32,
    #[serde(default = "default_max_chars")]
    pub max_chars: u32,
    /// As in [`ReadRequest::tool_chars`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_chars: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub session_id: String,
    #[serde(default)]
    pub source_path: String,
    pub offset: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    #[serde(default)]
    pub next_offset: Option<u32>,
    pub records: Vec<RecordView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordView {
    #[serde(default)]
    pub record_id: String,
    pub ts_ms: i64,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub text: String,
    /// Length of the complete record text before any truncation.
    pub total_chars: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub kind: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub host: String,
    pub version: String,
    pub protocol: u32,
    pub backend: BackendInfo,
    pub uptime_ms: u64,
    pub active_requests: u32,
}

fn default_search_limit() -> u32 {
    SEARCH_LIMIT_DEFAULT
}
fn default_snippet_chars() -> u32 {
    SNIPPET_CHARS_DEFAULT
}
fn default_sessions_limit() -> u32 {
    SESSIONS_LIMIT_DEFAULT
}
fn default_read_limit() -> u32 {
    READ_LIMIT_DEFAULT
}
fn default_max_chars() -> u32 {
    MAX_CHARS_DEFAULT
}
fn default_context_before() -> u32 {
    CONTEXT_BEFORE_DEFAULT
}
fn default_context_after() -> u32 {
    CONTEXT_AFTER_DEFAULT
}

fn check_range(name: &str, value: u32, min: u32, max: u32) -> Result<(), String> {
    if value < min || value > max {
        return Err(format!(
            "{name} must be between {min} and {max}, got {value}"
        ));
    }
    Ok(())
}

impl Filters {
    pub fn validate(&self) -> Result<(), String> {
        if let (Some(since), Some(until)) = (self.since_ms, self.until_ms)
            && since > until
        {
            return Err("since must not be later than until".into());
        }
        Ok(())
    }
}

impl SearchRequest {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: SEARCH_LIMIT_DEFAULT,
            snippet_chars: SNIPPET_CHARS_DEFAULT,
            filters: Filters::default(),
            unique_sessions: false,
            prefix: false,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.query.trim().is_empty() {
            return Err("query must not be empty".into());
        }
        check_range("limit", self.limit, 1, SEARCH_LIMIT_MAX)?;
        check_range(
            "snippet_chars",
            self.snippet_chars,
            SNIPPET_CHARS_MIN,
            SNIPPET_CHARS_MAX,
        )?;
        self.filters.validate()
    }
}

impl Default for SessionsRequest {
    fn default() -> Self {
        Self {
            limit: SESSIONS_LIMIT_DEFAULT,
            offset: 0,
            filters: Filters::default(),
            brief: false,
        }
    }
}

impl SessionsRequest {
    pub fn validate(&self) -> Result<(), String> {
        check_range("limit", self.limit, 1, SESSIONS_LIMIT_MAX)?;
        // Each machine is asked for offset + limit rows, so bound the sum.
        if self.offset > 10_000 {
            return Err("offset must be at most 10000".into());
        }
        self.filters.validate()
    }
}

impl ReadRequest {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            source_path: None,
            offset: 0,
            limit: READ_LIMIT_DEFAULT,
            max_chars: MAX_CHARS_DEFAULT,
            tool_chars: None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.session_id.trim().is_empty() {
            return Err("session_id must not be empty".into());
        }
        check_range("limit", self.limit, 1, READ_LIMIT_MAX)?;
        check_range("max_chars", self.max_chars, MAX_CHARS_MIN, MAX_CHARS_MAX)?;
        check_tool_chars(self.tool_chars)
    }
}

impl ContextRequest {
    pub fn new(record_id: impl Into<String>) -> Self {
        Self {
            record_id: record_id.into(),
            session_id: None,
            before: CONTEXT_BEFORE_DEFAULT,
            after: CONTEXT_AFTER_DEFAULT,
            max_chars: MAX_CHARS_DEFAULT,
            tool_chars: None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.record_id.trim().is_empty() {
            return Err("record_id must not be empty".into());
        }
        check_range("before", self.before, 0, CONTEXT_SIDE_MAX)?;
        check_range("after", self.after, 0, CONTEXT_SIDE_MAX)?;
        check_range("max_chars", self.max_chars, MAX_CHARS_MIN, MAX_CHARS_MAX)?;
        check_tool_chars(self.tool_chars)
    }
}

fn check_tool_chars(value: Option<u32>) -> Result<(), String> {
    match value {
        Some(v) => check_range("tool_chars", v, 1, MAX_CHARS_MAX),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_defaults_apply_when_fields_are_missing() {
        let req: SearchRequest = serde_json::from_str(r#"{"query":"x"}"#).unwrap();
        assert_eq!(req.limit, SEARCH_LIMIT_DEFAULT);
        assert_eq!(req.snippet_chars, SNIPPET_CHARS_DEFAULT);
        assert!(req.validate().is_ok());
    }

    #[test]
    fn validation_rejects_out_of_range_values() {
        let mut req = SearchRequest::new("x");
        req.limit = 0;
        assert!(req.validate().is_err());
        req.limit = SEARCH_LIMIT_MAX + 1;
        assert!(req.validate().is_err());
        assert!(SearchRequest::new("  ").validate().is_err());

        let mut read = ReadRequest::new("s");
        read.max_chars = 10;
        assert!(read.validate().is_err());

        let mut sessions = SessionsRequest::default();
        sessions.filters.since_ms = Some(10);
        sessions.filters.until_ms = Some(5);
        assert!(sessions.validate().is_err());
    }
}
