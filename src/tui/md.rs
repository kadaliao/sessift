//! Turning transcript text into styled, wrapped terminal lines: a small Markdown subset
//! (headings, lists, quotes, code fences, `code`, **bold**), query-term highlighting, and
//! wrapping by display width so CJK text and scrolling line counts stay exact.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::theme;
use crate::model::RecordView;
use crate::text;
use crate::timefmt::local_second;

/// One logical line; when wrapped, continuation lines are indented by `hang` columns.
#[derive(Debug, Clone)]
pub struct Para {
    pub spans: Vec<Span<'static>>,
    pub hang: usize,
}

impl Para {
    pub fn new(spans: Vec<Span<'static>>) -> Self {
        Self { spans, hang: 0 }
    }

    pub fn blank() -> Self {
        Self::new(Vec::new())
    }

    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.content.as_ref()).collect()
    }
}

/// Split `segment` into spans, marking every occurrence of a query term.
fn highlighted(segment: &str, style: Style, terms: &[String]) -> Vec<Span<'static>> {
    if terms.is_empty() || segment.is_empty() {
        return vec![Span::styled(segment.to_string(), style)];
    }
    let chars: Vec<char> = segment.chars().collect();
    let lower: Vec<char> = segment.to_lowercase().chars().collect();
    if lower.len() != chars.len() {
        return vec![Span::styled(segment.to_string(), style)];
    }
    let mut marked = vec![false; chars.len()];
    for term in terms {
        let needle: Vec<char> = term.chars().collect();
        if needle.is_empty() || needle.len() > lower.len() {
            continue;
        }
        for start in 0..=lower.len() - needle.len() {
            if lower[start..start + needle.len()] == needle[..] {
                marked[start..start + needle.len()].fill(true);
            }
        }
    }
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let j = (i..chars.len())
            .find(|&k| marked[k] != marked[i])
            .unwrap_or(chars.len());
        let piece: String = chars[i..j].iter().collect();
        let piece_style = if marked[i] {
            style.patch(theme::highlight())
        } else {
            style
        };
        spans.push(Span::styled(piece, piece_style));
        i = j;
    }
    spans
}

/// `code` and **bold** inside one line.
fn inline(line: &str, base: Style, terms: &[String]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let (mut code, mut strong) = (false, false);
    let style_for = |code: bool, strong: bool| {
        let mut style = base;
        if code {
            style = style.fg(theme::CODE);
        }
        if strong {
            style = style.add_modifier(Modifier::BOLD);
        }
        style
    };
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        let toggle = if c == '`' {
            Some(true)
        } else if c == '*' && chars.peek() == Some(&'*') && !code {
            chars.next();
            Some(false)
        } else {
            None
        };
        match toggle {
            Some(is_code) => {
                spans.extend(highlighted(&buf, style_for(code, strong), terms));
                buf.clear();
                if is_code {
                    code = !code;
                } else {
                    strong = !strong;
                }
            }
            None => buf.push(c),
        }
    }
    spans.extend(highlighted(&buf, style_for(code, strong), terms));
    spans.retain(|s| !s.content.is_empty());
    spans
}

/// A small Markdown subset; good enough for agent transcripts.
pub fn markdown(source: &str, base: Style, terms: &[String]) -> Vec<Para> {
    let mut out = Vec::new();
    let mut in_code = false;
    // Terminal cells cannot hold tabs; expand them.
    for raw in text::sanitize_block(source).replace('\t', "    ").lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            let mut spans = vec![Span::styled("│ ", theme::faint())];
            spans.extend(highlighted(raw, Style::default().fg(theme::CODE), terms));
            out.push(Para { spans, hang: 2 });
            continue;
        }
        if trimmed.is_empty() {
            out.push(Para::blank());
            continue;
        }
        let indent = raw.len() - trimmed.len();
        let pad = " ".repeat(indent.min(8));
        if let Some(heading) = trimmed
            .strip_prefix("### ")
            .or_else(|| trimmed.strip_prefix("## "))
            .or_else(|| trimmed.strip_prefix("# "))
        {
            let style = base.fg(theme::ACCENT).add_modifier(Modifier::BOLD);
            out.push(Para::new(inline(heading, style, terms)));
        } else if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let mut spans = vec![Span::raw(format!("{pad}• "))];
            spans.extend(inline(item, base, terms));
            out.push(Para {
                spans,
                hang: pad.len() + 2,
            });
        } else if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled("▎ ", theme::faint())];
            spans.extend(inline(quote, base.add_modifier(Modifier::ITALIC), terms));
            out.push(Para { spans, hang: 2 });
        } else {
            let numbered = trimmed.split_once(". ").filter(|(n, _)| {
                !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit())
            });
            let hang = numbered
                .map(|(n, _)| indent.min(8) + n.len() + 2)
                .unwrap_or(indent.min(8));
            let mut spans = vec![Span::raw(pad)];
            spans.extend(inline(trimmed, base, terms));
            out.push(Para { spans, hang });
        }
    }
    while out.last().is_some_and(|p| p.spans.is_empty()) {
        out.pop();
    }
    out
}

