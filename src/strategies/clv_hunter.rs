use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::espn::types::GamePhase;

use super::common::evaluate_edge;

/// CLV (Closing Line Value) Hunter.
///
/// Pre-game: compares Kalshi prices to DK / Polymarket reference lines.
/// If Kalshi diverges significantly, posts limit orders early — expects the market
/// to converge toward sharp lines by game time.
pub struct ClvHunter {
    pub min_edge: f64,
}

impl ClvHunter {
    pub fn new(min_edge: f64) -> Self {
        Self { min_edge }
    }

    pub fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
    ) -> Option<OrderSignal> {
        if game.phase != GamePhase::PreGame {
            return None;
        }
        evaluate_edge(game, risk, current_game_exposure, self.min_edge, "clv_hunter")
    }
}
