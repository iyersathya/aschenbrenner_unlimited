//! CLI dispatch. `main()` parses argv and routes to one async command per verb.
//!
//! Trading verbs (`build`, `daily`/`track`, `rebalance`) are ARMED by default —
//! they submit orders (PA-paper-guarded). Pass `dry-run` to compute-and-log only.

use crate::core::alpaca::AlpacaClient;
use crate::core::config::get_config;
use crate::core::safety::safety_check;
use crate::core::vault::VaultClient;
use crate::core::{delivery, safety::SafetyVerdict};
use crate::lifecycle::daily::{self, RunMode};
use crate::lifecycle::review;
use crate::portfolio::{metrics, state::PortfolioState, targets};
use crate::signals::daily_analysis;

fn is_mode_token(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "dry-run" | "dryrun" | "execute" | "armed")
}

/// Armed unless a `dry-run` token is present (execute/armed are harmless no-ops).
fn armed_from(args: &[String]) -> bool {
    !args.iter().any(|a| matches!(a.to_lowercase().as_str(), "dry-run" | "dryrun"))
}

pub async fn main() -> i32 {
    crate::core::logging::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "help".into());
    let rest: &[String] = if args.len() > 1 { &args[1..] } else { &[] };

    match cmd.as_str() {
        "config" => cmd_config(),
        "status" => cmd_status().await,
        "positions" => cmd_positions().await,
        "build" => {
            println!("{}", daily::run(RunMode::Build, armed_from(rest)).await);
        }
        "daily" | "track" | "rebalance" => {
            println!("{}", daily::run(RunMode::Maintenance, armed_from(rest)).await);
        }
        "review" => {
            println!("{}", review::run().await);
        }
        "reconcile" => cmd_reconcile(rest).await,
        "signals" => cmd_signals(rest).await,
        "cancel" => cmd_cancel().await,
        "notify" => {
            let text = rest.iter().filter(|a| !is_mode_token(a)).cloned().collect::<Vec<_>>().join(" ");
            let text = if text.is_empty() { "aschenbrenner-unlimited test notification".into() } else { text };
            let n = delivery::notify_text(&text).await;
            println!("notify: delivered to {n} channel(s)");
        }
        "test" => cmd_test(),
        "scheduler" => crate::scheduler::run_scheduler(armed_from(rest)).await,
        "help" | "--help" | "-h" => cmd_help(),
        other => {
            println!("Unknown command: {other}\n");
            cmd_help();
            return 2;
        }
    }
    0
}

fn cmd_config() {
    let cfg = get_config();
    let vault = VaultClient::new(Some(cfg.vault_path.clone()));
    println!("aschenbrenner-unlimited config");
    println!("  base_url        : {}", cfg.alpaca_base_url);
    println!("  keys present    : {}", !cfg.alpaca_api_key.is_empty());
    println!("  vault           : {}", cfg.vault_path.display());
    println!("  daily_analysis  : {}", cfg.daily_analysis_dir().display());
    println!("  portfolio.md    : {} ({})", cfg.portfolio_md_path.display(), if cfg.portfolio_md_path.exists() { "found" } else { "MISSING" });
    println!("  target          : ${:.0} → 26% CAGR over {} yr", cfg.target_nav_start, cfg.target_horizon_years);
    println!(
        "  rules           : cluster-cap {:.0}% · trim {:.0}%→{:.0}% · stop -{:.0}% · deploy>{:.0}% dd · band {:.0}%",
        cfg.cluster_cap_pct * 100.0,
        cfg.position_trim_trigger_pct * 100.0,
        cfg.position_trim_target_pct * 100.0,
        cfg.position_stop_pct * 100.0,
        cfg.drawdown_deploy_pct * 100.0,
        cfg.rebalance_band_pct * 100.0,
    );
    println!("  signal bias     : {}", cfg.signal_bias_enabled);
    for sub in crate::core::vault::SUBFOLDERS {
        println!("  vault/{:<11}: {} file(s)", sub, vault.list_pattern(sub, "", "").len());
    }
}

