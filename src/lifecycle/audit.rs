//! Account-wide vault↔broker position reconciliation audit.
//!
//! Ported from the qqq-trader `lifecycle/audit.rs` pattern (born from its
//! 7/02–8/03 untracked short-assignment incident), adapted to this sleeve's
//! books. Unlike qqq-trader there is NO position ledger here: quantities are
//! broker-as-book by design (`PortfolioState::from_alpaca` every cycle) and the
//! vault stores only high-water marks / NAV history / build-state / daily
//! cards. The authoritative intended-holdings model is therefore
//! `portfolio::targets::TARGETS` (22 invested targets), resolved to concrete
//! broker symbols via `targets::instrument_symbol` — stocks by ticker, LEAPS
//! by exact OCC (underlying + Jan-3rd-Friday expiry + strike), micro-caps by
//! the config pins.
//!
//! With no expected quantities, the audit inverts the usual emphasis:
//! - ALARM — `ShortPosition`: any short stock (this is a long-only sleeve).
//! - ALARM — `NonLongCallOption`: any held option that is not a single-leg
//!   LONG CALL (a put, or a short option leg). This sleeve's core invariant is
//!   "options = single-leg long calls only"; anything else means a foreign or
//!   manual order reached the account.
//! - ALARM — `Untracked`: a broker symbol outside the designed universe —
//!   a stock that is no TARGETS ticker / micro-cap pin, or a long call whose
//!   OCC does not byte-match a resolved LEAPS target (wrong underlying,
//!   expiry, or strike). Invisible/foreign risk.
//! - NOTE — `MissingTarget`: a designed target not (yet) held. Expected daily
//!   during the phased build, so it never alarms; it is recorded in the audit
//!   note and log only.
//! - NOTE — `UnresolvedTarget`: a target with no broker symbol (micro-cap
//!   slot unset in config).
//!
//! What is and is not validated: full OCC identity of held options against
//! the TARGETS spec IS validated (exact symbol match); per-symbol share or
//! contract QUANTITIES are NOT (broker-as-book — no vault source of truth
//! exists for them; drift in dollars is the rebalancer's job, not the
//! audit's).
//!
//! Read-only against the broker; the only write is the
//! `meta/reconcile-<date>.json` audit note. Telegram alarm (when `send`) only
//! on alarm-class findings. Network failure / absent keys → `None` (skip) —
//! a blip must never masquerade as a clean or dirty book.

use std::collections::{BTreeMap, BTreeSet};

use crate::core::alpaca::{AlpacaClient, Position};
use crate::core::alpaca_data::parse_occ;
use crate::core::config::AppConfig;
use crate::core::delivery;
use crate::core::vault::VaultClient;
use crate::portfolio::targets;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum FindingKind {
    /// Short stock at the broker — long-only sleeve, always an alarm.
    ShortPosition,
    /// Held option that is not a single-leg LONG call (put or short leg) —
    /// violates the sleeve's core options invariant, always an alarm.
    NonLongCallOption,
    /// Broker symbol outside the designed universe (manual/foreign position,
    /// or a long call whose OCC doesn't match any resolved LEAPS target).
    Untracked,
    /// Designed target not held (normal during phased build) — note only.
    MissingTarget,
    /// Target with no resolvable broker symbol (micro-cap slot unset) — note.
    UnresolvedTarget,
}

