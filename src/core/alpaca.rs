//! Alpaca paper-trading client (aschenbrenner account). Equity-only surface:
//! account, positions, stock orders, cancel. The HARD paper-account guard runs
//! on every order/cancel path — a non-`PA*` account is an immediate halt, never
//! an order. No market orders: rebalance fills use marketable LIMIT orders only.

use serde_json::{json, Value};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AlpacaError {
    #[error("Alpaca cold-start failure: {0}")]
    ColdStart(String),
    #[error("ALPACA PAPER-GUARD VIOLATION: detected non-paper account {0}. All order submission halted. Regenerate paper keys immediately.")]
    PaperGuard(String),
    #[error("Alpaca order error: {0}")]
    Order(String),
    #[error("Alpaca request failed: {0}")]
    Request(String),
}

/// Submitted-order summary returned by Alpaca.
#[allow(dead_code)] // fields surfaced in logs/cards; not every one read on every path
#[derive(Debug, Clone, Default)]
pub struct Order {
    pub id: String,
    pub symbol: String,
    pub qty: String,
    pub side: String,
    pub kind: String,
    pub status: String,
    pub filled_qty: String,
    pub filled_avg_price: Option<String>,
    pub limit_price: Option<String>,
}

// Broker fields parsed from /v2/account — kept whole even where not yet read.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct AlpacaAccount {
    pub account_number: String,
    pub status: String,
    pub currency: String,
    pub cash: f64,
    pub portfolio_value: f64,
    pub buying_power: f64,
    pub equity: f64,
    pub last_equity: f64,
    pub trading_blocked: bool,
    pub account_blocked: bool,
    pub paper: bool,
}

// Broker fields parsed from /v2/positions — kept whole even where not yet read.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct Position {
    pub symbol: String,
    pub qty: f64,
    pub side: String,
    pub market_value: f64,
    pub cost_basis: f64,
    pub unrealized_pl: f64,
    pub unrealized_plpc: f64,
    pub current_price: f64,
    pub avg_entry_price: f64,
}

fn f(v: &Value, k: &str) -> f64 {
    let n = v.get(k);
    n.and_then(|x| x.as_f64())
        .or_else(|| n.and_then(|x| x.as_str()).and_then(|s| s.parse().ok()))
        .unwrap_or(0.0)
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(false)
}

