use std::collections::HashMap;

use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::kalshi::orderbook::LocalOrderBook;

use super::Strategy;
use super::common::evaluate_edge;

/// Position Closer — passively unwinds orphaned positions during live play.
///
/// When a game is live (not break, not pre-game), evaluates all markets for
/// close-only signals. Uses an impossibly high min_edge (1.0 = 100%) so
/// the target is always 0 — only Case B fires (close existing positions).
/// This catches positions opened by CLV hunter pre-game that would otherwise
/// ride to settlement unmanaged.
pub struct PositionCloser {
    pub contracts_per_pct_edge: f64,
    pub min_trade_contracts: i64,
    pub max_contracts_per_order: i64,
}

impl PositionCloser {
    pub fn new(contracts_per_pct_edge: f64, min_trade_contracts: i64, max_contracts_per_order: i64) -> Self {
        Self { contracts_per_pct_edge, min_trade_contracts, max_contracts_per_order }
    }
}

impl Strategy for PositionCloser {
    fn name(&self) -> &str {
        "position_closer"
    }

    fn can_evaluate(&self, game: &GameState) -> bool {
        // Evaluate during live play (not breaks — break_ev handles those)
        game.phase == crate::espn::types::GamePhase::Live
    }

    fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
        order_books: &HashMap<String, LocalOrderBook>,
    ) -> Option<OrderSignal> {
        // min_edge = 1.0 means target is always 0 — only close signals fire
        evaluate_edge(
            game, risk, current_game_exposure, 1.0, self.name(), order_books,
            self.contracts_per_pct_edge,
            self.min_trade_contracts,
            self.max_contracts_per_order,
        )
    }
}
