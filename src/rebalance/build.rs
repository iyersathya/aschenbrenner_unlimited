//! Phased initial build (PDF §11) for the v4 strategy. Week 1 Tier A floor →
//! Week 2 Tier B asymmetric (DCA) → Week 3 Tier D LEAPS → Weeks 4-6 micro-caps,
//! always holding the cash buffer back. Idempotent per day: each run tops up the
//! active stages toward target against live NAV.
//!
//! `plan_build` is pure — it emits dollar-denominated `Action::buy`s keyed by the
//! target LABEL; `execute` resolves the instrument (stock/LEAPS/micro-cap),
//! prices it, and converts to whole shares/contracts.

use crate::core::config::AppConfig;
use crate::core::safety::cash_available_for_buys;
use crate::portfolio::state::PortfolioState;
use crate::portfolio::targets::{self, Instrument, Tier};
use crate::rebalance::Action;
use chrono::{Datelike, Duration, NaiveDate, Weekday};

/// Inclusive count of weekdays from `start` to `asof` (cheap trading-day counter).
pub fn weekdays_between(start: NaiveDate, asof: NaiveDate) -> i64 {
    if asof < start {
        return 0;
    }
    let mut d = start;
    let mut n = 0;
    while d <= asof {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            n += 1;
        }
        d += Duration::days(1);
    }
    n
}

/// Build stages, in fill order.
#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Floor,
    Asymmetric,
    MoonLeaps,
    MoonMicro,
}

fn stage_of(t: &targets::Target) -> Stage {
    match (t.tier, t.instrument) {
        (Tier::Floor, _) => Stage::Floor,
        (Tier::Asymmetric, _) => Stage::Asymmetric,
        (Tier::Moonshot, Instrument::MicroCap { .. }) => Stage::MoonMicro,
        (Tier::Moonshot, _) => Stage::MoonLeaps,
        _ => Stage::MoonMicro,
    }
}

/// Stages active given trading days elapsed (PDF §11 weekly cadence).
fn active(elapsed: i64) -> Vec<Stage> {
    let mut v = vec![Stage::Floor];
    if elapsed >= 7 {
        v.push(Stage::Asymmetric);
    }
    if elapsed >= 14 {
        v.push(Stage::MoonLeaps);
    }
    if elapsed >= 21 {
        v.push(Stage::MoonMicro);
    }
    v
}

/// Current dollar value of a target's holding (resolved to its broker symbol).
fn current_mv(t: &targets::Target, state: &PortfolioState, cfg: &AppConfig) -> f64 {
    targets::instrument_symbol(t, cfg).map(|s| state.market_value(&s)).unwrap_or(0.0)
}

/// True when every invested target is within `band` of target (unset micro-cap
/// slots count as satisfied so the build can still complete).
pub fn build_complete(state: &PortfolioState, cfg: &AppConfig) -> bool {
    let nav = state.nav.max(1e-9);
    let band = cfg.rebalance_band_pct;
    targets::invested().all(|t| {
        if targets::instrument_symbol(t, cfg).is_none() {
            return true; // unset micro-cap slot — nothing to buy
        }
        let w = current_mv(t, state, cfg) / nav;
        let tgt = targets::invested_target_weight(t.ticker, cfg.cash_buffer_pct);
        (w - tgt).abs() <= band || w >= tgt
    })
}