#[derive(Clone)]
pub struct AlpacaClient {
    api_key: String,
    secret_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl AlpacaClient {
    pub fn new(api_key: &str, secret_key: &str, base_url: &str) -> Self {
        AlpacaClient {
            api_key: api_key.to_string(),
            secret_key: secret_key.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Build a client from config; `None` when keys are absent (→ ADVISORY).
    pub fn from_config(cfg: &crate::core::config::AppConfig) -> Option<Self> {
        if cfg.alpaca_api_key.is_empty() || cfg.alpaca_secret_key.is_empty() {
            return None;
        }
        Some(Self::new(&cfg.alpaca_api_key, &cfg.alpaca_secret_key, &cfg.alpaca_base_url))
    }

    fn req(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header("APCA-API-KEY-ID", &self.api_key)
            .header("APCA-API-SECRET-KEY", &self.secret_key)
            .header("Accept", "application/json")
    }

    async fn get(&self, path: &str) -> Result<Value, AlpacaError> {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        let send = |timeout: u64| {
            self.req(self.client.get(&url))
                .timeout(Duration::from_secs(timeout))
                .send()
        };
        match send(30).await.and_then(|r| r.error_for_status()) {
            Ok(r) => r.json().await.map_err(|e| AlpacaError::Request(e.to_string())),
            Err(_) => {
                // Cold-start: Alpaca paper API can be slow to wake. Wait, retry once.
                tokio::time::sleep(Duration::from_secs(30)).await;
                let r = send(60)
                    .await
                    .and_then(|r| r.error_for_status())
                    .map_err(|_| AlpacaError::ColdStart(format!("Alpaca unreachable for {}", path)))?;
                r.json().await.map_err(|e| AlpacaError::Request(e.to_string()))
            }
        }
    }

    pub async fn get_account(&self) -> Result<AlpacaAccount, AlpacaError> {
        let d = self.get("v2/account").await?;
        let acct_num = s(&d, "account_number");
        Ok(AlpacaAccount {
            account_number: acct_num.clone(),
            status: s(&d, "status"),
            currency: {
                let c = s(&d, "currency");
                if c.is_empty() { "USD".into() } else { c }
            },
            cash: f(&d, "cash"),
            portfolio_value: f(&d, "portfolio_value"),
            buying_power: f(&d, "buying_power"),
            equity: f(&d, "equity"),
            last_equity: f(&d, "last_equity"),
            trading_blocked: b(&d, "trading_blocked"),
            account_blocked: b(&d, "account_blocked"),
            paper: acct_num.starts_with("PA"),
        })
    }

    pub async fn get_positions(&self) -> Result<Vec<Position>, AlpacaError> {
        let d = self.get("v2/positions").await?;
        let mut out = vec![];
        if let Some(arr) = d.as_array() {
            for p in arr {
                out.push(Position {
                    symbol: s(p, "symbol"),
                    qty: f(p, "qty"),
                    side: s(p, "side"),
                    market_value: f(p, "market_value"),
                    cost_basis: f(p, "cost_basis"),
                    unrealized_pl: f(p, "unrealized_pl"),
                    unrealized_plpc: f(p, "unrealized_plpc"),
                    current_price: f(p, "current_price"),
                    avg_entry_price: f(p, "avg_entry_price"),
                });
            }
        }
        Ok(out)
    }

    /// HARD paper guard: Ok(true) only for `PA*` accounts; PaperGuard otherwise.
    pub async fn verify_paper_account(&self) -> Result<bool, AlpacaError> {
        let account = self.get_account().await?;
        if account.account_number.starts_with("PA") {
            Ok(true)
        } else {
            Err(AlpacaError::PaperGuard(account.account_number))
        }
    }

    async fn post(&self, path: &str, data: &Value) -> Result<Order, AlpacaError> {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        let resp = self
            .req(self.client.post(&url))
            .header("Content-Type", "application/json")
            .json(data)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| AlpacaError::Request(e.to_string()))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| AlpacaError::Request(e.to_string()))?;
        if !status.is_success() {
            let msg = body.get("message").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(AlpacaError::Order(format!("{}: {}", status.as_u16(), msg)));
        }
        Ok(order_from_json(&body))
    }

    /// Submit a stock order. Paper-guarded; `limit` orders only (no market
    /// orders — marketable limits cross the spread for fill probability).
    pub async fn submit_stock_order(
        &self,
        symbol: &str,
        qty: i64,
        side: &str,
        limit_price: f64,
        time_in_force: &str,
    ) -> Result<Order, AlpacaError> {
        self.verify_paper_account().await?;
        let data = json!({
            "symbol": symbol,
            "qty": qty.to_string(),
            "side": side,
            "type": "limit",
            "limit_price": crate::core::numr::pyround(limit_price, 2).to_string(),
            "time_in_force": time_in_force,
            "extended_hours": false,
        });
        self.post("v2/orders", &data).await
    }

    /// Submit a single-leg option order (LEAPS). Paper-guarded; `limit` only.
    /// `occ` is the OCC symbol; `contracts` is the number of contracts (×100
    /// shares each). `side` is "buy" (→ buy_to_open) or "sell" (→ sell_to_close).
    pub async fn submit_option_order(
        &self,
        occ: &str,
        contracts: i64,
        side: &str,
        limit_price: f64,
        time_in_force: &str,
    ) -> Result<Order, AlpacaError> {
        self.verify_paper_account().await?;
        let intent = if side == "buy" { "buy_to_open" } else { "sell_to_close" };
        let data = json!({
            "symbol": occ,
            "qty": contracts.to_string(),
            "side": side,
            "type": "limit",
            "limit_price": crate::core::numr::pyround(limit_price, 2).to_string(),
            "time_in_force": time_in_force,
            "position_intent": intent,
        });
        self.post("v2/orders", &data).await
    }

    /// Cancel all open orders. Paper-guarded. Returns the broker's DELETE result.
    pub async fn cancel_all_orders(&self) -> Result<usize, AlpacaError> {
        self.verify_paper_account().await?;
        let url = format!("{}/v2/orders", self.base_url);
        let resp = self
            .req(self.client.delete(&url))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| AlpacaError::Request(e.to_string()))?;
        let body: Value = resp.json().await.unwrap_or(Value::Array(vec![]));
        Ok(body.as_array().map(|a| a.len()).unwrap_or(0))
    }
}

fn order_from_json(d: &Value) -> Order {
    let to_s = |k: &str, default: &str| {
        d.get(k)
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| default.to_string())
    };
    Order {
        id: s(d, "id"),
        symbol: s(d, "symbol"),
        qty: to_s("qty", "0"),
        side: s(d, "side"),
        kind: s(d, "type"),
        status: s(d, "status"),
        filled_qty: to_s("filled_qty", "0"),
        filled_avg_price: d.get("filled_avg_price").and_then(|v| v.as_str()).map(String::from),
        limit_price: d.get("limit_price").and_then(|v| v.as_str()).map(String::from),
    }
}
