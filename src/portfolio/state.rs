//! Live portfolio snapshot — holdings + NAV + cash, assembled from the Alpaca
//! account and positions. Cheap value type the rebalance rules operate on.

use crate::core::alpaca::{AlpacaAccount, Position};
use std::collections::HashMap;

#[allow(dead_code)] // qty/cost_basis/avg_entry_price retained for cards + future sizing
#[derive(Debug, Clone)]
pub struct Holding {
    pub ticker: String,
    pub qty: f64,
    pub price: f64,
    pub market_value: f64,
    pub cost_basis: f64,
    pub avg_entry_price: f64,
    /// Unrealized P&L as a fraction of cost basis (e.g. -0.35 = down 35%).
    pub unrealized_plpc: f64,
}

#[derive(Debug, Clone)]
pub struct PortfolioState {
    pub holdings: Vec<Holding>,
    pub cash: f64,
    /// Total account value (positions + cash) — the NAV the targets weigh against.
    pub nav: f64,
}

impl PortfolioState {
    pub fn from_alpaca(account: &AlpacaAccount, positions: &[Position]) -> Self {
        let holdings = positions
            .iter()
            .filter(|p| p.side != "short") // long-only book
            .map(|p| Holding {
                ticker: p.symbol.to_uppercase(),
                qty: p.qty,
                price: if p.current_price > 0.0 {
                    p.current_price
                } else if p.qty != 0.0 {
                    p.market_value / p.qty
                } else {
                    0.0
                },
                market_value: p.market_value,
                cost_basis: p.cost_basis,
                avg_entry_price: p.avg_entry_price,
                unrealized_plpc: p.unrealized_plpc,
            })
            .collect();
        let nav = if account.portfolio_value > 0.0 {
            account.portfolio_value
        } else {
            account.equity
        };
        PortfolioState { holdings, cash: account.cash, nav }
    }

    pub fn holding(&self, ticker: &str) -> Option<&Holding> {
        let up = ticker.to_uppercase();
        self.holdings.iter().find(|h| h.ticker == up)
    }

    pub fn market_value(&self, ticker: &str) -> f64 {
        self.holding(ticker).map(|h| h.market_value).unwrap_or(0.0)
    }

    /// Live price map of every held name (for limit pricing / metrics).
    #[allow(dead_code)]
    pub fn price_map(&self) -> HashMap<String, f64> {
        self.holdings.iter().map(|h| (h.ticker.clone(), h.price)).collect()
    }
}
