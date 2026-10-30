//! Ranking sessions for a search. A session's score starts from its best matching record
//! (BM25 × role weight × session-kind weight) and is then adjusted for how much of the
//! session matches, its title and how recently it was active.
//!
//! The settings can be overridden with `SESSIFT_RANK` (comma-separated `key=value`), which
//! `scripts/eval.py` uses to compare variants on the same index, e.g.
//! `SESSIFT_RANK=cross=0,title=0`.

use std::sync::OnceLock;

use super::tokenizer;

#[derive(Debug, Clone, PartialEq)]
pub struct Rank {
    /// Weights of sessions people did not talk to directly.
    pub guardian: f32,
    pub exec: f32,
    pub subagent: f32,
    /// Terms spread over several records of a session match at this fraction of the sum of
    /// their best records' scores; 0 requires every term in one record.
    pub cross: f32,
    /// Bonus per natural log of the number of matching records.
    pub agg: f32,
    /// Bonus when the session title contains the query (scaled by the share of terms).
    pub title: f32,
    /// Bonus for recent activity, halving every `half_life_days`.
    pub recency: f32,
    pub half_life_days: f32,
}

impl Default for Rank {
    fn default() -> Self {
        Self {
            guardian: 0.3,
            exec: 0.6,
            subagent: 0.8,
            cross: 0.5,
            agg: 0.1,
            title: 0.3,
            recency: 0.1,
            half_life_days: 30.0,
        }
    }
}

impl Rank {
    /// The settings in effect: defaults with `SESSIFT_RANK` applied.
    pub fn current() -> &'static Rank {
        static RANK: OnceLock<Rank> = OnceLock::new();
        RANK.get_or_init(|| {
            let mut rank = Rank::default();
            if let Ok(spec) = std::env::var("SESSIFT_RANK")
                && let Err(err) = rank.apply(&spec)
            {
                eprintln!("sessift: SESSIFT_RANK: {err}");
            }
            rank
        })
    }

    pub fn apply(&mut self, spec: &str) -> Result<(), String> {
        for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| format!("expected key=value, got {pair:?}"))?;
            let value: f32 = value
                .trim()
                .parse()
                .map_err(|_| format!("{key}: not a number: {value:?}"))?;
            match key.trim() {
                "guardian" => self.guardian = value,
                "exec" => self.exec = value,
                "subagent" => self.subagent = value,
                // `kind=1` turns the kind weights off.
                "kind" => (self.guardian, self.exec, self.subagent) = (value, value, value),
                "cross" => self.cross = value,
                "agg" => self.agg = value,
                "title" => self.title = value,
                "recency" => self.recency = value,
                "half_life" => self.half_life_days = value.max(0.01),
                other => return Err(format!("unknown setting {other:?}")),
            }
        }
        Ok(())
    }

    pub fn kind_weight(&self, kind: Option<&str>) -> f32 {
        match kind {
            Some("guardian") => self.guardian,
            Some("exec") => self.exec,
            Some("subagent") => self.subagent,
            _ => 1.0,
        }
    }

    /// Score of a session from its matching records, before title and recency.
    pub fn base(&self, best: f32, records: u32) -> f32 {
        best * (1.0 + self.agg * (records.max(1) as f32).ln())
    }

    /// Final score: `base` adjusted for the title and the last activity.
    pub fn adjust(&self, base: f32, title_share: f32, age_ms: i64) -> f32 {
        let days = age_ms.max(0) as f32 / 86_400_000.0;
        let fresh = 0.5f32.powf(days / self.half_life_days);
        base * (1.0 + self.title * title_share) * (1.0 + self.recency * fresh)
    }
}

/// Share of the query terms found in a title (a term counts when all its tokens are there;
/// a lone CJK character when it occurs anywhere).
pub fn title_share(terms: &[String], title: &str) -> f32 {
    if terms.is_empty() {
        return 0.0;
    }
    let have = tokenizer::tokens(title);
    let found = terms
        .iter()
        .filter(|term| {
            let wanted = tokenizer::tokens(term);
            !wanted.is_empty()
                && wanted.iter().all(|w| {
                    if tokenizer::is_single_cjk(w) {
                        title.contains(w.as_str())
                    } else {
                        have.contains(w)
                    }
                })
        })
        .count();
    found as f32 / terms.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_and_override() {
        let mut rank = Rank::default();
        rank.apply("kind=1, cross=0,half_life=7").unwrap();
        assert_eq!(rank.kind_weight(Some("guardian")), 1.0);
        assert_eq!(rank.cross, 0.0);
        assert_eq!(rank.half_life_days, 7.0);
        assert!(rank.apply("cross").is_err());
        assert!(rank.apply("nope=1").is_err());
        assert!(rank.apply("agg=x").is_err());
    }

    #[test]
    fn more_matching_records_and_title_rank_higher() {
        let rank = Rank::default();
        assert_eq!(rank.base(2.0, 1), 2.0);
        assert!(rank.base(2.0, 20) > rank.base(2.0, 2));
        let day = 86_400_000;
        assert!(rank.adjust(1.0, 1.0, 400 * day) > rank.adjust(1.0, 0.0, 400 * day));
        assert!(rank.adjust(1.0, 0.0, day) > rank.adjust(1.0, 0.0, 300 * day));
        let off = Rank {
            title: 0.0,
            recency: 0.0,
            agg: 0.0,
            ..Rank::default()
        };
        assert_eq!(off.adjust(off.base(3.0, 50), 1.0, 0), 3.0);
    }

    #[test]
    fn title_share_counts_terms() {
        let terms = |q: &str| q.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            title_share(&terms("部署 rollback"), "部署失败后怎么回滚"),
            0.5
        );
        assert_eq!(
            title_share(&terms("deploying 回滚"), "Deploy 和回滚脚本"),
            1.0
        );
        assert_eq!(title_share(&terms("滚"), "回滚"), 1.0);
        assert_eq!(title_share(&terms("tantivy"), "分词"), 0.0);
    }
}
