//! Drawing. Pure functions of the app state, except for layout caches kept on the state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Focus, MachineState, PreviewState, Range, Row};
use super::md::{self, Para, RecordOpts};
use super::theme;
use crate::text::{self, display_width, fit_width};
use crate::timefmt::now_ms;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner(app: &App) -> &'static str {
    SPINNER[app.tick % SPINNER.len()]
}

/// `now`, `5m`, `3h`, `2d`, `4mo`, `1y`.
pub fn age(ts_ms: i64) -> String {
    if ts_ms <= 0 {
        return "-".into();
    }
    let secs = ((now_ms() - ts_ms) / 1000).max(0);
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 30 * 86_400 => format!("{}d", s / 86_400),
        s if s < 365 * 86_400 => format!("{}mo", s / (30 * 86_400)),
        s => format!("{}y", s / (365 * 86_400)),
    }
}

/// Fit to `width` columns, eliding the middle: `someone-…-sessift`.
pub fn middle(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return text::pad(s, width);
    }
    if width < 3 {
        return fit_width(s, width);
    }
    let chars: Vec<char> = s.chars().collect();
    let budget = width - 1;
    let head_budget = budget / 2;
    let mut head = String::new();
    let mut used = 0;
    for &c in &chars {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > head_budget {
            break;
        }
        head.push(c);
        used += w;
    }
    let mut tail: Vec<char> = Vec::new();
    let mut tail_used = 0;
    for &c in chars.iter().rev() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + tail_used + w > budget {
            break;
        }
        tail.push(c);
        tail_used += w;
    }
    tail.reverse();
    text::pad(
        &format!("{head}…{}", tail.into_iter().collect::<String>()),
        width,
    )
}

/// Draw everything; returns the screen row of the first list entry (for mouse clicks).
pub fn draw(frame: &mut Frame, app: &mut App) -> u16 {
    let area = frame.area();
    if app.reader.is_some() {
        draw_reader(frame, area, app);
        return u16::MAX;
    }
    let outer = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    let [_, search, _, status, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(outer);
    draw_search(frame, search, app);
    draw_status(frame, status, app);
    let list_area = if app.split && body.width >= 60 {
        let [list, preview] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(body);
        draw_preview_pane(frame, preview, app);
        list
    } else {
        body
    };
    draw_list(frame, list_area, app);
    draw_footer(frame, footer, app);
    if app.peek {
        draw_peek(frame, area, app);
    }
    list_area.y
}

fn draw_search(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Query && !app.peek;
    let bar = Span::styled(
        "▌",
        if focused {
            theme::accent()
        } else {
            theme::faint()
        },
    );
    let text = if app.query.is_empty() {
        Span::styled("搜索会话：输入关键词，中英文都可以", theme::muted())
    } else {
        Span::styled(text::sanitize_line(&app.query), theme::bold())
    };
    let lines = vec![
        Line::from(bar.clone()),
        Line::from(vec![bar.clone(), Span::raw("   "), text]),
        Line::from(bar),
    ];
    frame.render_widget(Paragraph::new(lines), area);
    if focused {
        let before: String = app.query.chars().take(app.cursor).collect();
        let x = area.x + 4 + UnicodeWidthStr::width(text::sanitize_line(&before).as_str()) as u16;
        frame.set_cursor_position((x.min(area.right().saturating_sub(1)), area.y + 1));
    }
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let mut left = vec![
        Span::styled(
            if app.matches_mode {
                "匹配 "
            } else {
                "最近 "
            },
            theme::bold(),
        ),
        Span::styled(app.rows.len().to_string(), theme::muted()),
    ];
    for (machine, state) in &app.machine_states {
        left.push(Span::raw("   "));
        match state {
            MachineState::Loading => {
                left.push(Span::styled(spinner(app), theme::accent()));
                left.push(Span::styled(format!(" {machine} 加载中"), theme::muted()));
            }
            MachineState::Done { count, .. } => {
                left.push(Span::styled("●", Style::default().fg(theme::OK)));
                left.push(Span::styled(format!(" {machine} {count}"), theme::muted()));
            }
            MachineState::Failed(message) => {
                left.push(Span::styled("●", Style::default().fg(theme::ERR)));
                left.push(Span::styled(
                    format!(
                        " {machine} {}",
                        fit_width(&text::sanitize_line(message), 40)
                    ),
                    Style::default().fg(theme::ERR),
                ));
            }
        }
    }
    let chip = |key: &str, label: String, active: bool| {
        vec![
            Span::styled(format!("{key} "), theme::faint()),
            Span::styled(
                format!("{label} ▾"),
                if active {
                    theme::accent()
                } else {
                    theme::muted()
                },
            ),
        ]
    };
    let mut right = Vec::new();
    right.extend(chip("m", app.machine_label(), app.machine_choice.is_some()));
    right.push(Span::raw("   "));
    right.extend(chip("s", app.source_label(), app.source.is_some()));
    right.push(Span::raw("   "));
    right.extend(chip(
        "t",
        app.range.label().to_string(),
        app.range != Range::All,
    ));
    let right_width: u16 = right.iter().map(|s| s.width() as u16).sum();
    let [l, r] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(right_width)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(left)), l);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        r,
    );
}

