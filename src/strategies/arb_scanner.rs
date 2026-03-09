use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::espn::types::GamePhase;

use super::Strategy;
use super::common::evaluate_edge;

/// Cross-Market Arb Scanner.
///
/// Continuously compares Kalshi implied probability against ESPN reference prices
/// during live play. When Kalshi is significantly mispriced (accounting for fees),
/// posts passive limit orders.
pub struct ArbScanner {
    pub min_edge: f64,
}

impl ArbScanner {
    pub fn new(min_edge: f64) -> Self {
        Self { min_edge }
    }
}

impl Strategy for ArbScanner {
    fn name(&self) -> &str {
        "arb_scanner"
    }

    fn can_evaluate(&self, game: &GameState) -> bool {
        matches!(game.phase, GamePhase::Live | GamePhase::Halftime | GamePhase::Break)
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
        evaluate_edge(game, risk, current_game_exposure, self.min_edge, self.name())
    }
}
