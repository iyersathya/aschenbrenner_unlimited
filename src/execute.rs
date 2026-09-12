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
        let mut note = String::new();
        if a.kind == ActionKind::Buy {
            dollars = dollars.min(buy_budget);
            // The round-up tolerance is a property of the SLOT, not of today's slice
            // and not of NAV. 2026-09-12: with a NAV-scaled band ($1,082) the guard
            // approved buying one $2,023 NVDA Jan-28 contract into a $1,350 target
            // slot — 50% past target — because the overshoot past the daily slice
            // happened to be $673. What matters is whether one indivisible unit fits
            // the slot at all; if it does not, that is an operator sizing decision
            // (raise the slot or pick a cheaper strike), exactly as the 2026-09-03
            // note said.
            let tgt_d = targets::invested_target_weight(&a.ticker, cfg.cash_buffer_pct)
                * state.nav.max(1e-9);
            let slot_cap = tgt_d * (1.0 + cfg.rebalance_band_pct);
            // build_complete()'s tolerance for THIS name, used by the BLOCKS
            // diagnostic below. Same rescale as rebalance::build (2026-09-12).
            let band_d = cfg.rebalance_band_pct * tgt_d;
            if dollars < r.unit_price && round_up_to_unit(a.dollars, r.unit_price, buy_budget, slot_cap) {
                // The planned slice is under one unit, but one unit fits
                // today's deployable cash (no-leverage cap intact) and lands
                // no further past target than the rebalance band already
                // tolerates — buy the unit instead of deferring forever.
                note = format!(
                    " [slice ${:.0} rounded up to 1 unit ${:.2}; unit fits the ${:.0} slot]",
                    a.dollars,
                    r.unit_price,
                    slot_cap
                );
                dollars = r.unit_price;
            }
            if dollars < r.unit_price {
                // Distinguish a genuinely cash-capped buy from a planned slice
                // that is simply smaller than one unit — the old label blamed
                // the no-leverage cap with five figures of cash in the account
                // (2026-07-01 diagnosis; fixed in lockstep with the sibling).
                // Does this deferral pin the build phase? build_complete()
                // needs |w - target| ≤ band (or w ≥ target); a shortfall wider
                // than the band that can never round up to one unit is the
                // structural blocker (day 47+ of "build", 2026-08-25 diagnosis).
                let blocker = if a.dollars > band_d && a.dollars < r.unit_price {
                    format!(
                        " — BLOCKS build_complete: shortfall ${:.0} > band ${:.0} but < 1 unit",
                        a.dollars, band_d
                    )
                } else {
                    String::new()
                };
                let why = if a.dollars < r.unit_price {
                    if r.unit_price <= buy_budget {
                        // The unit itself fits in today's deployable cash — the
                        // pro-rata slice, not affordability, is what deferred it.
                        // Surfacing this distinguishes "waiting on cash" from the
                        // allocator structurally never crossing a unit price
                        // (42 straight all-skip days as of 2026-08-18).
                        format!(
                            "slice ${:.0} < 1 unit (${:.2}) — deferred, though the unit fits deployable ${:.0} (allocator gap, not cash){}",
                            a.dollars, r.unit_price, buy_budget, blocker
                        )
                    } else {
                        format!("slice ${:.0} < 1 unit (${:.2}) — deferred to a later tranche{}", a.dollars, r.unit_price, blocker)
                    }
                } else {
                    format!("no-leverage cap: cash budget ${:.0} < 1 unit (${:.2})", buy_budget, r.unit_price)
                };
                results.push(skip(a, &why));
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
            reason: format!("{}{}", a.reason, note),
        });
    }
    results
}

