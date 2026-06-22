//! Quarterly review checkpoint (PDF §6 rule 6). Recompute NAV vs the 26%-CAGR
//! glide path, surface any names on the -35% thesis-revalidate list, report
//! cluster health vs the cap, and write a dated markdown reflection. Advisory
//! only — it proposes, it does not trade (the PDF caps churn at <1 move/quarter).

use crate::core::alpaca::AlpacaClient;
use crate::core::config::get_config;
use crate::core::vault::VaultClient;
use crate::lifecycle::daily::today_in_tz;
use crate::portfolio::metrics;
use crate::portfolio::state::PortfolioState;
use chrono::{Datelike, NaiveDate};

fn quarter(today: NaiveDate) -> u32 {
    (today.month() - 1) / 3 + 1
}

/// Run the quarterly checkpoint. Returns the digest; also writes a markdown
/// reflection to the vault. Never submits orders.
pub async fn run() -> String {
    let cfg = get_config();
    let vault = VaultClient::new(Some(cfg.vault_path.clone()));
    let today = today_in_tz(cfg);

    let Some(client) = AlpacaClient::from_config(cfg) else {
        return "advisory: no Alpaca keys — cannot run quarterly review".into();
    };
    let account = match client.get_account().await {
        Ok(a) => a,
        Err(e) => return format!("error: account fetch failed: {e}"),
    };
    let positions = client.get_positions().await.unwrap_or_default();
    let state = PortfolioState::from_alpaca(&account, &positions);
    let high_water = vault.high_water();

    let start = NaiveDate::from_ymd_opt(2026, 6, 22).unwrap();
    let years = ((today - start).num_days().max(0) as f64) / 365.25;
    let (target_nav, delta) = metrics::glide_path_status(state.nav, cfg.target_nav_start, cfg.target_cagr, years);

    let mut md = String::new();
    md.push_str(&format!("# Quarterly review — {} Q{}\n\n", today.year(), quarter(today)));
    md.push_str(&format!(
        "- NAV: **${:.0}** vs glide-path **${:.0}** → **{}{:.1}%** vs the 26% CAGR pace\n",
        state.nav,
        target_nav,
        if delta >= 0.0 { "+" } else { "" },
        delta * 100.0
    ));
    md.push_str(&format!("- Cash buffer: ${:.0} ({:.1}% of NAV)\n\n", state.cash, state.cash / state.nav.max(1e-9) * 100.0));

    md.push_str("## Cluster health (cap 30%)\n\n");
    for (c, cur, tgt) in metrics::cluster_weights(&state, cfg.cash_buffer_pct) {
        let flag = if cur > cfg.cluster_cap_pct { " ⚠ OVER CAP" } else { "" };
        md.push_str(&format!("- {}: {:.1}% (target {:.1}%){}\n", c, cur * 100.0, tgt * 100.0, flag));
    }

    md.push_str("\n## Thesis-revalidate watch (-35% from cost)\n\n");
    let stops: Vec<&_> = state
        .holdings
        .iter()
        .filter(|h| h.unrealized_plpc <= -cfg.position_stop_pct)
        .collect();
    if stops.is_empty() {
        md.push_str("- none\n");
    } else {
        for h in stops {
            md.push_str(&format!("- {}: down {:.0}% — re-validate the thesis pillar before adding\n", h.ticker, h.unrealized_plpc * 100.0));
        }
    }

    md.push_str("\n## Position drift (largest 6)\n\n");
    let mut m = metrics::position_metrics(&state, cfg.cash_buffer_pct, &high_water);
    m.sort_by(|a, b| b.drift.abs().partial_cmp(&a.drift.abs()).unwrap_or(std::cmp::Ordering::Equal));
    for r in m.iter().take(6) {
        md.push_str(&format!(
            "- {} ({}): {:.1}% vs target {:.1}% → drift {:+.1}%\n",
            r.ticker, r.tier, r.weight * 100.0, r.target_weight * 100.0, r.drift * 100.0
        ));
    }
    md.push_str("\n_Checklist: (a) NAV on the 26% glide-path? (b) any thesis pillar broken (transformer shortage, hyperscaler capex guidance, NRC policy)? (c) move <1 position this quarter._\n");

    let name = format!("quarterly-{}-Q{}.md", today.year(), quarter(today));
    let _ = vault.write("reflections", &name, &md);
    md
}
