//! Execution — turn rebalance/build `Action`s into marketable-LIMIT orders,
//! branching on the target's instrument: stocks/micro-caps become whole-share
//! stock orders; LEAPS become single-leg option orders (contracts × 100).
//!
//! Hard invariants: no market orders (cross with a small limit buffer), the
//! PA-paper guard fires inside every submit, and the no-leverage cap bounds total
//! buy notional at available cash less the protected buffer (PDF: no leverage).

use crate::core::alpaca::AlpacaClient;
use crate::core::alpaca_data::AlpacaDataClient;
use crate::core::config::AppConfig;
use crate::core::numr::pyround;
use crate::core::safety::cash_available_for_buys;
use crate::portfolio::state::PortfolioState;
use crate::portfolio::targets::{self, Instrument};
use crate::rebalance::{Action, ActionKind};

const STOCK_BUF: f64 = 0.003;
const OPTION_BUF: f64 = 0.02; // far-OTM LEAPS spreads are wide; cross a bit more

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub ticker: String,
    pub side: String,
    pub qty: i64,
    pub limit: f64,
    pub dollars: f64,
    pub status: String,
    pub reason: String,
}

impl ExecResult {
    pub fn line(&self) -> String {
        format!(
            "{:<11} {:<4} qty {:>4} @ ${:<8.2} (${:>8.2}) [{}] — {}",
            self.ticker, self.side, self.qty, self.limit, self.dollars, self.status, self.reason
        )
    }
    fn is_advisory(&self) -> bool {
        self.status == "advisory"
    }
}

/// What a resolved action trades as: (display label, broker symbol, per-unit
/// price, unit multiplier, is_option).
struct Resolved {
    symbol: String,
    unit_price: f64, // dollars per share (stock) or per contract (option = mid×100)
    limit_quote: f64, // the per-share / per-contract-share quote used for the limit
    is_option: bool,
}

async fn resolve(
    a: &Action,
    cfg: &AppConfig,
    state: &PortfolioState,
    data: Option<&AlpacaDataClient>,
) -> Option<Resolved> {
    let t = targets::find(&a.ticker);
    match t.map(|t| t.instrument) {
        Some(Instrument::Leaps { .. }) => {
            let occ = targets::instrument_symbol(t.unwrap(), cfg)?;
            // Prefer the held price if we already own it; else the live option mid.
            let mid = match state.holding(&occ) {
                Some(h) if h.price > 0.0 => h.price,
                _ => match data {
                    Some(d) => d.get_option_mid(&occ).await?,
                    None => t.unwrap().spot_anchor, // offline guidance only
                },
            };
            if mid <= 0.0 {
                return None;
            }
            Some(Resolved { symbol: occ, unit_price: mid * 100.0, limit_quote: mid, is_option: true })
        }
        _ => {
            // Stock or micro-cap (resolve the pinned ticker); cash is never an action.
            let sym = match t {
                Some(t) => targets::instrument_symbol(t, cfg)?,
                None => a.ticker.clone(), // fallback: treat label as a stock symbol
            };
            let px = stock_price(&sym, state, data, t.map(|t| t.spot_anchor).unwrap_or(0.0)).await;
            if px <= 0.0 {
                return None;
            }
            Some(Resolved { symbol: sym, unit_price: px, limit_quote: px, is_option: false })
        }
    }
}

async fn stock_price(sym: &str, state: &PortfolioState, data: Option<&AlpacaDataClient>, anchor: f64) -> f64 {
    if let Some(h) = state.holding(sym) {
        if h.price > 0.0 {
            return h.price;
        }
    }
    if let Some(d) = data {
        if let Some(px) = d.get_stock_price(sym).await {
            if px > 0.0 {
                return px;
            }
        }
    }
    anchor
}

