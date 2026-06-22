//! Authoritative target model for the `2x_unlimited_stocks.pdf` v4 strategy —
//! *Floor 2x / Ceiling UNLIMITED*. 24 positions across 4 tiers: a 50% large-cap
//! Floor, a 32.5% Asymmetric multi-bagger sleeve, a 12.5% **Moonshot** sleeve
//! (LEAPS long calls + config-pinned micro-caps), and a 5% cash buffer.
//!
//! Tier D (Moonshot) is exempt from the 30% cluster cap and the position cap — it
//! has its own lifecycle (time-stops + profit-takes). `portfolio.md` is the human
//! mirror; the two must stay in lockstep.

use crate::core::alpaca_data::{build_occ, third_friday};
use crate::core::config::AppConfig;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Tier A — large-cap floor builders. Protects the 2x floor.
    Floor,
    /// Tier B — mid/small-cap asymmetric multi-baggers. NO stop.
    Asymmetric,
    /// Tier D — moonshot sleeve: LEAPS long calls + micro-cap lotto. May go to
    /// zero; exempt from the equity cluster/position caps.
    Moonshot,
    /// Tier C — cash buffer.
    Cash,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Floor => "Floor",
            Tier::Asymmetric => "Asym",
            Tier::Moonshot => "Moon",
            Tier::Cash => "Cash",
        }
    }
}

/// What a target actually trades as.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Instrument {
    Stock,
    /// Long-dated OTM call (LEAPS). Expiry = 3rd Friday of January `expiry_year`.
    Leaps { underlying: &'static str, expiry_year: i32, strike: f64 },
    /// Config-pinned micro-cap (slot 1 = quantum, slot 2 = defense/space).
    MicroCap { slot: u8 },
}

#[allow(dead_code)] // thesis/spot_anchor surfaced in digests/docs, not every path
#[derive(Clone, Copy, Debug)]
pub struct Target {
    /// Unique label (stock symbol for stocks; e.g. "NVDA_LEAPS"/"MICROCAP1" otherwise).
    pub ticker: &'static str,
    pub tier: Tier,
    /// Correlated cluster (Tier A/B only; empty for Moonshot/Cash).
    pub cluster: &'static str,
    pub target_pct: f64,
    /// Mid-June 2026 reference price (stock spot, or LEAPS premium guidance).
    pub spot_anchor: f64,
    pub instrument: Instrument,
    pub thesis: &'static str,
}

#[allow(dead_code)]
pub const CASH_TICKER: &str = "CASH";

const S: Instrument = Instrument::Stock;

