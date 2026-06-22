//! Deterministic rebalance rules for the v4 *Floor 2x / Ceiling UNLIMITED*
//! strategy (PDF §9), applied in a fixed order. The equity sleeve (Tier A/B)
//! carries the stops, profit-takes, position cap, cluster cap, and cash deploy.
//! The Moonshot sleeve (Tier D) has its own lifecycle: LEAPS time-stop +500%
//! profit-take, micro-cap 10x trim. This is the floor; the signal only re-orders.

use crate::core::alpaca_data::parse_occ;
use crate::core::config::AppConfig;
use crate::core::safety::cash_available_for_buys;
use crate::portfolio::state::PortfolioState;
use crate::portfolio::targets::{self, Tier};
use crate::rebalance::signal_bias::SignalBias;
use crate::rebalance::Action;
use chrono::NaiveDate;
use std::collections::{HashMap, HashSet};

// Profit-take thresholds (unrealized P&L as a fraction of cost; 2.0 = +200% = 3x).
const FLOOR_PT_MULT: f64 = 2.0; // 3x → trim 25%
const FLOOR_PT_TRIM: f64 = 0.25;
const ASYM_PT_MULT: f64 = 4.0; // 5x → trim 30%
const ASYM_PT_TRIM: f64 = 0.30;
const LEAPS_PT_MULT: f64 = 5.0; // +500% → sell half
const LEAPS_PT_TRIM: f64 = 0.50;
const LEAPS_TIME_STOP_DTE: i64 = 60;
const MICROCAP_PT_MULT: f64 = 9.0; // 10x → trim 50%
const MICROCAP_PT_TRIM: f64 = 0.50;
/// Floor names get a thesis-review flag at -25%; the -35% cut is noted separately.
const FLOOR_REVIEW_DD: f64 = 0.25;

