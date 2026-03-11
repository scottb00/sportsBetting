use std::collections::HashMap;

/// Tracks average entry price and size for a position (keyed by "ticker:side").
#[derive(Debug, Clone)]
struct PositionEntry {
    avg_price: f64, // in dollars (0.0–1.0)
    contracts: i64,
}

/// Position tracking and P&L accounting.
/// Sizing limits are enforced by the executor via max_contracts_per_game (StrategyConfig).
#[derive(Clone)]
pub struct RiskManager {
    pub daily_pnl: f64,
    /// Position tracking for PnL computation: "ticker:side" -> entry info.
    positions: HashMap<String, PositionEntry>,
}

impl RiskManager {
    pub fn new() -> Self {
        Self {
            daily_pnl: 0.0,
            positions: HashMap::new(),
        }
    }

    /// Calculate Kalshi maker fee in cents (1.75% rate).
    pub fn maker_fee(contracts: i64, price_cents: i64) -> f64 {
        Self::kalshi_fee(0.0175, contracts, price_cents)
    }

    /// Kalshi fee formula: ceil(rate * contracts * price * (1 - price))
    fn kalshi_fee(rate: f64, contracts: i64, price_cents: i64) -> f64 {
        let p = price_cents as f64 / 100.0;
        (rate * contracts as f64 * p * (1.0 - p)).ceil()
    }

    /// Record a fill — track entry price for P&L computation.
    pub fn record_fill(&mut self, ticker: &str, action: &str, side: &str, price_cents: i64, contracts: i64) {
        let price = price_cents as f64 / 100.0;
        let key = format!("{}:{}", ticker, side);

        if action == "buy" {
            let entry = self.positions.entry(key).or_insert(PositionEntry {
                avg_price: 0.0,
                contracts: 0,
            });
            let total_cost = entry.avg_price * entry.contracts as f64 + price * contracts as f64;
            entry.contracts += contracts;
            if entry.contracts > 0 {
                entry.avg_price = total_cost / entry.contracts as f64;
            }
        } else {
            // Sell: realize P&L vs average entry
            if let Some(entry) = self.positions.get_mut(&key) {
                let avg_entry_cents = entry.avg_price * 100.0;
                let realized_pnl = (price - entry.avg_price) * contracts as f64;
                self.daily_pnl += realized_pnl;
                entry.contracts -= contracts;
                if entry.contracts <= 0 {
                    self.positions.remove(&key);
                }
                tracing::info!(
                    "Realized PnL on {} {}: ${:.4} ({} contracts @ {}c vs avg entry {:.2}c)",
                    ticker, side, realized_pnl, contracts, price_cents, avg_entry_cents,
                );
            }
        }
    }

    /// Seed position tracking from Kalshi positions (call on startup).
    /// Accumulates — does NOT clear existing entries for the ticker first.
    pub fn seed_positions(&mut self, ticker: &str, side: &str, avg_price_cents: i64, contracts: i64) {
        if contracts > 0 && avg_price_cents > 0 {
            let key = format!("{}:{}", ticker, side);
            self.positions.insert(key, PositionEntry {
                avg_price: avg_price_cents as f64 / 100.0,
                contracts,
            });
        }
    }

    /// Re-seed position for a ticker from the source of truth (position reconciliation).
    /// Clears ALL existing position entries for this ticker first, then sets the new value.
    pub fn reseed_position(&mut self, ticker: &str, side: &str, avg_price_cents: i64, contracts: i64) {
        for s in &["yes", "no"] {
            self.positions.remove(&format!("{}:{}", ticker, s));
        }
        if contracts > 0 && avg_price_cents > 0 {
            let key = format!("{}:{}", ticker, side);
            self.positions.insert(key, PositionEntry {
                avg_price: avg_price_cents as f64 / 100.0,
                contracts,
            });
        }
    }

    /// Net position for a ticker: positive = YES contracts, negative = NO contracts.
    pub fn net_position(&self, ticker: &str) -> i64 {
        let yes = self.positions.get(&format!("{}:yes", ticker))
            .map(|e| e.contracts).unwrap_or(0);
        let no = self.positions.get(&format!("{}:no", ticker))
            .map(|e| e.contracts).unwrap_or(0);
        yes - no
    }

    /// Compute the signed net game-level risk across all markets in the game, home-team aligned.
    /// Positive = net long the home team winning, negative = net long the away team winning.
    pub fn net_game_home_risk(&self, markets: &[crate::engine::game_state::KalshiMarketState]) -> i64 {
        markets.iter().map(|m| {
            let net = self.net_position(&m.ticker);
            if m.is_home { net } else { -net }
        }).sum()
    }

    /// Determine whether placing an order on `ticker` with `side` reduces game-level exposure.
    pub fn is_reduce_order(
        &self,
        markets: &[crate::engine::game_state::KalshiMarketState],
        ticker: &str,
        side: &crate::kalshi::types::OrderSide,
    ) -> bool {
        let net = self.net_game_home_risk(markets);
        if net == 0 {
            return false;
        }
        let market = match markets.iter().find(|m| m.ticker == ticker) {
            Some(m) => m,
            None => return false,
        };
        let order_is_long_home = match side {
            crate::kalshi::types::OrderSide::Yes => market.is_home,
            crate::kalshi::types::OrderSide::No => !market.is_home,
        };
        (net > 0 && !order_is_long_home) || (net < 0 && order_is_long_home)
    }

    /// Reset daily P&L (call at start of trading day).
    pub fn reset_daily(&mut self) {
        self.daily_pnl = 0.0;
        tracing::info!("Daily P&L reset.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_maker_fee() {
        // 10 contracts at 50 cents: ceil(0.0175 * 10 * 0.5 * 0.5) = ceil(0.04375) = 1
        assert_eq!(RiskManager::maker_fee(10, 50), 1.0);

        // 100 contracts at 50 cents: ceil(0.0175 * 100 * 0.5 * 0.5) = ceil(0.4375) = 1
        assert_eq!(RiskManager::maker_fee(100, 50), 1.0);
    }

    #[test]
    fn test_record_fill_pnl() {
        let mut rm = RiskManager::new();

        // Buy 100 contracts at 50c, sell at 60c → PnL = (0.60 - 0.50) * 100 = $10
        rm.record_fill("T1", "buy", "yes", 50, 100);
        rm.record_fill("T1", "sell", "yes", 60, 100);
        assert!((rm.daily_pnl - 10.0).abs() < 0.01);
    }
}