impl FindingKind {
    pub fn is_alarm(self) -> bool {
        matches!(
            self,
            FindingKind::ShortPosition | FindingKind::NonLongCallOption | FindingKind::Untracked
        )
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub symbol: String,
    /// Signed broker qty (0 for missing/unresolved targets).
    pub qty: f64,
    /// Broker market value (0 when not held).
    pub market_value: f64,
    pub detail: String,
}

/// The designed universe: every invested TARGETS entry resolved to the broker
/// symbol it should trade as.
pub struct ExpectedBook {
    /// Resolved broker symbol → target label (e.g. "CRWV270115C00200000" → "CRWV_LEAPS").
    pub symbols: BTreeMap<String, String>,
    /// Target labels with no resolvable symbol (micro-cap slot unset).
    pub unresolved: Vec<String>,
}

pub fn expected_book(cfg: &AppConfig) -> ExpectedBook {
    let mut symbols = BTreeMap::new();
    let mut unresolved = vec![];
    for t in targets::invested() {
        match targets::instrument_symbol(t, cfg) {
            Some(sym) => {
                symbols.insert(sym, t.ticker.to_string());
            }
            None => unresolved.push(t.ticker.to_string()),
        }
    }
    ExpectedBook { symbols, unresolved }
}

/// A broker position reduced to what the diff needs. `qty` is signed
/// (short → negative).
#[derive(Debug, Clone)]
pub struct BrokerPos {
    pub symbol: String,
    pub qty: f64,
    pub market_value: f64,
}

impl BrokerPos {
    pub fn from_position(p: &Position) -> Self {
        let qty = if p.side.eq_ignore_ascii_case("short") { -p.qty.abs() } else { p.qty };
        BrokerPos { symbol: p.symbol.to_uppercase(), qty, market_value: p.market_value }
    }
}

/// Pure diff of the broker book against the designed universe. Invariant
/// checks (short / non-long-call) run first and beat tracking: a short call on
/// a tracked LEAPS symbol still alarms.
pub fn diff_positions(expected: &ExpectedBook, actual: &[BrokerPos]) -> Vec<Finding> {
    let mut out = vec![];
    let mut held: BTreeSet<String> = BTreeSet::new();
    for p in actual {
        held.insert(p.symbol.clone());
        if let Some((root, expiry, is_call, strike)) = parse_occ(&p.symbol) {
            if !is_call || p.qty < 0.0 {
                let what = if !is_call { "PUT" } else { "SHORT call leg" };
                out.push(Finding {
                    kind: FindingKind::NonLongCallOption,
                    symbol: p.symbol.clone(),
                    qty: p.qty,
                    market_value: p.market_value,
                    detail: format!(
                        "{what} ({root} {expiry} ${strike:.0}) — sleeve invariant is single-leg LONG calls only"
                    ),
                });
                continue;
            }
            if !expected.symbols.contains_key(&p.symbol) {
                out.push(Finding {
                    kind: FindingKind::Untracked,
                    symbol: p.symbol.clone(),
                    qty: p.qty,
                    market_value: p.market_value,
                    detail: format!(
                        "long call {root} {expiry} ${strike:.0} matches no resolved LEAPS target (wrong underlying/expiry/strike, or foreign order)"
                    ),
                });
            }
            continue;
        }
        if p.qty < 0.0 {
            out.push(Finding {
                kind: FindingKind::ShortPosition,
                symbol: p.symbol.clone(),
                qty: p.qty,
                market_value: p.market_value,
                detail: "short stock in a long-only sleeve (assignment/manual/foreign order)".into(),
            });
            continue;
        }
        if !expected.symbols.contains_key(&p.symbol) {
            out.push(Finding {
                kind: FindingKind::Untracked,
                symbol: p.symbol.clone(),
                qty: p.qty,
                market_value: p.market_value,
                detail: "stock outside the designed universe (no TARGETS ticker / micro-cap pin)".into(),
            });
        }
    }
    for (sym, label) in &expected.symbols {
        if !held.contains(sym) {
            out.push(Finding {
                kind: FindingKind::MissingTarget,
                symbol: sym.clone(),
                qty: 0.0,
                market_value: 0.0,
                detail: format!("target {label} not held (expected during phased build)"),
            });
        }
    }
    for label in &expected.unresolved {
        out.push(Finding {
            kind: FindingKind::UnresolvedTarget,
            symbol: label.clone(),
            qty: 0.0,
            market_value: 0.0,
            detail: "no broker symbol resolvable (micro-cap slot unset in config)".into(),
        });
    }
    out
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditReport {
    pub date: String,
    pub broker_positions: usize,
    /// Resolved designed-universe size (22 minus unresolved slots).
    pub expected_targets: usize,
    /// Held symbols explained by a target, as "SYMBOL (label)".
    pub explained: Vec<String>,
    pub alarms: usize,
    pub notes: usize,
    pub findings: Vec<Finding>,
}

pub fn build_report(
    date: &str,
    expected: &ExpectedBook,
    actual: &[BrokerPos],
    findings: Vec<Finding>,
) -> AuditReport {
    let alarmed: BTreeSet<&String> =
        findings.iter().filter(|f| f.kind.is_alarm()).map(|f| &f.symbol).collect();
    let explained: Vec<String> = actual
        .iter()
        .filter(|p| !alarmed.contains(&p.symbol))
        .filter_map(|p| expected.symbols.get(&p.symbol).map(|label| format!("{} ({label})", p.symbol)))
        .collect();
    let alarms = findings.iter().filter(|f| f.kind.is_alarm()).count();
    let notes = findings.len() - alarms;
    AuditReport {
        date: date.to_string(),
        broker_positions: actual.len(),
        expected_targets: expected.symbols.len(),
        explained,
        alarms,
        notes,
        findings,
    }
}

fn kind_tag(k: FindingKind) -> &'static str {
    match k {
        FindingKind::ShortPosition => "SHORT position",
        FindingKind::NonLongCallOption => "NON-LONG-CALL option",
        FindingKind::Untracked => "UNTRACKED at broker",
        FindingKind::MissingTarget => "missing target",
        FindingKind::UnresolvedTarget => "unresolved target",
    }
}

fn compose_alarm(r: &AuditReport) -> String {
    let mut s = format!(
        "🚨 aschenbrenner-unlimited reconcile {}: {} alarm(s) between broker and the target book\n\
         (broker positions: {}, designed targets: {}, notes: {})\n",
        r.date, r.alarms, r.broker_positions, r.expected_targets, r.notes
    );
    for f in r.findings.iter().filter(|f| f.kind.is_alarm()) {
        s.push_str(&format!(
            "• {} — {}: qty {:+.0}, mv ${:+.0} — {}\n",
            kind_tag(f.kind), f.symbol, f.qty, f.market_value, f.detail
        ));
    }
    s.push_str(
        "Short / non-long-call findings violate the sleeve's core invariant; \
         Untracked = foreign risk the book can't see. Check meta/reconcile-<date>.json.",
    );
    s
}

/// Run the audit: fetch ALL broker positions, diff against the resolved TARGETS
/// universe, write the audit note, Telegram on alarm (when `send`). Returns
/// `None` when Alpaca keys are absent or the broker is unreachable.
pub async fn run_audit(cfg: &AppConfig, vault: &VaultClient, send: bool) -> Option<AuditReport> {
    let alpaca = AlpacaClient::from_config(cfg)?;
    let positions = match alpaca.get_positions().await {
        Ok(ps) => ps,
        Err(e) => {
            tracing::warn!("reconcile audit: could not fetch broker positions ({e}) — skipping");
            return None;
        }
    };
    let actual: Vec<BrokerPos> = positions.iter().map(BrokerPos::from_position).collect();
    let expected = expected_book(cfg);
    let findings = diff_positions(&expected, &actual);
    let date = crate::lifecycle::daily::today_in_tz(cfg).format("%Y-%m-%d").to_string();
    let report = build_report(&date, &expected, &actual, findings);

    if let Ok(json) = serde_json::to_string_pretty(&report) {
        let _ = vault.write("meta", &format!("reconcile-{}.json", report.date), &json);
    }
    if report.alarms == 0 {
        tracing::info!(
            "reconcile audit {}: clean — {} broker position(s) all explained by TARGETS ({} note(s): targets still to build/resolve)",
            report.date, report.broker_positions, report.notes
        );
    } else {
        let msg = compose_alarm(&report);
        tracing::warn!("reconcile audit: {msg}");
        if send {
            let _ = delivery::notify_text(&msg).await;
        }
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AppConfig {
        AppConfig::default() // microcap pins default to IONQ / ASTS
    }

    fn pos(symbol: &str, qty: f64, mv: f64) -> BrokerPos {
        BrokerPos { symbol: symbol.into(), qty, market_value: mv }
    }

    fn alarms(fs: &[Finding]) -> Vec<&Finding> {
        fs.iter().filter(|f| f.kind.is_alarm()).collect()
    }

    /// The designed universe resolves to 22 symbols with default config:
    /// 16 stocks + 4 LEAPS OCC + 2 micro-cap pins.
    #[test]
    fn expected_book_resolves_all_22_targets() {
        let e = expected_book(&cfg());
        assert_eq!(e.symbols.len(), 22);
        assert!(e.unresolved.is_empty());
        assert_eq!(e.symbols.get("CRWV270115C00200000").map(String::as_str), Some("CRWV_LEAPS"));
        assert_eq!(e.symbols.get("OKLO270115C00150000").map(String::as_str), Some("OKLO_LEAPS"));
        assert_eq!(e.symbols.get("IONQ").map(String::as_str), Some("MICROCAP1"));
    }

    /// Short stock (the qqq 7/02 assignment class) must alarm.
    #[test]
    fn short_stock_position_alarms() {
        let e = expected_book(&cfg());
        let fs = diff_positions(&e, &[pos("GEV", -50.0, -52_000.0)]);
        let a = alarms(&fs);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, FindingKind::ShortPosition);
        assert_eq!(a[0].symbol, "GEV");
        assert_eq!(a[0].qty, -50.0);
    }

    /// A held PUT violates the single-leg-long-call invariant even on a
    /// tracked underlying/strike/expiry.
    #[test]
    fn long_put_alarms_as_non_long_call() {
        let e = expected_book(&cfg());
        let fs = diff_positions(&e, &[pos("CRWV270115P00200000", 1.0, 1_500.0)]);
        let a = alarms(&fs);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, FindingKind::NonLongCallOption);
        assert!(a[0].detail.contains("PUT"));
    }

