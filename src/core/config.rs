//! Config loader. Mirrors the sibling sleeves: hardcoded defaults → `.env` file
//! → process-environment overlay, cached in a `OnceCell` singleton. Specialized
//! for the long-term Aschenbrenner equity portfolio (targets + rebalance rules
//! from the PDF, not an intraday/options config).

use once_cell::sync::OnceCell;
use regex::Regex;
use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    // Alpaca (dedicated aschenbrenner paper account)
    pub alpaca_api_key: String,
    pub alpaca_secret_key: String,
    pub alpaca_base_url: String,
    pub paper_mode: bool,

    // Thesis target (PDF §1: $40K → $80K in 3 years = 26% CAGR)
    pub target_nav_start: f64,
    pub target_cagr: f64,
    pub target_horizon_years: f64,

    // Rebalance rules (PDF §6)
    pub cash_buffer_pct: f64,
    pub position_trim_trigger_pct: f64,
    pub position_trim_target_pct: f64,
    pub cluster_cap_pct: f64,
    /// Absolute drawdown from cost basis that flags a name for thesis
    /// re-validation. Stored positive (0.35 = -35%).
    pub position_stop_pct: f64,
    /// Deploy the cash buffer when a COHORT (cluster) is down more than this
    /// fraction from its high-water marks (PDF §6 rule 5: cohort >15%).
    pub drawdown_deploy_pct: f64,
    /// Deploy into a single NAME when it is down more than this fraction from its
    /// high-water mark (PDF §6 rule 5: single-name >25%).
    pub drawdown_deploy_single_pct: f64,
    /// Ignore target drift smaller than this fraction of NAV (low-churn hold).
    pub rebalance_band_pct: f64,
    pub max_orders_per_day: i64,

    // Phased initial build (PDF §8)
    pub build_daily_budget: f64,

    // Daily-analysis signal bias
    pub signal_bias_enabled: bool,

    // Tier-D micro-cap moonshot picks (config-pinned tickers).
    pub microcap_1: String,
    pub microcap_2: String,

    // Scheduling
    pub timezone_str: String,
    pub catchup_on_start: bool,

    // Paths
    pub project_root: PathBuf,
    pub vault_path: PathBuf,
    pub portfolio_md_path: PathBuf,
    /// Shared nightly multi-agent reports. `None` => resolve lazily as
    /// `<vault_path>/../daily_analysis`. Override with `DAILY_ANALYSIS_PATH`.
    pub daily_analysis_path: Option<PathBuf>,
    /// Filesystem kill switch. Presence halts the cycle.
    pub kill_file: PathBuf,
    /// Real-money opt-in (out of scope). Even when true the PA-paper guard still
    /// applies — flipping it requires real keys + a passed go-live review.
    pub live_trading_enabled: bool,

    // Logging
    pub log_level: String,
    // Delivery — Telegram (optional; empty = disabled).
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        let root = home().join("projects").join("aschenbrenner_unlimited");
        AppConfig {
            alpaca_api_key: String::new(),
            alpaca_secret_key: String::new(),
            alpaca_base_url: "https://paper-api.alpaca.markets".into(),
            paper_mode: true,
            target_nav_start: 40_000.0,
            target_cagr: 0.26,
            target_horizon_years: 3.0,
            cash_buffer_pct: 0.05,
            position_trim_trigger_pct: 0.15,
            position_trim_target_pct: 0.10,
            cluster_cap_pct: 0.30,
            position_stop_pct: 0.35,
            drawdown_deploy_pct: 0.15,
            drawdown_deploy_single_pct: 0.25,
            rebalance_band_pct: 0.03,
            max_orders_per_day: 10,
            build_daily_budget: 2000.0,
            signal_bias_enabled: true,
            microcap_1: "IONQ".into(),
            microcap_2: "ASTS".into(),
            timezone_str: "America/Los_Angeles".into(),
            catchup_on_start: true,
            project_root: root.clone(),
            vault_path: home().join("gdrive").join("vault").join("aschenbrenner-unlimited"),
            portfolio_md_path: root.join("portfolio.md"),
            daily_analysis_path: None,
            kill_file: root.join("aschenbrenner-unlimited.HALT"),
            live_trading_enabled: false,
            log_level: "info".into(),
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
        }
    }
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes")
}

