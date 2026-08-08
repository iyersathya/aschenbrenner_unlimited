//! Daily scheduler — a blocking loop on the configured timezone. This is a
//! long-horizon hold, so the cadence is deliberately sparse: one tracking +
//! rebalance window per weekday around midday (10:00 PT ≈ 13:00 ET, a calm
//! window with good fills), plus a quarterly review on the first weekday of
//! Jan/Apr/Jul/Oct.
//!
//! The daily window always calls the BUILD cycle, which self-promotes to plain
//! maintenance once the 16-name book is within band of target — so the same loop
//! drives both the initial accumulation and the steady-state rebalancing.

use crate::core::config::get_config;
use crate::core::vault::VaultClient;
use crate::lifecycle::{audit, daily, daily::RunMode, review};
use chrono::{Datelike, Timelike, Weekday};
use chrono_tz::Tz;
use std::collections::HashSet;
use std::time::Duration;

const DAILY_HOUR: u32 = 10; // 10:00 in the configured tz

/// Post-cycle broker↔targets reconcile audit. Read-only against the broker;
/// writes only the `meta/reconcile-<date>.json` note. Telegrams on alarm even
/// in dry-run (same idiom as the daily digest — alerts are not orders).
async fn run_reconcile_audit() {
    let cfg = get_config();
    let vault = VaultClient::new(Some(cfg.vault_path.clone()));
    let _ = audit::run_audit(cfg, &vault, true).await;
}

fn is_weekday(wd: Weekday) -> bool {
    !matches!(wd, Weekday::Sat | Weekday::Sun)
}

fn quarter(month: u32) -> u32 {
    (month - 1) / 3 + 1
}

pub async fn run_scheduler(armed: bool) {
    let cfg = get_config();
    let tz: Tz = cfg.timezone_str.parse().unwrap_or(chrono_tz::America::Los_Angeles);
    let mode = if armed { "ARMED" } else { "dry-run" };
    tracing::info!("scheduler up ({mode}) tz={} · daily window {:02}:00 + quarterly review", cfg.timezone_str, DAILY_HOUR);

    let mut fired: HashSet<String> = HashSet::new();

    // Catch-up on boot: if we start a weekday past the daily window and haven't
    // run today, run once immediately.
    let now = chrono::Utc::now().with_timezone(&tz);
    if cfg.catchup_on_start && is_weekday(now.weekday()) && now.hour() >= DAILY_HOUR {
        let key = format!("daily@{}", now.date_naive());
        if fired.insert(key) {
            tracing::info!("catch-up: running missed daily window");
            println!("{}", daily::run(RunMode::Build, armed).await);
            run_reconcile_audit().await;
        }
    }

    loop {
        let now = chrono::Utc::now().with_timezone(&tz);
        let date = now.date_naive();

        if is_weekday(now.weekday()) && now.hour() == DAILY_HOUR && now.minute() < 5 {
            // Quarterly review fires first (advisory), at the same window, on the
            // first weekday of a quarter-start month.
            if matches!(now.month(), 1 | 4 | 7 | 10) && now.day() <= 3 {
                let qkey = format!("quarterly@{}-Q{}", now.year(), quarter(now.month()));
                if fired.insert(qkey) {
                    tracing::info!("quarterly review window");
                    println!("{}", review::run().await);
                }
            }

            let key = format!("daily@{date}");
            if fired.insert(key) {
                tracing::info!("daily window");
                println!("{}", daily::run(RunMode::Build, armed).await);
                // Post-cycle reconcile: catches anything the cycle's orders (or
                // a foreign/manual order) left inconsistent with TARGETS.
                run_reconcile_audit().await;
            }
        }

        // Trim the dedupe set so it doesn't grow unbounded across days.
        if fired.len() > 400 {
            fired.clear();
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
