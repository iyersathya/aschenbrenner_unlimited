//! Rebalance engine: the deterministic risk rules (PDF §6) are the floor; the
//! daily-analysis signal only re-orders rule-authorized choices (signal_bias);
//! the phased initial build (PDF §8) layers the book in over trading days.

pub mod build;
pub mod rules;
pub mod signal_bias;

/// A single proposed portfolio action. Dollar-denominated; `execute` converts to
/// whole-share marketable-limit orders against live quotes.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionKind {
    /// Buy `dollars` worth (build accumulation or drawdown deployment).
    Buy,
    /// Sell `dollars` worth (position-cap or cluster-cap trim).
    Trim,
    /// Advisory only — no order. A held name breached the -35% stop; re-validate
    /// the thesis before adding (PDF §6 rule 1).
    ThesisRevalidate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub ticker: String,
    pub kind: ActionKind,
    /// Absolute dollar size (0 for advisory ThesisRevalidate).
    pub dollars: f64,
    pub reason: String,
}

impl Action {
    pub fn buy(ticker: &str, dollars: f64, reason: impl Into<String>) -> Self {
        Action { ticker: ticker.to_string(), kind: ActionKind::Buy, dollars, reason: reason.into() }
    }
    pub fn trim(ticker: &str, dollars: f64, reason: impl Into<String>) -> Self {
        Action { ticker: ticker.to_string(), kind: ActionKind::Trim, dollars, reason: reason.into() }
    }
    pub fn revalidate(ticker: &str, reason: impl Into<String>) -> Self {
        Action {
            ticker: ticker.to_string(),
            kind: ActionKind::ThesisRevalidate,
            dollars: 0.0,
            reason: reason.into(),
        }
    }

    pub fn side(&self) -> &'static str {
        match self.kind {
            ActionKind::Buy => "buy",
            ActionKind::Trim => "sell",
            ActionKind::ThesisRevalidate => "none",
        }
    }
}
