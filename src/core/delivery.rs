//! Notification delivery — Telegram, mirroring the sibling sleeves. Optional:
//! when `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID` are unset the send is a no-op
//! (the CLI already prints everything). Never errors out the caller.

use crate::core::config::get_config;

/// HTTP client pinned to IPv4. The deploy host resolves api.telegram.org to an
/// IPv6 address first but has no working IPv6 route, so a default client stalls
/// until timeout on every Telegram call. Binding 0.0.0.0 forces IPv4 sockets.
fn ipv4_client() -> reqwest::Client {
    reqwest::Client::builder()
        .local_address(std::net::IpAddr::from([0, 0, 0, 0]))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Send `text` to Telegram when configured. Returns the number of channels that
/// accepted it (0 when unconfigured or the send failed — never errors).
pub async fn notify_text(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let cfg = get_config();
    if cfg.telegram_bot_token.is_empty() || cfg.telegram_chat_id.is_empty() {
        return 0;
    }
    let url = format!("https://api.telegram.org/bot{}/sendMessage", cfg.telegram_bot_token);
    let truncated: String = text.chars().take(4096).collect();
    let payload = serde_json::json!({
        "chat_id": cfg.telegram_chat_id,
        "text": truncated,
        "disable_web_page_preview": true,
    });
    match ipv4_client()
        .post(&url)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => 1,
        Ok(r) => {
            tracing::warn!("telegram send failed: {}", r.status());
            0
        }
        Err(e) => {
            tracing::warn!("telegram send failed: {}", e);
            0
        }
    }
}
