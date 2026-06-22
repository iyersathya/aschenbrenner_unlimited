//! Reader for the shared nightly multi-agent analysis (trading-agents-scheduler).
//!
//! Looks up `<daily_analysis_dir>/<date>/<TICKER>.json` (today, falling back to
//! the most recent prior date present) and folds its `direction`/`confidence`
//! into a directional signal. Graceful-neutral when missing or `status != ok`.
//! In this sleeve the signal only BIASES rule-authorized rebalance choices — it
//! never creates a trade on its own (see rebalance::signal_bias).

use crate::core::config::AppConfig;
use chrono::{Duration, NaiveDate};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum Direction {
    Bullish,
    Bearish,
    Neutral,
}

/// Optional structured market context (the schema's additive `quant` block).
/// Parsed whole from the contract; individual fields are read as the rebalance
/// overlay grows to use them.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quant {
    pub source: Option<String>,
    pub price: Option<f64>,
    pub as_of: Option<String>,
    pub realized_vol_20d: Option<f64>,
    pub atr_pct_14d: Option<f64>,
    pub range_position_52w: Option<f64>,
    pub avg_dollar_volume_20d: Option<f64>,
    pub iv: Option<f64>,
    pub iv_vs_realized: Option<f64>,
    pub expected_move_30d_pct: Option<f64>,
    pub iv_rank: Option<f64>,
    pub next_earnings_date: Option<String>,
    pub days_to_earnings: Option<i64>,
    pub analyst_target: Option<f64>,
    pub analyst_target_upside_pct: Option<f64>,
    pub free_float_pct: Option<f64>,
    pub float_shares: Option<i64>,
    /// Market-regime gate (0 stress – 100 calm), identical across the batch.
    pub macro_score: Option<i64>,
}

#[allow(dead_code)] // ticker/trade_date/source_path/quant carried for callers + digests
#[derive(Debug, Clone)]
pub struct DailyAnalysis {
    pub ticker: String,
    pub direction: Direction,
    pub confidence: f64,
    pub decision: String,
    pub trade_date: String,
    /// Whether a real (status=ok) record was found vs. a neutral fallback.
    pub found: bool,
    pub source_path: Option<PathBuf>,
    pub quant: Option<Quant>,
}

impl DailyAnalysis {
    fn neutral(ticker: &str) -> Self {
        DailyAnalysis {
            ticker: ticker.into(),
            direction: Direction::Neutral,
            confidence: 0.0,
            decision: String::new(),
            trade_date: String::new(),
            found: false,
            source_path: None,
            quant: None,
        }
    }

    /// Strong-bearish per the contract's tiering (confidence ≥ 0.8). Used to flag
    /// a held name for thesis re-validation and to prioritize trims.
    pub fn is_strong_bearish(&self) -> bool {
        self.found && self.direction == Direction::Bearish && self.confidence >= 0.8
    }

    /// Strong-bullish (confidence ≥ 0.8). Prioritizes a name when deploying cash.
    pub fn is_strong_bullish(&self) -> bool {
        self.found && self.direction == Direction::Bullish && self.confidence >= 0.8
    }

    /// Signed conviction in [-1, 1]: + bullish, - bearish, 0 neutral/absent.
    pub fn signed_conviction(&self) -> f64 {
        match self.direction {
            Direction::Bullish => self.confidence,
            Direction::Bearish => -self.confidence,
            Direction::Neutral => 0.0,
        }
    }
}

fn parse_dir(s: &str) -> Direction {
    match s.trim().to_lowercase().as_str() {
        "bullish" | "buy" | "overweight" => Direction::Bullish,
        "bearish" | "sell" | "underweight" => Direction::Bearish,
        _ => Direction::Neutral,
    }
}

fn parse_quant(j: &Value) -> Option<Quant> {
    let q = j.get("quant")?;
    if !q.is_object() {
        return None;
    }
    let s = |k: &str| q.get(k).and_then(|v| v.as_str()).map(String::from);
    let f = |k: &str| q.get(k).and_then(|v| v.as_f64());
    Some(Quant {
        source: s("source"),
        price: f("price"),
        as_of: s("as_of"),
        realized_vol_20d: f("realized_vol_20d"),
        atr_pct_14d: f("atr_pct_14d"),
        range_position_52w: f("range_position_52w"),
        avg_dollar_volume_20d: f("avg_dollar_volume_20d"),
        iv: f("iv"),
        iv_vs_realized: f("iv_vs_realized"),
        expected_move_30d_pct: f("expected_move_30d_pct"),
        iv_rank: f("iv_rank"),
        next_earnings_date: s("next_earnings_date"),
        days_to_earnings: q.get("days_to_earnings").and_then(|v| v.as_i64()),
        analyst_target: f("analyst_target"),
        analyst_target_upside_pct: f("analyst_target_upside_pct"),
        free_float_pct: f("free_float_pct"),
        float_shares: q.get("float_shares").and_then(|v| v.as_i64()),
        macro_score: q.get("macro_score").and_then(|v| v.as_i64()),
    })
}

