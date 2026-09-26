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
use crate::rebalance::{Action, ActionKind};
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

/// Build today's rebalance plan (maintenance mode): every rule, including the
/// Rule 4 cash deploy. Pure over its inputs (no IO).
pub fn plan_rebalance(
    state: &PortfolioState,
    cfg: &AppConfig,
    high_water: &HashMap<String, f64>,
    bias: &SignalBias,
    asof: NaiveDate,
) -> Vec<Action> {
    plan_inner(state, cfg, high_water, bias, asof, true)
}

/// The PROTECTIVE rules only - stops, profit-takes, the position and cluster
/// caps, and the Tier-D lifecycle - with Rule 4's cash deploy suppressed.
///
/// This exists so the risk floor can run while the book is still building.
/// Until 2026-09-19 every one of these rules sat behind `build_complete()`, so
/// a book that never finished building never ran a stop, a profit-take or a
/// cluster cap. The unlimited sleeve sat in build mode for 65 consecutive
/// sessions - a $602/day tranche against a $1,958 LEAPS unit could never fill,
/// and the unfilled slot pinned `build_complete` false - during which KLAC fell
/// to -28% with its -25% Floor review never firing and the Tier-D LEAPS reached
/// -74%/-91% with no time-stop flag. (Rule 2 is a separate matter: the book's
/// 68.7%-of-NAV AI-infrastructure exposure splits across six clusters of
/// 7-22% each, so the 30% cluster cap would not have fired even had it run.
/// Restoring the rule does not fix that; the cluster taxonomy is its own
/// defect.) The deploy rule stays behind the gate because the build stages own
/// buying while the book is building; letting both buy would double-deploy.
pub fn plan_protective(
    state: &PortfolioState,
    cfg: &AppConfig,
    high_water: &HashMap<String, f64>,
    bias: &SignalBias,
    asof: NaiveDate,
) -> Vec<Action> {
    plan_inner(state, cfg, high_water, bias, asof, false)
}

