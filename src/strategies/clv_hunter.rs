use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::espn::types::GamePhase;

use super::Strategy;
use super::common::evaluate_edge;

/// CLV (Closing Line Value) Hunter.
///
/// Pre-game: compares Kalshi prices to ESPN/DK reference lines.
/// If Kalshi diverges significantly, posts limit orders early — expects the market
/// to converge toward sharp lines by game time. Orders auto-expire at tipoff.
pub struct ClvHunter {
    pub min_edge: f64,
}

impl ClvHunter {
    pub fn new(min_edge: f64) -> Self {
        Self { min_edge }
    }
}

impl Strategy for ClvHunter {
    fn name(&self) -> &str {
        "clv_hunter"
    }

    fn can_evaluate(&self, game: &GameState) -> bool {
        game.phase == GamePhase::PreGame
    }

    fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
    ) -> Option<OrderSignal> {
        if !self.can_evaluate(game) {
            return None;
        }
        let mut signal = evaluate_edge(game, risk, current_game_exposure, self.min_edge, self.name())?;
        // Set expiration to game start time so Kalshi auto-expires the order at tipoff
        signal.expiration_ts = game.start_time_ts;
        Some(signal)
    }
}
