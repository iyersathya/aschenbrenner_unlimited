//! Portfolio metrics — per-position and per-cluster weights, drift vs target,
//! drawdown from high-water marks, and the 26%-CAGR glide-path check. Pure
//! functions over `PortfolioState` + the target table, so they unit-test cleanly.

use crate::portfolio::state::PortfolioState;
use crate::portfolio::targets;
use std::collections::HashMap;

/// One row of the daily tracking table.
#[allow(dead_code)] // cluster/unrealized_plpc carried in the row for cards + digests
#[derive(Debug, Clone)]
pub struct PositionMetric {
    pub ticker: String,
    pub tier: &'static str,
    pub cluster: &'static str,
    pub market_value: f64,
    pub weight: f64,        // fraction of NAV
    pub target_weight: f64, // fraction of NAV
    pub drift: f64,         // weight - target_weight
    pub unrealized_plpc: f64,
    /// Drawdown from high-water mark as a positive fraction (0.20 = down 20%).
    pub drawdown: f64,
}

/// Build the per-position metric rows for the equity sleeve (Tier A + B). The
/// Moonshot tier (LEAPS / micro-caps) has its own lifecycle and is reported
/// separately, not in this drift table.
pub fn position_metrics(
    state: &PortfolioState,
    cash_buffer_pct: f64,
    high_water: &HashMap<String, f64>,
) -> Vec<PositionMetric> {
    let nav = state.nav.max(1e-9);
    targets::equity_invested()
        .map(|t| {
            let mv = state.market_value(t.ticker);
            let h = state.holding(t.ticker);
            let price = h.map(|h| h.price).unwrap_or(0.0);
            let hw = high_water.get(t.ticker).copied().unwrap_or(price);
            let drawdown = if hw > 0.0 && price > 0.0 {
                ((hw - price) / hw).max(0.0)
            } else {
                0.0
            };
            PositionMetric {
                ticker: t.ticker.to_string(),
                tier: t.tier.label(),
                cluster: t.cluster,
                market_value: mv,
                weight: mv / nav,
                target_weight: targets::invested_target_weight(t.ticker, cash_buffer_pct),
                drift: mv / nav - targets::invested_target_weight(t.ticker, cash_buffer_pct),
                unrealized_plpc: h.map(|h| h.unrealized_plpc).unwrap_or(0.0),
                drawdown,
            }
        })
        .collect()
}

/// Cluster weight rollup: `cluster -> (current_weight, target_weight)`.
pub fn cluster_weights(state: &PortfolioState, cash_buffer_pct: f64) -> Vec<(String, f64, f64)> {
    let nav = state.nav.max(1e-9);
    targets::clusters()
        .into_iter()
        .map(|c| {
            let cur: f64 = targets::cluster_members(c).iter().map(|t| state.market_value(t)).sum::<f64>() / nav;
            let tgt = targets::cluster_target_weight(c, cash_buffer_pct);
            (c.to_string(), cur, tgt)
        })
        .collect()
}

/// The 26%-CAGR glide-path target NAV at `years_elapsed` since the start.
pub fn glide_path_target(start_nav: f64, cagr: f64, years_elapsed: f64) -> f64 {
    start_nav * (1.0 + cagr).powf(years_elapsed.max(0.0))
}

/// `(target_nav, ahead_or_behind_fraction)` for today vs the glide path.
/// Positive = ahead of the 26% pace.
pub fn glide_path_status(nav: f64, start_nav: f64, cagr: f64, years_elapsed: f64) -> (f64, f64) {
    let target = glide_path_target(start_nav, cagr, years_elapsed);
    let delta = if target > 0.0 { (nav - target) / target } else { 0.0 };
    (target, delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::alpaca::{AlpacaAccount, Position};

    fn state_with(positions: Vec<Position>, cash: f64, nav: f64) -> PortfolioState {
        let acct = AlpacaAccount { cash, portfolio_value: nav, equity: nav, ..Default::default() };
        PortfolioState::from_alpaca(&acct, &positions)
    }

    fn pos(sym: &str, mv: f64, price: f64, plpc: f64) -> Position {
        Position {
            symbol: sym.into(),
            qty: if price > 0.0 { mv / price } else { 0.0 },
            side: "long".into(),
            market_value: mv,
            cost_basis: mv / (1.0 + plpc),
            current_price: price,
            avg_entry_price: price / (1.0 + plpc),
            unrealized_plpc: plpc,
            ..Default::default()
        }
    }

    #[test]
    fn weights_and_drift_compute() {
        // GEV target ≈ 10.5/89.5*0.95 = 0.1114. Put it at 15% → positive drift.
        let st = state_with(vec![pos("GEV", 6_000.0, 1000.0, 0.1)], 34_000.0, 40_000.0);
        let m = position_metrics(&st, 0.05, &HashMap::new());
        let gev = m.iter().find(|r| r.ticker == "GEV").unwrap();
        assert!((gev.weight - 0.15).abs() < 1e-9);
        assert!(gev.drift > 0.0);
    }

    #[test]
    fn drawdown_from_high_water() {
        let st = state_with(vec![pos("IREN", 1_000.0, 18.0, -0.2)], 39_000.0, 40_000.0);
        let mut hw = HashMap::new();
        hw.insert("IREN".to_string(), 24.0); // high 24, now 18 → 25% drawdown
        let m = position_metrics(&st, 0.05, &hw);
        let iren = m.iter().find(|r| r.ticker == "IREN").unwrap();
        assert!((iren.drawdown - 0.25).abs() < 1e-9);
    }

    #[test]
    fn glide_path_doubles_in_three_years() {
        let t = glide_path_target(40_000.0, 0.26, 3.0);
        assert!((t - 80_032.0).abs() < 50.0, "got {t}"); // ~$80K
    }
}
