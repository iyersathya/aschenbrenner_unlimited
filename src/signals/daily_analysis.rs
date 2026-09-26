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
/// `quant.dilution` (additive, 2026-09-26; SEC EDGAR via the producer). An
/// ABSENT block means the checks did not run — unknown, never "clean".
/// `active_takedown` is a 424B priced within 2 days: the same window as the
/// EDGAR overlay's ActiveOffering, so the two sources agree on what "selling
/// into the tape right now" means and one can stand in when the other fails.
/// `severity` mirrors trader-agent's daily-dilution-scan: severe (active
/// takedown OR share count +15%/yr), moderate (recent takedown OR +7–15%/yr),
/// watch (shelf on file OR +3–7%/yr), clear.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dilution {
    pub severity: String,
    pub active_takedown: bool,
    pub recent_takedown: bool,
    pub shelf: bool,
    pub share_growth_pct_yr: Option<f64>,
    pub quality: String,
}

impl Dilution {
    /// One-line "why" for a card or journal line.
    pub fn detail(&self) -> String {
        let mut parts = vec![];
        if self.active_takedown {
            parts.push("active 424B takedown".to_string());
        } else if self.recent_takedown {
            parts.push("424B takedown ≤90d".to_string());
        }
        if self.shelf {
            parts.push("shelf on file".to_string());
        }
        if let Some(g) = self.share_growth_pct_yr {
            parts.push(format!("shares {g:+.1}%/yr"));
        }
        if parts.is_empty() {
            parts.push(self.severity.clone());
        }
        parts.join(", ")
    }
}

/// `quant.short` (additive, 2026-09-26; FINRA via the producer). Two facts the
/// producer keeps apart and so does this reader: daily short-sale VOLUME graded
/// as a z-score against the name's OWN baseline (flow — the absolute level is
/// 40–60% on nearly every liquid name and carries nothing), and bi-monthly
/// short INTEREST (positioning, up to three weeks old; `settlement_date` says
/// how old). Either half may be absent. Absent block = unknown, never "no
/// shorts".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShortData {
    /// elevated | watch | normal | depressed | insufficient_baseline
    pub daily_read: Option<String>,
    pub daily_z: Option<f64>,
    pub daily_date: Option<String>,
    /// crowded | elevated | building | normal
    pub si_read: Option<String>,
    pub short_pct_float: Option<f64>,
    pub days_to_cover: Option<f64>,
    pub settlement_date: Option<String>,
    pub quality: String,
}

impl ShortData {
    /// One-line positioning summary, always carrying the settlement date so a
    /// two-week-old number is never read as today's.
    pub fn detail(&self) -> String {
        let mut parts = vec![];
        if let Some(p) = self.short_pct_float {
            parts.push(format!("{p:.1}% of float"));
        }
        if let Some(d) = self.days_to_cover {
            parts.push(format!("DTC {d:.1}"));
        }
        if let Some(s) = &self.settlement_date {
            parts.push(format!("settled {s}"));
        }
        parts.join(", ")
    }
}

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
    /// CapEx-cycle / cash-flow sub-block (SEC XBRL; stock_analysis_playbook).
    /// Absent for ETFs and un-analyzed names — treat as "unknown".
    pub fundamentals: Option<Fundamentals>,
    /// Additive 2026-09-26 sub-blocks. `None` = the producer did not run the
    /// check — unknown, never clean (see the struct docs).
    pub dilution: Option<Dilution>,
    pub short: Option<ShortData>,
}

/// The subset of `quant.fundamentals` the long-horizon overlay uses. All TTM;
/// trends are percentage-POINT changes vs the year-ago TTM.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fundamentals {
    pub revenue_yoy_pct: Option<f64>,
    /// OCF-margin trend. Positive while `capex_cycle` ⇒ the spend is funded
    /// by real cash generation (the playbook's strongest health signal).
    pub ocf_margin_trend_pp: Option<f64>,
    /// Operating-margin trend — compression here is the REAL warning
    /// (FCF compression during a CapEx cycle is expected by construction).
    pub op_margin_trend_pp: Option<f64>,
    pub capex_cycle: Option<bool>,
    /// Current P/OCF vs the ticker's OWN 5y median (%): >0 ⇒ above own
    /// history ⇒ expensive regardless of how the multiple looks vs peers.
    pub p_ocf_vs_median_pct: Option<f64>,
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