/// Plain text, at most `max_lines` non-empty lines, with a note about the rest.
pub fn plain(source: &str, style: Style, terms: &[String], max_lines: usize) -> Vec<Para> {
    let clean = text::sanitize_block(source).replace('\t', "    ");
    let lines: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out: Vec<Para> = lines
        .iter()
        .take(max_lines)
        .map(|l| Para::new(highlighted(l, style, terms)))
        .collect();
    if lines.len() > max_lines {
        out.push(Para::new(vec![Span::styled(
            format!("… 还有 {} 行", lines.len() - max_lines),
            theme::faint(),
        )]));
    }
    out
}

/// Wrap paragraphs to `width` columns. Continuation lines get each paragraph's hang indent;
/// every paragraph is indented by `margin` columns.
pub fn wrap(paras: &[Para], width: usize, margin: usize) -> Vec<Line<'static>> {
    let width = width.max(margin + 8);
    let mut lines = Vec::new();
    for para in paras {
        let mut line: Vec<Span<'static>> = vec![Span::raw(" ".repeat(margin))];
        let mut used = margin;
        let mut current = String::new();
        let mut current_style = Style::default();
        let flush = |line: &mut Vec<Span<'static>>, current: &mut String, style: Style| {
            if !current.is_empty() {
                line.push(Span::styled(std::mem::take(current), style));
            }
        };
        for span in &para.spans {
            if span.style != current_style {
                flush(&mut line, &mut current, current_style);
                current_style = span.style;
            }
            for ch in span.content.chars() {
                let w = ch.width().unwrap_or(0);
                if used + w > width && used > margin + para.hang {
                    flush(&mut line, &mut current, current_style);
                    lines.push(Line::from(std::mem::take(&mut line)));
                    let indent = margin + para.hang;
                    line.push(Span::raw(" ".repeat(indent)));
                    used = indent;
                    if ch == ' ' {
                        continue;
                    }
                }
                current.push(ch);
                used += w;
            }
        }
        flush(&mut line, &mut current, current_style);
        lines.push(Line::from(line));
    }
    lines
}

pub struct RecordOpts<'a> {
    /// Shorten tool traffic and long messages (previews); the reader shows more.
    pub compact: bool,
    pub expand_tools: bool,
    pub anchor: bool,
    pub terms: &'a [String],
}