/// Read the analysis for `ticker`, searching today then up to `lookback_days`
/// prior days (skips weekends/holidays implicitly by file presence).
pub fn read(cfg: &AppConfig, ticker: &str, asof: NaiveDate, lookback_days: i64) -> DailyAnalysis {
    let base = cfg.daily_analysis_dir();
    for back in 0..=lookback_days.max(0) {
        let date = asof - Duration::days(back);
        let path = base.join(date.format("%Y-%m-%d").to_string()).join(format!("{}.json", ticker));
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(j) = serde_json::from_str::<Value>(&text) else { continue };
        if j.get("status").and_then(|v| v.as_str()) != Some("ok") {
            continue;
        }
        let direction = j.get("direction").and_then(|v| v.as_str()).map(parse_dir).unwrap_or(Direction::Neutral);
        let confidence = j.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0);
        return DailyAnalysis {
            ticker: ticker.into(),
            direction,
            confidence,
            decision: j.get("decision").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            trade_date: j.get("trade_date").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            found: true,
            source_path: Some(path),
            quant: parse_quant(&j),
        };
    }
    DailyAnalysis::neutral(ticker)
}

/// The run-wide market-regime score (0 stress – 100 calm) from `<date>/macro.json`,
/// falling back through prior days. `None` when absent.
pub fn macro_score(cfg: &AppConfig, asof: NaiveDate, lookback_days: i64) -> Option<i64> {
    let base = cfg.daily_analysis_dir();
    for back in 0..=lookback_days.max(0) {
        let date = asof - Duration::days(back);
        let path = base.join(date.format("%Y-%m-%d").to_string()).join("macro.json");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(j) = serde_json::from_str::<Value>(&text) else { continue };
        if let Some(s) = j.get("score").and_then(|v| v.as_i64()) {
            return Some(s);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_dir(dir: PathBuf) -> AppConfig {
        let mut c = AppConfig::default();
        c.daily_analysis_path = Some(dir);
        c
    }

    #[test]
    fn reads_ok_record_and_parses_direction() {
        let root = std::env::temp_dir().join(format!("asch-da-{}", std::process::id()));
        let day = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        let dir = root.join("2026-05-25");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("CEG.json"),
            r#"{"status":"ok","direction":"bearish","confidence":0.85,"decision":"Sell","trade_date":"2026-05-25"}"#,
        )
        .unwrap();
        let cfg = cfg_with_dir(root.clone());
        let a = read(&cfg, "CEG", day, 5);
        assert!(a.found);
        assert_eq!(a.direction, Direction::Bearish);
        assert!(a.is_strong_bearish());
        assert_eq!(a.confidence, 0.85);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_is_neutral_fallback() {
        let cfg = cfg_with_dir(std::env::temp_dir().join("asch-da-absent-xyz"));
        let a = read(&cfg, "GEV", NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(), 3);
        assert!(!a.found);
        assert_eq!(a.direction, Direction::Neutral);
        assert_eq!(a.signed_conviction(), 0.0);
    }

    // ---- Cross-project contract tests (vendored canonical fixtures) ---------

    fn write_fixture(root: &std::path::Path, date: &str, ticker: &str, fixture: &str) {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture);
        let body = std::fs::read_to_string(&src)
            .unwrap_or_else(|e| panic!("missing vendored fixture {src:?}: {e}"));
        let dir = root.join(date);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{ticker}.json")), body).unwrap();
    }

    #[test]
    fn reads_canonical_ok_contract_fixture() {
        let root = std::env::temp_dir().join(format!("asch-da-contract-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_fixture(&root, "2026-06-08", "AAPL", "daily_analysis_contract_v1_buy.json");
        let cfg = cfg_with_dir(root.clone());
        let day = NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        let a = read(&cfg, "AAPL", day, 5);
        assert!(a.found, "ok record must be found");
        assert_eq!(a.direction, Direction::Bullish);
        assert_eq!(a.confidence, 0.85);
        assert_eq!(a.decision, "Buy");
        assert_eq!(a.trade_date, "2026-06-08");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn canonical_fail_contract_fixture_is_neutral_fallback() {
        let root = std::env::temp_dir().join(format!("asch-da-contract-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_fixture(&root, "2026-06-08", "AAPL", "daily_analysis_contract_v1_fail.json");
        let cfg = cfg_with_dir(root.clone());
        let day = NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        let a = read(&cfg, "AAPL", day, 5);
        assert!(!a.found, "fail record must degrade to neutral fallback");
        assert_eq!(a.direction, Direction::Neutral);
        assert_eq!(a.confidence, 0.0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_canonical_quant_contract_fixture() {
        let root = std::env::temp_dir().join(format!("asch-da-contract-quant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_fixture(&root, "2026-06-08", "AAPL", "daily_analysis_contract_v1_buy_quant.json");
        let cfg = cfg_with_dir(root.clone());
        let day = NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        let a = read(&cfg, "AAPL", day, 5);
        assert!(a.found);
        let q = a.quant.expect("quant block must parse from the enriched fixture");
        assert_eq!(q.source.as_deref(), Some("alpaca+fmp"));
        assert_eq!(q.price, Some(196.5));
        assert_eq!(q.macro_score, Some(61));
        assert_eq!(q.analyst_target, Some(235.0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ok_record_without_quant_has_none() {
        let root = std::env::temp_dir().join(format!("asch-da-noquant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_fixture(&root, "2026-06-08", "AAPL", "daily_analysis_contract_v1_buy.json");
        let cfg = cfg_with_dir(root.clone());
        let day = NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        let a = read(&cfg, "AAPL", day, 5);
        assert!(a.found);
        assert!(a.quant.is_none(), "record without a quant block must yield None");
        let _ = std::fs::remove_dir_all(&root);
    }
}