fn parse_dilution(q: &Value) -> Option<Dilution> {
    let d = q.get("dilution")?.as_object()?;
    let s = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let present = |k: &str| d.get(k).map(|v| v.is_object()).unwrap_or(false);
    Some(Dilution {
        severity: s("severity"),
        active_takedown: present("active_takedown"),
        recent_takedown: present("recent_takedown"),
        shelf: present("shelf"),
        share_growth_pct_yr: d.get("share_growth_pct_yr").and_then(|v| v.as_f64()),
        quality: s("quality"),
    })
}

fn parse_short(q: &Value) -> Option<ShortData> {
    let s = q.get("short")?.as_object()?;
    let dv = s.get("daily_short_volume").and_then(|v| v.as_object());
    let si = s.get("short_interest").and_then(|v| v.as_object());
    let gs = |o: Option<&serde_json::Map<String, Value>>, k: &str| {
        o.and_then(|m| m.get(k)).and_then(|v| v.as_str()).map(String::from)
    };
    let gf = |o: Option<&serde_json::Map<String, Value>>, k: &str| o.and_then(|m| m.get(k)).and_then(|v| v.as_f64());
    Some(ShortData {
        daily_read: gs(dv, "read"),
        daily_z: gf(dv, "z"),
        daily_date: gs(dv, "date"),
        si_read: gs(si, "read"),
        short_pct_float: gf(si, "short_pct_float"),
        days_to_cover: gf(si, "days_to_cover"),
        settlement_date: gs(si, "settlement_date"),
        quality: s.get("quality").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    })
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
        fundamentals: q.get("fundamentals").and_then(|v| v.as_object()).map(|fq| {
            let ff = |k: &str| fq.get(k).and_then(|v| v.as_f64());
            Fundamentals {
                revenue_yoy_pct: ff("revenue_yoy_pct"),
                ocf_margin_trend_pp: ff("ocf_margin_trend_pp"),
                op_margin_trend_pp: ff("op_margin_trend_pp"),
                capex_cycle: fq.get("capex_cycle").and_then(|v| v.as_bool()),
                p_ocf_vs_median_pct: ff("p_ocf_vs_median_pct"),
            }
        }),
        analyst_target: f("analyst_target"),
        analyst_target_upside_pct: f("analyst_target_upside_pct"),
        free_float_pct: f("free_float_pct"),
        float_shares: q.get("float_shares").and_then(|v| v.as_i64()),
        macro_score: q.get("macro_score").and_then(|v| v.as_i64()),
        dilution: parse_dilution(q),
        short: parse_short(q),
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
        // The additive fundamentals sub-block parses from the same fixture.
        let f = q.fundamentals.expect("fundamentals sub-block must parse");
        assert_eq!(f.p_ocf_vs_median_pct, Some(6.64));
        assert_eq!(f.op_margin_trend_pp, Some(0.45));
        assert_eq!(f.capex_cycle, Some(false));
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

    #[test]
    fn parses_dilution_and_short_sub_blocks() {
        let j: Value = serde_json::from_str(
            r#"{"quant":{"price":1.0,
                "dilution":{"severity":"severe","active_takedown":{"form":"424B5","filed":"2026-09-25","age_days":1},
                            "recent_takedown":null,"shelf":{"form":"S-3","filed":"2026-07-02","age_days":86},
                            "share_growth_pct_yr":66.0,"quality":"verified","source":"sec_edgar"},
                "short":{"daily_short_volume":{"date":"2026-09-25","today_pct":65.7,"z":1.9,"read":"watch"},
                         "short_interest":{"settlement_date":"2026-09-15","short_pct_float":24.0,"days_to_cover":5.5,"read":"crowded"},
                         "quality":"verified","source":"finra"}}}"#,
        )
        .unwrap();
        let q = parse_quant(&j).unwrap();
        let d = q.dilution.as_ref().unwrap();
        assert_eq!(d.severity, "severe");
        assert!(d.active_takedown && !d.recent_takedown && d.shelf);
        assert_eq!(d.share_growth_pct_yr, Some(66.0));
        assert!(d.detail().contains("active 424B") && d.detail().contains("+66.0%/yr"));
        let s = q.short.as_ref().unwrap();
        assert_eq!(s.si_read.as_deref(), Some("crowded"));
        assert_eq!(s.daily_read.as_deref(), Some("watch"));
        assert_eq!(s.daily_z, Some(1.9));
        assert!(s.detail().contains("settled 2026-09-15"));
        // Absent blocks parse to None — unknown, never clean.
        let j2: Value = serde_json::from_str(r#"{"quant":{"price":1.0}}"#).unwrap();
        let q2 = parse_quant(&j2).unwrap();
        assert!(q2.dilution.is_none() && q2.short.is_none());
    }
}
