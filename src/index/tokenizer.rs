//! Mixed Chinese/English tokenizer, used for both indexing and queries.
//!
//! - Runs of letters and digits become one lowercase token; pure ASCII words are stemmed
//!   (English Snowball), so `tests` finds `test`.
//! - Runs of Han, kana and Hangul become overlapping two-character tokens (bigrams); a run of
//!   one character stays a single token. A Chinese word is then a phrase of bigrams, which is
//!   how `回滚` is found inside `在那之前回滚还来得及`.
//! - Everything else separates tokens. Full-width ASCII is folded to ASCII.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

pub const NAME: &str = "sessift";
/// Longer letter/digit runs (base64, minified code) are dropped; they only bloat the index.
const MAX_WORD_CHARS: usize = 48;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Word,
    Cjk,
    Other,
}

fn fold(c: char) -> char {
    match c {
        '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap_or(c),
        _ => c,
    }
}

pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30ff      // Hiragana, Katakana
        | 0x31f0..=0x31ff    // Katakana extensions
        | 0x3400..=0x4dbf    // CJK Extension A
        | 0x4e00..=0x9fff    // CJK Unified Ideographs
        | 0xac00..=0xd7af    // Hangul syllables
        | 0x1100..=0x11ff    // Hangul Jamo
        | 0x3130..=0x318f    // Hangul compatibility Jamo
        | 0xf900..=0xfaff    // CJK compatibility ideographs
        | 0x20000..=0x3134f  // CJK Extensions B–G
    )
}

fn class(c: char) -> Class {
    if is_cjk(c) {
        Class::Cjk
    } else if c.is_alphanumeric() {
        Class::Word
    } else {
        Class::Other
    }
}

/// Tokenize `text`, calling `emit(token_text, byte_from, byte_to)` in order. Positions are
/// the call order.
pub fn tokenize(text: &str, mut emit: impl FnMut(&str, usize, usize)) {
    walk(text, |t, from, to| {
        emit(t, from, to);
        true
    });
}

/// Like [`tokenize`], stopping as soon as `emit` returns `false`.
pub fn walk(text: &str, mut emit: impl FnMut(&str, usize, usize) -> bool) {
    let stemmer = frostem::Stemmer::new(frostem::Algorithm::English);
    let mut word = String::new();
    let mut run: Vec<(usize, char)> = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        match class(fold(c)) {
            Class::Other => {
                chars.next();
            }
            Class::Word => {
                word.clear();
                let mut end = start;
                let mut count = 0;
                while let Some(&(i, original)) = chars.peek() {
                    let c = fold(original);
                    if class(c) != Class::Word {
                        break;
                    }
                    word.extend(c.to_lowercase());
                    count += 1;
                    end = i + original.len_utf8();
                    chars.next();
                }
                if count > MAX_WORD_CHARS {
                    continue;
                }
                let go = if word.bytes().all(|b| b.is_ascii_lowercase()) {
                    emit(&stemmer.stem(&word), start, end)
                } else {
                    emit(&word, start, end)
                };
                if !go {
                    return;
                }
            }
            Class::Cjk => {
                run.clear();
                while let Some(&(i, c)) = chars.peek() {
                    if !is_cjk(c) {
                        break;
                    }
                    run.push((i, c));
                    chars.next();
                }
                let end_of = |k: usize| run[k].0 + run[k].1.len_utf8();
                if run.len() == 1 {
                    let mut buf = [0u8; 4];
                    if !emit(run[0].1.encode_utf8(&mut buf), run[0].0, end_of(0)) {
                        return;
                    }
                    continue;
                }
                let mut pair = String::with_capacity(8);
                for k in 0..run.len() - 1 {
                    pair.clear();
                    pair.push(run[k].1);
                    pair.push(run[k + 1].1);
                    if !emit(&pair, run[k].0, end_of(k + 1)) {
                        return;
                    }
                }
            }
        }
    }
}

