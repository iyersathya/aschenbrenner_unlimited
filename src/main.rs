//! aschenbrenner-unlimited — the *Floor 2x / Ceiling UNLIMITED* (v4) sleeve:
//! a $40K AGI-infrastructure barbell with a Tier-D **LEAPS-options + micro-cap
//! moonshot** sleeve on top of the stock floor (24 positions, 4 tiers).
//!
//! A daily-cadence portfolio TRACKER + REBALANCER that trades both stocks and
//! long-dated call options. Capital-isolated paper sleeve: its own Alpaca paper
//! account, gdrive vault, Telegram bot, and config. Sibling of
//! `aschenbrenner_portfolio` (the stocks-only floor2x_10x sleeve); the two run
//! independently and share only the daily_analysis vault + ticker universe.

mod core;
mod execute;
mod lifecycle;
mod portfolio;
mod rebalance;
mod run;
mod scheduler;
mod signals;

#[tokio::main]
async fn main() {
    std::process::exit(run::main().await);
}
