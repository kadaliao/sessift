//! Character-safe truncation, snippets and terminal width helpers. CJK text is common here,
//! so nothing in this module slices by bytes.

use unicode_width::UnicodeWidthChar;

use crate::model::RecordView;

/// First `max` characters of `s`, and whether anything was cut.
pub fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((byte, _)) => (s[..byte].to_string(), true),
        None => (s.to_string(), false),
    }
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Collapse every run of whitespace (including newlines) into one space.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(|ch| ch.width().unwrap_or(0)).sum()
}

/// Truncate to at most `width` terminal columns, ending with `…` when cut.
pub fn fit_width(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Left-align `s` in `width` columns, truncating when needed.
pub fn pad(s: &str, width: usize) -> String {
    let fitted = fit_width(s, width);
    let fill = width.saturating_sub(display_width(&fitted));
    format!("{fitted}{}", " ".repeat(fill))
}

pub fn pad_left(s: &str, width: usize) -> String {
    let fitted = fit_width(s, width);
    let fill = width.saturating_sub(display_width(&fitted));
    format!("{}{fitted}", " ".repeat(fill))
}

/// Make untrusted text safe for a terminal on one line: control characters cannot move the
/// cursor, recolour the screen or set the window title. Whitespace controls become spaces,
/// ESC becomes `␛`, anything else `�`.
pub fn sanitize_line(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            '\x1b' => '␛',
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect()
}

/// Like [`sanitize_line`] but keeps newlines and tabs and drops carriage returns.
pub fn sanitize_block(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\r')
        .map(|c| match c {
            '\n' | '\t' => c,
            '\x1b' => '␛',
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect()
}

/// Query terms used for matching and highlighting: whitespace-separated, lowercase.
pub fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|term| term.trim_matches('"').to_lowercase())
        .filter(|term| !term.is_empty())
        .collect()
}

/// A window of about `max_chars` characters around the first occurrence of any term.
pub fn snippet_around(text: &str, terms: &[String], max_chars: usize) -> String {
    let flat = collapse_ws(text);
    let lower: Vec<char> = flat.to_lowercase().chars().collect();
    let chars: Vec<char> = flat.chars().collect();
    // Lowercasing can change length for a few scripts; fall back to the start then.
    let first = if lower.len() == chars.len() {
        terms
            .iter()
            .filter_map(|term| find_chars(&lower, &term.chars().collect::<Vec<_>>()))
            .min()
    } else {
        None
    };
    let start = first
        .map(|pos| pos.saturating_sub(max_chars / 4))
        .unwrap_or(0);
    let end = (start + max_chars).min(chars.len());
    let start = end.saturating_sub(max_chars).min(start);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}

fn find_chars(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Enforce a shared character budget across a page, in record order. Records past the budget
/// keep their metadata with empty text and `truncated = true`.
/// Fit a page of records into `max_chars`: the record that does not fit is cut and the ones
/// after it are left for the next page. Returns how many records to keep (at least one).
pub fn fit_page(records: &mut [RecordView], max_chars: usize) -> usize {
    let mut remaining = max_chars;
    for (i, record) in records.iter_mut().enumerate() {
        let len = char_len(&record.text);
        if len <= remaining {
            remaining -= len;
            continue;
        }
        if remaining == 0 && i > 0 {
            return i;
        }
        let (kept, _) = truncate_chars(&record.text, remaining);
        record.text = kept;
        record.truncated = true;
        return i + 1;
    }
    records.len()
}

/// Cut each tool call and tool output to `max` characters.
pub fn cap_tools(records: &mut [RecordView], max: usize) {
    for record in records {
        if matches!(record.role.as_str(), "tool_use" | "tool_result")
            && char_len(&record.text) > max
        {
            record.text = truncate_chars(&record.text, max).0;
            record.truncated = true;
        }
    }
}

/// Share `max_chars` out from record `anchor` outwards: the anchor, then what people and
/// assistants wrote, then tool calls and output, each nearest first. The conversation around
/// a hit survives long tool output, and the far ends give way. Returns the records worth
/// showing: the far ends that got nothing at all are left out.
pub fn budget_around(
    records: &mut [RecordView],
    anchor: usize,
    max_chars: usize,
) -> std::ops::Range<usize> {
    let n = records.len();
    if n == 0 {
        return 0..0;
    }
    let anchor = anchor.min(n - 1);
    let mut nearest = Vec::with_capacity(n);
    for d in 1..n {
        if d <= anchor {
            nearest.push(anchor - d);
        }
        if anchor + d < n {
            nearest.push(anchor + d);
        }
    }
    let is_tool = |i: &usize| matches!(records[*i].role.as_str(), "tool_use" | "tool_result");
    let mut order = vec![anchor];
    order.extend(nearest.iter().copied().filter(|i| !is_tool(i)));
    order.extend(nearest.iter().copied().filter(is_tool));
    let mut remaining = max_chars;
    let mut starved = vec![false; n];
    for i in order {
        let record = &mut records[i];
        let len = char_len(&record.text);
        if len <= remaining {
            remaining -= len;
            continue;
        }
        starved[i] = remaining == 0 && i != anchor;
        record.text = truncate_chars(&record.text, remaining).0;
        record.truncated = true;
        remaining = 0;
    }
    let start = (0..anchor).find(|&i| !starved[i]).unwrap_or(anchor);
    let end = (anchor + 1..n)
        .rev()
        .find(|&i| !starved[i])
        .map_or(anchor + 1, |i| i + 1);
    start..end
}

pub fn apply_page_budget(records: &mut [RecordView], max_chars: usize) {
    let mut remaining = max_chars;
    for record in records {
        let len = char_len(&record.text);
        if len <= remaining {
            remaining -= len;
            continue;
        }
        let (kept, _) = truncate_chars(&record.text, remaining);
        record.text = kept;
        record.truncated = true;
        remaining = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_of(role: &str, text: &str) -> RecordView {
        RecordView {
            record_id: String::new(),
            ts_ms: 0,
            role: role.into(),
            tool_name: None,
            text: text.into(),
            total_chars: char_len(text) as u64,
            truncated: false,
        }
    }

    #[test]
    fn tool_records_are_capped() {
        let mut page = vec![
            record_of("user", "abcdef"),
            record_of("tool_result", "abcdef"),
            record_of("tool_use", "ab"),
        ];
        cap_tools(&mut page, 3);
        assert_eq!(page[0].text, "abcdef");
        assert_eq!((page[1].text.as_str(), page[1].truncated), ("abc", true));
        assert_eq!((page[2].text.as_str(), page[2].truncated), ("ab", false));
    }

    #[test]
    fn the_budget_spreads_out_from_the_anchor() {
        let texts = ["aaaa", "bbbb", "HHHH", "cccc", "dddddddd", "eeee"];
        let mut page: Vec<_> = texts.iter().map(|t| record_of("user", t)).collect();
        // Anchor 2 first, then 1, 3, 0, 4: 4 + 4 + 4 + 4 + 2 of the 8 left for 4.
        let keep = budget_around(&mut page, 2, 18);
        assert_eq!(keep, 0..5, "the last one got nothing and is left out");
        assert_eq!(page[0].text, "aaaa");
        assert_eq!(page[1].text, "bbbb");
        assert_eq!(page[3].text, "cccc");
        assert_eq!((page[4].text.as_str(), page[4].truncated), ("dd", true));

        let mut page: Vec<_> = texts.iter().map(|t| record_of("user", t)).collect();
        let keep = budget_around(&mut page, 2, 5);
        assert_eq!(keep, 1..3, "the anchor and one before it");
        assert_eq!(page[2].text, "HHHH");
        assert_eq!(page[1].text, "b");

        // Tool output next to the anchor waits until the conversation further out is served.
        let mut page = vec![
            record_of("user", "question"),
            record_of("tool_result", "a long tool output"),
            record_of("assistant", "HIT"),
            record_of("tool_use", "a long tool call"),
            record_of("assistant", "answer"),
        ];
        let keep = budget_around(&mut page, 2, 3 + 8 + 6 + 2);
        assert_eq!(keep, 0..5);
        assert_eq!(page[0].text, "question");
        assert_eq!(page[4].text, "answer");
        assert_eq!(page[1].text, "a ");
        assert_eq!(page[3].text, "", "starved, but kept: it is not at an end");
    }

    #[test]
    fn pages_stop_where_the_budget_runs_out() {
        let record = |text: &str| RecordView {
            record_id: String::new(),
            ts_ms: 0,
            role: "user".into(),
            tool_name: None,
            text: text.into(),
            total_chars: char_len(text) as u64,
            truncated: false,
        };
        let mut page = vec![record("abcd"), record("efgh"), record("ijkl")];
        assert_eq!(fit_page(&mut page, 6), 2);
        assert_eq!(page[1].text, "ef");
        assert!(page[1].truncated);
        let mut page = vec![record("abcd"), record("efgh")];
        assert_eq!(fit_page(&mut page, 4), 1, "nothing left for the second");
        assert!(!page[0].truncated);
        let mut page = vec![record("abcdef"), record("g")];
        assert_eq!(fit_page(&mut page, 3), 1, "the first record is always kept");
        assert_eq!(page[0].text, "abc");
        let mut page = vec![record("ab"), record("cd")];
        assert_eq!(fit_page(&mut page, 10), 2);
    }

    #[test]
    fn truncation_never_splits_characters() {
        assert_eq!(truncate_chars("部署完成", 2), ("部署".to_string(), true));
        assert_eq!(truncate_chars("ab", 5), ("ab".to_string(), false));
        assert_eq!(truncate_chars("", 0), (String::new(), false));
    }

    #[test]
    fn width_helpers_count_wide_characters() {
        assert_eq!(display_width("回滚ab"), 6);
        assert_eq!(fit_width("回滚还来得及", 7), "回滚还…");
        assert_eq!(pad("回滚", 6), "回滚  ");
        assert_eq!(pad_left("7", 3), "  7");
    }

    #[test]
    fn snippet_centres_on_a_match_inside_chinese_text() {
        let text = format!(
            "{}在那之前回滚还来得及{}",
            "前".repeat(100),
            "后".repeat(100)
        );
        let snippet = snippet_around(&text, &query_terms("回滚"), 20);
        assert!(snippet.contains("回滚"), "{snippet}");
        assert!(snippet.starts_with('…') && snippet.ends_with('…'));
        assert_eq!(snippet.chars().filter(|c| *c != '…').count(), 20);
    }

    #[test]
    fn page_budget_is_shared_in_order() {
        let record = |text: &str| RecordView {
            record_id: String::new(),
            ts_ms: 0,
            role: "user".into(),
            tool_name: None,
            text: text.into(),
            total_chars: text.chars().count() as u64,
            truncated: false,
        };
        let mut page = vec![record("一二三"), record("四五六七"), record("八")];
        apply_page_budget(&mut page, 5);
        assert_eq!(page[0].text, "一二三");
        assert!(!page[0].truncated);
        assert_eq!(page[1].text, "四五");
        assert!(page[1].truncated);
        assert_eq!(page[2].text, "");
        assert!(page[2].truncated);
    }

    #[test]
    fn terminal_controls_are_neutralised() {
        let hostile = "ok\x1b[2J\x1b]0;pwned\x07 line\r\nnext\tcol\u{9b}x";
        assert_eq!(
            sanitize_line(hostile),
            "ok␛[2J␛]0;pwned\u{fffd} line  next col\u{fffd}x"
        );
        assert_eq!(
            sanitize_block(hostile),
            "ok␛[2J␛]0;pwned\u{fffd} line\nnext\tcol\u{fffd}x"
        );
        assert_eq!(sanitize_line("部署 ok"), "部署 ok");
    }

    #[test]
    fn collapse_whitespace() {
        assert_eq!(collapse_ws("  a \n\n b\tc  "), "a b c");
    }
}