/// Plan today's build tranche. Pure over its inputs.
pub fn plan_build(state: &PortfolioState, cfg: &AppConfig, asof: NaiveDate, start: NaiveDate) -> Vec<Action> {
    let nav = state.nav.max(1e-9);
    let elapsed = weekdays_between(start, asof);
    let stages = active(elapsed);
    let mut deployable = cash_available_for_buys(state.cash, nav, cfg.cash_buffer_pct);
    if deployable <= 0.0 {
        return vec![];
    }
    let target_d = |label: &str| targets::invested_target_weight(label, cfg.cash_buffer_pct) * nav;
    let mut actions = vec![];

    for stage in [Stage::Floor, Stage::Asymmetric, Stage::MoonLeaps, Stage::MoonMicro] {
        if !stages.contains(&stage) || deployable <= 0.0 {
            continue;
        }
        // Underfilled targets in this stage (skip unset micro-caps).
        let mut names: Vec<(&'static targets::Target, f64)> = targets::invested()
            .filter(|t| stage_of(t) == stage)
            .filter(|t| targets::instrument_symbol(t, cfg).is_some())
            .map(|t| (t, (target_d(t.ticker) - current_mv(t, state, cfg)).max(0.0)))
            .filter(|(_, short)| *short > 1.0)
            .collect();
        names.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if names.is_empty() {
            continue;
        }

        // Tier B (Asymmetric) is DCA-capped at the daily budget; others buy full.
        let stage_budget = if stage == Stage::Asymmetric {
            cfg.build_daily_budget.min(deployable)
        } else {
            deployable
        };
        let mut spent = 0.0;
        for (t, short) in &names {
            if deployable <= 0.0 || spent >= stage_budget {
                break;
            }
            // Sequential fill, biggest shortfall first: each name may take up
            // to the whole remaining daily budget. The old pro-rata slices
            // (budget/N) sat below one share's price for the expensive names
            // (STRL $777, MTZ $391, VRT $312 …), so `execute` skipped them
            // every day while the cheap names filled — the book was building
            // itself in share-price order (2026-07-01 analysis, same flaw as
            // the sibling aschenbrenner_portfolio; fixed in lockstep).
            // Names still rotate across days: once funded, a name's shortfall
            // drops below the next name's and the budget moves on.
            let want = *short;
            let take = want.min(deployable).min(stage_budget - spent);
            if take > 1.0 {
                actions.push(Action::buy(t.ticker, take, format!("build {} — day {} of phased accumulation", t.tier.label(), elapsed)));
                deployable -= take;
                spent += take;
            }
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::alpaca::AlpacaAccount;

    fn empty_state(cash: f64) -> PortfolioState {
        let a = AlpacaAccount { cash, portfolio_value: cash, equity: cash, ..Default::default() };
        PortfolioState::from_alpaca(&a, &[])
    }

    #[test]
    fn day_one_builds_only_floor() {
        let st = empty_state(40_000.0);
        let start = NaiveDate::from_ymd_opt(2026, 6, 22).unwrap();
        let acts = plan_build(&st, &AppConfig::default(), start, start);
        assert!(!acts.is_empty());
        for a in &acts {
            assert_eq!(targets::find(&a.ticker).unwrap().tier, Tier::Floor, "{} not Floor", a.ticker);
        }
    }

    #[test]
    fn leaps_stage_active_only_after_two_weeks() {
        let st = empty_state(40_000.0);
        let start = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        // 8 weekdays in → Floor+Asymmetric, but NOT LEAPS yet.
        let asof = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let acts = plan_build(&st, &AppConfig::default(), asof, start);
        assert!(!acts.iter().any(|a| targets::is_leaps(targets::find(&a.ticker).unwrap())));
        // 16 weekdays in → LEAPS now eligible.
        let asof2 = NaiveDate::from_ymd_opt(2026, 6, 22).unwrap();
        let acts2 = plan_build(&st, &AppConfig::default(), asof2, start);
        assert!(acts2.iter().any(|a| targets::is_leaps(targets::find(&a.ticker).unwrap())));
    }

    #[test]
    fn asymmetric_fill_is_sequential_not_pro_rata() {
        // DCA-starvation regression (2026-07-01, fixed in lockstep with the
        // sibling aschenbrenner_portfolio): pro-rata slices of budget/N sat
        // below one share of the expensive names (STRL $777, MTZ $391), so
        // they were skipped every day. The biggest-shortfall name must get a
        // slice bounded only by the daily budget.
        let st = empty_state(40_000.0);
        let start = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let asof = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap(); // Asym active
        let cfg = AppConfig::default();
        let acts = plan_build(&st, &cfg, asof, start);
        let asym: Vec<_> = acts
            .iter()
            .filter(|a| targets::find(&a.ticker).map(|t| t.tier == Tier::Asymmetric).unwrap_or(false))
            .collect();
        assert!(!asym.is_empty());
        assert!(
            asym[0].dollars >= cfg.build_daily_budget * 0.9,
            "first asym slice {} should be ~the full budget {}",
            asym[0].dollars,
            cfg.build_daily_budget
        );
    }

    #[test]
    fn never_deploys_the_cash_buffer() {
        let st = empty_state(40_000.0);
        let start = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let asof = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let cfg = AppConfig::default();
        let acts = plan_build(&st, &cfg, asof, start);
        let total: f64 = acts.iter().map(|a| a.dollars).sum();
        assert!(total <= 40_000.0 * (1.0 - cfg.cash_buffer_pct) + 1.0, "deployed {total}");
    }
}
