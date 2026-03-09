use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;

use super::common::evaluate_edge;

/// Break-Based +EV Quoter.
///
/// When a game enters a break (halftime, TV timeout), compares consensus fair value
/// (ESPN win prob, DK odds, Polymarket) against Kalshi book price.
/// If dislocation exceeds fee-adjusted threshold, posts passive limit orders at top of book.
pub struct BreakEvQuoter {
    pub min_edge: f64,
}

impl BreakEvQuoter {
    pub fn new(min_edge: f64) -> Self {
        Self { min_edge }
    }

    pub fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
    ) -> Option<OrderSignal> {
        if !game.phase.is_break() {
            return None;
        }
        evaluate_edge(game, risk, current_game_exposure, self.min_edge, "break_ev")
    }
}