/// 22 invested positions + cash. 50% Floor / 32.5% Asymmetric / 12.5% Moonshot / 5% cash.
pub const TARGETS: [Target; 23] = [
    // ── Tier A — Floor builders · 50% ──────────────────────────────────────
    Target { ticker: "GEV",  tier: Tier::Floor,      cluster: "Grid hardware",      target_pct: 10.0, spot_anchor: 1049.0, instrument: S, thesis: "Gas-turbine OEM + grid solutions; $163B backlog; closest public analog to Aschenbrenner's BE long." },
    Target { ticker: "CEG",  tier: Tier::Floor,      cluster: "Nuclear baseload",   target_pct: 10.0, spot_anchor: 267.0,  instrument: S, thesis: "~25% of US nuclear; Microsoft/Meta PPAs; deepest options chain in the nuclear cohort." },
    Target { ticker: "ETN",  tier: Tier::Floor,      cluster: "Grid hardware",      target_pct: 7.5,  spot_anchor: 410.0,  instrument: S, thesis: "Switchgear + power distribution; DC orders +240% YoY; sell-side PT $654." },
    Target { ticker: "VST",  tier: Tier::Floor,      cluster: "Nuclear baseload",   target_pct: 7.5,  spot_anchor: 160.0,  instrument: S, thesis: "Comanche Peak nuclear + 3,800 MW Meta/AWS PPAs + KKR Helix $10B JV anchor." },
    Target { ticker: "KLAC", tier: Tier::Floor,      cluster: "Fab",                target_pct: 7.5,  spot_anchor: 255.0,  instrument: S, thesis: "Process control = silent CoWoS winner; highest margins in WFE; HBM-fab capex tailwind." },
    Target { ticker: "NOC",  tier: Tier::Floor,      cluster: "Defense AGI",        target_pct: 7.5,  spot_anchor: 550.0,  instrument: S, thesis: "Replicator Phase-2 + B-21 + space-based interceptor; cheapest fwd P/E of primes; defense-AGI ballast." },
    // ── Tier B — Asymmetric multi-baggers · 32.5% ──────────────────────────
    Target { ticker: "IREN", tier: Tier::Asymmetric, cluster: "AI compute/hosting", target_pct: 5.0,  spot_anchor: 22.0,   instrument: S, thesis: "Aschenbrenner long #2 ($401M). BTC miner → B200 AI-compute pivot. Also held as LEAPS in Tier D." },
    Target { ticker: "APLD", tier: Tier::Asymmetric, cluster: "AI compute/hosting", target_pct: 3.8,  spot_anchor: 11.0,   instrument: S, thesis: "Aschenbrenner long ($320M). AI-data-center hosting; CRWV-type customer leverage at lower base." },
    Target { ticker: "CRWV", tier: Tier::Asymmetric, cluster: "AI compute/hosting", target_pct: 3.8,  spot_anchor: 70.0,   instrument: S, thesis: "Aschenbrenner long #1 ($697M combined). Also held as LEAPS in Tier D." },
    Target { ticker: "PLTR", tier: Tier::Asymmetric, cluster: "Defense AGI",        target_pct: 3.8,  spot_anchor: 133.0,  instrument: S, thesis: "AI + state pure-play; Maven program of record (Sept 2026); -26% YTD pullback is the entry." },
    Target { ticker: "STRL", tier: Tier::Asymmetric, cluster: "DC build-out",       target_pct: 3.8,  spot_anchor: 867.0,  instrument: S, thesis: "E-Infrastructure +174% YoY; >90% of $3.8B backlog is AI DC + semi site work." },
    Target { ticker: "MTZ",  tier: Tier::Asymmetric, cluster: "DC build-out",       target_pct: 2.5,  spot_anchor: 373.0,  instrument: S, thesis: "Fiber backbone + utility power delivery; +106% TTM; long runway." },
    Target { ticker: "BWXT", tier: Tier::Asymmetric, cluster: "Defense AGI",        target_pct: 2.5,  spot_anchor: 203.0,  instrument: S, thesis: "$1.4B+ naval reactor backlog + mPower SMR licensing for hyperscaler compute." },
    Target { ticker: "VRT",  tier: Tier::Asymmetric, cluster: "Grid hardware",      target_pct: 2.5,  spot_anchor: 318.0,  instrument: S, thesis: "~75% revenue from DCs; $15B+ backlog doubled YoY; cooling + power distribution." },
    Target { ticker: "AVAV", tier: Tier::Asymmetric, cluster: "Defense AGI",        target_pct: 2.5,  spot_anchor: 168.0,  instrument: S, thesis: "Pure-play autonomy / loitering munitions; depressed entry on lawsuits; 10x candidate on M&A." },
    Target { ticker: "OKLO", tier: Tier::Asymmetric, cluster: "Nuclear baseload",   target_pct: 2.5,  spot_anchor: 68.0,   instrument: S, thesis: "Aurora-INL criticality target ~July 4; binary but 10x+ on commercial deployment. Also held as LEAPS in Tier D." },
    // ── Tier D — Moonshot sleeve · 12.5% · LEAPS + micro-caps ──────────────
    Target { ticker: "NVDA_LEAPS", tier: Tier::Moonshot, cluster: "", target_pct: 3.75, spot_anchor: 30.0, instrument: Instrument::Leaps { underlying: "NVDA", expiry_year: 2028, strike: 300.0 }, thesis: "Anchor leg (replaces v3's BTC). NVDA $900+ by Jan-2028 → calls 10x; $1,300 mega-bull → 20x." },
    Target { ticker: "CRWV_LEAPS", tier: Tier::Moonshot, cluster: "", target_pct: 2.5,  spot_anchor: 15.0, instrument: Instrument::Leaps { underlying: "CRWV", expiry_year: 2027, strike: 200.0 }, thesis: "Aschenbrenner's #1 long with leverage. CRWV $400 → 13x; $700 → 33x; $1,500 mega-bull → 87x." },
    Target { ticker: "IREN_LEAPS", tier: Tier::Moonshot, cluster: "", target_pct: 2.5,  spot_anchor: 3.0,  instrument: Instrument::Leaps { underlying: "IREN", expiry_year: 2027, strike: 30.0 },  thesis: "Replaces v3's SOL/alt slot. IREN $66 → ~12x; $150 (7x target) → 40x; $300 → 90x." },
    Target { ticker: "OKLO_LEAPS", tier: Tier::Moonshot, cluster: "", target_pct: 1.25, spot_anchor: 8.0,  instrument: Instrument::Leaps { underlying: "OKLO", expiry_year: 2027, strike: 150.0 }, thesis: "Highest-beta single leg. Aurora-INL criticality + rerate to $300-500 → calls 19-44x." },
    Target { ticker: "MICROCAP1",  tier: Tier::Moonshot, cluster: "", target_pct: 1.25, spot_anchor: 0.0,  instrument: Instrument::MicroCap { slot: 1 }, thesis: "Quantum slot (IONQ/RGTI/QBTS). 10-30x on quantum-supremacy breakthrough or DoE/DARPA contract; else ~zero." },
    Target { ticker: "MICROCAP2",  tier: Tier::Moonshot, cluster: "", target_pct: 1.25, spot_anchor: 0.0,  instrument: Instrument::MicroCap { slot: 2 }, thesis: "Defense/space slot (ASTS/RKLB/KTOS/MRCY). 5-30x on a commercial-deployment milestone or DoD win." },
    // ── Tier C — Cash buffer · 5% (incl. IPO reserve) ──────────────────────
    Target { ticker: "CASH", tier: Tier::Cash, cluster: "", target_pct: 5.0, spot_anchor: 0.0, instrument: S, thesis: "Drawdown deployment + $500 IPO reserve. Deploy on single-name >25% OR cohort >15%." },
];

