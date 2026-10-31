//! TUI state and input handling. Rendering lives in `view`, I/O in `load`; this module only
//! decides what to show and what to fetch next, so it is easy to test.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

use super::load::{Key, Msg, Preview, PreviewKey, Purpose};
use crate::fanout::{Outcome, merge_hits, merge_sessions};
use crate::model::{Filters, RecordView, SearchRequest, SessionsRequest};
use crate::proto::{Op, Reply};
use crate::timefmt::now_ms;

pub const LIST_LIMIT: u32 = 150;
pub const SEARCH_LIMIT: u32 = 80;
const TYPING_PAUSE: Duration = Duration::from_millis(150);
const PREVIEW_PAUSE: Duration = Duration::from_millis(60);
const STATUS_FOR: Duration = Duration::from_secs(3);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub key: Key,
    /// Set for search hits: the record that matched.
    pub record_id: Option<String>,
    pub ts_ms: i64,
    pub source: String,
    pub project: String,
    /// Session title, or the hit's snippet.
    pub text: String,
    pub message_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Query,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    All,
    Day,
    Week,
    Month,
    Quarter,
}

impl Range {
    pub const CHOICES: [Range; 5] = [
        Range::All,
        Range::Day,
        Range::Week,
        Range::Month,
        Range::Quarter,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Range::All => "全部时间",
            Range::Day => "24 小时",
            Range::Week => "7 天",
            Range::Month => "30 天",
            Range::Quarter => "90 天",
        }
    }

    fn since_ms(self) -> Option<i64> {
        let day = 86_400_000;
        let days = match self {
            Range::All => return None,
            Range::Day => 1,
            Range::Week => 7,
            Range::Month => 30,
            Range::Quarter => 90,
        };
        Some(now_ms() - days * day)
    }
}

/// A filter on the status line. Its letter works in the list, Alt plus the letter anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Kinds,
    Machine,
    Source,
    Time,
}

impl Filter {
    pub fn key(self) -> char {
        match self {
            Filter::Kinds => 'a',
            Filter::Machine => 'm',
            Filter::Source => 's',
            Filter::Time => 't',
        }
    }

    fn from_key(c: char) -> Option<Self> {
        [Filter::Kinds, Filter::Machine, Filter::Source, Filter::Time]
            .into_iter()
            .find(|f| f.key() == c)
    }
}

/// The dropdown opened by clicking a filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub filter: Filter,
    pub options: Vec<String>,
    pub current: usize,
    /// Highlighted option (mouse hover or arrow keys).
    pub hover: usize,
}

/// Where the last frame drew things, for mouse hits. The view fills it in on every draw.
#[derive(Debug, Default)]
pub struct Regions {
    pub search: Rect,
    pub list: Rect,
    /// The split preview pane.
    pub preview: Rect,
    /// The peek overlay.
    pub peek: Rect,
    pub chips: Vec<(Rect, Filter)>,
    /// Clickable hints; a click presses the key.
    pub buttons: Vec<(Rect, KeyCode)>,
    /// One per menu option.
    pub menu: Vec<Rect>,
}

impl Regions {
    pub fn chip_at(&self, at: Position) -> Option<Filter> {
        self.chips
            .iter()
            .find(|(r, _)| r.contains(at))
            .map(|&(_, f)| f)
    }