/// A transcript record as paragraphs: a header line, then the body. Bookkeeping records
/// (lifecycle markers and the like) render as nothing.
pub fn record(rec: &RecordView, opts: &RecordOpts) -> Vec<Para> {
    let (label, color) = match rec.role.as_str() {
        "user" => ("● 用户".to_string(), theme::USER),
        "assistant" => ("● 助手".to_string(), theme::ASSISTANT),
        "tool_use" => (
            format!(
                "⚙ {}",
                text::sanitize_line(rec.tool_name.as_deref().unwrap_or("工具"))
            ),
            theme::TOOL,
        ),
        "tool_result" => ("↳ 结果".to_string(), theme::MUTED),
        _ => return Vec::new(),
    };
    let mut header = Vec::new();
    if opts.anchor {
        header.push(Span::styled("▶ ", theme::highlight()));
    }
    header.push(Span::styled(
        label,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
    header.push(Span::styled(
        format!("  {}", local_second(rec.ts_ms)),
        theme::faint(),
    ));
    let mut out = vec![Para::new(header)];
    let body = match rec.role.as_str() {
        "user" | "assistant" => {
            let mut paras = markdown(&rec.text, Style::default(), opts.terms);
            let limit = if opts.compact { 30 } else { usize::MAX };
            if paras.len() > limit {
                let rest = paras.len() - limit;
                paras.truncate(limit);
                paras.push(Para::new(vec![Span::styled(
                    format!("… 还有 {rest} 行"),
                    theme::faint(),
                )]));
            }
            paras
        }
        _ => {
            let limit = match (opts.compact, opts.expand_tools, rec.role.as_str()) {
                (_, true, _) => usize::MAX,
                (true, _, "tool_use") => 2,
                (true, _, _) => 3,
                (false, _, "tool_use") => 6,
                (false, _, _) => 10,
            };
            plain(&rec.text, theme::muted(), opts.terms, limit)
        }
    };
    out.extend(body.into_iter().map(|mut p| {
        p.spans.insert(0, Span::raw("  "));
        p.hang += 2;
        p
    }));
    if rec.truncated {
        out.push(Para::new(vec![Span::styled(
            format!("  …（共 {} 字）", rec.total_chars),
            theme::faint(),
        )]));
    }
    out.push(Para::blank());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn inline_markup_is_styled_and_stripped() {
        let paras = markdown("用 `cargo test` 跑 **全部** 测试", Style::default(), &[]);
        assert_eq!(paras.len(), 1);
        assert_eq!(paras[0].text(), "用 cargo test 跑 全部 测试");
        let code = paras[0]
            .spans
            .iter()
            .find(|s| s.content == "cargo test")
            .unwrap();
        assert_eq!(code.style.fg, Some(theme::CODE));
        let strong = paras[0].spans.iter().find(|s| s.content == "全部").unwrap();
        assert!(strong.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn blocks_lists_and_headings() {
        let src = "# 标题\n- 第一项\n  - 子项\n```\nlet x = 1;\n```\n> 引用\n\n\n";
        let texts: Vec<String> = markdown(src, Style::default(), &[])
            .iter()
            .map(Para::text)
            .collect();
        assert_eq!(
            texts,
            vec!["标题", "• 第一项", "  • 子项", "│ let x = 1;", "▎ 引用"]
        );
    }

    #[test]
    fn terms_are_highlighted_case_insensitively() {
        let paras = markdown(
            "Deploy 后部署完成",
            Style::default(),
            &["deploy".into(), "部署".into()],
        );
        let marked: Vec<&str> = paras[0]
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, vec!["Deploy", "部署"]);
    }

    #[test]
    fn tabs_become_spaces() {
        let paras = plain("origin\tgit@github.com", Style::default(), &[], 5);
        assert_eq!(paras[0].text(), "origin    git@github.com");
    }

    #[test]
    fn wrapping_counts_wide_characters_and_hangs() {
        let para = Para {
            spans: vec![Span::raw("• 一二三四五六七八九十")],
            hang: 2,
        };
        let lines = flat(&wrap(&[para], 10, 0));
        assert_eq!(lines, vec!["• 一二三四", "  五六七八", "  九十"]);
        let lines = flat(&wrap(&[Para::new(vec![Span::raw("abc def")])], 12, 2));
        assert_eq!(lines, vec!["  abc def"]);
    }

    #[test]
    fn records_render_by_role() {
        let rec = |role: &str, text: &str| RecordView {
            record_id: "r".into(),
            ts_ms: 1_793_000_000_000,
            role: role.into(),
            tool_name: Some("Bash".into()),
            text: text.into(),
            total_chars: text.chars().count() as u64,
            truncated: false,
        };
        let opts = RecordOpts {
            compact: true,
            expand_tools: false,
            anchor: true,
            terms: &[],
        };
        let user = record(&rec("user", "你好"), &opts);
        assert!(user[0].text().starts_with("▶ ● 用户"));
        assert_eq!(user[1].text(), "  你好");
        let tool = record(&rec("tool_result", "a\nb\nc\nd\ne"), &opts);
        let texts: Vec<String> = tool.iter().map(Para::text).collect();
        assert!(texts.contains(&"  … 还有 2 行".to_string()), "{texts:?}");
        assert!(record(&rec("lifecycle", "Turn started"), &opts).is_empty());
    }
}