/// All non-cash targets (22 invested positions).
pub fn invested() -> impl Iterator<Item = &'static Target> {
    TARGETS.iter().filter(|t| t.tier != Tier::Cash)
}

/// Equity-sleeve invested names (Tier A + B) — these carry the cluster/position caps.
pub fn equity_invested() -> impl Iterator<Item = &'static Target> {
    TARGETS.iter().filter(|t| matches!(t.tier, Tier::Floor | Tier::Asymmetric))
}

/// Moonshot-sleeve targets (Tier D).
pub fn moonshot() -> impl Iterator<Item = &'static Target> {
    TARGETS.iter().filter(|t| t.tier == Tier::Moonshot)
}

fn invested_raw_sum() -> f64 {
    invested().map(|t| t.target_pct).sum()
}

/// Look up a target by label (case-insensitive).
pub fn find(ticker: &str) -> Option<&'static Target> {
    let up = ticker.to_uppercase();
    TARGETS.iter().find(|t| t.ticker == up)
}

/// Normalized target weight (fraction of NAV) for an invested name: raw % rescaled
/// so all 22 invested positions sum to `1 - cash_buffer_pct`.
pub fn invested_target_weight(ticker: &str, cash_buffer_pct: f64) -> f64 {
    let Some(t) = find(ticker) else { return 0.0 };
    if t.tier == Tier::Cash {
        return 0.0;
    }
    let sum = invested_raw_sum();
    if sum <= 0.0 {
        return 0.0;
    }
    (t.target_pct / sum) * (1.0 - cash_buffer_pct).max(0.0)
}