fn row_line(app: &App, row: &Row, width: usize, selected: bool, terms: &[String]) -> Line<'static> {
    let show_machine = app.machines.len() > 1;
    let machine_w = app
        .machines
        .iter()
        .map(|m| display_width(m))
        .max()
        .unwrap_or(0);
    let mut spans = Vec::new();
    let marker = if selected && app.focus == Focus::List {
        Span::styled("▌", theme::accent())
    } else {
        Span::raw(" ")
    };
    spans.push(marker);
    spans.push(Span::styled(
        format!("{:>4} ", age(row.ts_ms)),
        theme::muted(),
    ));
    spans.push(Span::styled(
        "● ",
        Style::default().fg(theme::source_color(&row.source)),
    ));
    // Narrow lists (split view) keep only the coloured dot for the source.
    let compact = width < 100;
    let mut used = 1 + 5 + 2;
    if !compact {
        spans.push(Span::styled(
            text::pad(&text::sanitize_line(&row.source), 9),
            theme::muted(),
        ));
        used += 9;
    }
    if show_machine {
        spans.push(Span::styled(
            text::pad(&row.key.machine, machine_w + 2),
            theme::faint(),
        ));
        used += machine_w + 2;
    }
    let project_w = if compact { 14 } else { 20 }
        .min(width.saturating_sub(used + 10) / 3)
        .max(8);
    let project = if row.project.is_empty() {
        "-".to_string()
    } else {
        text::sanitize_line(&row.project)
    };
    spans.push(Span::raw(middle(&project, project_w)));
    spans.push(Span::raw("  "));
    used += project_w + 2;
    let rest = width.saturating_sub(used);
    let body = fit_width(&text::sanitize_line(&row.text), rest);
    let body_style = if selected {
        theme::bold()
    } else {
        Style::default()
    };
    if app.matches_mode {
        let paras = md::markdown(&body, body_style, terms);
        if let Some(first) = paras.into_iter().next() {
            spans.extend(first.spans);
        }
    } else {
        spans.push(Span::styled(body, body_style));
    }
    if let Some(n) = row.message_count {
        let note = format!("  {n}");
        let line_width: usize = spans.iter().map(|s| s.width()).sum();
        if line_width + note.len() <= width {
            spans.push(Span::styled(note, theme::faint()));
        }
    }
    let mut line = Line::from(spans);
    if selected {
        let line_width: usize = line.spans.iter().map(|s| s.width()).sum();
        if line_width < width {
            line.spans.push(Span::raw(" ".repeat(width - line_width)));
        }
        line = line.style(Style::default().bg(theme::SELECTED_BG));
    }
    line
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let height = area.height as usize;
    app.sync_list_window(height);
    if app.rows.is_empty() {
        let loading = app
            .machine_states
            .iter()
            .any(|(_, s)| *s == MachineState::Loading);
        let message = if loading {
            Line::from(vec![
                Span::styled(spinner(app), theme::accent()),
                Span::styled(" 加载中…", theme::muted()),
            ])
        } else if app.query.trim().is_empty() {
            Line::styled("  没有会话", theme::muted())
        } else {
            Line::styled(
                "  没有匹配的会话（试试别的关键词，或按 t 放宽时间范围）",
                theme::muted(),
            )
        };
        frame.render_widget(Paragraph::new(vec![Line::default(), message]), area);
        return;
    }
    let terms = app.terms();
    let width = area.width as usize;
    let lines: Vec<Line> = app
        .rows
        .iter()
        .enumerate()
        .skip(app.list_offset)
        .take(height)
        .map(|(i, row)| row_line(app, row, width, i == app.selected, &terms))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// The preview of the selected row, laid out for `width` columns.