/// Execute (or dry-run) the planned actions. `armed=false` (or no client) → dry-run.
pub async fn execute_actions(
    actions: &[Action],
    cfg: &AppConfig,
    alpaca: Option<&AlpacaClient>,
    data: Option<&AlpacaDataClient>,
    armed: bool,
    state: &PortfolioState,
) -> Vec<ExecResult> {
    let mut results = vec![];
    let mut buy_budget = cash_available_for_buys(state.cash, state.nav, cfg.cash_buffer_pct);
    let mut submitted = 0i64;

    for a in actions {
        if a.kind == ActionKind::ThesisRevalidate {
            results.push(ExecResult {
                ticker: a.ticker.clone(),
                side: "none".into(),
                qty: 0,
                limit: 0.0,
                dollars: 0.0,
                status: "advisory".into(),
                reason: a.reason.clone(),
            });
            continue;
        }
        if submitted >= cfg.max_orders_per_day {
            results.push(skip(a, "max orders/day reached"));
            continue;
        }
        let Some(r) = resolve(a, cfg, state, data).await else {
            results.push(skip(a, "unresolved instrument / no price"));
            continue;
        };

        let mut dollars = a.dollars;
        if a.kind == ActionKind::Buy {
            dollars = dollars.min(buy_budget);
            if dollars < r.unit_price {
                results.push(skip(a, "no-leverage cap: insufficient cash for 1 unit"));
                continue;
            }
        }
        let qty = (dollars / r.unit_price).floor() as i64;
        if qty < 1 {
            let why = if r.is_option {
                format!(
                    "LEAPS premium ~${:.0}/contract > slot cap ${:.0} — skipped (raise the slot alloc or pick a cheaper strike/expiry)",
                    r.unit_price, dollars
                )
            } else {
                "rounds to < 1 share".to_string()
            };
            results.push(skip(a, &why));
            continue;
        }
        let buy = a.kind == ActionKind::Buy;
        let buf = if r.is_option { OPTION_BUF } else { STOCK_BUF };
        let limit = pyround(r.limit_quote * (1.0 + if buy { buf } else { -buf }), 2);
        let side = if buy { "buy" } else { "sell" };
        let notional = qty as f64 * r.unit_price;

        let status = if armed {
            match alpaca {
                Some(client) => {
                    let res = if r.is_option {
                        client.submit_option_order(&r.symbol, qty, side, limit, "day").await
                    } else {
                        client.submit_stock_order(&r.symbol, qty, side, limit, "day").await
                    };
                    match res {
                        Ok(o) => {
                            if buy {
                                buy_budget -= notional;
                            }
                            submitted += 1;
                            format!("submitted ({})", o.status)
                        }
                        Err(e) => format!("error: {e}"),
                    }
                }
                None => "skipped: no alpaca client".into(),
            }
        } else {
            if buy {
                buy_budget -= notional;
            }
            submitted += 1;
            "dry-run".into()
        };

        results.push(ExecResult {
            ticker: format!("{} ({})", a.ticker, r.symbol),
            side: side.into(),
            qty,
            limit,
            dollars: notional,
            status,
            reason: a.reason.clone(),
        });
    }
    results
}

/// Count advisory (non-order) rows for the digest.
pub fn advisory_count(results: &[ExecResult]) -> usize {
    results.iter().filter(|r| r.is_advisory()).count()
}

fn skip(a: &Action, why: &str) -> ExecResult {
    ExecResult {
        ticker: a.ticker.clone(),
        side: a.side().into(),
        qty: 0,
        limit: 0.0,
        dollars: 0.0,
        status: format!("skipped: {why}"),
        reason: a.reason.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::alpaca::AlpacaAccount;

    fn state(cash: f64, nav: f64) -> PortfolioState {
        let a = AlpacaAccount { cash, portfolio_value: nav, equity: nav, ..Default::default() };
        PortfolioState::from_alpaca(&a, &[])
    }

    #[tokio::test]
    async fn dry_run_respects_no_leverage() {
        // $3,000 deployable. Two $2,000 stock buys → second capped.
        let st = state(5_000.0, 40_000.0);
        let acts = vec![Action::buy("KLAC", 2_000.0, "build"), Action::buy("ETN", 2_000.0, "build")];
        let res = execute_actions(&acts, &AppConfig::default(), None, None, false, &st).await;
        let total: f64 = res.iter().filter(|r| r.side == "buy").map(|r| r.dollars).sum();
        assert!(total <= 3_000.0 + 1.0, "no-leverage breached: {total}");
    }

    #[tokio::test]
    async fn leaps_buy_sizes_in_contracts_offline() {
        // Offline: NVDA LEAPS prices off spot_anchor ($30 → $3,000/contract).
        // $4,000 alloc but only $3,000 deployable → 1 contract.
        let st = state(5_000.0, 40_000.0);
        let acts = vec![Action::buy("NVDA_LEAPS", 4_000.0, "build LEAPS")];
        let res = execute_actions(&acts, &AppConfig::default(), None, None, false, &st).await;
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].qty, 1, "expected 1 contract, got {}", res[0].qty);
        assert!(res[0].ticker.contains("NVDA280121C00300000"));
    }

    #[tokio::test]
    async fn revalidate_is_advisory() {
        let st = state(40_000.0, 40_000.0);
        let acts = vec![Action::revalidate("NVDA_LEAPS", "time-stop")];
        let res = execute_actions(&acts, &AppConfig::default(), None, None, true, &st).await;
        assert_eq!(res[0].status, "advisory");
    }
}
