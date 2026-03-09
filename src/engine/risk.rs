/// Risk management and Kelly sizing.
pub struct RiskManager {
    pub max_position_per_game: f64,  // dollars
    pub max_total_exposure: f64,     // dollars
    pub daily_loss_limit: f64,       // dollars
    pub kelly_fraction: f64,         // 0.5 = half Kelly
    pub min_edge_threshold: f64,     // minimum edge after fees to trade

    pub current_total_exposure: f64,
    pub daily_pnl: f64,
    halted: bool,
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
        }
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Check if we can take a new position.
    pub fn can_trade(&self, additional_exposure: f64) -> bool {
        if self.halted {
            return false;
        }
        if self.daily_pnl <= -self.daily_loss_limit {
            return false;
        }
        if self.current_total_exposure + additional_exposure > self.max_total_exposure {
            return false;
        }
        true
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

    /// Record a fill — update exposure and P&L.
    pub fn record_fill(&mut self, exposure_change: f64, pnl_change: f64) {
        self.current_total_exposure += exposure_change;
        self.daily_pnl += pnl_change;

        if self.daily_pnl <= -self.daily_loss_limit {
            tracing::warn!(
                "Daily loss limit hit: ${:.2}. Halting all trading.",
                self.daily_pnl
            );
            self.halted = true;
        }
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

        rm.record_fill(0.0, -200.0);
        assert!(rm.is_halted());
        assert!(!rm.can_trade(10.0));

        rm.reset_daily();
        assert!(!rm.is_halted());
    }
}