    pub fn button_at(&self, at: Position) -> Option<KeyCode> {
        self.buttons
            .iter()
            .find(|(r, _)| r.contains(at))
            .map(|&(_, k)| k)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineState {
    Loading,
    Done { count: usize, ms: u64 },
    Failed(String),
}

pub enum PreviewState {
    Loading,
    Ready(Preview),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Fetch {
        generation: u64,
        machines: Vec<String>,
        op: Op,
    },
    Preview(PreviewKey),
    Page {
        key: Key,
        offset: u32,
    },
    Command {
        key: Key,
        purpose: Purpose,
    },
    Copy(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Quit,
    Run(String),
    Edit(String),
}

/// Full-screen reading of one session.
pub struct Reader {
    pub row: Row,
    pub records: Vec<RecordView>,
    pub next_offset: Option<u32>,
    pub total: Option<u32>,
    pub loading: bool,
    pub error: Option<String>,
    pub scroll: usize,
    pub expand_tools: bool,
    /// Text being typed after `/`.
    pub find_input: Option<String>,
    pub find: Option<String>,
    /// Laid-out lines, rebuilt by the view when the inputs change.
    pub lines: Vec<Line<'static>>,
    pub layout_key: Option<(usize, bool, usize, Option<String>)>,
    /// First line of each record in `lines`, by record id.
    pub record_lines: HashMap<String, usize>,
    pub view_height: usize,
    /// Scroll to the hit once it has been loaded and laid out.
    pub jump_to_anchor: bool,
}

impl Reader {
    fn new(row: Row) -> Self {
        let jump_to_anchor = row.record_id.is_some();
        Self {
            row,
            records: Vec::new(),
            next_offset: Some(0),
            total: None,
            loading: false,
            error: None,
            scroll: 0,
            expand_tools: false,
            find_input: None,
            find: None,
            lines: Vec::new(),
            layout_key: None,
            record_lines: HashMap::new(),
            view_height: 20,
            jump_to_anchor,
        }
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.view_height.max(1))
    }

    fn scroll_by(&mut self, delta: isize) {
        let target = self.scroll as isize + delta;
        self.scroll = target.clamp(0, self.max_scroll() as isize) as usize;
    }

    /// Line indices containing the find term.
    pub fn matches(&self) -> Vec<usize> {
        let Some(term) = self.find.as_ref().map(|t| t.to_lowercase()) else {
            return Vec::new();
        };
        if term.is_empty() {
            return Vec::new();
        }
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .to_lowercase()
                    .contains(&term)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn jump_match(&mut self, forward: bool) -> bool {
        let matches = self.matches();
        let target = if forward {
            matches
                .iter()
                .find(|&&l| l > self.scroll + 1)
                .or(matches.first())
        } else {
            matches
                .iter()
                .rev()
                .find(|&&l| l + 1 < self.scroll + 1)
                .or(matches.last())
        };
        match target {
            Some(&line) => {
                self.scroll = line.saturating_sub(2).min(self.max_scroll());
                true
            }
            None => false,
        }
    }
}

pub struct App {
    /// Machines in the default selection, in order.
    pub machines: Vec<String>,
    /// `None` means every machine in `machines`.
    pub machine_choice: Option<usize>,
    pub source: Option<String>,
    pub sources_seen: Vec<String>,
    pub range: Range,
    /// List every session kind instead of only conversations (searches always see all).
    pub all_kinds: bool,
    pub query: String,
    /// Cursor position in characters.
    pub cursor: usize,
    pub focus: Focus,
    pub split: bool,
    pub peek: bool,
    pub rows: Vec<Row>,
    pub selected: usize,
    pub list_offset: usize,
    pub list_height: usize,
    /// The rows are search hits (as opposed to recent sessions).
    pub matches_mode: bool,
    /// Query the rows were computed for (highlighting).
    pub shown_query: String,
    pub generation: u64,
    outcomes: Vec<Outcome>,
    rows_stale: bool,
    pub machine_states: Vec<(String, MachineState)>,
    search_due: Option<Instant>,
    preview_due: Option<Instant>,
    pub previews: HashMap<PreviewKey, PreviewState>,
    pub preview_scroll: usize,
    pub reader: Option<Reader>,
    pub status: Option<(String, Instant)>,
    pub pending: Option<Purpose>,
    pub effects: Vec<Effect>,
    pub exit: Option<Exit>,
    pub tick: usize,
    /// Launched by a shell binding, so `e` can put the command on the prompt.
    pub from_shell: bool,
    pub menu: Option<Menu>,
    pub regions: Regions,
    /// Last known mouse position, for hover highlights.
    pub mouse: Option<Position>,
    /// Row and time of the last click, to tell a double click.
    last_click: Option<(Key, Instant)>,
}

impl App {
    pub fn new(machines: Vec<String>, query: String, from_shell: bool) -> Self {
        let cursor = query.chars().count();
        let mut app = Self {
            machines,
            machine_choice: None,
            source: None,
            sources_seen: Vec::new(),
            range: Range::All,
            all_kinds: false,
            query,
            cursor,
            focus: Focus::Query,
            split: false,
            peek: false,
            rows: Vec::new(),
            selected: 0,
            list_offset: 0,
            list_height: 20,
            matches_mode: false,
            shown_query: String::new(),
            generation: 0,
            outcomes: Vec::new(),
            rows_stale: true,
            machine_states: Vec::new(),
            search_due: None,
            preview_due: None,
            previews: HashMap::new(),
            preview_scroll: 0,
            reader: None,
            status: None,
            pending: None,
            effects: Vec::new(),
            exit: None,
            tick: 0,
            from_shell,
            menu: None,
            regions: Regions::default(),
            mouse: None,
            last_click: None,
        };
        app.fetch();
        app
    }

    pub fn selected_machines(&self) -> Vec<String> {
        match self.machine_choice {
            Some(i) => vec![self.machines[i].clone()],
            None => self.machines.clone(),
        }
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    pub fn terms(&self) -> Vec<String> {
        crate::text::query_terms(&self.shown_query)
    }

    /// Start a new request generation for the current query and filters.
    fn fetch(&mut self) {
        self.search_due = None;
        self.generation += 1;
        self.outcomes.clear();
        self.rows_stale = true;
        let machines = self.selected_machines();
        self.machine_states = machines
            .iter()
            .map(|m| (m.clone(), MachineState::Loading))
            .collect();
        let filters = Filters {
            source: self.source.clone(),
            since_ms: self.range.since_ms(),
            kind: self.all_kinds.then(|| "all".to_string()),
            ..Filters::default()
        };
        let query = self.query.trim().to_string();
        let op = if query.is_empty() {
            Op::Sessions(SessionsRequest {
                limit: LIST_LIMIT,
                filters,
                brief: true,
                ..SessionsRequest::default()
            })
        } else {
            Op::Search(SearchRequest {
                query: query.clone(),
                limit: SEARCH_LIMIT,
                snippet_chars: 240,
                filters,
                unique_sessions: true,
                prefix: true,
            })
        };
        self.effects.push(Effect::Fetch {
            generation: self.generation,
            machines,
            op,
        });
    }

    fn schedule_search(&mut self) {
        self.search_due = Some(Instant::now() + TYPING_PAUSE);
    }

    fn schedule_preview(&mut self) {
        self.preview_scroll = 0;
        if self.split || self.peek {
            self.preview_due = Some(Instant::now() + PREVIEW_PAUSE);
        }
    }

    pub fn preview_key(&self) -> Option<PreviewKey> {
        self.selected_row().map(|row| PreviewKey {
            key: row.key.clone(),
            record_id: row.record_id.clone(),
        })
    }

    fn rebuild_rows(&mut self) {
        let previous = self.selected_row().map(|r| r.key.clone());
        let query_shown = self.query.trim().to_string();
        let mut rows = Vec::new();
        let limit = (SEARCH_LIMIT as usize) * self.machines.len().max(1);
        let hits = merge_hits(&self.outcomes, limit);
        if !hits.is_empty()
            || self
                .outcomes
                .iter()
                .any(|o| matches!(&o.result, Ok(r) if matches!(r.reply, Reply::Search { .. })))
        {
            self.matches_mode = true;
            let mut seen = HashSet::new();
            for hit in hits {
                if !seen.insert((hit.machine.clone(), hit.item.session_id.clone())) {
                    continue;
                }
                rows.push(Row {
                    key: Key {
                        machine: hit.machine,
                        session_id: hit.item.session_id,
                        source_path: hit.item.source_path,
                    },
                    record_id: Some(hit.item.record_id).filter(|r| !r.is_empty()),
                    ts_ms: hit.item.ts_ms,
                    source: hit.item.source,
                    project: hit.item.project,
                    text: crate::text::collapse_ws(&hit.item.snippet),
                    message_count: None,
                });
            }
        } else {
            self.matches_mode = false;
            for session in merge_sessions(
                &self.outcomes,
                0,
                LIST_LIMIT as usize * self.machines.len().max(1),
            ) {
                rows.push(Row {
                    key: Key {
                        machine: session.machine,
                        session_id: session.item.session_id,
                        source_path: session.item.source_path,
                    },
                    record_id: None,
                    ts_ms: session.item.last_ms,
                    source: session.item.source,
                    project: session.item.project,
                    text: session
                        .item
                        .title
                        .map(|t| crate::text::collapse_ws(&t))
                        .unwrap_or_default(),
                    message_count: session.item.message_count,
                });
            }
            if self.source.is_none() {
                let mut counts: HashMap<&str, usize> = HashMap::new();
                for row in &rows {
                    *counts.entry(row.source.as_str()).or_default() += 1;
                }
                let mut seen: Vec<(&str, usize)> = counts.into_iter().collect();
                seen.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                let mut sources: Vec<String> =
                    seen.into_iter().map(|(s, _)| s.to_string()).collect();
                for known in &self.sources_seen {
                    if !sources.contains(known) {
                        sources.push(known.clone());
                    }
                }
                self.sources_seen = sources;
            }
        }
        self.rows = rows;
        self.shown_query = query_shown;
        self.selected = previous
            .and_then(|key| self.rows.iter().position(|r| r.key == key))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        self.schedule_preview();
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Rows {
                generation,
                outcome,
            } => {
                if generation != self.generation {
                    return;
                }
                let state = match &outcome.result {
                    Ok(response) => MachineState::Done {
                        count: match &response.reply {
                            Reply::Sessions { sessions } => sessions.len(),
                            Reply::Search { hits } => hits.len(),
                            _ => 0,
                        },
                        ms: outcome.elapsed.as_millis() as u64,
                    },
                    Err(err) => MachineState::Failed(err.message.clone()),
                };
                if let Some(slot) = self
                    .machine_states
                    .iter_mut()
                    .find(|(m, _)| *m == outcome.machine)
                {
                    slot.1 = state;
                }
                self.outcomes.push(outcome);
                if self.rows_stale {
                    // A new query or filter: start from the top, not the old selection.
                    self.rows_stale = false;
                    self.rows.clear();
                    self.selected = 0;
                    self.list_offset = 0;
                }
                self.rebuild_rows();
            }
            Msg::Preview { key, result } => {
                let state = match result {
                    Ok(preview) => PreviewState::Ready(preview),
                    Err(message) => PreviewState::Failed(message),
                };
                self.previews.insert(key, state);
            }
            Msg::Page {
                key,
                offset,
                result,
            } => {
                let Some(reader) = self.reader.as_mut() else {
                    return;
                };
                if reader.row.key != key {
                    return;
                }
                reader.loading = false;
                match result {
                    Ok(page) if page.offset == offset => {
                        reader.records.extend(page.records);
                        reader.total = page.total;
                        reader.next_offset = page.next_offset;
                        reader.layout_key = None;
                    }
                    Ok(_) => {}
                    Err(message) => reader.error = Some(message),
                }
            }
            Msg::Command { purpose, command } => {
                self.pending = None;
                match purpose {
                    Purpose::Run => self.exit = Some(Exit::Run(command)),
                    Purpose::Edit => self.exit = Some(Exit::Edit(command)),
                    Purpose::Copy => {
                        self.effects.push(Effect::Copy(command));
                        self.set_status("已复制恢复命令");
                    }
                }
            }
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        let now = Instant::now();
        if self.search_due.is_some_and(|due| now >= due) {
            self.fetch();
        }
        if self.preview_due.is_some_and(|due| now >= due) {
            self.preview_due = None;
            if let Some(key) = self.preview_key()
                && !self.previews.contains_key(&key)
            {
                if self.previews.len() > 300 {
                    self.previews.clear();
                }
                self.previews.insert(key.clone(), PreviewState::Loading);
                self.effects.push(Effect::Preview(key));
            }
        }
        if self
            .status
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > STATUS_FOR)
        {
            self.status = None;
        }
        // Load further pages as the reader nears the end of what it has.
        if let Some(reader) = self.reader.as_mut()
            && !reader.loading
            && reader.error.is_none()
            && let Some(offset) = reader.next_offset
        {
            let anchor_missing = reader.jump_to_anchor
                && reader
                    .row
                    .record_id
                    .as_ref()
                    .is_some_and(|id| !reader.records.iter().any(|r| &r.record_id == id));
            let near_end = reader.scroll + reader.view_height * 2 >= reader.lines.len();
            if reader.records.is_empty() || anchor_missing || near_end {
                reader.loading = true;
                self.effects.push(Effect::Page {
                    key: reader.row.key.clone(),
                    offset,
                });
            }
        }
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    fn request_command(&mut self, purpose: Purpose) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let key = match &self.reader {
            Some(reader) => reader.row.key.clone(),
            None => row.key.clone(),
        };
        self.pending = Some(purpose);
        self.set_status(match purpose {
            Purpose::Run => "正在准备恢复…",
            Purpose::Edit => "正在准备命令…",
            Purpose::Copy => "正在获取恢复命令…",
        });
        self.effects.push(Effect::Command { key, purpose });
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let target =
            (self.selected as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        if target != self.selected {
            self.selected = target;
            self.schedule_preview();
        }
    }

    /// The choices of a filter and the index of the current one.
    pub fn filter_options(&self, filter: Filter) -> (Vec<String>, usize) {
        match filter {
            Filter::Kinds => (
                vec!["对话".into(), "全部会话".into()],
                self.all_kinds as usize,
            ),
            Filter::Machine => (
                std::iter::once("全部机器".to_string())
                    .chain(self.machines.iter().cloned())
                    .collect(),
                self.machine_choice.map_or(0, |i| i + 1),
            ),
            Filter::Source => (
                std::iter::once("全部来源".to_string())
                    .chain(self.sources_seen.iter().cloned())
                    .collect(),
                self.source
                    .as_ref()
                    .and_then(|s| self.sources_seen.iter().position(|seen| seen == s))
                    .map_or(0, |i| i + 1),
            ),
            Filter::Time => (
                Range::CHOICES.iter().map(|r| r.label().into()).collect(),
                Range::CHOICES
                    .iter()
                    .position(|r| *r == self.range)
                    .unwrap_or(0),
            ),
        }
    }

    fn set_filter(&mut self, filter: Filter, index: usize) {
        let (options, current) = self.filter_options(filter);
        if index == current || index >= options.len() {
            return;
        }
        match filter {
            Filter::Kinds => self.all_kinds = index == 1,
            Filter::Machine => self.machine_choice = index.checked_sub(1),
            Filter::Source => {
                self.source = index
                    .checked_sub(1)
                    .and_then(|i| self.sources_seen.get(i).cloned())
            }
            Filter::Time => self.range = Range::CHOICES[index],
        }
        self.fetch();
    }

    /// Whether the filter means anything right now; says why not otherwise.
    fn filter_usable(&mut self, filter: Filter) -> bool {
        match filter {
            Filter::Kinds if !self.query.trim().is_empty() => {
                self.set_status("搜索本来就包含全部会话");
                false
            }
            _ => true,
        }
    }

    fn cycle(&mut self, filter: Filter) {
        if filter == Filter::Machine && self.machines.len() < 2 {
            self.set_status("只配置了一台机器");
            return;
        }
        if !self.filter_usable(filter) {
            return;
        }
        let (options, current) = self.filter_options(filter);
        self.set_filter(filter, (current + 1) % options.len());
    }

    /// Open the dropdown of a filter, or close it if it is already open.
    fn toggle_menu(&mut self, filter: Filter) {
        if self.menu.as_ref().is_some_and(|m| m.filter == filter) {
            self.menu = None;
            return;
        }
        if !self.filter_usable(filter) {
            return;
        }
        let (options, current) = self.filter_options(filter);
        self.menu = Some(Menu {
            filter,
            options,
            current,
            hover: current,
        });
    }

    pub fn kinds_label(&self) -> &'static str {
        if self.all_kinds {
            "全部会话"
        } else {
            "对话"
        }
    }

    pub fn machine_label(&self) -> String {
        match self.machine_choice {
            None => "全部机器".into(),
            Some(i) => self.machines[i].clone(),
        }
    }

    pub fn source_label(&self) -> String {
        self.source.clone().unwrap_or_else(|| "全部来源".into())
    }

    fn insert_char(&mut self, c: char) {
        let byte = self.byte_at(self.cursor);
        self.query.insert(byte, c);
        self.cursor += 1;
        self.schedule_search();
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.query
            .char_indices()
            .nth(chars)
            .map(|(b, _)| b)
            .unwrap_or(self.query.len())
    }

    fn delete_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_at(self.cursor);
        let start = self.byte_at(self.cursor - 1);
        self.query.replace_range(start..end, "");
        self.cursor -= 1;
        self.schedule_search();
    }

    fn delete_word(&mut self) {
        let chars: Vec<char> = self.query.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let (a, b) = (self.byte_at(start), self.byte_at(self.cursor));
        self.query.replace_range(a..b, "");
        self.cursor = start;
        self.schedule_search();
    }

    fn clear_query(&mut self) {
        self.query.clear();
        self.cursor = 0;
        self.fetch();
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.exit = Some(Exit::Quit);
            return;
        }
        if self.pending.is_some() {
            if key.code == KeyCode::Esc {
                self.pending = None;
                self.status = None;
            }
            return;
        }
        if self.menu.is_some() && self.menu_key(key) {
            return;
        }
        // Alt plus a filter letter works in the search box too, where plain letters type.
        if key.modifiers.contains(KeyModifiers::ALT)
            && self.reader.is_none()
            && !self.peek
            && let KeyCode::Char(c) = key.code
            && let Some(filter) = Filter::from_key(c)
        {
            self.cycle(filter);
            return;
        }
        if self.reader.is_some() {
            self.reader_key(key);
        } else if self.peek {
            self.peek_key(key);
        } else {
            match self.focus {
                Focus::Query => self.query_key(key),
                Focus::List => self.list_key(key),
            }
        }
    }

    fn query_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.query.chars().count(),
            KeyCode::Char('u') if ctrl => self.clear_query(),
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Char('n') | KeyCode::Char('j') if ctrl => self.enter_list(),
            KeyCode::Char(c) if !ctrl => self.insert_char(c),
            KeyCode::Backspace => self.delete_back(),
            KeyCode::Delete => {
                if self.cursor < self.query.chars().count() {
                    self.cursor += 1;
                    self.delete_back();
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.query.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.query.chars().count(),
            KeyCode::Down | KeyCode::Enter => self.enter_list(),
            KeyCode::Tab => {
                self.toggle_split();
                self.enter_list();
            }
            KeyCode::Esc => {
                if self.query.is_empty() {
                    self.exit = Some(Exit::Quit);
                } else {
                    self.clear_query();
                }
            }
            _ => {}
        }
    }

    /// Bracketed paste goes into the search box as one edit.
    pub fn on_paste(&mut self, pasted: &str) {
        if self.reader.is_some() || self.peek || self.pending.is_some() {
            return;
        }
        self.focus = Focus::Query;
        for c in crate::text::collapse_ws(pasted).chars() {
            let byte = self.byte_at(self.cursor);
            self.query.insert(byte, c);
            self.cursor += 1;
        }
        self.schedule_search();
    }

    fn enter_list(&mut self) {
        if self.search_due.is_some() {
            self.fetch();
        }
        if !self.rows.is_empty() || self.search_due.is_none() {
            self.focus = Focus::List;
        }
    }

    fn toggle_split(&mut self) {
        self.split = !self.split;
        self.schedule_preview();
    }

    fn list_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.list_height / 2).max(1) as isize;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected == 0 {
                    self.focus = Focus::Query;
                } else {
                    self.move_selection(-1);
                }
            }
            KeyCode::Char('p') if ctrl => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('n') if ctrl => self.move_selection(1),
            KeyCode::Char('d') if ctrl => self.move_selection(half),
            KeyCode::Char('u') if ctrl => self.move_selection(-half),
            KeyCode::Char('r') if ctrl => self.fetch(),
            KeyCode::PageDown => self.move_selection(half * 2),
            KeyCode::PageUp => self.move_selection(-half * 2),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(-(self.rows.len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(self.rows.len() as isize),
            KeyCode::Enter | KeyCode::Char('r') => self.request_command(Purpose::Run),
            KeyCode::Char('e') => self.request_command(Purpose::Edit),
            KeyCode::Char('Y') => self.request_command(Purpose::Copy),
            KeyCode::Char('y') => {
                if let Some(row) = self.selected_row() {
                    let id = row.key.session_id.clone();
                    self.effects.push(Effect::Copy(id));
                    self.set_status("已复制会话 ID");
                }
            }
            KeyCode::Char(' ') => {
                if !self.rows.is_empty() {
                    self.peek = true;
                    self.schedule_preview();
                }
            }
            KeyCode::Tab => self.toggle_split(),
            KeyCode::Char('o') | KeyCode::Char('l') | KeyCode::Right => self.open_reader(),
            KeyCode::Char('J') => self.preview_scroll += 3,
            KeyCode::Char('K') => self.preview_scroll = self.preview_scroll.saturating_sub(3),
            KeyCode::Char(c @ ('s' | 't' | 'm' | 'a')) => {
                if let Some(filter) = Filter::from_key(c) {
                    self.cycle(filter);
                }
            }
            KeyCode::Char('/') | KeyCode::Char('i') | KeyCode::Esc => self.focus = Focus::Query,
            KeyCode::Char('q') => self.exit = Some(Exit::Quit),
            KeyCode::Backspace => {
                self.focus = Focus::Query;
                self.delete_back();
            }
            // Anything else typed goes to the search box.
            KeyCode::Char(c) if !ctrl => {
                self.focus = Focus::Query;
                self.cursor = self.query.chars().count();
                self.insert_char(c);
            }
            _ => {}
        }
    }

    /// Keys while a dropdown is open. Returns false for keys that close it and should still
    /// do their usual thing.
    fn menu_key(&mut self, key: KeyEvent) -> bool {
        let Some(menu) = self.menu.as_mut() else {
            return false;
        };
        let last = menu.options.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => menu.hover = menu.hover.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => menu.hover = (menu.hover + 1).min(last),
            KeyCode::Enter => {
                let (filter, index) = (menu.filter, menu.hover);
                self.menu = None;
                self.set_filter(filter, index);
            }
            KeyCode::Esc => self.menu = None,
            _ => {
                self.menu = None;
                return false;
            }
        }
        true
    }

    fn peek_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char(' ') | KeyCode::Char('q') => self.peek = false,
            KeyCode::Down | KeyCode::Char('j') => self.preview_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1)
            }
            KeyCode::Char('d') | KeyCode::PageDown => self.preview_scroll += 10,
            KeyCode::Char('u') | KeyCode::PageUp => {
                self.preview_scroll = self.preview_scroll.saturating_sub(10)
            }
            KeyCode::Char('n') => self.move_selection(1),
            KeyCode::Char('p') => self.move_selection(-1),
            KeyCode::Enter => {
                self.peek = false;
                self.request_command(Purpose::Run);
            }
            KeyCode::Char('o') => {
                self.peek = false;
                self.open_reader();
            }
            _ => {}
        }
    }

    fn open_reader(&mut self) {
        if let Some(row) = self.selected_row().cloned() {
            self.reader = Some(Reader::new(row));
        }
    }

    fn reader_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        if let Some(input) = reader.find_input.as_mut() {
            match key.code {
                KeyCode::Esc => reader.find_input = None,
                KeyCode::Enter => {
                    let term = reader.find_input.take().unwrap_or_default();
                    reader.find = (!term.trim().is_empty()).then(|| term.trim().to_string());
                    reader.layout_key = None;
                    // Matching needs the new layout; jump after it is built.
                    reader.scroll = reader.scroll.saturating_sub(1);
                    let found = reader.jump_match(true);
                    if !found && reader.find.is_some() {
                        self.set_status("暂未找到（已加载部分）");
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) if !ctrl => input.push(c),
                _ => {}
            }
            return;
        }
        let page = reader.view_height.max(2) as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
                self.reader = None
            }
            KeyCode::Down | KeyCode::Char('j') => reader.scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => reader.scroll_by(-1),
            KeyCode::Char('d') if ctrl => reader.scroll_by(page / 2),
            KeyCode::Char('u') if ctrl => reader.scroll_by(-page / 2),
            KeyCode::Char('d') => reader.scroll_by(page / 2),
            KeyCode::Char('u') => reader.scroll_by(-page / 2),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => {
                reader.scroll_by(page - 1)
            }
            KeyCode::PageUp | KeyCode::Char('b') => reader.scroll_by(-(page - 1)),
            KeyCode::Home | KeyCode::Char('g') => reader.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => reader.scroll = reader.max_scroll(),
            KeyCode::Char('/') => reader.find_input = Some(String::new()),
            KeyCode::Char('n') => {
                if !reader.jump_match(true) {
                    self.set_status("没有更多匹配");
                }
            }
            KeyCode::Char('N') => {
                if !reader.jump_match(false) {
                    self.set_status("没有更多匹配");
                }
            }
            KeyCode::Char('t') => {
                reader.expand_tools = !reader.expand_tools;
                reader.layout_key = None;
            }
            KeyCode::Enter | KeyCode::Char('r') => self.request_command(Purpose::Run),
            KeyCode::Char('e') => self.request_command(Purpose::Edit),
            KeyCode::Char('y') => {
                let id = reader.row.key.session_id.clone();
                self.effects.push(Effect::Copy(id));
                self.set_status("已复制会话 ID");
            }
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        let at = Position::new(mouse.column, mouse.row);
        self.mouse = Some(at);
        if self.pending.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(menu) = self.menu.as_mut()
                    && let Some(i) = self.regions.menu.iter().position(|r| r.contains(at))
                {
                    menu.hover = i;
                }
            }
            MouseEventKind::ScrollDown => self.wheel(at, 1),
            MouseEventKind::ScrollUp => self.wheel(at, -1),
            MouseEventKind::Down(MouseButton::Left) => self.click(at),
            _ => {}
        }
    }

    fn wheel(&mut self, at: Position, delta: isize) {
        if self.menu.is_some() {
            return;
        }
        if let Some(reader) = self.reader.as_mut() {
            reader.scroll_by(delta * 3);
        } else if self.peek || self.regions.preview.contains(at) {
            self.preview_scroll = self.preview_scroll.saturating_add_signed(delta * 3);
        } else {
            self.move_selection(delta);
        }
    }

    fn click(&mut self, at: Position) {
        // Hints in the footer and the peek overlay press their key.
        if let Some(code) = self.regions.button_at(at) {
            self.menu = None;
            self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
            return;
        }
        if self.reader.is_some() {
            return;
        }
        if let Some(menu) = &self.menu {
            let filter = menu.filter;
            match self.regions.menu.iter().position(|r| r.contains(at)) {
                Some(i) => {
                    self.menu = None;
                    self.set_filter(filter, i);
                }
                // A click elsewhere only closes the menu, unless it opens another one.
                None => match self.regions.chip_at(at) {
                    Some(other) if other != filter => self.toggle_menu(other),
                    _ => self.menu = None,
                },
            }
            return;
        }
        if self.peek {
            if !self.regions.peek.contains(at) {
                self.peek = false;
            }
            return;
        }
        if let Some(filter) = self.regions.chip_at(at) {
            self.toggle_menu(filter);
        } else if self.regions.search.contains(at) {
            self.focus = Focus::Query;
            self.cursor = self.cursor_at(at.x.saturating_sub(self.regions.search.x + 4));
        } else if self.regions.list.contains(at) {
            let index = self.list_offset + (at.y - self.regions.list.y) as usize;
            let Some(row) = self.rows.get(index) else {
                return;
            };
            self.focus = Focus::List;
            let double = self
                .last_click
                .as_ref()
                .is_some_and(|(key, when)| *key == row.key && when.elapsed() < DOUBLE_CLICK);
            if double {
                self.last_click = None;
                self.request_command(Purpose::Run);
            } else {
                self.last_click = Some((row.key.clone(), Instant::now()));
                if index != self.selected {
                    self.selected = index;
                    self.schedule_preview();
                }
            }
        }
    }

    /// The cursor position for a click `column` cells into the query text.
    fn cursor_at(&self, column: u16) -> usize {
        let mut used = 0usize;
        for (i, c) in self.query.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + w.div_ceil(2) > column as usize {
                return i;
            }
            used += w;
        }
        self.query.chars().count()
    }

    /// Keep the selection visible in a list of `height` rows.
    pub fn sync_list_window(&mut self, height: usize) {
        self.list_height = height.max(1);
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        } else if self.selected >= self.list_offset + self.list_height {
            self.list_offset = self.selected + 1 - self.list_height;
        }
        let max_offset = self.rows.len().saturating_sub(self.list_height);
        self.list_offset = self.list_offset.min(max_offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fanout::Via;
    use crate::model::{SearchHit, SessionSummary};
    use crate::rpc::Response;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn session(id: &str, last: i64, source: &str) -> SessionSummary {
        SessionSummary {
            source: source.into(),
            session_id: id.into(),
            source_path: format!("/{id}"),
            project: "p".into(),
            cwd: None,
            title: Some(format!("title {id}")),
            started_ms: None,
            last_ms: last,
            message_count: Some(3),
            kind: None,
            resume_cmd: None,
        }
    }

    fn outcome(machine: &str, reply: Reply) -> Outcome {
        Outcome {
            machine: machine.into(),
            result: Ok(Response {
                reply,
                took_ms: 1,
                wire_bytes: 1,
            }),
            elapsed: Duration::from_millis(5),
            via: Via::Local,
        }
    }

    fn app() -> App {
        let mut app = App::new(vec!["local".into(), "mm".into()], String::new(), true);
        app.take_effects();
        app
    }

    #[test]
    fn rows_merge_progressively_and_keep_the_selection() {
        let mut app = app();
        let generation = app.generation;
        app.on_msg(Msg::Rows {
            generation,
            outcome: outcome(
                "local",
                Reply::Sessions {
                    sessions: vec![session("a", 10, "claude"), session("b", 5, "codex")],
                },
            ),
        });
        assert_eq!(app.rows.len(), 2);
        app.focus = Focus::List;
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected_row().unwrap().key.session_id, "b");
        app.on_msg(Msg::Rows {
            generation,
            outcome: outcome(
                "mm",
                Reply::Sessions {
                    sessions: vec![session("c", 20, "claude")],
                },
            ),
        });
        let ids: Vec<&str> = app.rows.iter().map(|r| r.key.session_id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a", "b"]);
        assert_eq!(
            app.selected_row().unwrap().key.session_id,
            "b",
            "selection follows the row"
        );
        assert_eq!(app.sources_seen, vec!["claude", "codex"]);
        // Results of an older generation are ignored.
        app.on_msg(Msg::Rows {
            generation: generation - 1,
            outcome: outcome("mm", Reply::Sessions { sessions: vec![] }),
        });
        assert_eq!(app.rows.len(), 3);
    }

    #[test]
    fn typing_searches_after_a_pause() {
        let mut app = app();
        for c in "部署".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.query, "部署");
        assert!(app.take_effects().is_empty(), "debounced");
        app.search_due = Some(Instant::now() - Duration::from_millis(1));
        app.on_tick();
        let effects = app.take_effects();
        assert!(
            matches!(&effects[0], Effect::Fetch { op: Op::Search(req), .. } if req.query == "部署" && req.unique_sessions)
        );
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.query, "");
    }

    #[test]
    fn list_keys_filter_preview_and_resume() {
        let mut app = app();
        let generation = app.generation;
        app.on_msg(Msg::Rows {
            generation,
            outcome: outcome(
                "local",
                Reply::Sessions {
                    sessions: vec![session("a", 10, "claude"), session("b", 5, "codex")],
                },
            ),
        });
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::List);

        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.source.as_deref(), Some("claude"));
        assert!(
            matches!(&app.take_effects()[0], Effect::Fetch { op: Op::Sessions(req), .. } if req.filters.source.as_deref() == Some("claude"))
        );
        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(app.range, Range::Day);
        app.on_key(key(KeyCode::Char('a')));
        assert!(app.all_kinds);
        assert!(
            matches!(&app.take_effects().last().unwrap(), Effect::Fetch { op: Op::Sessions(req), .. } if req.filters.kind.as_deref() == Some("all"))
        );
        app.on_key(key(KeyCode::Char('m')));
        assert!(
            matches!(&app.take_effects().last().unwrap(), Effect::Fetch { machines, .. } if machines == &vec!["local".to_string()])
        );

        // The stale rows stay until new ones arrive; Space peeks at the selection.
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.peek);
        app.preview_due = Some(Instant::now() - Duration::from_millis(1));
        app.on_tick();
        assert!(matches!(&app.take_effects()[0], Effect::Preview(_)));
        app.on_key(key(KeyCode::Esc));
        assert!(!app.peek);

        app.on_key(key(KeyCode::Enter));
        assert!(matches!(
            &app.take_effects()[0],
            Effect::Command {
                purpose: Purpose::Run,
                ..
            }
        ));
        app.on_msg(Msg::Command {
            purpose: Purpose::Run,
            command: "claude --resume a".into(),
        });
        assert_eq!(app.exit, Some(Exit::Run("claude --resume a".into())));
    }

    #[test]
    fn alt_letters_filter_from_the_search_box() {
        let mut app = app();
        let generation = app.generation;
        app.on_msg(Msg::Rows {
            generation,
            outcome: outcome(
                "local",
                Reply::Sessions {
                    sessions: vec![session("a", 10, "claude"), session("b", 5, "codex")],
                },
            ),
        });
        assert_eq!(app.focus, Focus::Query);
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        app.on_key(alt('s'));
        app.on_key(alt('t'));
        app.on_key(alt('m'));
        app.on_key(alt('a'));
        assert_eq!(app.query, "", "nothing typed");
        assert_eq!(app.focus, Focus::Query, "still in the search box");
        assert_eq!(app.source.as_deref(), Some("claude"));
        assert_eq!(app.range, Range::Day);
        assert_eq!(app.machine_choice, Some(0));
        assert!(app.all_kinds);
        // Plain letters still type.
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.query, "s");
        app.on_key(alt('a'));
        assert!(app.all_kinds, "kinds do not apply to a search");
    }

    #[test]
    fn menu_keys_choose_and_close() {
        let mut app = app();
        app.toggle_menu(Filter::Time);
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.options.len(), 5);
        assert_eq!(menu.current, 0);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert!(app.menu.is_none());
        assert_eq!(app.range, Range::Week);
        assert!(matches!(&app.take_effects()[..], [Effect::Fetch { .. }]));

        app.toggle_menu(Filter::Time);
        app.on_key(key(KeyCode::Esc));
        assert!(app.menu.is_none());
        assert_eq!(app.query, "", "Esc only closed the menu");
        assert!(app.exit.is_none());
        // Other keys close the menu and still do their thing.
        app.toggle_menu(Filter::Time);
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.menu.is_none());
        assert_eq!(app.query, "x");
    }

    #[test]
    fn typing_in_the_list_returns_to_the_search_box() {
        let mut app = app();
        app.focus = Focus::List;
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.focus, Focus::Query);
        assert_eq!(app.query, "x");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.query, "");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.exit, Some(Exit::Quit));
    }

    #[test]
    fn hits_become_one_row_per_session() {
        let mut app = app();
        let hit = |id: &str, score: f32, record: &str| SearchHit {
            score,
            ts_ms: 1,
            source: "claude".into(),
            session_id: id.into(),
            source_path: format!("/{id}"),
            record_id: record.into(),
            project: "p".into(),
            role: "user".into(),
            snippet: "部署\n完成".into(),
        };
        app.query = "部署".into();
        let generation = app.generation;
        app.on_msg(Msg::Rows {
            generation,
            outcome: outcome(
                "local",
                Reply::Search {
                    hits: vec![
                        hit("a", 3.0, "r1"),
                        hit("a", 2.0, "r2"),
                        hit("b", 1.0, "r3"),
                    ],
                },
            ),
        });
        assert!(app.matches_mode);
        assert_eq!(app.rows.len(), 2);
        assert_eq!(app.rows[0].record_id.as_deref(), Some("r1"));
        assert_eq!(app.rows[0].text, "部署 完成");
    }

    #[test]
    fn reader_loads_pages_until_the_hit_and_finds_text() {
        let mut app = app();
        let generation = app.generation;
        app.on_msg(Msg::Rows {
            generation,
            outcome: outcome(
                "local",
                Reply::Sessions {
                    sessions: vec![session("a", 10, "claude")],
                },
            ),
        });
        app.focus = Focus::List;
        app.rows[0].record_id = Some("r9".into());
        app.on_key(key(KeyCode::Char('o')));
        app.on_tick();
        assert!(matches!(
            &app.take_effects()[..],
            [Effect::Page { offset: 0, .. }]
        ));
        let rec = |id: &str| RecordView {
            record_id: id.into(),
            ts_ms: 1,
            role: "user".into(),
            tool_name: None,
            text: format!("text {id}"),
            total_chars: 6,
            truncated: false,
        };
        let page = crate::model::Page {
            session_id: "a".into(),
            source_path: "/a".into(),
            offset: 0,
            total: Some(10),
            next_offset: Some(5),
            records: vec![rec("r1"), rec("r2")],
        };
        let k = app.reader.as_ref().unwrap().row.key.clone();
        app.on_msg(Msg::Page {
            key: k.clone(),
            offset: 0,
            result: Ok(page),
        });
        app.on_tick();
        // The hit is not loaded yet, so the next page is requested right away.
        assert!(matches!(
            &app.take_effects()[..],
            [Effect::Page { offset: 5, .. }]
        ));

        let reader = app.reader.as_mut().unwrap();
        reader.lines = (0..50).map(|i| Line::from(format!("line {i}"))).collect();
        reader.view_height = 10;
        reader.find = Some("line 42".into());
        assert!(reader.jump_match(true));
        assert_eq!(reader.scroll, 40);
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.reader.is_none());
    }
}
