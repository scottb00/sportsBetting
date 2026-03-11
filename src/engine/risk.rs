use std::collections::HashMap;

/// Tracks average entry price and size for a position (keyed by "ticker:side").
#[derive(Debug, Clone, Default)]
struct PositionEntry {
    avg_price: f64, // in dollars (0.0–1.0)
    contracts: i64,
}

/// Risk management and Kelly sizing.
#[derive(Clone)]
pub struct RiskManager {
    pub max_position_per_game: f64,  // dollars
    pub max_total_exposure: f64,     // dollars
    pub daily_loss_limit: f64,       // dollars
    pub kelly_fraction: f64,         // 0.5 = half Kelly
    pub min_edge_threshold: f64,     // minimum edge after fees to trade

    pub current_total_exposure: f64,
    pub daily_pnl: f64,
    halted: bool,
    /// Position tracking for PnL computation: "ticker:side" -> entry info.
    positions: HashMap<String, PositionEntry>,
}

impl RiskManager {
    pub fn new(
        max_position_per_game: f64,
        max_total_exposure: f64,
        daily_loss_limit: f64,
        kelly_fraction: f64,
        min_edge_threshold: f64,
    ) -> Self {
        Self {
            max_position_per_game,
            max_total_exposure,
            daily_loss_limit,
            kelly_fraction,
            min_edge_threshold,
            current_total_exposure: 0.0,
            daily_pnl: 0.0,
            halted: false,
            positions: HashMap::new(),
        }
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Check if we can take a new position.
    pub fn can_trade(&self, additional_exposure: f64) -> bool {
        !self.halted
            && self.daily_pnl > -self.daily_loss_limit
            && self.current_total_exposure + additional_exposure <= self.max_total_exposure
    }

    /// Calculate optimal position size using fractional Kelly criterion.
    ///
    /// Kelly formula: f* = (bp - q) / b
    /// where b = odds (net payout per dollar bet), p = prob of winning, q = 1-p
    ///
    /// For a binary market at price `price_cents` with fair value `fair_prob`:
    /// - If buying YES at price p: b = (1-p)/p, prob of winning = fair_prob
    /// - Kelly = (b * fair_prob - (1 - fair_prob)) / b
    pub fn kelly_size(
        &self,
        fair_prob: f64,
        price_cents: f64,
        current_game_exposure: f64,
    ) -> f64 {
        if fair_prob <= 0.0 || fair_prob >= 1.0 || price_cents <= 0.0 || price_cents >= 100.0 {
            return 0.0;
        }

        let price = price_cents / 100.0;
        let edge = fair_prob - price;

        // Must exceed minimum edge threshold
        if edge.abs() < self.min_edge_threshold {
            return 0.0;
        }

        // Kelly for buying YES at `price`
        let b = (1.0 - price) / price; // net payout ratio
        let kelly_f = (b * fair_prob - (1.0 - fair_prob)) / b;

        if kelly_f <= 0.0 {
            return 0.0;
        }

        // Apply fractional Kelly
        let raw_size = kelly_f * self.kelly_fraction * self.max_total_exposure;

        // Cap by per-game limit
        let game_remaining = (self.max_position_per_game - current_game_exposure).max(0.0);
        let total_remaining = (self.max_total_exposure - self.current_total_exposure).max(0.0);

        raw_size.min(game_remaining).min(total_remaining)
    }

    /// Calculate Kalshi maker fee in cents (1.75% rate).
    pub fn maker_fee(contracts: i64, price_cents: i64) -> f64 {
        Self::kalshi_fee(0.0175, contracts, price_cents)
    }

    /// Calculate Kalshi taker fee in cents (7% rate).
    pub fn taker_fee(contracts: i64, price_cents: i64) -> f64 {
        Self::kalshi_fee(0.07, contracts, price_cents)
    }

    /// Kalshi fee formula: ceil(rate * contracts * price * (1 - price))
    fn kalshi_fee(rate: f64, contracts: i64, price_cents: i64) -> f64 {
        let p = price_cents as f64 / 100.0;
        (rate * contracts as f64 * p * (1.0 - p)).ceil()
    }

    /// Record a fill — update exposure and P&L based on action direction.
    ///
    /// - Buy fills increase exposure and track entry price for later PnL.
    /// - Sell fills decrease exposure and realize PnL vs average entry.
    pub fn record_fill(&mut self, ticker: &str, action: &str, side: &str, price_cents: i64, contracts: i64) {
        let price = price_cents as f64 / 100.0;
        let notional = contracts as f64 * price;
        let key = format!("{}:{}", ticker, side);

        if action == "buy" {
            self.current_total_exposure += notional;
            // Update weighted average entry price
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
            // Sell: decrease exposure by entry cost (not sell price), realize PnL
            if let Some(entry) = self.positions.get_mut(&key) {
                let entry_cost = entry.avg_price * contracts as f64;
                self.current_total_exposure = (self.current_total_exposure - entry_cost).max(0.0);
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
            } else {
                // No position entry — best effort: subtract sell notional
                self.current_total_exposure = (self.current_total_exposure - notional).max(0.0);
            }
        }

        if self.daily_pnl <= -self.daily_loss_limit {
            tracing::warn!(
                "Daily loss limit hit: ${:.2}. Halting all trading.",
                self.daily_pnl
            );
            self.halted = true;
        }
    }

    /// Seed position tracking from Kalshi positions (call on startup).
    pub fn seed_positions(&mut self, ticker: &str, side: &str, avg_price_cents: i64, contracts: i64) {
        if contracts > 0 && avg_price_cents > 0 {
            let key = format!("{}:{}", ticker, side);
            self.positions.insert(key, PositionEntry {
                avg_price: avg_price_cents as f64 / 100.0,
                contracts,
            });
            self.current_total_exposure += contracts as f64 * avg_price_cents as f64 / 100.0;
        }
    }

    /// Reset and re-seed position for a ticker from Kalshi API data.
    /// Used by position reconciliation to correct drift from missed fills.
    pub fn reseed_position(&mut self, ticker: &str, side: &str, avg_price_cents: i64, contracts: i64) {
        // Remove old entries for both sides of this ticker
        for s in ["yes", "no"] {
            let key = format!("{}:{}", ticker, s);
            if let Some(old) = self.positions.remove(&key) {
                self.current_total_exposure -= old.avg_price * old.contracts as f64;
            }
        }
        self.current_total_exposure = self.current_total_exposure.max(0.0);
        // Re-seed with current position
        self.seed_positions(ticker, side, avg_price_cents, contracts);
    }

    /// Net position for a ticker: positive = YES contracts, negative = NO contracts.
    /// Computed from the internal "ticker:yes" and "ticker:no" entries.
    pub fn net_position(&self, ticker: &str) -> i64 {
        let yes = self.positions.get(&format!("{}:yes", ticker))
            .map(|e| e.contracts).unwrap_or(0);
        let no = self.positions.get(&format!("{}:no", ticker))
            .map(|e| e.contracts).unwrap_or(0);
        yes - no
    }

    /// Reset daily P&L (call at start of trading day).
    pub fn reset_daily(&mut self) {
        self.daily_pnl = 0.0;
        self.halted = false;
        tracing::info!("Daily risk reset. Trading enabled.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kelly_sizing() {
        let rm = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.02);

        // Fair value 60%, price 50 cents -> edge = 10%
        let size = rm.kelly_size(0.60, 50.0, 0.0);
        assert!(size > 0.0);
        assert!(size <= 50.0); // capped by per-game limit

        // No edge -> no trade
        let size = rm.kelly_size(0.50, 50.0, 0.0);
        assert_eq!(size, 0.0);
    }

    #[test]
    fn test_maker_fee() {
        // 10 contracts at 50 cents: ceil(0.0175 * 10 * 0.5 * 0.5) = ceil(0.04375) = 1
        assert_eq!(RiskManager::maker_fee(10, 50), 1.0);

        // 100 contracts at 50 cents: ceil(0.0175 * 100 * 0.5 * 0.5) = ceil(0.4375) = 1
        assert_eq!(RiskManager::maker_fee(100, 50), 1.0);
    }

    #[test]
    fn test_daily_loss_halt() {
        let mut rm = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.02);
        assert!(!rm.is_halted());

        // Buy 100 contracts at 50c, then sell at 30c => realized PnL = (0.30 - 0.50) * 100 = -$20
        rm.record_fill("T1", "buy", "yes", 50, 100);
        assert!(!rm.is_halted());
        rm.record_fill("T1", "sell", "yes", 30, 100);
        assert_eq!(rm.daily_pnl, -20.0);

        // Another big loss to hit the $200 limit
        rm.record_fill("T2", "buy", "yes", 90, 500);
        rm.record_fill("T2", "sell", "yes", 50, 500);
        // PnL = -20 + (0.50 - 0.90) * 500 = -20 + -200 = -220
        assert!(rm.is_halted());
        assert!(!rm.can_trade(10.0));

        rm.reset_daily();
        assert!(!rm.is_halted());
    }

    #[test]
    fn test_exposure_decreases_on_sell() {
        let mut rm = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.02);

        rm.record_fill("T1", "buy", "yes", 50, 10);
        assert_eq!(rm.current_total_exposure, 5.0); // 10 * 0.50

        rm.record_fill("T1", "sell", "yes", 60, 10);
        assert!(rm.current_total_exposure < 0.01); // decreased by 10 * 0.60, clamped to ~0
        assert!((rm.daily_pnl - 1.0).abs() < 0.01); // (0.60 - 0.50) * 10 = $1
    }
}