fn plan_inner(
    state: &PortfolioState,
    cfg: &AppConfig,
    high_water: &HashMap<String, f64>,
    bias: &SignalBias,
    asof: NaiveDate,
    deploy: bool,
) -> Vec<Action> {
    let nav = state.nav.max(1e-9);
    let mut actions: Vec<Action> = Vec::new();

    // Working dollar map over the EQUITY sleeve (Tier A/B). Tier D handled below.
    let mut mv: HashMap<String, f64> =
        targets::equity_invested().map(|t| (t.ticker.to_string(), state.market_value(t.ticker))).collect();
    let mut cash = state.cash;
    let target_d = |ticker: &str| -> f64 { targets::invested_target_weight(ticker, cfg.cash_buffer_pct) * nav };
    // The band is a tolerance on THIS name's position, so it scales with that
    // name's target — the same correction build.rs got on 2026-09-12. As
    // `band_pct × NAV` ($1,051 on a $35k book) a 2.5%-weight name ($876 target)
    // could never clear Rule 4's `room > band`, and a Floor profit-take trim
    // had to exceed 3% of NAV before it was emitted.
    let band_of = |ticker: &str| -> f64 { cfg.rebalance_band_pct * target_d(ticker) };
    // Cash-level "anything worth deploying" gate: the smallest per-name band.
    let band_floor = targets::equity_invested()
        .map(|t| band_of(t.ticker))
        .filter(|b| *b > 0.0)
        .fold(f64::INFINITY, f64::min)
        .min(cfg.rebalance_band_pct * nav);

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
                    if d > band_of(&h.ticker) {
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
                    if d > band_of(&h.ticker) {
                        add_trim(&h.ticker, d, format!("Asymmetric at {:.1}x → trim {:.0}% and ride", h.unrealized_plpc + 1.0, ASYM_PT_TRIM * 100.0), &mut trims, &mut trim_reasons);
                        if let Some(m) = mv.get_mut(t.ticker) { *m -= d; }
                        cash += d;
                    }
                }
            }
            _ => {}
        }
    }

    // ── Rule 1b: contract flags on HELD names (added 2026-09-26) ────────────
    // Until now the producer only touched what the book was about to BUY. A
    // held name the contract grades `severe` on dilution, or a strong-bearish
    // consensus, is surfaced for a thesis re-validation even when it is not
    // down 35% — the floor review reads the list. Advisory only: no order, no
    // trim. Skipped when Rule 1 already put the name on the list this session.
    for h in &state.holdings {
        if actions.iter().any(|a| a.kind == ActionKind::ThesisRevalidate && a.ticker == h.ticker) {
            continue;
        }
        let mut why: Vec<String> = Vec::new();
        if let Some(v) = bias.buy_vetoed(&h.ticker) {
            why.push(format!("contract: {v}"));
        }
        if bias.strong_bearish.contains(&h.ticker) {
            why.push("daily-analysis: strong-bearish consensus".to_string());
        }
        if !why.is_empty() {
            actions.push(Action::revalidate(
                &h.ticker,
                format!("{} — re-validate thesis (held, {:+.0}% from cost)", why.join("; "), h.unrealized_plpc * 100.0),
            ));
        }
    }

    // ── Rule 3: single-position cap (>15% → trim back to 10%), equity only ───
    for t in targets::equity_invested() {
        let cur = mv[t.ticker];
        if cur / nav > cfg.position_trim_trigger_pct {
            let dest = cfg.position_trim_target_pct * nav;
            let sell = cur - dest;
            if sell > band_of(t.ticker) {
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

    // ── Rule 2b: theme drift cap (added 2026-09-20) ─────────────────────────
    // Rule 2's 30%-per-cluster test cannot see a book concentrated in ONE driver
    // across several sub-30% clusters. On 2026-09-20 this sleeve ran 75.0% of NAV
    // in ai-infrastructure across six clusters of 7-22% — Rule 2 emitted nothing.
    // The band is measured against the theme's OWN target (66.3% for
    // ai-infrastructure), so this bounds drift without overriding a strategy that
    // deliberately concentrates. Trims only names already above their individual
    // target, exactly as Rule 2 does.
    for th in targets::themes() {
        let members = targets::theme_members(th);
        if members.is_empty() {
            continue;
        }
        let tw: f64 = members.iter().map(|m| mv[*m]).sum::<f64>() / nav;
        let tgt = targets::theme_target_weight(th, cfg.cash_buffer_pct);
        let limit = tgt + cfg.theme_drift_band_pct;
        if tw <= limit {
            continue;
        }
        let mut excess = (tw - limit) * nav;
        let mut cand: Vec<(&str, f64)> =
            members.iter().map(|m| (*m, mv[*m] - target_d(m))).filter(|(_, room)| *room > 0.0).collect();
        cand.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(
                bias.trim_priority(a.0).partial_cmp(&bias.trim_priority(b.0)).unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        for (m, room) in cand {
            if excess <= 0.0 {
                break;
            }
            let take = room.min(excess);
            if take > 0.0 {
                add_trim(m, take, format!("theme '{}' {:.1}% > target {:.1}% + {:.0}pp drift band → trim", th, tw * 100.0, tgt * 100.0, cfg.theme_drift_band_pct * 100.0), &mut trims, &mut trim_reasons);
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
        if d > band_of(t) {
            actions.push(Action::trim(t, d, trim_reasons.get(t).map(|v| v.join("; ")).unwrap_or_default()));
        }
    }

    // ── Rule 4: deploy cash on visible drawdown (equity only) ───────────────
    // Per-name drawdown from high-water marks (Floor + Asymmetric only; the
    // Tier-D lifecycle is Rule 5). Computed before the deploy gate so the
    // evaluation trace below runs in protective mode too — the sibling
    // sleeve has traced every triggered name since 2026-09-17, this one
    // emitted nothing (0 lines 9/21–9/23) because the block was skipped
    // whenever `deploy` was false.
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
    let deployable_at_eval = cash_available_for_buys(cash, nav, cfg.cash_buffer_pct) * bias.deploy_multiplier();
    // Evaluation trace: a name past the drawdown trigger that gets no buy must
    // be distinguishable in the journal from "nothing triggered today".
    for t in targets::equity_invested() {
        let name_dd = drawdown.get(t.ticker).copied().unwrap_or(0.0);
        let cluster_hit = stressed.contains(t.cluster);
        if name_dd < cfg.drawdown_deploy_single_pct && !cluster_hit {
            continue;
        }
        let room = target_d(t.ticker) - mv[t.ticker];
        let band = band_of(t.ticker);
        let verdict = if !deploy {
            "skipped: protective mode — the build stages own buying"
        } else if deployable_at_eval <= band_floor {
            "blocked: deployable ≤ band"
        } else if room > band {
            "eligible"
        } else {
            "blocked: room ≤ band"
        };
        tracing::info!(
            "rule4: {} dd {:.1}% from high-water (trigger: {}) room ${:.0} vs band ${:.0}, deployable ${:.0} — {}",
            t.ticker,
            name_dd * 100.0,
            if cluster_hit && name_dd < cfg.drawdown_deploy_single_pct { "cluster stressed" } else { "single-name" },
            room,
            band,
            deployable_at_eval,
            verdict
        );
    }
    // Zero in protective mode: while the book is building the build stages own
    // all buying, so running both would double-deploy. `band_floor` is always
    // > 0, so a zero budget skips the whole rule without re-indenting it.
    let mut deployable = if deploy { deployable_at_eval } else { 0.0 };
    if deployable > band_floor {
        let mut eligible: Vec<&'static targets::Target> = targets::equity_invested()
            .filter(|t| {
                let room = target_d(t.ticker) - mv[t.ticker];
                let name_dd = drawdown.get(t.ticker).copied().unwrap_or(0.0);
                bias.buy_vetoed(t.ticker).is_none()
                    && room > band_of(t.ticker)
                    && (name_dd >= cfg.drawdown_deploy_single_pct || stressed.contains(t.cluster))
            })
            .collect();
        for (t, why) in &bias.buy_veto {
            tracing::info!("rule4: {} buy VETOED this session by the contract — {}", t, why);
        }
        eligible.sort_by(|a, b| {
            bias.buy_priority(b.ticker).partial_cmp(&bias.buy_priority(a.ticker)).unwrap_or(std::cmp::Ordering::Equal)
                .then((target_d(b.ticker) - mv[b.ticker]).partial_cmp(&(target_d(a.ticker) - mv[a.ticker])).unwrap_or(std::cmp::Ordering::Equal))
        });
        for t in eligible {
            if deployable <= band_floor { break; }
            let room = target_d(t.ticker) - mv[t.ticker];
            let take = room.min(deployable);
            if take > band_of(t.ticker) {
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
    fn band_scales_with_the_name_not_nav() {
        // MTZ (2.5% target ≈ $875 on $35k) down 30% from high and $600 under
        // target, $2,000 deployable. With band = 3% × NAV = $1,050 the $600
        // room never cleared Rule 4; per-name band = 3% × $875 ≈ $26 does.
        let nav = 35_000.0;
        let tgt = targets::invested_target_weight("MTZ", 0.05) * nav;
        assert!(tgt < 1_200.0, "test assumes a small target, got {tgt}");
        let held = (tgt - 600.0).max(50.0);
        let st = state(vec![pos("MTZ", held, 200.0, -0.30)], 2_000.0 + nav * 0.05, nav);
        let mut hw = HashMap::new();
        hw.insert("MTZ".to_string(), 300.0);
        let acts = plan_rebalance(&st, &cfg(), &hw, &SignalBias::default(), day());
        let buy = acts.iter().find(|a| a.ticker == "MTZ" && a.kind == ActionKind::Buy).expect("rule-4 buy");
        assert!((buy.dollars - (tgt - held)).abs() < 1.0, "buys the room, got {}", buy.dollars);
    }

    // ── protective-mode split (2026-09-19 build-deadlock fix) ───────────────

    #[test]
    fn protective_mode_suppresses_the_rule_4_deploy() {
        // Exactly the band_scales_with_the_name_not_nav setup, which DOES emit a
        // Rule 4 buy in maintenance mode — protective mode must emit none, so a
        // building book never double-deploys against the build stages.
        let nav = 35_000.0;
        let tgt = targets::invested_target_weight("MTZ", 0.05) * nav;
        let held = (tgt - 600.0).max(50.0);
        let st = state(vec![pos("MTZ", held, 200.0, -0.30)], 2_000.0 + nav * 0.05, nav);
        let mut hw = HashMap::new();
        hw.insert("MTZ".to_string(), 300.0);
        assert!(
            plan_rebalance(&st, &cfg(), &hw, &SignalBias::default(), day())
                .iter()
                .any(|a| a.kind == ActionKind::Buy),
            "maintenance mode should still deploy"
        );
        assert!(
            !plan_protective(&st, &cfg(), &hw, &SignalBias::default(), day())
                .iter()
                .any(|a| a.kind == ActionKind::Buy),
            "protective mode must never emit a Buy"
        );
    }

    #[test]
    fn protective_mode_still_runs_the_floor_review() {
        // The KLAC case: a Floor name at -28% while the book is still building.
        // Before the fix this review lived behind build_complete() and never ran.
        let st = state(vec![pos("GEV", 500.0, 1000.0, -0.28)], 39_000.0, 40_000.0);
        let acts = plan_protective(&st, &cfg(), &HashMap::new(), &SignalBias::default(), day());
        assert!(
            acts.iter().any(|a| a.ticker == "GEV" && a.kind == ActionKind::ThesisRevalidate),
            "a -28% Floor name must be flagged while building, got {acts:?}"
        );
    }

    #[test]
    fn protective_mode_still_caps_an_oversized_position() {
        // GEV at 20% of NAV → the Rule 3 trim must fire during the build too.
        let st = state(vec![pos("GEV", 8_000.0, 1000.0, 0.1)], 32_000.0, 40_000.0);
        let acts = plan_protective(&st, &cfg(), &HashMap::new(), &SignalBias::default(), day());
        let trim = acts
            .iter()
            .find(|a| a.ticker == "GEV" && a.kind == ActionKind::Trim)
            .expect("position cap must apply while building");
        assert!((trim.dollars - 4_000.0).abs() < 1.0, "got {}", trim.dollars);
    }

    #[test]
    fn protective_and_maintenance_agree_once_the_deploy_is_excluded() {
        // The split must change ONLY the deploy rule: every non-Buy action is
        // identical in both modes.
        let st = state(
            vec![pos("GEV", 8_000.0, 1000.0, -0.40), pos("PLTR", 5_000.0, 665.0, 4.0)],
            30_000.0,
            40_000.0,
        );
        let hw = HashMap::new();
        let maint: Vec<_> = plan_rebalance(&st, &cfg(), &hw, &SignalBias::default(), day())
            .into_iter()
            .filter(|a| a.kind != ActionKind::Buy)
            .collect();
        let prot: Vec<_> = plan_protective(&st, &cfg(), &hw, &SignalBias::default(), day())
            .into_iter()
            .filter(|a| a.kind != ActionKind::Buy)
            .collect();
        assert_eq!(maint, prot, "the split must not alter any protective rule");
        assert!(!maint.is_empty(), "test needs at least one protective action");
    }

    // ── theme drift cap (Rule 2b, 2026-09-20) ───────────────────────────────

    #[test]
    fn theme_layer_maps_every_equity_cluster() {
        // Every cluster must resolve to a real theme; an unmapped cluster would
        // silently fall into "other" and escape Rule 2b — the exact failure mode
        // this layer exists to fix.
        for t in targets::equity_invested() {
            assert_ne!(
                targets::theme_of(t.cluster),
                "other",
                "cluster '{}' ({}) is not mapped to a theme",
                t.cluster,
                t.ticker
            );
        }
    }

    #[test]
    fn ai_infrastructure_theme_target_matches_the_strategy() {
        // The v4 strategy deliberately targets ~66% of NAV in ai-infrastructure.
        // Rule 2b measures drift against THIS number, so if the target model ever
        // changes, the guard follows it instead of fighting it.
        let w = targets::theme_target_weight("ai-infrastructure", 0.05);
        assert!((0.60..0.72).contains(&w), "ai-infrastructure target weight {w}");
        let d = targets::theme_target_weight("defense", 0.05);
        assert!((0.12..0.21).contains(&d), "defense target weight {d}");
    }

    #[test]
    fn theme_cap_does_not_fire_at_the_strategys_own_concentration() {
        // THE SAFETY TEST. A book sitting exactly at its target weights is fully
        // concentrated by design; Rule 2b must emit nothing. If this fails, the
        // guard is overriding the strategy rather than bounding its drift.
        let nav = 40_000.0;
        let cfg = cfg();
        let mut ps = vec![];
        for t in targets::equity_invested() {
            let w = targets::invested_target_weight(t.ticker, cfg.cash_buffer_pct);
            ps.push(pos(t.ticker, w * nav, 100.0, 0.0));
        }
        let st = state(ps, nav * cfg.cash_buffer_pct, nav);
        let acts = plan_rebalance(&st, &cfg, &HashMap::new(), &SignalBias::default(), day());
        assert!(
            !acts.iter().any(|a| a.kind == ActionKind::Trim && a.reason.contains("theme")),
            "Rule 2b must not trim a book at its own targets, got {acts:?}"
        );
    }

    #[test]
    fn theme_cap_fires_once_drift_exceeds_the_band() {
        // Scale every ai-infrastructure name up 25% and leave defense at target:
        // the theme drifts well past target + 12pp and Rule 2b must trim.
        let nav = 40_000.0;
        let cfg = cfg();
        let mut ps = vec![];
        let mut total = 0.0;
        for t in targets::equity_invested() {
            let w = targets::invested_target_weight(t.ticker, cfg.cash_buffer_pct);
            let mult = if targets::theme_of(t.cluster) == "ai-infrastructure" { 1.25 } else { 1.0 };
            let v = w * nav * mult;
            total += v;
            ps.push(pos(t.ticker, v, 100.0, 0.0));
        }
        // NAV must be what the book is actually worth, or the theme weight is
        // measured against a denominator the positions do not sum to.
        let st = state(ps, 0.0, total);
        let acts = plan_rebalance(&st, &cfg, &HashMap::new(), &SignalBias::default(), day());
        let themed: Vec<_> =
            acts.iter().filter(|a| a.kind == ActionKind::Trim && a.reason.contains("theme")).collect();
        assert!(!themed.is_empty(), "Rule 2b should have trimmed, got {acts:?}");
        for a in &themed {
            assert_eq!(
                targets::theme_of(targets::find(&a.ticker).unwrap().cluster),
                "ai-infrastructure",
                "Rule 2b trimmed a name outside the breaching theme: {}",
                a.ticker
            );
        }
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

    #[test]
    fn contract_flags_on_a_held_name_surface_a_revalidate_without_a_drawdown() {
        let nav = 36_000.0;
        let st = state(vec![pos("MTZ", 1_000.0, 200.0, 0.10), pos("GEV", 1_000.0, 500.0, 0.05)], nav * 0.30, nav);
        let mut bias = SignalBias { enabled: true, ..Default::default() };
        bias.buy_veto.insert("MTZ".into(), "dilution severe: active 424B takedown".into());
        bias.strong_bearish.insert("GEV".into());
        let acts = plan_protective(&st, &cfg(), &HashMap::new(), &bias, NaiveDate::from_ymd_opt(2026, 9, 26).unwrap());
        let rv: Vec<&Action> = acts.iter().filter(|a| a.kind == ActionKind::ThesisRevalidate).collect();
        assert!(rv.iter().any(|a| a.ticker == "MTZ" && a.reason.contains("contract:")), "{acts:?}");
        assert!(rv.iter().any(|a| a.ticker == "GEV" && a.reason.contains("strong-bearish")), "{acts:?}");
        // Advisory only: no buy or trim was created for either name.
        assert!(!acts.iter().any(|a| a.kind != ActionKind::ThesisRevalidate && (a.ticker == "MTZ" || a.ticker == "GEV")));
        // A disabled bias flags nothing.
        let off = SignalBias { enabled: false, buy_veto: bias.buy_veto.clone(), ..Default::default() };
        assert!(!plan_protective(&st, &cfg(), &HashMap::new(), &off, NaiveDate::from_ymd_opt(2026, 9, 26).unwrap()).iter().any(|a| a.kind == ActionKind::ThesisRevalidate));
    }
}