fn apply(key: &str, value: &str, c: &mut AppConfig) {
    let pf = |c: &mut f64, v: &str| {
        if let Ok(x) = v.parse() {
            *c = x
        }
    };
    let pi = |c: &mut i64, v: &str| {
        if let Ok(x) = v.parse() {
            *c = x
        }
    };
    match key {
        "ALPACA_API_KEY_ID" => c.alpaca_api_key = value.into(),
        "ALPACA_API_SECRET_KEY" => c.alpaca_secret_key = value.into(),
        "ALPACA_BASE_URL" => c.alpaca_base_url = value.trim_end_matches('/').into(),
        "PAPER_MODE" => c.paper_mode = truthy(value),
        "TARGET_NAV_START" => pf(&mut c.target_nav_start, value),
        "TARGET_CAGR" => pf(&mut c.target_cagr, value),
        "TARGET_HORIZON_YEARS" => pf(&mut c.target_horizon_years, value),
        "CASH_BUFFER_PCT" => pf(&mut c.cash_buffer_pct, value),
        "POSITION_TRIM_TRIGGER_PCT" => pf(&mut c.position_trim_trigger_pct, value),
        "POSITION_TRIM_TARGET_PCT" => pf(&mut c.position_trim_target_pct, value),
        "CLUSTER_CAP_PCT" => pf(&mut c.cluster_cap_pct, value),
        "POSITION_STOP_PCT" => pf(&mut c.position_stop_pct, value),
        "DRAWDOWN_DEPLOY_PCT" => pf(&mut c.drawdown_deploy_pct, value),
        "DRAWDOWN_DEPLOY_SINGLE_PCT" => pf(&mut c.drawdown_deploy_single_pct, value),
        "REBALANCE_BAND_PCT" => pf(&mut c.rebalance_band_pct, value),
        "MAX_ORDERS_PER_DAY" => pi(&mut c.max_orders_per_day, value),
        "BUILD_DAILY_BUDGET" => pf(&mut c.build_daily_budget, value),
        "SIGNAL_BIAS_ENABLED" => c.signal_bias_enabled = truthy(value),
        "MICROCAP_1" => c.microcap_1 = value.trim().to_uppercase(),
        "MICROCAP_2" => c.microcap_2 = value.trim().to_uppercase(),
        "TIMEZONE" => c.timezone_str = value.into(),
        "CATCHUP_ON_START" => c.catchup_on_start = truthy(value),
        "VAULT_PATH" => c.vault_path = PathBuf::from(value),
        "DAILY_ANALYSIS_PATH" => c.daily_analysis_path = Some(PathBuf::from(value)),
        "KILL_FILE" => c.kill_file = PathBuf::from(value),
        "LIVE_TRADING_ENABLED" => c.live_trading_enabled = truthy(value),
        "LOG_LEVEL" => c.log_level = value.trim().into(),
        "TELEGRAM_BOT_TOKEN" => c.telegram_bot_token = value.trim().into(),
        "TELEGRAM_CHAT_ID" => c.telegram_chat_id = value.trim().into(),
        _ => {}
    }
}

fn from_env_file(c: &mut AppConfig) {
    let candidates = [
        std::env::current_dir().ok().map(|d| d.join(".env")),
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env")),
        Some(c.project_root.join(".env")),
    ];
    let env_file = candidates.into_iter().flatten().find(|p| p.exists());
    let Some(path) = env_file else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let inline = Regex::new(r"\s+#").unwrap();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let key = k.trim();
        let raw = v.trim();
        let value = inline.splitn(raw, 2).next().unwrap_or(raw).trim();
        apply(key, value, c);
    }
}

fn with_env_overrides(c: &mut AppConfig) {
    for (k, v) in std::env::vars() {
        apply(&k.to_uppercase(), &v, c);
    }
}

impl AppConfig {
    /// Resolved daily-analysis directory (`<vault>/../daily_analysis` if unset).
    pub fn daily_analysis_dir(&self) -> PathBuf {
        self.daily_analysis_path.clone().unwrap_or_else(|| {
            self.vault_path
                .parent()
                .map(|p| p.join("daily_analysis"))
                .unwrap_or_else(|| home().join("gdrive").join("vault").join("daily_analysis"))
        })
    }
}

static CONFIG: OnceCell<AppConfig> = OnceCell::new();

pub fn get_config() -> &'static AppConfig {
    CONFIG.get_or_init(|| {
        let mut c = AppConfig::default();
        from_env_file(&mut c);
        with_env_overrides(&mut c);
        c
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lines: &[(&str, &str)]) -> AppConfig {
        let mut c = AppConfig::default();
        for (k, v) in lines {
            apply(k, v, &mut c);
        }
        c
    }

    #[test]
    fn defaults_are_paper_and_thesis() {
        let c = AppConfig::default();
        assert!(c.paper_mode);
        assert_eq!(c.alpaca_base_url, "https://paper-api.alpaca.markets");
        assert_eq!(c.target_nav_start, 40_000.0);
        assert_eq!(c.target_cagr, 0.26);
        assert_eq!(c.cluster_cap_pct, 0.30);
    }

    #[test]
    fn base_url_trailing_slash_stripped() {
        let c = parse(&[("ALPACA_BASE_URL", "https://paper-api.alpaca.markets/")]);
        assert_eq!(c.alpaca_base_url, "https://paper-api.alpaca.markets");
    }

    #[test]
    fn daily_analysis_dir_defaults_to_sibling() {
        let mut c = AppConfig::default();
        c.vault_path = PathBuf::from("/x/vault/aschenbrenner-unlimited");
        assert_eq!(c.daily_analysis_dir(), PathBuf::from("/x/vault/daily_analysis"));
    }

    #[test]
    fn rule_overrides_parse() {
        let c = parse(&[("CLUSTER_CAP_PCT", "0.25"), ("REBALANCE_BAND_PCT", "0.05")]);
        assert_eq!(c.cluster_cap_pct, 0.25);
        assert_eq!(c.rebalance_band_pct, 0.05);
    }
}