fn preview_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(row) = app.selected_row() else {
        return Vec::new();
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                if row.project.is_empty() {
                    "-".to_string()
                } else {
                    text::sanitize_line(&row.project)
                },
                theme::bold(),
            ),
            Span::raw("  "),
            Span::styled("● ", Style::default().fg(theme::source_color(&row.source))),
            Span::styled(text::sanitize_line(&row.source), theme::muted()),
            Span::styled(format!("  {}", row.key.machine), theme::faint()),
        ]),
        Line::from(Span::styled(
            format!(
                "{}  ·  {}{}",
                text::sanitize_line(&row.key.session_id),
                age(row.ts_ms),
                row.message_count
                    .map(|n| format!("  ·  {n} 条"))
                    .unwrap_or_default()
            ),
            theme::faint(),
        )),
        Line::styled("─".repeat(width.min(200)), theme::faint()),
    ];
    let key = app.preview_key();
    match key.as_ref().and_then(|k| app.previews.get(k)) {
        None | Some(PreviewState::Loading) => lines.push(Line::from(vec![
            Span::styled(spinner(app), theme::accent()),
            Span::styled(" 加载预览…", theme::muted()),
        ])),
        Some(PreviewState::Failed(message)) => lines.push(Line::styled(
            format!("预览失败：{}", text::sanitize_line(message)),
            Style::default().fg(theme::ERR),
        )),
        Some(PreviewState::Ready(preview)) => {
            let terms = app.terms();
            let mut paras: Vec<Para> = Vec::new();
            for (i, record) in preview.records.iter().enumerate() {
                if let Some((at, omitted)) = preview.gap
                    && at == i
                {
                    paras.push(Para::new(vec![Span::styled(
                        format!("    ··· 省略 {omitted} 条 ···"),
                        theme::faint(),
                    )]));
                    paras.push(Para::blank());
                }
                let anchor = row.record_id.as_deref() == Some(record.record_id.as_str());
                paras.extend(md::record(
                    record,
                    &RecordOpts {
                        compact: true,
                        expand_tools: false,
                        anchor,
                        terms: &terms,
                    },
                ));
            }
            if paras.is_empty() {
                paras.push(Para::new(vec![Span::styled(
                    "（没有可显示的消息）",
                    theme::muted(),
                )]));
            }
            lines.extend(md::wrap(&paras, width, 0));
        }
    }
    lines
}

fn render_scrolled(frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>, scroll: &mut usize) {
    let height = area.height as usize;
    let max = lines.len().saturating_sub(height);
    *scroll = (*scroll).min(max);
    let visible: Vec<Line> = lines.into_iter().skip(*scroll).take(height).collect();
    frame.render_widget(Paragraph::new(visible), area);
}

fn draw_preview_pane(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme::faint());
    let inner = block.inner(area).inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    frame.render_widget(block, area);
    let lines = preview_lines(app, inner.width as usize);
    let mut scroll = app.preview_scroll;
    render_scrolled(frame, inner, lines, &mut scroll);
    app.preview_scroll = scroll;
}

fn draw_peek(frame: &mut Frame, area: Rect, app: &mut App) {
    let width = (area.width as f32 * 0.9) as u16;
    let height = (area.height as f32 * 0.85) as u16;
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Span::styled(" 速览 ", theme::bold()))
        .title_bottom(Line::from(Span::styled(
            " Enter 恢复  o 全文  j/k 滚动  n/p 下一条/上一条  Esc 关闭 ",
            theme::muted(),
        )));
    let inner = block.inner(rect).inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    frame.render_widget(block, rect);
    let lines = preview_lines(app, inner.width as usize);
    let mut scroll = app.preview_scroll;
    render_scrolled(frame, inner, lines, &mut scroll);
    app.preview_scroll = scroll;
}