    /// A SHORT call alarms even when the OCC matches a tracked LEAPS target —
    /// the invariant check beats tracking.
    #[test]
    fn short_call_on_tracked_symbol_alarms_as_non_long_call() {
        let e = expected_book(&cfg());
        assert!(e.symbols.contains_key("CRWV270115C00200000"));
        let fs = diff_positions(&e, &[pos("CRWV270115C00200000", -1.0, -1_500.0)]);
        let a = alarms(&fs);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, FindingKind::NonLongCallOption);
        assert!(a[0].detail.contains("SHORT call"));
    }

    /// A stock outside the designed universe is Untracked (foreign/manual).
    #[test]
    fn untracked_stock_alarms() {
        let e = expected_book(&cfg());
        let fs = diff_positions(&e, &[pos("TSLA", 10.0, 4_000.0)]);
        let a = alarms(&fs);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, FindingKind::Untracked);
        assert_eq!(a[0].symbol, "TSLA");
    }

    /// A long call whose strike doesn't match the resolved LEAPS target is
    /// Untracked — this is the OCC-identity validation.
    #[test]
    fn long_call_with_wrong_strike_is_untracked() {
        let e = expected_book(&cfg());
        let fs = diff_positions(&e, &[pos("NVDA280121C00250000", 1.0, 3_000.0)]);
        let a = alarms(&fs);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, FindingKind::Untracked);
        assert!(a[0].detail.contains("no resolved LEAPS target"));
    }

    /// Held targets are silent; unheld targets are MissingTarget NOTES only —
    /// never alarms (phased build holds a partial book for months).
    #[test]
    fn missing_targets_are_notes_not_alarms() {
        let e = expected_book(&cfg());
        let held = [pos("GEV", 4.0, 4_200.0), pos("CRWV270115C00200000", 1.0, 1_600.0)];
        let fs = diff_positions(&e, &held);
        assert!(alarms(&fs).is_empty());
        assert_eq!(fs.len(), 20); // 22 targets − 2 held
        assert!(fs.iter().all(|f| f.kind == FindingKind::MissingTarget));
        assert!(fs.iter().any(|f| f.symbol == "NVDA280121C00300000"));
    }

    /// An unset micro-cap slot is an UnresolvedTarget note.
    #[test]
    fn unset_microcap_slot_is_note() {
        let mut c = cfg();
        c.microcap_1 = "".into();
        let e = expected_book(&c);
        assert_eq!(e.symbols.len(), 21);
        assert_eq!(e.unresolved, vec!["MICROCAP1".to_string()]);
        let fs = diff_positions(&e, &[]);
        let unres: Vec<_> =
            fs.iter().filter(|f| f.kind == FindingKind::UnresolvedTarget).collect();
        assert_eq!(unres.len(), 1);
        assert_eq!(unres[0].symbol, "MICROCAP1");
        assert!(!unres[0].kind.is_alarm());
    }

    /// A fully built book — every resolved target held long — is clean.
    #[test]
    fn fully_built_book_is_clean() {
        let e = expected_book(&cfg());
        let held: Vec<BrokerPos> =
            e.symbols.keys().map(|s| pos(s, 1.0, 1_000.0)).collect();
        let fs = diff_positions(&e, &held);
        assert!(fs.is_empty());
        let r = build_report("2026-08-07", &e, &held, fs);
        assert_eq!(r.alarms, 0);
        assert_eq!(r.notes, 0);
        assert_eq!(r.explained.len(), 22);
    }

    /// build_report excludes alarmed symbols from `explained` and counts
    /// alarm/note classes correctly.
    #[test]
    fn report_separates_alarms_from_notes() {
        let e = expected_book(&cfg());
        let held = [pos("GEV", 4.0, 4_200.0), pos("TSLA", 10.0, 4_000.0)];
        let fs = diff_positions(&e, &held);
        let r = build_report("2026-08-07", &e, &held, fs);
        assert_eq!(r.alarms, 1); // TSLA untracked
        assert_eq!(r.notes, 21); // 21 unheld targets
        assert_eq!(r.explained, vec!["GEV (GEV)".to_string()]);
        let msg = compose_alarm(&r);
        assert!(msg.contains("TSLA"));
        assert!(!msg.contains("MissingTarget"));
    }
}
