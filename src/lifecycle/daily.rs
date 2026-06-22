//! Daily lifecycle — the once-a-day tracking + rebalance cycle, plus the phased
//! build cycle. Shared scaffold: safety → snapshot → ratchet high-water → record
//! NAV → load signal bias → plan (build | maintenance) → execute → persist cards
//! → digest (+ best-effort Telegram).

use crate::core::alpaca::AlpacaClient;
use crate::core::alpaca_data::AlpacaDataClient;
use crate::core::config::{get_config, AppConfig};
use crate::core::safety::{safety_check, SafetyVerdict};
use crate::core::{delivery, vault::VaultClient};
use crate::execute::{execute_actions, ExecResult};
use crate::portfolio::state::PortfolioState;
use crate::portfolio::{metrics, targets};
use crate::rebalance::build::{self, build_complete};
use crate::rebalance::rules::plan_rebalance;
use crate::rebalance::signal_bias::SignalBias;
use crate::rebalance::Action;
use chrono::NaiveDate;
use chrono_tz::Tz;
use serde_json::json;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq)]
pub enum RunMode {
    Build,
    Maintenance,
}

/// Today's date in the configured timezone.
pub fn today_in_tz(cfg: &AppConfig) -> NaiveDate {
    let tz: Tz = cfg.timezone_str.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    chrono::Utc::now().with_timezone(&tz).date_naive()
}

/// The persisted build start date (inception), creating it on first call.
fn build_start(vault: &VaultClient, today: NaiveDate) -> NaiveDate {
    let v = vault.read_json("meta", "build-state.json");
    if let Some(s) = v.get("start_date").and_then(|x| x.as_str()) {
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return d;
        }
    }
    let _ = vault.write_json(
        "meta",
        "build-state.json",
        &json!({"start_date": today.format("%Y-%m-%d").to_string()}),
    );
    today
}

/// Ratchet per-name high-water marks up to the latest observed prices.
fn update_high_water(vault: &VaultClient, state: &PortfolioState) -> HashMap<String, f64> {
    let mut marks = vault.high_water();
    for h in &state.holdings {
        if h.price > 0.0 {
            let e = marks.entry(h.ticker.clone()).or_insert(h.price);
            if h.price > *e {
                *e = h.price;
            }
        }
    }
    vault.save_high_water(&marks);
    marks
}

/// Run a daily cycle. `armed` submits orders; otherwise dry-run. Returns the
/// human-readable digest (also sent to Telegram when configured).
pub async fn run(mode: RunMode, armed: bool) -> String {
    let cfg = get_config();
    let vault = VaultClient::new(Some(cfg.vault_path.clone()));
    let today = today_in_tz(cfg);
    let alpaca = AlpacaClient::from_config(cfg);
    let data = AlpacaDataClient::from_config(cfg);

    // Safety gate (kill switch + PA guard).
    match safety_check(cfg, alpaca.as_ref()).await {
        SafetyVerdict::Halted(why) => {
            let msg = format!("⛔ aschenbrenner-unlimited HALTED: {why}");
            tracing::error!("{msg}");
            delivery::notify_text(&msg).await;
            return msg;
        }
        SafetyVerdict::Ok => {}
    }

    // Without keys we cannot read the account → advisory note only.
    let Some(client) = alpaca.as_ref() else {
        return "advisory: no Alpaca keys configured — set ALPACA_API_KEY_ID/SECRET in .env".into();
    };

    let account = match client.get_account().await {
        Ok(a) => a,
        Err(e) => return format!("error: account fetch failed: {e}"),
    };
    let positions = client.get_positions().await.unwrap_or_default();
    let state = PortfolioState::from_alpaca(&account, &positions);

    let high_water = update_high_water(&vault, &state);
    vault.record_nav(&today.format("%Y-%m-%d").to_string(), state.nav, 1500);
    let bias = SignalBias::load(cfg, today);

    // Plan.
    let (actions, mode_label) = match mode {
        RunMode::Build => {
            let start = build_start(&vault, today);
            if build_complete(&state, cfg) {
                // Book is built — fall through to maintenance so a stray build
                // invocation doesn't keep over-buying.
                (plan_rebalance(&state, cfg, &high_water, &bias, today), "build→maintenance (book complete)")
            } else {
                (build::plan_build(&state, cfg, today, start), "build")
            }
        }
        RunMode::Maintenance => (plan_rebalance(&state, cfg, &high_water, &bias, today), "maintenance"),
    };

    let results = execute_actions(&actions, cfg, alpaca.as_ref(), data.as_ref(), armed, &state).await;
    persist_cards(&vault, today, mode_label, armed, &actions, &results);

    let digest = build_digest(cfg, today, mode_label, armed, &state, &high_water, &bias, &results);
    delivery::notify_text(&digest).await;
    digest
}

