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