/// Build today's rebalance plan. Pure over its inputs (no IO).
pub fn plan_rebalance(
    state: &PortfolioState,
    cfg: &AppConfig,
    high_water: &HashMap<String, f64>,
    bias: &SignalBias,
    asof: NaiveDate,
) -> Vec<Action> {
    let nav = state.nav.max(1e-9);
    let band = cfg.rebalance_band_pct * nav;
    let mut actions: Vec<Action> = Vec::new();

    // Working dollar map over the EQUITY sleeve (Tier A/B). Tier D handled below.
    let mut mv: HashMap<String, f64> =
        targets::equity_invested().map(|t| (t.ticker.to_string(), state.market_value(t.ticker))).collect();
    let mut cash = state.cash;
    let target_d = |ticker: &str| -> f64 { targets::invested_target_weight(ticker, cfg.cash_buffer_pct) * nav };

    let mut trims: HashMap<String, f64> = HashMap::new();
    let mut trim_reasons: HashMap<String, Vec<String>> = HashMap::new();
    let add_trim = |ticker: &str, d: f64, reason: String, trims: &mut HashMap<String, f64>, trim_reasons: &mut HashMap<String, Vec<String>>| {
        *trims.entry(ticker.to_string()).or_insert(0.0) += d;
        trim_reasons.entry(ticker.to_string()).or_default().push(reason);
    };

    // ── Rule 1: tier-aware stop + profit-take (PDF §9) ──────────────────────
    for h in &state.holdings {
        let Some(t) = targets::find(&h.ticker) else { continue };
        match t.tier {
            Tier::Floor => {
                if h.unrealized_plpc <= -FLOOR_REVIEW_DD {
                    let cut = if h.unrealized_plpc <= -cfg.position_stop_pct { " (≤ -35% cut zone — exit if a pillar is broken)" } else { "" };
                    actions.push(Action::revalidate(
                        &h.ticker,
                        format!("Floor down {:.0}% — re-validate thesis{cut}", h.unrealized_plpc * 100.0),
                    ));
                }
                if h.unrealized_plpc >= FLOOR_PT_MULT {
                    let d = h.market_value * FLOOR_PT_TRIM;
                    if d > band {
                        add_trim(&h.ticker, d, format!("Floor at {:.1}x → trim {:.0}% and ride", h.unrealized_plpc + 1.0, FLOOR_PT_TRIM * 100.0), &mut trims, &mut trim_reasons);
                        if let Some(m) = mv.get_mut(t.ticker) { *m -= d; }
                        cash += d;
                    }
                }
            }
            Tier::Asymmetric => {
                // NO stop by design. Profit-take only.
                if h.unrealized_plpc >= ASYM_PT_MULT {
                    let d = h.market_value * ASYM_PT_TRIM;
                    if d > band {
                        add_trim(&h.ticker, d, format!("Asymmetric at {:.1}x → trim {:.0}% and ride", h.unrealized_plpc + 1.0, ASYM_PT_TRIM * 100.0), &mut trims, &mut trim_reasons);
                        if let Some(m) = mv.get_mut(t.ticker) { *m -= d; }
                        cash += d;
                    }
                }
            }
            _ => {}
        }
    }

    // ── Rule 3: single-position cap (>15% → trim back to 10%), equity only ───
    for t in targets::equity_invested() {
        let cur = mv[t.ticker];
        if cur / nav > cfg.position_trim_trigger_pct {
            let dest = cfg.position_trim_target_pct * nav;
            let sell = cur - dest;
            if sell > band {
                add_trim(t.ticker, sell, format!("position {:.1}% > {:.0}% cap → trim to {:.0}%", cur / nav * 100.0, cfg.position_trim_trigger_pct * 100.0, cfg.position_trim_target_pct * 100.0), &mut trims, &mut trim_reasons);
                *mv.get_mut(t.ticker).unwrap() -= sell;
                cash += sell;
            }
        }
    }

    // ── Rule 2: cluster cap (>30% → trim overweight members), equity only ───
    for c in targets::clusters() {
        let members = targets::cluster_members(c);
        let cw: f64 = members.iter().map(|m| mv[*m]).sum::<f64>() / nav;
        if cw <= cfg.cluster_cap_pct {
            continue;
        }
        let mut excess = (cw - cfg.cluster_cap_pct) * nav;
        let mut cand: Vec<(&str, f64)> = members.iter().map(|m| (*m, mv[*m] - target_d(m))).filter(|(_, room)| *room > 0.0).collect();
        cand.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                .then(bias.trim_priority(a.0).partial_cmp(&bias.trim_priority(b.0)).unwrap_or(std::cmp::Ordering::Equal))
        });
        for (m, room) in cand {
            if excess <= 0.0 { break; }
            let take = room.min(excess);
            if take > 0.0 {
                add_trim(m, take, format!("cluster '{}' {:.1}% > {:.0}% cap → trim", c, cw * 100.0, cfg.cluster_cap_pct * 100.0), &mut trims, &mut trim_reasons);
                *mv.get_mut(m).unwrap() -= take;
                cash += take;
                excess -= take;
            }
        }
    }

    // Emit accumulated equity trims (band-gated on the combined size).
    let mut trim_tickers: Vec<&String> = trims.keys().collect();
    trim_tickers.sort();
    for t in trim_tickers {
        let d = trims[t];
        if d > band {
            actions.push(Action::trim(t, d, trim_reasons.get(t).map(|v| v.join("; ")).unwrap_or_default()));
        }
    }

    // ── Rule 4: deploy cash on visible drawdown (equity only) ───────────────
    let mut deployable = cash_available_for_buys(cash, nav, cfg.cash_buffer_pct) * bias.deploy_multiplier();
    if deployable > band {
        let mut drawdown: HashMap<String, f64> = HashMap::new();
        for h in &state.holdings {
            if targets::find(&h.ticker).map(|t| matches!(t.tier, Tier::Floor | Tier::Asymmetric)).unwrap_or(false) {
                let hw = high_water.get(&h.ticker).copied().unwrap_or(h.price);
                if hw > 0.0 && h.price > 0.0 {
                    drawdown.insert(h.ticker.clone(), ((hw - h.price) / hw).max(0.0));
                }
            }
        }
        let mut stressed: HashSet<&str> = HashSet::new();
        for c in targets::clusters() {
            if targets::cluster_members(c).iter().any(|m| drawdown.get(*m).copied().unwrap_or(0.0) >= cfg.drawdown_deploy_pct) {
                stressed.insert(c);
            }
        }
        let mut eligible: Vec<&'static targets::Target> = targets::equity_invested()
            .filter(|t| {
                let room = target_d(t.ticker) - mv[t.ticker];
                let name_dd = drawdown.get(t.ticker).copied().unwrap_or(0.0);
                room > band && (name_dd >= cfg.drawdown_deploy_single_pct || stressed.contains(t.cluster))
            })
            .collect();
        eligible.sort_by(|a, b| {
            bias.buy_priority(b.ticker).partial_cmp(&bias.buy_priority(a.ticker)).unwrap_or(std::cmp::Ordering::Equal)
                .then((target_d(b.ticker) - mv[b.ticker]).partial_cmp(&(target_d(a.ticker) - mv[a.ticker])).unwrap_or(std::cmp::Ordering::Equal))
        });
        for t in eligible {
            if deployable <= band { break; }
            let room = target_d(t.ticker) - mv[t.ticker];
            let take = room.min(deployable);
            if take > band {
                let dd = drawdown.get(t.ticker).copied().unwrap_or(0.0);
                actions.push(Action::buy(t.ticker, take, format!("drawdown deploy: down {:.0}% from high, below target → add", dd * 100.0)));
                deployable -= take;
            }
        }
    }

    // ── Rule 5: Tier D moonshot lifecycle (LEAPS + micro-caps) ──────────────
    for t in targets::moonshot() {
        let Some(sym) = targets::instrument_symbol(t, cfg) else { continue };
        let Some(h) = state.holding(&sym) else { continue };
        if targets::is_leaps(t) {
            // Time-stop: flag when the contract is inside 60 DTE (advisory).
            if let Some((_, expiry, _, _)) = parse_occ(&sym) {
                let dte = (expiry - asof).num_days();
                if dte <= LEAPS_TIME_STOP_DTE {
                    actions.push(Action::revalidate(t.ticker, format!("LEAPS {} DTE ≤ {} — time-stop window; if deeply OTM accept zero, else roll to a NEW thesis", dte, LEAPS_TIME_STOP_DTE)));
                }
            }
            // Profit-take: sell half at ≥ +500%.
            if h.unrealized_plpc >= LEAPS_PT_MULT && h.market_value * LEAPS_PT_TRIM > 1.0 {
                actions.push(Action::trim(t.ticker, h.market_value * LEAPS_PT_TRIM, format!("LEAPS at +{:.0}% → sell half, ride the rest to expiry", h.unrealized_plpc * 100.0)));
            }
        } else {
            // Micro-cap: trim 50% at 10x.
            if h.unrealized_plpc >= MICROCAP_PT_MULT && h.market_value * MICROCAP_PT_TRIM > 1.0 {
                actions.push(Action::trim(t.ticker, h.market_value * MICROCAP_PT_TRIM, format!("micro-cap at {:.0}x → trim {:.0}%", h.unrealized_plpc + 1.0, MICROCAP_PT_TRIM * 100.0)));
            }
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::alpaca::{AlpacaAccount, Position};
    use crate::rebalance::ActionKind;

    fn cfg() -> AppConfig {
        AppConfig::default()
    }
    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 22).unwrap()
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

    fn state(positions: Vec<Position>, cash: f64, nav: f64) -> PortfolioState {
        let a = AlpacaAccount { cash, portfolio_value: nav, equity: nav, ..Default::default() };
        PortfolioState::from_alpaca(&a, &positions)
    }

    #[test]
    fn floor_stop_only_no_asym_stop() {
        let st = state(vec![pos("GEV", 500.0, 1000.0, -0.40), pos("OKLO", 500.0, 40.0, -0.50)], 39_000.0, 40_000.0);
        let acts = plan_rebalance(&st, &cfg(), &HashMap::new(), &SignalBias::default(), day());
        assert!(acts.iter().any(|a| a.ticker == "GEV" && a.kind == ActionKind::ThesisRevalidate));
        assert!(!acts.iter().any(|a| a.ticker == "OKLO" && a.kind == ActionKind::ThesisRevalidate));
    }

    #[test]
    fn position_over_15pct_trims_to_10() {
        // GEV at 20% → trim to 10% ($4,000) → sell ~$4,000.
        let st = state(vec![pos("GEV", 8_000.0, 1000.0, 0.1)], 32_000.0, 40_000.0);
        let acts = plan_rebalance(&st, &cfg(), &HashMap::new(), &SignalBias::default(), day());
        let trim = acts.iter().find(|a| a.ticker == "GEV" && a.kind == ActionKind::Trim).unwrap();
        assert!((trim.dollars - 4_000.0).abs() < 1.0, "got {}", trim.dollars);
    }

    #[test]
    fn asymmetric_profit_take_at_5x() {
        // PLTR up +400% (=5x) → trim 30%.
        let st = state(vec![pos("PLTR", 5_000.0, 665.0, 4.0)], 35_000.0, 40_000.0);
        let acts = plan_rebalance(&st, &cfg(), &HashMap::new(), &SignalBias::default(), day());
        assert!(acts.iter().any(|a| a.ticker == "PLTR" && a.kind == ActionKind::Trim && (a.dollars - 1_500.0).abs() < 1.0));
    }

    #[test]
    fn leaps_profit_take_sells_half_at_500pct() {
        // NVDA LEAPS up +600% → sell half. Held under its resolved OCC symbol.
        let occ = targets::instrument_symbol(targets::find("NVDA_LEAPS").unwrap(), &cfg()).unwrap();
        let st = state(vec![pos(&occ, 4_000.0, 200.0, 6.0)], 36_000.0, 40_000.0);
        let acts = plan_rebalance(&st, &cfg(), &HashMap::new(), &SignalBias::default(), day());
        assert!(acts.iter().any(|a| a.ticker == "NVDA_LEAPS" && a.kind == ActionKind::Trim && (a.dollars - 2_000.0).abs() < 1.0));
    }
}
