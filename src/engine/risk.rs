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

    /// Kalshi fee formula in cents: ceil(rate * contracts * price_cents * (100 - price_cents) / 100)
    fn kalshi_fee(rate: f64, contracts: i64, price_cents: i64) -> f64 {
        let p = price_cents as f64;
        (rate * contracts as f64 * p * (100.0 - p) / 100.0).ceil()
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

    /// Effective net position for a specific market, accounting for equivalent exposure across
    /// ALL markets in the game. YES-DUKE and NO-UNC are the same "Duke wins" bet and should
    /// reduce each other's effective position.
    ///
    /// Returns: positive = net long YES on this market, negative = net long NO on this market.
    /// Falls back to ticker-specific net_position if the ticker isn't in the provided markets slice.
    pub fn effective_net_for_market(
        &self,
        markets: &[crate::engine::game_state::KalshiMarketState],
        ticker: &str,
    ) -> i64 {
        let net_home = self.net_game_home_risk(markets);
        match markets.iter().find(|m| m.ticker == ticker) {
            Some(m) if m.is_home => net_home,
            Some(_) => -net_home,
            None => self.net_position(ticker),
        }
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

    fn make_market(ticker: &str, is_home: bool) -> crate::engine::game_state::KalshiMarketState {
        crate::engine::game_state::KalshiMarketState {
            ticker: ticker.to_string(),
            is_home,
            volume: None,
        }
    }

    #[test]
    fn effective_net_single_market() {
        let mut rm = RiskManager::new();
        rm.seed_positions("DUKE", "yes", 50, 5);
        let markets = vec![make_market("DUKE", true)];
        assert_eq!(rm.effective_net_for_market(&markets, "DUKE"), 5);
    }

    #[test]
    fn effective_net_cross_market_suppresses_unc() {
        // 5 YES-DUKE (home). Evaluating UNC (away) should see net = -5.
        let mut rm = RiskManager::new();
        rm.seed_positions("DUKE", "yes", 50, 5);
        let markets = vec![make_market("DUKE", true), make_market("UNC", false)];
        assert_eq!(rm.effective_net_for_market(&markets, "DUKE"), 5);
        assert_eq!(rm.effective_net_for_market(&markets, "UNC"), -5);
    }

    #[test]
    fn effective_net_partial_cross_market_allows_delta() {
        // 3 YES-DUKE. Target for NO-UNC = -7. Effective net for UNC = -3.
        // delta = -7 - (-3) = -4 → should add 4 NO-UNC.
        let mut rm = RiskManager::new();
        rm.seed_positions("DUKE", "yes", 50, 3);
        let markets = vec![make_market("DUKE", true), make_market("UNC", false)];
        assert_eq!(rm.effective_net_for_market(&markets, "UNC"), -3);
    }

    #[test]
    fn effective_net_no_positions() {
        let rm = RiskManager::new();
        let markets = vec![make_market("DUKE", true), make_market("UNC", false)];
        assert_eq!(rm.effective_net_for_market(&markets, "DUKE"), 0);
        assert_eq!(rm.effective_net_for_market(&markets, "UNC"), 0);
    }

    #[test]
    fn test_maker_fee() {
        // 1 contract at 50c: ceil(0.0175 * 1 * 50 * 50 / 100) = ceil(0.4375) = 1
        assert_eq!(RiskManager::maker_fee(1, 50), 1.0);

        // 10 contracts at 50c: ceil(0.0175 * 10 * 50 * 50 / 100) = ceil(4.375) = 5
        assert_eq!(RiskManager::maker_fee(10, 50), 5.0);

        // 100 contracts at 50c: ceil(0.0175 * 100 * 50 * 50 / 100) = ceil(43.75) = 44
        assert_eq!(RiskManager::maker_fee(100, 50), 44.0);

        // 1 contract at 10c: ceil(0.0175 * 1 * 10 * 90 / 100) = ceil(0.1575) = 1
        assert_eq!(RiskManager::maker_fee(1, 10), 1.0);
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