fn persist_cards(
    vault: &VaultClient,
    today: NaiveDate,
    mode_label: &str,
    armed: bool,
    actions: &[Action],
    results: &[ExecResult],
) {
    let cards: Vec<_> = actions
        .iter()
        .map(|a| json!({"ticker": a.ticker, "kind": format!("{:?}", a.kind), "dollars": a.dollars, "reason": a.reason}))
        .collect();
    let execs: Vec<_> = results
        .iter()
        .map(|r| json!({"ticker": r.ticker, "side": r.side, "qty": r.qty, "limit": r.limit, "dollars": r.dollars, "status": r.status}))
        .collect();
    let payload = json!({
        "date": today.format("%Y-%m-%d").to_string(),
        "mode": mode_label,
        "armed": armed,
        "actions": cards,
        "results": execs,
    });
    let _ = vault.write_json("cards", &format!("cards-{}.json", today.format("%Y-%m-%d")), &payload);
}

#[allow(clippy::too_many_arguments)]
fn build_digest(
    cfg: &AppConfig,
    today: NaiveDate,
    mode_label: &str,
    armed: bool,
    state: &PortfolioState,
    high_water: &HashMap<String, f64>,
    bias: &SignalBias,
    results: &[ExecResult],
) -> String {
    let mut out = String::new();
    let arm = if armed { "ARMED" } else { "dry-run" };
    out.push_str(&format!(
        "🚀 aschenbrenner-unlimited · {} · {} ({})\n",
        today.format("%Y-%m-%d"),
        mode_label,
        arm
    ));

    // NAV + glide path.
    let start = NaiveDate::from_ymd_opt(2026, 6, 22).unwrap();
    let years = ((today - start).num_days().max(0) as f64) / 365.25;
    let (target_nav, delta) = metrics::glide_path_status(state.nav, cfg.target_nav_start, cfg.target_cagr, years);
    out.push_str(&format!(
        "NAV ${:.0} · cash ${:.0} · glide-path ${:.0} ({}{:.1}% vs 26% CAGR pace)\n",
        state.nav,
        state.cash,
        target_nav,
        if delta >= 0.0 { "+" } else { "" },
        delta * 100.0
    ));
    if let Some(s) = bias.macro_score {
        out.push_str(&format!("macro regime {}/100 · deploy mult {:.2}\n", s, bias.deploy_multiplier()));
    }

    // Cluster weights vs cap.
    out.push_str("clusters: ");
    let cw = metrics::cluster_weights(state, cfg.cash_buffer_pct);
    let parts: Vec<String> = cw
        .iter()
        .map(|(c, cur, _tgt)| {
            let flag = if *cur > cfg.cluster_cap_pct { "⚠" } else { "" };
            format!("{} {:.0}%{}", c, cur * 100.0, flag)
        })
        .collect();
    out.push_str(&parts.join(" · "));
    out.push('\n');

    // Largest drifts.
    let mut m = metrics::position_metrics(state, cfg.cash_buffer_pct, high_water);
    m.sort_by(|a, b| b.drift.abs().partial_cmp(&a.drift.abs()).unwrap_or(std::cmp::Ordering::Equal));
    out.push_str("top drift: ");
    let drifts: Vec<String> = m
        .iter()
        .take(4)
        .map(|r| format!("{} {:+.1}%", r.ticker, r.drift * 100.0))
        .collect();
    out.push_str(&drifts.join(" · "));
    out.push('\n');

    // Moonshot sleeve (Tier D — LEAPS + micro-caps): held value + P&L.
    let held: Vec<String> = targets::moonshot()
        .filter_map(|t| {
            let sym = targets::instrument_symbol(t, cfg)?;
            let h = state.holding(&sym)?;
            Some(format!("{} ${:.0} ({:+.0}%)", targets::display_name(t, cfg), h.market_value, h.unrealized_plpc * 100.0))
        })
        .collect();
    if !held.is_empty() {
        out.push_str("moonshot: ");
        out.push_str(&held.join(" · "));
        out.push('\n');
    }

    // Actions.
    let trade_count = results.iter().filter(|r| r.qty > 0).count();
    let advisories = crate::execute::advisory_count(results);
    if results.is_empty() {
        out.push_str("actions: none — portfolio within rebalance band\n");
    } else {
        out.push_str(&format!("actions: {} order(s), {} advisory\n", trade_count, advisories));
        for r in results {
            out.push_str("  ");
            out.push_str(&r.line());
            out.push('\n');
        }
    }
    out
}
