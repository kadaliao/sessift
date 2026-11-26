//! Drawing. Pure functions of the app state, except for layout caches kept on the state.

use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Filter, Focus, MachineState, PreviewState, Range, Regions, Row};
use super::load::Preview;
use super::md::{self, Para, RecordOpts};
use super::theme;
use crate::model::RecordView;
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

/// Draw everything, and note where things went for mouse clicks.
pub fn draw(frame: &mut Frame, app: &mut App) {
    app.regions = Regions::default();
    let area = frame.area();
    if app.reader.is_some() {
        draw_reader(frame, area, app);
        return;
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
    app.regions.search = search;
    draw_search(frame, search, app);
    draw_status(frame, status, app);
    let list_area = if app.split && body.width >= 60 {
        let [list, preview] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(body);
        app.regions.preview = preview;
        draw_preview_pane(frame, preview, app);
        list
    } else {
        body
    };
    app.regions.list = list_area;
    draw_list(frame, list_area, app);
    draw_footer(frame, footer, app);
    if app.peek {
        draw_peek(frame, area, app);
    } else if app.menu.is_some() {
        draw_menu(frame, area, app);
    }
}

fn hover_bg() -> Style {
    Style::default().bg(theme::SELECTED_BG)
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

fn draw_status(frame: &mut Frame, area: Rect, app: &mut App) {
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
    // Filters, right-aligned; each one opens its dropdown on click.
    let in_query = app.focus == Focus::Query;
    let mut chips: Vec<(Filter, String, bool)> = Vec::new();
    if app.query.trim().is_empty() {
        chips.push((Filter::Kinds, app.kinds_label().into(), app.all_kinds));
    }
    chips.push((
        Filter::Machine,
        app.machine_label(),
        app.machine_choice.is_some(),
    ));
    chips.push((Filter::Source, app.source_label(), app.source.is_some()));
    chips.push((
        Filter::Time,
        app.range.label().into(),
        app.range != Range::All,
    ));
    const GAP: u16 = 3;
    let chips: Vec<(Filter, Line)> = chips
        .into_iter()
        .map(|(filter, label, active)| {
            let key = if in_query {
                format!("Alt-{} ", filter.key())
            } else {
                format!("{} ", filter.key())
            };
            let label = Span::styled(
                format!("{} ▾", text::sanitize_line(&label)),
                if active {
                    theme::accent()
                } else {
                    theme::muted()
                },
            );
            (
                filter,
                Line::from(vec![Span::styled(key, theme::faint()), label]),
            )
        })
        .collect();
    let total: u16 = chips.iter().map(|(_, l)| l.width() as u16).sum::<u16>()
        + GAP * (chips.len() as u16).saturating_sub(1);
    let start = area
        .right()
        .saturating_sub(total)
        .max(area.x + 10.min(area.width));
    let mut x = start;
    for (filter, line) in chips {
        let width = line.width() as u16;
        let rect = Rect {
            x,
            y: area.y,
            width,
            height: 1,
        }
        .intersection(area);
        let open = app.menu.as_ref().is_some_and(|m| m.filter == filter);
        let hovered = app.mouse.is_some_and(|p| rect.contains(p));
        let line = if open || hovered {
            line.style(hover_bg())
        } else {
            line
        };
        frame.render_widget(Paragraph::new(line), rect);
        app.regions.chips.push((rect, filter));
        x = x.saturating_add(width + GAP);
    }
    let left_area = Rect {
        width: start.saturating_sub(area.x + 1),
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(left)), left_area);
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
    let mut rest = width.saturating_sub(used);
    let body_style = if selected {
        theme::bold()
    } else {
        Style::default()
    };
    if app.matches_mode {
        // The session's title first, so a hit is easy to place; then what matched, dimmer.
        // Too narrow for both (beside the preview, which shows the hit), only the title.
        let title = text::sanitize_line(&row.title);
        let snippet_style = if text::title_in_snippet(&row.title, &row.text) {
            body_style
        } else if rest < 48 {
            spans.extend(md::highlighted(&fit_width(&title, rest), body_style, terms));
            rest = 0;
            body_style
        } else {
            let title_w = display_width(&title).min(rest * 2 / 5);
            let title = fit_width(&title, title_w);
            spans.extend(md::highlighted(&title, body_style, terms));
            spans.push(Span::styled("  ", body_style));
            rest = rest.saturating_sub(title_w + 2);
            theme::muted()
        };
        if rest > 0 {
            let body = fit_width(&text::sanitize_line(&row.text), rest);
            let paras = md::markdown(&body, snippet_style, terms);
            if let Some(first) = paras.into_iter().next() {
                spans.extend(first.spans);
            }
        }
    } else {
        let body = fit_width(&text::sanitize_line(&row.text), rest);
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

/// A session's title, or a muted placeholder.
fn title_span(title: &str, style: Style) -> Span<'static> {
    if title.trim().is_empty() {
        Span::styled("（无标题）", theme::muted())
    } else {
        Span::styled(text::sanitize_line(title), style)
    }
}

/// What a preview shows, in display order: records, and how many were left out between them.
enum Shown<'a> {
    Record(&'a RecordView),
    Gap(u32),
}

fn preview_items(preview: &Preview, newest_first: bool) -> Vec<Shown<'_>> {
    let mut items = Vec::new();
    let mut at = 0;
    for part in &preview.parts {
        if part.offset > at {
            items.push(Shown::Gap(part.offset - at));
        }
        items.extend(part.records.iter().map(Shown::Record));
        at = part.end();
    }
    if let Some(total) = preview.total
        && total > at
    {
        items.push(Shown::Gap(total - at));
    }
    if newest_first {
        items.reverse();
    }
    items
}

/// The preview of the selected row, laid out for `width` columns.
fn preview_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(row) = app.selected_row() else {
        return Vec::new();
    };
    let mut lines = md::wrap(
        &[Para::new(vec![title_span(&row.title, theme::bold())])],
        width,
        0,
    );
    lines.extend([
        Line::from(vec![
            Span::styled(
                if row.project.is_empty() {
                    "-".to_string()
                } else {
                    text::sanitize_line(&row.project)
                },
                theme::muted(),
            ),
            Span::raw("  "),
            Span::styled("● ", Style::default().fg(theme::source_color(&row.source))),
            Span::styled(text::sanitize_line(&row.source), theme::muted()),
            Span::styled(format!("  {}", row.key.machine), theme::faint()),
        ]),
        Line::from(vec![
            Span::styled(
                format!(
                    "{}{}  ·  ",
                    age(row.ts_ms),
                    row.message_count
                        .map(|n| format!("  ·  {n} 条"))
                        .unwrap_or_default()
                ),
                theme::faint(),
            ),
            Span::styled(app.order_label(), theme::muted()),
            Span::styled(
                format!(
                    "（O 切换）  ·  {}",
                    text::sanitize_line(&row.key.session_id)
                ),
                theme::faint(),
            ),
        ]),
        Line::styled("─".repeat(width.min(200)), theme::faint()),
    ]);
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
            for item in preview_items(preview, app.newest_first) {
                match item {
                    Shown::Gap(omitted) => {
                        paras.push(Para::new(vec![Span::styled(
                            format!("    ··· 省略 {omitted} 条 ···"),
                            theme::faint(),
                        )]));
                        paras.push(Para::blank());
                    }
                    Shown::Record(record) => {
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
                }
            }
            if !preview.parts.iter().any(|p| !p.records.is_empty()) {
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

/// Clear `rect` for an overlay. A wide character just left of it would still be drawn across
/// the overlay's first column, and the terminal backend skips the column a wide character
/// covers, so the left border went missing there. Herdr's popups have the same problem.
fn clear_overlay(frame: &mut Frame, rect: Rect) {
    frame.render_widget(Clear, rect);
    let buf = frame.buffer_mut();
    if rect.x <= buf.area.x {
        return;
    }
    for y in rect.top()..rect.bottom().min(buf.area.bottom()) {
        let cell = &mut buf[(rect.x - 1, y)];
        if cell.symbol().width() > 1 {
            cell.set_symbol(" ");
        }
    }
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
    app.regions.peek = rect;
    // The footer underneath does not apply while peeking.
    app.regions.buttons.clear();
    clear_overlay(frame, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Span::styled(" 速览 ", theme::bold()));
    let inner = block.inner(rect).inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    frame.render_widget(block, rect);
    // Hints sit on the bottom border.
    if rect.height >= 2 {
        let bottom = Rect {
            x: rect.x + 1,
            y: rect.bottom() - 1,
            width: rect.width.saturating_sub(2),
            height: 1,
        };
        let mut spans = vec![Span::raw(" ")];
        hints(
            &mut spans,
            bottom,
            &[
                ("Enter", "恢复", Some(KeyCode::Enter)),
                ("o", "全文", Some(KeyCode::Char('o'))),
                ("j/k", "滚动", None),
                ("O", "切换顺序", Some(KeyCode::Char('O'))),
                ("n", "下一条", Some(KeyCode::Char('n'))),
                ("p", "上一条", Some(KeyCode::Char('p'))),
                ("Esc", "关闭", Some(KeyCode::Esc)),
            ],
            app.mouse,
            &mut app.regions.buttons,
        );
        spans.push(Span::raw(" "));
        let line = Line::from(spans);
        let used = Rect {
            width: (line.width() as u16).min(bottom.width),
            ..bottom
        };
        frame.render_widget(Paragraph::new(line), used);
    }
    let lines = preview_lines(app, inner.width as usize);
    let mut scroll = app.preview_scroll;
    render_scrolled(frame, inner, lines, &mut scroll);
    app.preview_scroll = scroll;
}

/// A hint: key, what it does, and the key a click presses (none for key groups like `j/k`).
type Hint<'a> = (&'a str, &'a str, Option<KeyCode>);

/// Append hints to `spans`, which start at the left of `area`; clickable ones go into `buttons`.
fn hints(
    spans: &mut Vec<Span<'static>>,
    area: Rect,
    pairs: &[Hint],
    mouse: Option<Position>,
    buttons: &mut Vec<(Rect, KeyCode)>,
) {
    let mut x = area
        .x
        .saturating_add(spans.iter().map(|s| s.width()).sum::<usize>() as u16);
    for (i, &(key, label, press)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            x = x.saturating_add(2);
        }
        let width = (display_width(key) + 1 + display_width(label)) as u16;
        let rect = Rect {
            x,
            y: area.y,
            width,
            height: 1,
        }
        .intersection(area);
        let bg = if press.is_some() && mouse.is_some_and(|p| rect.contains(p)) {
            hover_bg()
        } else {
            Style::default()
        };
        spans.push(Span::styled(key.to_string(), theme::accent().patch(bg)));
        spans.push(Span::styled(format!(" {label}"), theme::muted().patch(bg)));
        if let Some(code) = press {
            buttons.push((rect, code));
        }
        x = x.saturating_add(width);
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &mut App) {
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
    let filters = match (app.focus, app.query.trim().is_empty()) {
        (Focus::Query, true) => "Alt-s/t/m/a",
        (Focus::Query, false) => "Alt-s/t/m",
        (Focus::List, true) => "s/t/m/a",
        (Focus::List, false) => "s/t/m",
    };
    let pairs: Vec<Hint> = match app.focus {
        Focus::Query => vec![
            ("输入", "搜索", None),
            ("↓/Enter", "选会话", Some(KeyCode::Enter)),
            (filters, "筛选", None),
            ("Tab", "分栏预览", Some(KeyCode::Tab)),
            ("Esc", "清空/退出", Some(KeyCode::Esc)),
        ],
        Focus::List => {
            let mut pairs = vec![
                ("Enter/r", "恢复", Some(KeyCode::Enter)),
                ("e", edit, Some(KeyCode::Char('e'))),
                ("Space", "速览", Some(KeyCode::Char(' '))),
                ("Tab", "分栏", Some(KeyCode::Tab)),
                ("o", "全文", Some(KeyCode::Char('o'))),
                ("y/Y", "复制ID/命令", None),
                (filters, "筛选", None),
                ("/", "搜索", Some(KeyCode::Char('/'))),
                ("q", "退出", Some(KeyCode::Char('q'))),
            ];
            if app.split {
                pairs.insert(4, ("O", "切换顺序", Some(KeyCode::Char('O'))));
            }
            pairs
        }
    };
    hints(
        &mut spans,
        area,
        &pairs,
        app.mouse,
        &mut app.regions.buttons,
    );
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The dropdown of a filter, under its chip.
fn draw_menu(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(menu) = &app.menu else {
        return;
    };
    let Some(&(chip, _)) = app.regions.chips.iter().find(|(_, f)| *f == menu.filter) else {
        return;
    };
    let label_w = menu
        .options
        .iter()
        .map(|o| display_width(&text::sanitize_line(o)))
        .max()
        .unwrap_or(0);
    let width = (label_w + 6) as u16;
    let height = menu.options.len() as u16 + 2;
    let rect = Rect {
        x: chip.x.min(area.right().saturating_sub(width)).max(area.x),
        y: chip.y + 1,
        width,
        height,
    }
    .intersection(area);
    clear_overlay(frame, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::faint());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let mut lines = Vec::new();
    for (i, option) in menu.options.iter().enumerate() {
        let current = i == menu.current;
        let mark = if current { "✓ " } else { "  " };
        let mut style = if current {
            theme::accent()
        } else {
            Style::default()
        };
        if i == menu.hover {
            style = style.patch(hover_bg());
        }
        lines.push(Line::styled(
            format!(
                " {mark}{} ",
                text::pad(&text::sanitize_line(option), label_w)
            ),
            style,
        ));
        // One rect per option, even clipped ones, so indices line up.
        app.regions.menu.push(
            Rect {
                x: inner.x,
                y: inner.y + i as u16,
                width: inner.width,
                height: 1,
            }
            .intersection(inner),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);
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
    let title = title_span(&row.title, theme::bold());
    let title = Span::styled(
        fit_width(&title.content, header.width as usize),
        title.style,
    );
    let loaded = reader.records.len();
    let header_lines = vec![
        Line::from(title),
        Line::from(vec![
            Span::styled("● ", Style::default().fg(theme::source_color(&row.source))),
            Span::styled(text::sanitize_line(&row.source), theme::muted()),
            Span::styled(
                format!(
                    "  {}  {}  {}  ·  已加载 {loaded}{} 条  ·  ",
                    text::sanitize_line(&row.project),
                    row.key.machine,
                    text::sanitize_line(&row.key.session_id),
                    reader.total.map(|t| format!("/{t}")).unwrap_or_default()
                ),
                theme::faint(),
            ),
            Span::styled(
                if reader.newest_first {
                    "最新在前"
                } else {
                    "最早在前"
                },
                theme::muted(),
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
        reader.start,
        reader.newest_first,
        reader.find.clone(),
    );
    if reader.layout_key.as_ref() != Some(&layout_key) {
        // Keep the record at the top where it is: pages added above it, a narrower window or
        // unfolded tools would otherwise move the text under the reader's eyes.
        let keep = reader.top_record();
        let mut lines = Vec::new();
        let mut starts = std::collections::HashMap::new();
        let mut line_starts = Vec::new();
        for record in reader.shown() {
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
            line_starts.push((lines.len(), record.record_id.clone()));
            lines.extend(md::wrap(&paras, width, 0));
        }
        reader.lines = lines;
        reader.record_lines = starts;
        reader.line_starts = line_starts;
        reader.layout_key = Some(layout_key);
        if let Some((id, into)) = keep
            && let Some(&line) = reader.record_lines.get(&id)
        {
            reader.scroll = line + into;
        }
    }
    reader.view_height = body.height as usize;
    if reader.jump_to_anchor
        && let Some(id) = &reader.row.record_id
        && let Some(&line) = reader.record_lines.get(id)
    {
        reader.scroll = line.saturating_sub(2);
        reader.jump_to_anchor = false;
    }
    if reader.to_bottom && !reader.records.is_empty() {
        reader.scroll = reader.lines.len().saturating_sub(body.height as usize);
        reader.to_bottom = false;
    }
    let mut lines = reader.lines.clone();
    if reader.records.is_empty() && reader.error.is_none() {
        lines.push(if reader.loading || reader.total != Some(0) {
            Line::from(vec![
                Span::styled(SPINNER[tick % SPINNER.len()], theme::accent()),
                Span::styled(" 加载中…", theme::muted()),
            ])
        } else {
            Line::styled("（没有可显示的消息）", theme::muted())
        });
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
    let loading = reader.loading && !reader.records.is_empty();
    let mut spans = Vec::new();
    if let Some(message) = status {
        spans.push(Span::styled(format!("{message}   "), theme::accent()));
    }
    let edit = if from_shell {
        "放到命令行"
    } else {
        "打印命令"
    };
    let position = if loading {
        format!("{} {percent}%", SPINNER[tick % SPINNER.len()])
    } else {
        format!("{percent}%")
    };
    let [l, r] = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(display_width(&position) as u16 + 1),
    ])
    .areas(footer);
    hints(
        &mut spans,
        l,
        &[
            ("j/k", "滚动", None),
            ("d/u", "半页", None),
            ("g/G", "首尾", None),
            ("O", "切换顺序", Some(KeyCode::Char('O'))),
            ("/", "查找", Some(KeyCode::Char('/'))),
            ("n/N", "下/上一个", None),
            (
                "t",
                if reader.expand_tools {
                    "收起工具"
                } else {
                    "展开工具"
                },
                Some(KeyCode::Char('t')),
            ),
            ("Enter", "恢复", Some(KeyCode::Enter)),
            ("e", edit, Some(KeyCode::Char('e'))),
            ("q", "返回", Some(KeyCode::Char('q'))),
        ],
        app.mouse,
        &mut app.regions.buttons,
    );
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

    use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn an_overlay_keeps_its_left_border_over_wide_characters() {
        // Frame one: a line of wide characters; frame two: an overlay whose left edge falls in
        // the middle of one. Only what the backend is sent ends up in its buffer.
        let mut terminal = Terminal::new(TestBackend::new(12, 4)).unwrap();
        let line = "中文中文中文";
        terminal
            .draw(|f| f.render_widget(Paragraph::new(line), f.area()))
            .unwrap();
        let rect = Rect::new(3, 0, 6, 3);
        terminal
            .draw(|f| {
                f.render_widget(Paragraph::new(line), f.area());
                clear_overlay(f, rect);
                f.render_widget(Block::default().borders(Borders::ALL), rect);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(3, 0)].symbol(), "┌");
        assert_eq!(buf[(3, 1)].symbol(), "│");
        assert_eq!(buf[(2, 0)].symbol(), " ", "the cut character gives way");
    }

    fn sessions_app(ids: &[(&str, &str)]) -> App {
        let mut app = App::new(vec!["local".into(), "mm".into()], String::new(), true);
        app.take_effects();
        let generation = app.generation;
        let sessions = ids
            .iter()
            .enumerate()
            .map(|(i, (id, source))| crate::model::SessionSummary {
                source: source.to_string(),
                session_id: id.to_string(),
                source_path: format!("/{id}"),
                project: "p".into(),
                cwd: None,
                title: Some(format!("title {id}")),
                started_ms: None,
                last_ms: now_ms() - 60_000 * (i as i64 + 1),
                message_count: Some(3),
                kind: None,
                resume_cmd: None,
            })
            .collect();
        app.on_msg(super::super::load::Msg::Rows {
            generation,
            outcome: crate::fanout::Outcome {
                machine: "local".into(),
                result: Ok(crate::rpc::Response {
                    reply: crate::proto::Reply::Sessions { sessions },
                    took_ms: 1,
                    wire_bytes: 1,
                }),
                elapsed: std::time::Duration::from_millis(3),
                via: crate::fanout::Via::Local,
            },
        });
        app
    }

    /// Draw, then return the cell where `needle` starts.
    fn locate(terminal: &mut Terminal<TestBackend>, app: &mut App, needle: &str) -> (u16, u16) {
        terminal.draw(|f| draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let mut row = String::new();
            let mut xs = Vec::new();
            let mut x = 0;
            while x < buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                for _ in symbol.chars() {
                    xs.push(x);
                }
                row.push_str(symbol);
                x += UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            if let Some(byte) = row.find(needle) {
                return (xs[row[..byte].chars().count()], y);
            }
        }
        panic!("{needle} not on screen");
    }

    fn click(app: &mut App, (column, row): (u16, u16)) {
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    #[test]
    fn mouse_opens_filter_menus_and_picks() {
        let mut app = sessions_app(&[("a", "claude"), ("b", "codex")]);
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        // The search box has focus, so the chips show the Alt keys.
        locate(&mut terminal, &mut app, "Alt-s 全部来源");
        let chip = locate(&mut terminal, &mut app, "全部来源");
        click(&mut app, chip);
        assert_eq!(app.menu.as_ref().unwrap().filter, Filter::Source);
        let current = locate(&mut terminal, &mut app, "✓ 全部来源");
        assert!(current.1 > chip.1, "menu drops below the chip");
        let options = &app.menu.as_ref().unwrap().options;
        assert_eq!(options, &["全部来源", "claude", "codex"]);
        let codex = app.regions.menu[2];
        assert_eq!(codex.y, current.1 + 2);
        click(&mut app, (codex.x + 3, codex.y));
        assert!(app.menu.is_none());
        assert_eq!(app.source.as_deref(), Some("codex"));
        assert!(app.take_effects().iter().any(|e| matches!(e,
            super::super::app::Effect::Fetch { op: crate::proto::Op::Sessions(req), .. }
                if req.filters.source.as_deref() == Some("codex"))));

        // A click outside closes the menu without touching what is under it.
        let chip = locate(&mut terminal, &mut app, "全部时间");
        click(&mut app, chip);
        assert!(app.menu.is_some());
        click(&mut app, (0, 0));
        assert!(app.menu.is_none());
        assert_eq!(app.range, Range::All);
    }

    #[test]
    fn mouse_selects_double_click_resumes_and_hints_press_keys() {
        let mut app = sessions_app(&[("a", "claude"), ("b", "codex")]);
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        let second = locate(&mut terminal, &mut app, "title b");
        click(&mut app, second);
        assert_eq!(app.focus, Focus::List);
        assert_eq!(app.selected, 1);
        assert!(app.take_effects().is_empty(), "one click only selects");
        click(&mut app, second);
        assert!(app.take_effects().iter().any(|e| matches!(
            e,
            super::super::app::Effect::Command {
                purpose: super::super::load::Purpose::Run,
                ..
            }
        )));
        app.pending = None;

        // The list footer: Space peeks; Esc on the overlay's border closes it.
        let peek = locate(&mut terminal, &mut app, "Space 速览");
        click(&mut app, peek);
        assert!(app.peek);
        let close = locate(&mut terminal, &mut app, "Esc 关闭");
        click(&mut app, close);
        assert!(!app.peek);

        // Clicking the search box puts the cursor where it was clicked.
        app.query = "部署 vercel".into();
        app.cursor = 0;
        let box_at = locate(&mut terminal, &mut app, "vercel");
        click(&mut app, (box_at.0 + 1, box_at.1));
        assert_eq!(app.focus, Focus::Query);
        assert_eq!(app.cursor, 4);
    }

    #[test]
    fn wheel_scrolls_what_is_under_the_mouse() {
        let mut app = sessions_app(&[("a", "claude"), ("b", "codex"), ("c", "claude")]);
        app.split = true;
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let wheel = |app: &mut App, (column, row): (u16, u16)| {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let list = app.regions.list;
        let preview = app.regions.preview;
        wheel(&mut app, (preview.x + 2, preview.y + 1));
        assert_eq!(app.selected, 0);
        assert_eq!(app.preview_scroll, 3);
        wheel(&mut app, (list.x + 2, list.y));
        assert_eq!(app.selected, 1);
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < buffer.area.width {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    x += UnicodeWidthStr::width(symbol).max(1) as u16;
                }
                row
            })
            .collect()
    }

    fn record(i: u32) -> RecordView {
        RecordView {
            record_id: format!("r{i}"),
            ts_ms: 1_700_000_000_000 + i as i64 * 60_000,
            role: if i.is_multiple_of(2) {
                "user"
            } else {
                "assistant"
            }
            .into(),
            tool_name: None,
            text: format!("message {i}"),
            total_chars: 9,
            truncated: false,
        }
    }

    #[test]
    fn previews_put_the_newest_first_or_last() {
        use super::super::load::Part;
        let preview = Preview {
            parts: vec![
                Part {
                    offset: 0,
                    records: (0..2).map(record).collect(),
                },
                Part {
                    offset: 8,
                    records: (8..10).map(record).collect(),
                },
            ],
            total: Some(12),
        };
        let names = |newest| -> Vec<String> {
            preview_items(&preview, newest)
                .into_iter()
                .map(|item| match item {
                    Shown::Record(r) => r.record_id.clone(),
                    Shown::Gap(n) => format!("gap {n}"),
                })
                .collect()
        };
        assert_eq!(
            names(false),
            ["r0", "r1", "gap 6", "r8", "r9", "gap 2"],
            "the tail that did not come back is counted too"
        );
        assert_eq!(names(true), ["gap 2", "r9", "r8", "gap 6", "r1", "r0"]);

        let mut app = sessions_app(&[("a", "claude")]);
        app.split = true;
        app.focus = Focus::List;
        let key = app.preview_key().unwrap();
        app.previews.insert(key, PreviewState::Ready(preview));
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let screen = screen_text(&terminal).join("\n");
        assert!(screen.contains("title a"), "the title heads the preview");
        assert!(screen.contains("最新在前"), "{screen}");
        assert!(screen.contains("O 切换顺序"), "split footer hint");
        let at = |s: &str| screen.find(s).unwrap_or_else(|| panic!("{s}: {screen}"));
        assert!(at("message 9") < at("message 8"));
        assert!(at("message 8") < at("省略 6 条"));
        assert!(at("省略 6 条") < at("message 0"));
    }

    #[test]
    fn hits_show_the_session_title_before_the_snippet() {
        let mut app = App::new(vec!["local".into()], "部署".into(), true);
        app.take_effects();
        let generation = app.generation;
        let hit = |id: &str, title: &str, snippet: &str| crate::model::SearchHit {
            score: 1.0,
            ts_ms: now_ms(),
            source: "claude".into(),
            session_id: id.into(),
            source_path: format!("/{id}"),
            record_id: format!("{id}-0-0"),
            project: "shop".into(),
            role: "user".into(),
            snippet: snippet.into(),
            title: Some(title.into()),
        };
        app.on_msg(super::super::load::Msg::Rows {
            generation,
            outcome: crate::fanout::Outcome {
                machine: "local".into(),
                result: Ok(crate::rpc::Response {
                    reply: crate::proto::Reply::Search {
                        hits: vec![
                            hit("a", "订单接口 502 排查", "回滚之后再部署一次"),
                            hit(
                                "b",
                                "帮我部署到预发环境",
                                "帮我部署到预发环境，然后跑一遍冒烟",
                            ),
                        ],
                    },
                    took_ms: 1,
                    wire_bytes: 1,
                }),
                elapsed: std::time::Duration::from_millis(3),
                via: crate::fanout::Via::Local,
            },
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let screen = screen_text(&terminal);
        let row = screen.iter().find(|l| l.contains("回滚之后")).unwrap();
        let (title, snippet) = (row.find("订单接口 502 排查"), row.find("回滚之后"));
        assert!(title.unwrap() < snippet.unwrap(), "{row}");
        // The title is the first question and the hit is in it: shown once.
        let row = screen.iter().find(|l| l.contains("冒烟")).unwrap();
        assert_eq!(row.matches("帮我部署到预发环境").count(), 1, "{row}");

        // Beside the preview there is room for the title only.
        app.split = true;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let screen = screen_text(&terminal);
        let row = screen.iter().find(|l| l.contains("订单接口")).unwrap();
        let list = row.split('│').next().unwrap();
        assert!(!list.contains("回滚之后"), "{row}");
    }

    #[test]
    fn the_reader_keeps_its_place_when_pages_arrive_above() {
        let mut app = sessions_app(&[("a", "claude")]);
        app.focus = Focus::List;
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        let key = app.reader.as_ref().unwrap().row.key.clone();
        let page = |offset: u32, count: u32| crate::model::Page {
            session_id: "a".into(),
            source_path: "/a".into(),
            offset,
            total: Some(100),
            next_offset: None,
            records: (offset..offset + count).map(record).collect(),
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        app.on_msg(super::super::load::Msg::Page {
            key: key.clone(),
            generation: 0,
            result: Ok(page(40, 20)),
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let reader = app.reader.as_mut().unwrap();
        assert_eq!(reader.top_record().unwrap().0, "r59", "newest first");
        reader.scroll = reader.record_lines["r50"] + 1;
        // Newer records arrive; newest first they go above what is on screen.
        app.on_msg(super::super::load::Msg::Page {
            key,
            generation: 0,
            result: Ok(page(60, 10)),
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let reader = app.reader.as_ref().unwrap();
        assert_eq!(reader.top_record(), Some(("r50".to_string(), 1)));
        assert_eq!(reader.shown().next().unwrap().record_id, "r69");
        let screen = screen_text(&terminal).join("\n");
        assert!(screen.contains("已加载 30/100 条"), "{screen}");
    }
}