/// Equity clusters (Tier A/B only), in first-seen order.
pub fn clusters() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = vec![];
    for t in equity_invested() {
        if !t.cluster.is_empty() && !out.contains(&t.cluster) {
            out.push(t.cluster);
        }
    }
    out
}

pub fn cluster_members(cluster: &str) -> Vec<&'static str> {
    equity_invested().filter(|t| t.cluster == cluster).map(|t| t.ticker).collect()
}

pub fn cluster_target_weight(cluster: &str, cash_buffer_pct: f64) -> f64 {
    cluster_members(cluster).iter().map(|t| invested_target_weight(t, cash_buffer_pct)).sum()
}

/// The tradable broker symbol for a target: stock ticker, resolved OCC for a
/// LEAPS, or the config-pinned ticker for a micro-cap. `None` when a micro-cap
/// slot is unset or a LEAPS expiry can't be computed.
pub fn instrument_symbol(t: &Target, cfg: &AppConfig) -> Option<String> {
    match t.instrument {
        Instrument::Stock => Some(t.ticker.to_string()),
        Instrument::Leaps { underlying, expiry_year, strike } => {
            let exp = third_friday(expiry_year, 1)?;
            Some(build_occ(underlying, exp, true, strike))
        }
        Instrument::MicroCap { slot } => {
            let pick = if slot == 1 { &cfg.microcap_1 } else { &cfg.microcap_2 };
            if pick.trim().is_empty() {
                None
            } else {
                Some(pick.trim().to_uppercase())
            }
        }
    }
}

/// Human-friendly display name for digests.
pub fn display_name(t: &Target, cfg: &AppConfig) -> String {
    match t.instrument {
        Instrument::Stock => t.ticker.to_string(),
        Instrument::Leaps { underlying, expiry_year, strike } => {
            format!("{underlying} ${strike:.0}C Jan-{}", expiry_year % 100)
        }
        Instrument::MicroCap { slot } => {
            let pick = if slot == 1 { &cfg.microcap_1 } else { &cfg.microcap_2 };
            if pick.trim().is_empty() {
                format!("{} (unset)", t.ticker)
            } else {
                format!("{} [{}]", pick.trim().to_uppercase(), t.ticker)
            }
        }
    }
}

pub fn is_leaps(t: &Target) -> bool {
    matches!(t.instrument, Instrument::Leaps { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn twenty_two_invested_plus_cash() {
        assert_eq!(invested().count(), 22);
        assert_eq!(equity_invested().count(), 16);
        assert_eq!(moonshot().count(), 6);
        assert_eq!(TARGETS.len(), 23);
    }

    #[test]
    fn invested_weights_sum_to_one_minus_cash() {
        let buf = 0.05;
        let total: f64 = invested().map(|t| invested_target_weight(t.ticker, buf)).sum();
        assert!((total - (1.0 - buf)).abs() < 1e-9, "got {total}");
    }

    #[test]
    fn no_equity_cluster_exceeds_cap_at_target() {
        for c in clusters() {
            let w = cluster_target_weight(c, 0.05);
            assert!(w <= 0.30 + 1e-9, "cluster {c} weight {w} breaches cap");
        }
    }

    #[test]
    fn leaps_resolve_to_expected_occ() {
        let t = find("NVDA_LEAPS").unwrap();
        assert_eq!(instrument_symbol(t, &cfg()).unwrap(), "NVDA280121C00300000");
        let t = find("CRWV_LEAPS").unwrap();
        assert_eq!(instrument_symbol(t, &cfg()).unwrap(), "CRWV270115C00200000");
    }

    #[test]
    fn microcap_resolves_from_config() {
        let mut c = cfg();
        c.microcap_1 = "IONQ".into();
        c.microcap_2 = "ASTS".into();
        assert_eq!(instrument_symbol(find("MICROCAP1").unwrap(), &c).unwrap(), "IONQ");
        assert_eq!(instrument_symbol(find("MICROCAP2").unwrap(), &c).unwrap(), "ASTS");
        c.microcap_1 = "".into();
        assert!(instrument_symbol(find("MICROCAP1").unwrap(), &c).is_none());
    }
}