fn hints(pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (key, label) in pairs {
        spans.push(Span::styled(key.to_string(), theme::accent()));
        spans.push(Span::styled(format!(" {label}  "), theme::muted()));
    }
    spans
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();
    if let Some((message, _)) = &app.status {
        if app.pending.is_some() {
            spans.push(Span::styled(format!("{} ", spinner(app)), theme::accent()));
        }
        spans.push(Span::styled(format!("{message}   "), theme::accent()));
    }
    let edit = if app.from_shell {
        "放到命令行"
    } else {
        "打印命令"
    };
    spans.extend(match app.focus {
        Focus::Query => hints(&[
            ("输入", "搜索"),
            ("↓/Enter", "选会话"),
            ("Tab", "分栏预览"),
            ("Esc", "清空/退出"),
        ]),
        Focus::List => hints(&[
            ("Enter/r", "恢复"),
            ("e", edit),
            ("Space", "速览"),
            ("Tab", "分栏"),
            ("o", "全文"),
            ("y/Y", "复制ID/命令"),
            ("s/t/m", "来源/时间/机器"),
            ("/", "搜索"),
            ("q", "退出"),
        ]),
    });
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_reader(frame: &mut Frame, area: Rect, app: &mut App) {
    let terms = app.terms();
    let tick = app.tick;
    let status = app.status.as_ref().map(|(m, _)| m.clone());
    let from_shell = app.from_shell;
    let Some(reader) = app.reader.as_mut() else {
        return;
    };
    let outer = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    let [header, rule, body, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(outer);

    let row = &reader.row;
    let title = if row.text.is_empty() {
        "（无标题）".to_string()
    } else {
        text::sanitize_line(&row.text)
    };
    let loaded = reader.records.len();
    let header_lines = vec![
        Line::from(Span::styled(
            fit_width(&title, header.width as usize),
            theme::bold(),
        )),
        Line::from(vec![
            Span::styled("● ", Style::default().fg(theme::source_color(&row.source))),
            Span::styled(text::sanitize_line(&row.source), theme::muted()),
            Span::styled(
                format!(
                    "  {}  {}  {}  ·  已加载 {loaded}{} 条",
                    text::sanitize_line(&row.project),
                    row.key.machine,
                    text::sanitize_line(&row.key.session_id),
                    reader.total.map(|t| format!("/{t}")).unwrap_or_default()
                ),
                theme::faint(),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(header_lines), header);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(rule.width as usize),
            theme::faint(),
        )),
        rule,
    );

    let width = body.width as usize;
    let find_terms: Vec<String> = match &reader.find {
        Some(term) => vec![term.to_lowercase()],
        None => terms,
    };
    let layout_key = (
        width,
        reader.expand_tools,
        reader.records.len(),
        reader.find.clone(),
    );
    if reader.layout_key.as_ref() != Some(&layout_key) {
        let mut lines = Vec::new();
        let mut starts = std::collections::HashMap::new();
        for record in &reader.records {
            let anchor = row.record_id.as_deref() == Some(record.record_id.as_str());
            let paras = md::record(
                record,
                &RecordOpts {
                    compact: false,
                    expand_tools: reader.expand_tools,
                    anchor,
                    terms: &find_terms,
                },
            );
            if paras.is_empty() {
                continue;
            }
            starts.insert(record.record_id.clone(), lines.len());
            lines.extend(md::wrap(&paras, width, 0));
        }
        reader.lines = lines;
        reader.record_lines = starts;
        reader.layout_key = Some(layout_key);
    }
    reader.view_height = body.height as usize;
    if reader.jump_to_anchor
        && let Some(id) = &row.record_id
        && let Some(&line) = reader.record_lines.get(id)
    {
        reader.scroll = line.saturating_sub(2);
        reader.jump_to_anchor = false;
    }
    let mut lines = reader.lines.clone();
    if reader.loading || (reader.records.is_empty() && reader.error.is_none()) {
        lines.push(Line::from(vec![
            Span::styled(SPINNER[tick % SPINNER.len()], theme::accent()),
            Span::styled(" 加载中…", theme::muted()),
        ]));
    }
    if let Some(error) = &reader.error {
        lines.push(Line::styled(
            format!("加载失败：{}", text::sanitize_line(error)),
            Style::default().fg(theme::ERR),
        ));
    }
    let max_scroll = lines.len().saturating_sub(body.height as usize);
    reader.scroll = reader.scroll.min(max_scroll);
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(reader.scroll)
        .take(body.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(visible), body);

    if let Some(input) = &reader.find_input {
        let line = Line::from(vec![
            Span::styled("/", theme::accent()),
            Span::styled(text::sanitize_line(input), theme::bold()),
        ]);
        frame.render_widget(Paragraph::new(line), footer);
        let x = footer.x + 1 + UnicodeWidthStr::width(text::sanitize_line(input).as_str()) as u16;
        frame.set_cursor_position((x, footer.y));
        return;
    }
    let total_lines = reader.lines.len().max(1);
    let percent = ((reader.scroll + body.height as usize).min(total_lines) * 100) / total_lines;
    let mut spans = Vec::new();
    if let Some(message) = status {
        spans.push(Span::styled(format!("{message}   "), theme::accent()));
    }
    let edit = if from_shell {
        "放到命令行"
    } else {
        "打印命令"
    };
    spans.extend(hints(&[
        ("j/k", "滚动"),
        ("d/u", "半页"),
        ("g/G", "首尾"),
        ("/", "查找"),
        ("n/N", "下/上一个"),
        (
            "t",
            if reader.expand_tools {
                "收起工具"
            } else {
                "展开工具"
            },
        ),
        ("Enter", "恢复"),
        ("e", edit),
        ("q", "返回"),
    ]));
    let position = format!("{percent}%");
    let [l, r] = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(position.len() as u16 + 1),
    ])
    .areas(footer);
    frame.render_widget(Paragraph::new(Line::from(spans)), l);
    frame.render_widget(
        Paragraph::new(Line::styled(position, theme::faint())).alignment(Alignment::Right),
        r,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_and_middle_elision() {
        let now = now_ms();
        assert_eq!(age(now), "now");
        assert_eq!(age(now - 5 * 60_000), "5m");
        assert_eq!(age(now - 3 * 3_600_000), "3h");
        assert_eq!(age(now - 2 * 86_400_000), "2d");
        assert_eq!(age(now - 100 * 86_400_000), "3mo");
        assert_eq!(middle("someone-workspace-sessift", 16), "someone…-sessift");
        assert_eq!(display_width(&middle("一二三四五六七八九十", 9)), 9);
        assert_eq!(middle("short", 8), "short   ");
    }

    #[test]
    fn renders_main_screen_and_reader() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new(vec!["local".into(), "mm".into()], String::new(), true);
        app.take_effects();
        let generation = app.generation;
        let session = crate::model::SessionSummary {
            source: "claude".into(),
            session_id: "abc-123".into(),
            source_path: "/x".into(),
            project: "someone-workspace-sessift".into(),
            cwd: None,
            title: Some("把 memex 做成交互界面".into()),
            started_ms: None,
            last_ms: now_ms() - 120_000,
            message_count: Some(42),
            kind: None,
            resume_cmd: None,
        };
        app.on_msg(super::super::load::Msg::Rows {
            generation,
            outcome: crate::fanout::Outcome {
                machine: "mm".into(),
                result: Ok(crate::rpc::Response {
                    reply: crate::proto::Reply::Sessions {
                        sessions: vec![session],
                    },
                    took_ms: 1,
                    wire_bytes: 1,
                }),
                elapsed: std::time::Duration::from_millis(3),
                via: crate::fanout::Via::Broker,
            },
        });
        let mut terminal = Terminal::new(TestBackend::new(110, 16)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &mut app);
            })
            .unwrap();
        // Wide characters occupy two cells; skip the placeholder cell after each.
        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..buffer.area.height {
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                screen.push_str(symbol);
                x += UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            screen.push('\n');
        }
        assert!(screen.contains("搜索会话"), "{screen}");
        assert!(screen.contains("最近 1"));
        assert!(screen.contains("mm 1"), "machine state");
        assert!(screen.contains("2m"));
        assert!(screen.contains("把 memex 做成交互界面"));
        assert!(screen.contains("全部来源"));
    }
}