async fn cmd_status() {
    let cfg = get_config();
    let alpaca = AlpacaClient::from_config(cfg);
    match safety_check(cfg, alpaca.as_ref()).await {
        SafetyVerdict::Halted(why) => {
            println!("SAFETY: HALTED — {why}");
            return;
        }
        SafetyVerdict::Ok => println!("SAFETY: ok (kill-switch clear, paper guard passed)"),
    }
    let Some(client) = alpaca else {
        println!("ADVISORY MODE: no Alpaca keys configured.");
        return;
    };
    match client.get_account().await {
        Ok(a) => {
            println!("account   : {} (paper={})", a.account_number, a.paper);
            println!("equity    : ${:.2}", a.equity);
            println!("cash      : ${:.2}", a.cash);
            println!("portfolio : ${:.2}", a.portfolio_value);
            println!("buying pwr: ${:.2}", a.buying_power);
            if !a.account_number.starts_with("PA") {
                println!("⛔ NON-PAPER ACCOUNT — order submission is blocked by the guard.");
            }
        }
        Err(e) => println!("error: {e}"),
    }
}

async fn cmd_positions() {
    let cfg = get_config();
    let Some(client) = AlpacaClient::from_config(cfg) else {
        println!("ADVISORY MODE: no Alpaca keys configured.");
        return;
    };
    let account = match client.get_account().await {
        Ok(a) => a,
        Err(e) => {
            println!("error: {e}");
            return;
        }
    };
    let positions = client.get_positions().await.unwrap_or_default();
    let state = PortfolioState::from_alpaca(&account, &positions);
    let vault = VaultClient::new(Some(cfg.vault_path.clone()));
    let hw = vault.high_water();

    println!("NAV ${:.0} · cash ${:.0}\n", state.nav, state.cash);
    println!("{:<6} {:<7} {:>10} {:>8} {:>8} {:>8} {:>7}", "TICK", "TIER", "MKT_VAL", "WEIGHT", "TARGET", "DRIFT", "DD");
    for r in metrics::position_metrics(&state, cfg.cash_buffer_pct, &hw) {
        println!(
            "{:<6} {:<7} {:>10.0} {:>7.1}% {:>7.1}% {:>+7.1}% {:>6.0}%",
            r.ticker, r.tier, r.market_value, r.weight * 100.0, r.target_weight * 100.0, r.drift * 100.0, r.drawdown * 100.0
        );
    }
    println!("\nclusters (cap {:.0}%):", cfg.cluster_cap_pct * 100.0);
    for (c, cur, tgt) in metrics::cluster_weights(&state, cfg.cash_buffer_pct) {
        let flag = if cur > cfg.cluster_cap_pct { " ⚠ OVER" } else { "" };
        println!("  {:<20} {:>5.1}% (target {:>4.1}%){}", c, cur * 100.0, tgt * 100.0, flag);
    }
}

async fn cmd_signals(rest: &[String]) {
    let cfg = get_config();
    let today = daily::today_in_tz(cfg);
    let tickers: Vec<String> = if rest.iter().any(|a| !is_mode_token(a)) {
        rest.iter().filter(|a| !is_mode_token(a)).map(|s| s.to_uppercase()).collect()
    } else {
        targets::invested().map(|t| t.ticker.to_string()).collect()
    };
    if let Some(s) = daily_analysis::macro_score(cfg, today, 5) {
        println!("macro regime: {s}/100\n");
    }
    println!("{:<6} {:<8} {:>6} {:<9} {}", "TICK", "DIR", "CONF", "FOUND", "DECISION");
    for t in tickers {
        let a = daily_analysis::read(cfg, &t, today, 5);
        println!(
            "{:<6} {:<8} {:>6.2} {:<9} {}",
            t,
            format!("{:?}", a.direction),
            a.confidence,
            a.found,
            a.decision
        );
    }
}