/// Should a planned buy slice below one unit be rounded UP to exactly one
/// unit? Yes only when the unit fits the remaining deployable cash (the
/// no-leverage cap stays intact) AND one unit fits the name's own target slot
/// within the rebalance band (`slot_cap` = target x (1 + band)).
///
/// Positions are indivisible, so a slice smaller than one unit can only ever be
/// filled by rounding up; refusing to do that is what left IREN LEAPS planned for
/// 18 days and never ordered. But the tolerance belongs to the slot: if one unit
/// costs more than the whole target position, holding that name at all overshoots
/// target, and that is an operator sizing decision (raise the slot or pick a
/// cheaper strike) — 2026-09-03's NVDA LEAPS $2,352 contract against a $1,350
/// slot, and 2026-09-12's near-miss where a NAV-scaled band approved exactly that.
pub fn round_up_to_unit(planned: f64, unit_price: f64, buy_budget: f64, slot_cap: f64) -> bool {
    planned > 0.0
        && planned < unit_price
        && unit_price <= buy_budget
        && unit_price <= slot_cap
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
mod round_up_tests {
    use super::round_up_to_unit;

    // slot_cap = the name's target position x (1 + rebalance band).
    const GEV_SLOT: f64 = 3_601.0 * 1.03; // $3,709
    const KLAC_SLOT: f64 = 2_700.0 * 1.03; // $2,781
    const NVDA_LEAPS_SLOT: f64 = 1_350.0 * 1.03; // $1,391

    #[test]
    fn rounds_up_only_inside_the_slot_and_budget() {
        // GEV 2026-09-03: slice $705, unit $942.91, deployable $2,639 — one share
        // fits a $3,709 slot comfortably.
        assert!(round_up_to_unit(705.0, 942.91, 2_639.0, GEV_SLOT));
        // KLAC: slice $74, one share $171.74 — indivisible, and it fits the slot.
        assert!(round_up_to_unit(74.0, 171.74, 2_639.0, KLAC_SLOT));
        // unit does not fit the remaining cash budget → never (no-leverage cap)
        assert!(!round_up_to_unit(705.0, 942.91, 900.0, GEV_SLOT));
        // slice already ≥ one unit → not a round-up case
        assert!(!round_up_to_unit(1_000.0, 942.91, 2_639.0, GEV_SLOT));
        // zero / negative slices never buy
        assert!(!round_up_to_unit(0.0, 942.91, 2_639.0, GEV_SLOT));
    }

    #[test]
    fn a_unit_larger_than_the_whole_slot_never_rounds_up() {
        // 2026-09-12 regression: one NVDA Jan-28 $300 contract costs $2,023-$2,352
        // against a $1,350 target slot. Buying it puts the slot 50% past target, so
        // it is an operator sizing decision, not an allocator one. The NAV-scaled
        // band this replaces said yes, because the overshoot past the DAILY SLICE
        // ($673) happened to be under $1,082 of NAV band — and the live dry-run on
        // main had it queued as the one order it would place.
        assert!(!round_up_to_unit(1_350.0, 2_023.0, 4_335.0, NVDA_LEAPS_SLOT));
        assert!(!round_up_to_unit(1_160.0, 2_352.0, 2_639.0, NVDA_LEAPS_SLOT));
        // cash being plentiful does not make it fit the slot
        assert!(!round_up_to_unit(1_350.0, 2_023.0, 99_000.0, NVDA_LEAPS_SLOT));
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
    async fn sub_unit_slice_wider_than_band_is_named_as_build_blocker() {
        // Offline NVDA LEAPS = $3,000/contract. On a $36,000 NAV the slot's target
        // is ~$1,347, so its band is 3% of THAT — about $40 (2026-09-12 rescale),
        // not 3% of NAV. A $1,200 shortfall is far outside the band yet still under
        // one contract: the exact shape that pins the sleeve in build mode forever
        // (2026-08-25).
        let st = state(5_000.0, 36_000.0);
        let acts = vec![Action::buy("NVDA_LEAPS", 1_200.0, "build")];
        let res = execute_actions(&acts, &AppConfig::default(), None, None, false, &st).await;
        assert!(res[0].status.contains("BLOCKS build_complete"), "{}", res[0].status);
        // A slice inside the slot's own band is an ordinary deferral, not a blocker.
        let acts = vec![Action::buy("NVDA_LEAPS", 30.0, "build")];
        let res = execute_actions(&acts, &AppConfig::default(), None, None, false, &st).await;
        assert!(res[0].status.contains("deferred"), "{}", res[0].status);
        assert!(!res[0].status.contains("BLOCKS"), "{}", res[0].status);
    }

    #[tokio::test]
    async fn revalidate_is_advisory() {
        let st = state(40_000.0, 40_000.0);
        let acts = vec![Action::revalidate("NVDA_LEAPS", "time-stop")];
        let res = execute_actions(&acts, &AppConfig::default(), None, None, true, &st).await;
        assert_eq!(res[0].status, "advisory");
    }
}
