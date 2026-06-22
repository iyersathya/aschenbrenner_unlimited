//! Alpaca market-data client (REST, data.alpaca.markets). Stocks (latest price +
//! daily bars) and options (LEAPS): OCC symbol round-trip, monthly-expiry helper,
//! and an option mid-quote fetch. Network failures degrade gracefully, never panic.

use chrono::{Datelike, Duration, NaiveDate, Weekday};

pub const DATA_URL: &str = "https://data.alpaca.markets";

/// Build an OCC option symbol: ROOT + YYMMDD + C/P + strike×1000 zero-padded to 8.
pub fn build_occ(underlying: &str, expiry: NaiveDate, is_call: bool, strike: f64) -> String {
    format!(
        "{}{}{}{:08}",
        underlying.to_uppercase(),
        expiry.format("%y%m%d"),
        if is_call { "C" } else { "P" },
        crate::core::numr::pyround(strike * 1000.0, 0) as i64
    )
}

/// Parse an OCC symbol → (underlying, expiry, is_call, strike). Inverse of `build_occ`.
pub fn parse_occ(symbol: &str) -> Option<(String, NaiveDate, bool, f64)> {
    if symbol.len() < 15 {
        return None;
    }
    let body = &symbol[symbol.len() - 15..];
    let root = &symbol[..symbol.len() - 15];
    let b = body.as_bytes();
    let yy: i32 = body.get(0..2)?.parse().ok()?;
    let mm: u32 = body.get(2..4)?.parse().ok()?;
    let dd: u32 = body.get(4..6)?.parse().ok()?;
    let is_call = (b[6] as char).to_ascii_uppercase() == 'C';
    let strike: f64 = body.get(7..15)?.parse::<i64>().ok()? as f64 / 1000.0;
    let expiry = NaiveDate::from_ymd_opt(2000 + yy, mm, dd)?;
    Some((root.to_string(), expiry, is_call, strike))
}

/// The standard monthly option expiry (3rd Friday) of a given year/month.
pub fn third_friday(year: i32, month: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let offset = (7 + Weekday::Fri.num_days_from_monday()
        - first.weekday().num_days_from_monday())
        % 7;
    Some(first + Duration::days(offset as i64 + 14))
}

/// One daily OHLCV bar.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Bar {
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

#[derive(Clone)]
pub struct AlpacaDataClient {
    data_url: String,
    api_key: String,
    secret_key: String,
    client: reqwest::Client,
}