async fn cmd_reconcile(rest: &[String]) {
    let cfg = get_config();
    let vault = VaultClient::new(Some(cfg.vault_path.clone()));
    let send = !rest.iter().any(|a| a.eq_ignore_ascii_case("no-send"));
    println!("aschenbrenner-unlimited — broker↔targets reconcile audit");
    match crate::lifecycle::audit::run_audit(cfg, &vault, send).await {
        None => println!("Alpaca keys not configured or broker unreachable — audit skipped."),
        Some(r) => {
            println!(
                "date {} · broker positions {} · designed targets {} · alarms {} · notes {}",
                r.date, r.broker_positions, r.expected_targets, r.alarms, r.notes
            );
            for line in &r.explained {
                println!("  explained : {line}");
            }
            for f in &r.findings {
                let sev = if f.kind.is_alarm() { "ALARM" } else { "note " };
                println!(
                    "  {sev}     : {:?} {} qty {:+.0} mv ${:+.0} — {}",
                    f.kind, f.symbol, f.qty, f.market_value, f.detail
                );
            }
            println!(
                "Audit note written to meta/reconcile-{}.json{}.",
                r.date,
                if r.alarms == 0 {
                    " (clean — no Telegram)"
                } else if send {
                    "; Telegram alarm sent"
                } else {
                    " (no-send)"
                }
            );
        }
    }
}

async fn cmd_cancel() {
    let cfg = get_config();
    let Some(client) = AlpacaClient::from_config(cfg) else {
        println!("ADVISORY MODE: no Alpaca keys configured.");
        return;
    };
    match client.cancel_all_orders().await {
        Ok(n) => println!("cancelled {n} open order(s)"),
        Err(e) => println!("error: {e}"),
    }
}

fn cmd_test() {
    let cfg = get_config();
    let mut ok = true;

    // Vault roundtrip.
    let vault = VaultClient::new(Some(cfg.vault_path.clone()));
    match vault.write("meta", "selftest.txt", "ok") {
        Ok(_) => match vault.read("meta", "selftest.txt").as_deref() {
            Some("ok") => println!("[ok] vault read/write roundtrip"),
            _ => {
                println!("[FAIL] vault read-back mismatch");
                ok = false;
            }
        },
        Err(e) => {
            println!("[FAIL] vault write: {e}");
            ok = false;
        }
    }

    // portfolio.md present.
    if cfg.portfolio_md_path.exists() {
        println!("[ok] portfolio.md present");
    } else {
        println!("[warn] portfolio.md missing at {}", cfg.portfolio_md_path.display());
    }

    // Paper-URL guard: base_url must be the paper host without a /v2 suffix.
    if cfg.alpaca_base_url.contains("paper-api") && !cfg.alpaca_base_url.ends_with("/v2") {
        println!("[ok] base_url is paper host, no /v2 suffix");
    } else {
        println!("[FAIL] base_url unexpected: {}", cfg.alpaca_base_url);
        ok = false;
    }

    // Target model integrity.
    let total: f64 = targets::invested().map(|t| targets::invested_target_weight(t.ticker, cfg.cash_buffer_pct)).sum();
    if (total - (1.0 - cfg.cash_buffer_pct)).abs() < 1e-6 && targets::invested().count() == 22 {
        println!("[ok] 22 invested targets normalize to {:.1}% (+{:.0}% cash)", total * 100.0, cfg.cash_buffer_pct * 100.0);
    } else {
        println!("[FAIL] target model integrity: sum {total}");
        ok = false;
    }

    println!("{}", if ok { "TEST: PASS" } else { "TEST: FAIL" });
}

fn cmd_help() {
    println!(
        "aschenbrenner-unlimited — long-term ($40K→$80K/3yr) AGI-infrastructure equity sleeve\n\n\
         Commands:\n\
         \x20 config              Show resolved config + vault counts\n\
         \x20 status              Account balance + safety/paper guard\n\
         \x20 positions           Holdings with weight/target/drift + cluster weights\n\
         \x20 build [dry-run]     Phased initial build (PDF §8). ARMED by default\n\
         \x20 daily|track [dry-run]   Daily tracking + maintenance rebalance. ARMED by default\n\
         \x20 rebalance [dry-run] Force a maintenance rebalance now. ARMED by default\n\
         \x20 review              Quarterly checkpoint digest (advisory; no orders)\n\
         \x20 reconcile [no-send] Broker↔targets position audit (read-only; alarms on short/\n\
         \x20                     non-long-call/untracked; writes meta/reconcile-<date>.json)\n\
         \x20 signals [TICKER..]  Show daily-analysis overlay for the portfolio names\n\
         \x20 cancel              Cancel all open orders\n\
         \x20 notify [text]       Send a test Telegram message\n\
         \x20 test                Offline sanity (no network)\n\
         \x20 scheduler [dry-run] Blocking daily loop (one midday window + quarterly). ARMED by default\n\
         \x20 help                This message"
    );
}
