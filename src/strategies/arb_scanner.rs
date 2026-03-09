use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;

use super::common::evaluate_edge;

/// Cross-Market Arb Scanner.
///
/// Continuously compares Kalshi implied probability against Polymarket + DK reference prices.
/// When Kalshi is significantly mispriced (accounting for fees), posts passive limit orders.
pub struct ArbScanner {
    pub min_edge: f64,
}

impl ArbScanner {
    pub fn new(min_edge: f64) -> Self {
        Self { min_edge }
    }

    pub fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
    ) -> Option<OrderSignal> {
        evaluate_edge(game, risk, current_game_exposure, self.min_edge, "arb_scanner")
    }
}