impl AlpacaDataClient {
    pub fn new(api_key: &str, secret_key: &str, data_url: &str) -> Self {
        AlpacaDataClient {
            data_url: data_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            secret_key: secret_key.to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn from_config(cfg: &crate::core::config::AppConfig) -> Option<Self> {
        if cfg.alpaca_api_key.is_empty() || cfg.alpaca_secret_key.is_empty() {
            return None;
        }
        Some(Self::new(&cfg.alpaca_api_key, &cfg.alpaca_secret_key, DATA_URL))
    }

    async fn get(&self, path: &str, params: &[(&str, String)]) -> Option<serde_json::Value> {
        let resp = self
            .client
            .get(format!("{}/{}", self.data_url, path.trim_start_matches('/')))
            .header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.secret_key)
            .header("Accept", "application/json")
            .query(params)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        resp.json().await.ok()
    }

    /// Most recent usable price for `symbol` (latest trade → daily/minute bars).
    pub async fn get_stock_price(&self, symbol: &str) -> Option<f64> {
        let data = self
            .get(&format!("v2/stocks/{}/snapshot", symbol), &[("feed", "iex".into())])
            .await?;
        for key in ["latestTrade", "dailyBar", "minuteBar", "prevDailyBar"] {
            if let Some(node) = data.get(key) {
                let px = if key == "latestTrade" { node.get("p") } else { node.get("c") };
                if let Some(px) = px.and_then(|v| v.as_f64()) {
                    if px > 0.0 {
                        return Some(px);
                    }
                }
            }
        }
        None
    }

    /// Daily OHLCV bars, oldest→newest. `limit` caps the count returned.
    /// (Used by `fifty_two_week_high`; also a building block for future metrics.)
    #[allow(dead_code)]
    pub async fn get_daily_bars(&self, symbol: &str, limit: i64) -> Vec<Bar> {
        let start = today() - Duration::days((limit as f64 * 1.6) as i64 + 15);
        let params = [
            ("timeframe", "1Day".to_string()),
            ("start", start.format("%Y-%m-%d").to_string()),
            ("limit", limit.max(1).to_string()),
            ("adjustment", "all".to_string()),
            ("sort", "desc".to_string()),
            ("feed", "iex".to_string()),
        ];
        let data = match self.get(&format!("v2/stocks/{}/bars", symbol), &params).await {
            Some(d) => d,
            None => return vec![],
        };
        let mut out = vec![];
        if let Some(arr) = data.get("bars").and_then(|b| b.as_array()) {
            for b in arr {
                let ts = b.get("t").and_then(|v| v.as_str()).unwrap_or("");
                let date = ts
                    .get(0..10)
                    .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
                let (Some(date), Some(c)) = (date, b.get("c").and_then(|v| v.as_f64())) else {
                    continue;
                };
                out.push(Bar {
                    date,
                    open: b.get("o").and_then(|v| v.as_f64()).unwrap_or(c),
                    high: b.get("h").and_then(|v| v.as_f64()).unwrap_or(c),
                    low: b.get("l").and_then(|v| v.as_f64()).unwrap_or(c),
                    close: c,
                    volume: b.get("v").and_then(|v| v.as_f64()).unwrap_or(0.0),
                });
            }
        }
        out.reverse(); // fetched newest-first; callers expect oldest→newest
        out
    }

    /// Mid price for an OCC option symbol (latest quote mid → last trade), in
    /// per-share terms (×100 for the contract dollar value). `None` when the
    /// chain has no quote — the caller treats that as "contract unavailable".
    pub async fn get_option_mid(&self, occ: &str) -> Option<f64> {
        let data = self
            .get(
                "v1beta1/options/snapshots",
                &[("symbols", occ.to_string()), ("feed", "indicative".into())],
            )
            .await?;
        let snap = data.get("snapshots")?.get(occ)?;
        if let Some(q) = snap.get("latestQuote") {
            let bp = q.get("bp").and_then(|v| v.as_f64());
            let ap = q.get("ap").and_then(|v| v.as_f64());
            if let (Some(b), Some(a)) = (bp, ap) {
                if a > 0.0 {
                    return Some(crate::core::numr::pyround((b + a) / 2.0, 2));
                }
            }
        }
        snap.get("latestTrade").and_then(|t| t.get("p")).and_then(|v| v.as_f64())
    }

    /// Highest high over roughly the last 52 weeks (≈252 trading bars). `None`
    /// when no bars are available. Seeds per-name high-water marks for the
    /// drawdown-deploy rule when a name has no recorded history yet.
    #[allow(dead_code)]
    pub async fn fifty_two_week_high(&self, symbol: &str) -> Option<f64> {
        let bars = self.get_daily_bars(symbol, 252).await;
        bars.iter()
            .map(|b| b.high)
            .filter(|h| *h > 0.0)
            .fold(None, |acc: Option<f64>, h| Some(acc.map_or(h, |a| a.max(h))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occ_roundtrip() {
        let exp = NaiveDate::from_ymd_opt(2028, 1, 21).unwrap();
        let occ = build_occ("NVDA", exp, true, 300.0);
        assert_eq!(occ, "NVDA280121C00300000");
        let (root, e, call, strike) = parse_occ(&occ).unwrap();
        assert_eq!(root, "NVDA");
        assert_eq!(e, exp);
        assert!(call);
        assert_eq!(strike, 300.0);
    }

    #[test]
    fn third_friday_matches_known_expiries() {
        assert_eq!(third_friday(2027, 1).unwrap(), NaiveDate::from_ymd_opt(2027, 1, 15).unwrap());
        assert_eq!(third_friday(2028, 1).unwrap(), NaiveDate::from_ymd_opt(2028, 1, 21).unwrap());
    }
}
