//! Safety guards. Checked at every cycle entry before any order path runs.
//! Three hard gates: the filesystem kill switch, the PA-paper-account guard, and
//! the no-leverage rule (PDF §6 rule 5: margin would amplify the 25% bear case
//! into a wipe-out — buys are always capped at settled cash).

use crate::core::alpaca::AlpacaClient;
use crate::core::config::AppConfig;

#[derive(Debug, PartialEq, Eq)]
pub enum SafetyVerdict {
    Ok,
    /// Kill file present or paper guard failed — no orders this cycle.
    Halted(String),
}

/// True when the operator kill file exists.
pub fn kill_switch_active(cfg: &AppConfig) -> bool {
    cfg.kill_file.exists()
}

/// Cycle-entry gate. Returns `Halted` if the kill switch is set or the account
/// fails the paper guard; otherwise `Ok`. Never panics.
pub async fn safety_check(cfg: &AppConfig, alpaca: Option<&AlpacaClient>) -> SafetyVerdict {
    if kill_switch_active(cfg) {
        return SafetyVerdict::Halted(format!("kill file present at {}", cfg.kill_file.display()));
    }
    if let Some(a) = alpaca {
        if let Err(e) = a.verify_paper_account().await {
            return SafetyVerdict::Halted(e.to_string());
        }
    }
    SafetyVerdict::Ok
}

/// No-leverage cap (PDF §6 rule 5). The total dollar value of new BUY orders in a
/// cycle may never exceed available settled cash less the protected buffer. This
/// is the deterministic floor — `execute` calls it before submitting buys.
pub fn cash_available_for_buys(cash: f64, nav: f64, cash_buffer_pct: f64) -> f64 {
    let protected = (nav * cash_buffer_pct).max(0.0);
    (cash - protected).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg_with_kill(path: PathBuf) -> AppConfig {
        let mut c = AppConfig::default();
        c.kill_file = path;
        c
    }

    #[test]
    fn kill_switch_detects_file() {
        let p = std::env::temp_dir().join(format!("asch-halt-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let cfg = cfg_with_kill(p.clone());
        assert!(!kill_switch_active(&cfg));
        std::fs::write(&p, "halt").unwrap();
        assert!(kill_switch_active(&cfg));
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn safety_halts_on_kill_file_without_touching_network() {
        let p = std::env::temp_dir().join(format!("asch-halt2-{}", std::process::id()));
        std::fs::write(&p, "halt").unwrap();
        let cfg = cfg_with_kill(p.clone());
        assert!(matches!(safety_check(&cfg, None).await, SafetyVerdict::Halted(_)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn no_leverage_caps_buys_at_cash_minus_buffer() {
        // $5,000 cash, $40,000 NAV, 5% buffer => $2,000 protected => $3,000 deployable.
        assert_eq!(cash_available_for_buys(5_000.0, 40_000.0, 0.05), 3_000.0);
        // Never negative even if cash is below the buffer.
        assert_eq!(cash_available_for_buys(1_000.0, 40_000.0, 0.05), 0.0);
    }
}
