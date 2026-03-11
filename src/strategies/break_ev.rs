use std::collections::HashMap;

use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::kalshi::orderbook::LocalOrderBook;

use super::Strategy;
use super::common::evaluate_edge;

/// Break-Based +EV Quoter.
///
/// When a game enters a break (halftime, TV timeout), compares ESPN fair value
/// against Kalshi book price. If dislocation exceeds fee-adjusted threshold,
/// posts passive limit orders at top of book.
pub struct BreakEvQuoter {
    pub min_edge: f64,
    pub contracts_per_pct_edge: f64,
    pub min_trade_contracts: i64,
}

impl BreakEvQuoter {
    pub fn new(min_edge: f64, contracts_per_pct_edge: f64, min_trade_contracts: i64) -> Self {
        Self { min_edge, contracts_per_pct_edge, min_trade_contracts }
    }
}

impl Strategy for BreakEvQuoter {
    fn name(&self) -> &str {
        "break_ev"
    }

    fn can_evaluate(&self, game: &GameState) -> bool {
        game.phase.is_break()
    }

    fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
        order_books: &HashMap<String, LocalOrderBook>,
    ) -> Option<OrderSignal> {
        evaluate_edge(
            game, risk, current_game_exposure, self.min_edge, self.name(), order_books,
            self.contracts_per_pct_edge, self.min_trade_contracts,
        )
    }
}
