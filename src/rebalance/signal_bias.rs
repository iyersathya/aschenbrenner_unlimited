//! Daily-analysis signal bias. The deterministic rules decide WHAT is permitted
//! (which names may be trimmed or funded and by how much); this layer only
//! decides the ORDER among already-permitted candidates and tempers cash-deploy
//! aggressiveness in a stressed macro regime. It can never authorize a trade a
//! rule didn't already permit (PDF: this is a thesis-driven hold, not a signal
//! chaser).

use crate::core::config::AppConfig;
use crate::portfolio::targets;
use crate::signals::daily_analysis;
use chrono::NaiveDate;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct SignalBias {
    pub enabled: bool,
    /// Signed conviction per ticker in [-1, 1] (+bullish / -bearish).
    pub conviction: HashMap<String, f64>,
    pub strong_bearish: HashSet<String>,
    pub strong_bullish: HashSet<String>,
    /// Run-wide market-regime score (0 stress – 100 calm), when published.
    pub macro_score: Option<i64>,
}

impl SignalBias {
    /// Read the nightly analysis for every invested name (5-day lookback).
    pub fn load(cfg: &AppConfig, asof: NaiveDate) -> Self {
        let mut b = SignalBias { enabled: cfg.signal_bias_enabled, ..Default::default() };
        if !b.enabled {
            return b;
        }
        for t in targets::invested() {
            let a = daily_analysis::read(cfg, t.ticker, asof, 5);
            if !a.found {
                continue;
            }
            b.conviction.insert(t.ticker.to_string(), a.signed_conviction());
            if a.is_strong_bearish() {
                b.strong_bearish.insert(t.ticker.to_string());
            }
            if a.is_strong_bullish() {
                b.strong_bullish.insert(t.ticker.to_string());
            }
        }
        b.macro_score = daily_analysis::macro_score(cfg, asof, 5);
        b
    }

    pub fn conviction_of(&self, ticker: &str) -> f64 {
        if !self.enabled {
            return 0.0;
        }
        self.conviction.get(ticker).copied().unwrap_or(0.0)
    }

    /// Tie-break key for TRIM selection: trim the most-bearish first → ascending
    /// conviction. Lower key = trimmed earlier.
    pub fn trim_priority(&self, ticker: &str) -> f64 {
        self.conviction_of(ticker)
    }

    /// Tie-break key for BUY/deploy selection: fund the most-bullish first →
    /// descending conviction. Higher key = funded earlier.
    pub fn buy_priority(&self, ticker: &str) -> f64 {
        self.conviction_of(ticker)
    }

    /// Macro-regime multiplier on the deployable cash (PDF: cash is optionality,
    /// deploy reluctantly in stress). Calm (≥70) → full; stress (<30) → half;
    /// otherwise linear. No score → neutral 1.0.
    pub fn deploy_multiplier(&self) -> f64 {
        match self.macro_score {
            Some(s) if s < 30 => 0.5,
            Some(s) if s >= 70 => 1.0,
            Some(s) => 0.5 + 0.5 * ((s - 30) as f64 / 40.0),
            None => 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bias(conv: &[(&str, f64)], macro_score: Option<i64>) -> SignalBias {
        SignalBias {
            enabled: true,
            conviction: conv.iter().map(|(t, c)| (t.to_string(), *c)).collect(),
            macro_score,
            ..Default::default()
        }
    }

    #[test]
    fn deploy_multiplier_scales_with_regime() {
        assert_eq!(bias(&[], Some(20)).deploy_multiplier(), 0.5);
        assert_eq!(bias(&[], Some(70)).deploy_multiplier(), 1.0);
        assert_eq!(bias(&[], None).deploy_multiplier(), 1.0);
        let mid = bias(&[], Some(50)).deploy_multiplier();
        assert!((mid - 0.75).abs() < 1e-9);
    }

    #[test]
    fn disabled_bias_is_neutral() {
        let b = SignalBias { enabled: false, ..Default::default() };
        assert_eq!(b.conviction_of("GEV"), 0.0);
    }
}
