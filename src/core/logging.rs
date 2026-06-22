//! Tracing setup. `RUST_LOG` wins; otherwise falls back to the configured
//! `LOG_LEVEL`. Safe to call more than once.

use crate::core::config::get_config;
use tracing_subscriber::{fmt, EnvFilter};

pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&get_config().log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}
