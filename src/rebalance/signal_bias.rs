//! Daily-analysis signal bias. The deterministic rules decide WHAT is permitted
//! (which names may be trimmed or funded and by how much); this layer only
//! decides the ORDER among already-permitted candidates and tempers cash-deploy
//! aggressiveness in a stressed macro regime. It can never authorize a trade a
//! rule didn't already permit (PDF: this is a thesis-driven hold, not a signal
//! chaser).

use crate::core::config::AppConfig;
use crate::portfolio::targets;
use crate::signals::daily_analysis::{self, Fundamentals};
use chrono::NaiveDate;
use std::collections::{HashMap, HashSet};

/// Weight of the fundamentals tilt inside the ordering keys. Deliberately
/// secondary to the LLM conviction (which spans ±1): fundamentals refine the
/// order among peers, they never dominate a strong signal.
const FUND_TILT_WEIGHT: f64 = 0.25;

#[derive(Debug, Clone, Default)]
pub struct SignalBias {
    pub enabled: bool,
    /// Signed conviction per ticker in [-1, 1] (+bullish / -bearish).
    pub conviction: HashMap<String, f64>,
    pub strong_bearish: HashSet<String>,
    pub strong_bullish: HashSet<String>,
    /// Run-wide market-regime score (0 stress – 100 calm), when published.
    pub macro_score: Option<i64>,
    /// Fundamentals tilt per ticker in [-1, 1] (stock_analysis_playbook read:
    /// cheap-vs-own-history + margin trends + CapEx-cycle health). 0 when the
    /// record carries no fundamentals (ETFs, thin filers).
    pub fund_tilt: HashMap<String, f64>,
}

/// The playbook read, reduced to one signed tilt:
/// - valuation vs OWN history dominates (±50% from the 5y P/OCF median
///   saturates the term) — cheap is positive;
/// - operating-margin compression is the real warning (±5pp saturates);
/// - a CapEx cycle funded by expanding OCF margin earns a bonus, one that
///   cash generation is NOT funding takes a penalty.
pub fn fundamentals_tilt(f: &Fundamentals) -> f64 {
    let mut tilt = 0.0;
    let mut weight = 0.0;
    if let Some(v) = f.p_ocf_vs_median_pct {
        tilt += 0.55 * (-v / 50.0).clamp(-1.0, 1.0);
        weight += 0.55;
    }
    if let Some(t) = f.op_margin_trend_pp {
        tilt += 0.30 * (t / 5.0).clamp(-1.0, 1.0);
        weight += 0.30;
    }
    if let (Some(true), Some(o)) = (f.capex_cycle, f.ocf_margin_trend_pp) {
        tilt += if o > 0.0 { 0.15 } else { -0.15 };
        weight += 0.15;
    }
    if weight == 0.0 {
        return 0.0;
    }
    (tilt / weight).clamp(-1.0, 1.0)
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
            if let Some(f) = a.quant.as_ref().and_then(|q| q.fundamentals.as_ref()) {
                b.fund_tilt.insert(t.ticker.to_string(), fundamentals_tilt(f));
            }
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

    pub fn fund_tilt_of(&self, ticker: &str) -> f64 {
        if !self.enabled {
            return 0.0;
        }
        self.fund_tilt.get(ticker).copied().unwrap_or(0.0)
    }

    /// Tie-break key for TRIM selection: trim the most-bearish first → ascending
    /// conviction, with a secondary fundamentals tilt (an expensive name with
    /// compressing margins trims ahead of a cheap, healthy one at equal
    /// conviction). Lower key = trimmed earlier. Ordering only — the rules
    /// already authorized every candidate.
    pub fn trim_priority(&self, ticker: &str) -> f64 {
        self.conviction_of(ticker) + FUND_TILT_WEIGHT * self.fund_tilt_of(ticker)
    }

    /// Tie-break key for BUY/deploy selection: fund the most-bullish first →
    /// descending conviction, fundamentals tilt secondary (cheap-vs-own-history
    /// with healthy margins funds ahead at equal conviction). Higher key =
    /// funded earlier.
    pub fn buy_priority(&self, ticker: &str) -> f64 {
        self.conviction_of(ticker) + FUND_TILT_WEIGHT * self.fund_tilt_of(ticker)
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
        assert_eq!(b.fund_tilt_of("GEV"), 0.0);
    }

    #[test]
    fn fundamentals_tilt_reads_the_playbook_signals() {
        // Cheap vs own history + expanding op margin + funded CapEx cycle → strongly positive.
        let healthy = Fundamentals {
            p_ocf_vs_median_pct: Some(-30.0),
            op_margin_trend_pp: Some(3.0),
            capex_cycle: Some(true),
            ocf_margin_trend_pp: Some(4.0),
            ..Default::default()
        };
        assert!(fundamentals_tilt(&healthy) > 0.5);
        // Expensive + compressing margins + unfunded spend → strongly negative.
        let sick = Fundamentals {
            p_ocf_vs_median_pct: Some(60.0),
            op_margin_trend_pp: Some(-6.0),
            capex_cycle: Some(true),
            ocf_margin_trend_pp: Some(-2.0),
            ..Default::default()
        };
        assert!(fundamentals_tilt(&sick) < -0.5);
        // No data → exactly neutral, never a fabricated opinion.
        assert_eq!(fundamentals_tilt(&Fundamentals::default()), 0.0);
        // FCF-style panic input is absent by design: only OCF/op-margin/valuation feed the tilt.
    }

    #[test]
    fn tilt_is_secondary_to_conviction_in_ordering() {
        let mut b = bias(&[("CHEAP", 0.3), ("RICH", 0.3)], None);
        b.fund_tilt.insert("CHEAP".into(), 1.0);
        b.fund_tilt.insert("RICH".into(), -1.0);
        // Equal conviction → tilt decides the order…
        assert!(b.buy_priority("CHEAP") > b.buy_priority("RICH"));
        assert!(b.trim_priority("RICH") < b.trim_priority("CHEAP")); // RICH trims first
        // …but a decisive conviction gap outweighs a full-scale tilt.
        let mut b2 = bias(&[("LOVED", 0.9), ("HATED", -0.9)], None);
        b2.fund_tilt.insert("LOVED".into(), -1.0);
        b2.fund_tilt.insert("HATED".into(), 1.0);
        assert!(b2.buy_priority("LOVED") > b2.buy_priority("HATED"));
    }
}