/// Token texts of `text`, in order.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    tokenize(text, |t, _, _| out.push(t.to_string()));
    out
}

/// Whether a token is a lone CJK character (queried by "contains" rather than equality,
/// since the index only holds it inside bigrams).
pub fn is_single_cjk(token: &str) -> bool {
    let mut chars = token.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if is_cjk(c))
}

#[derive(Clone, Default)]
pub struct MixedTokenizer {
    tokens: Vec<Token>,
}

pub struct MixedTokenStream<'a> {
    tokens: &'a [Token],
    index: usize,
    current: Token,
}

impl Tokenizer for MixedTokenizer {
    type TokenStream<'a> = MixedTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> MixedTokenStream<'a> {
        self.tokens.clear();
        let tokens = &mut self.tokens;
        tokenize(text, |t, from, to| {
            let position = tokens.len();
            tokens.push(Token {
                offset_from: from,
                offset_to: to,
                position,
                text: t.to_string(),
                position_length: 1,
            });
        });
        MixedTokenStream {
            tokens: &self.tokens,
            index: 0,
            current: Token::default(),
        }
    }
}

impl TokenStream for MixedTokenStream<'_> {
    fn advance(&mut self) -> bool {
        match self.tokens.get(self.index) {
            Some(token) => {
                self.current.clone_from(token);
                self.index += 1;
                true
            }
            None => false,
        }
    }

    fn token(&self) -> &Token {
        &self.current
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_runs_become_bigrams() {
        assert_eq!(
            tokens("在那之前回滚还来得及"),
            vec![
                "在那", "那之", "之前", "前回", "回滚", "滚还", "还来", "来得", "得及"
            ]
        );
        // The query side yields the same bigram, so `回滚` is a plain term match.
        assert_eq!(tokens("回滚"), vec!["回滚"]);
        assert_eq!(tokens("回滚还"), vec!["回滚", "滚还"]);
        assert_eq!(tokens("好"), vec!["好"]);
    }

    #[test]
    fn mixed_text_splits_on_script_changes() {
        assert_eq!(
            tokens("用 git rebase 处理v2版本，OK？"),
            vec!["用", "git", "rebas", "处理", "v2", "版本", "ok"]
        );
        assert_eq!(tokens("Ｒｏｌｌｂａｃｋ"), tokens("rollback"));
        assert_eq!(
            tokens("snake_case/path-name"),
            vec!["snake", "case", "path", "name"]
        );
        assert_eq!(tokens("Running tests"), vec!["run", "test"]);
        assert_eq!(tokens("日本語のテキスト"), tokens("日本語のテキスト"));
        assert!(tokens("日本語のテキスト").contains(&"テキ".to_string()));
    }

    #[test]
    fn long_runs_are_dropped_but_hashes_kept() {
        let sha = "3f786850e387550fdab836ed7e6dc881de23001b";
        assert_eq!(tokens(sha), vec![sha]);
        assert!(tokens(&"A".repeat(200)).is_empty());
    }

    #[test]
    fn offsets_point_at_the_source_text() {
        let text = "部署 Deploying 回滚";
        let mut spans = Vec::new();
        tokenize(text, |t, from, to| {
            spans.push((t.to_string(), &text[from..to]))
        });
        assert_eq!(
            spans,
            vec![
                ("部署".to_string(), "部署"),
                ("deploy".to_string(), "Deploying"),
                ("回滚".to_string(), "回滚"),
            ]
        );
        let wide = "ＡＢ";
        let mut spans = Vec::new();
        tokenize(wide, |_, from, to| spans.push(&wide[from..to]));
        assert_eq!(spans, vec!["ＡＢ"]);
    }

    #[test]
    fn single_cjk_detection() {
        assert!(is_single_cjk("滚"));
        assert!(!is_single_cjk("回滚"));
        assert!(!is_single_cjk("a"));
    }
}
