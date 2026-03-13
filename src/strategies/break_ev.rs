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
    pub max_contracts_per_order: i64,
}

impl BreakEvQuoter {
    pub fn new(min_edge: f64, contracts_per_pct_edge: f64, min_trade_contracts: i64, max_contracts_per_order: i64) -> Self {
        Self { min_edge, contracts_per_pct_edge, min_trade_contracts, max_contracts_per_order }
    }
}

impl Strategy for BreakEvQuoter {
    fn name(&self) -> &str {
        "break_ev"
    }

    fn can_evaluate(&self, game: &GameState) -> bool {
        game.is_tradeable_break() && !game.is_final_minutes(5.0)
    }

    fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
        order_books: &HashMap<String, LocalOrderBook>,
    ) -> Option<OrderSignal> {
        let mut signal = evaluate_edge(
            game, risk, current_game_exposure, self.min_edge, self.name(), order_books,
            self.contracts_per_pct_edge, self.min_trade_contracts, self.max_contracts_per_order,
        )?;
        // Set expiration tied to estimated break end.
        // 45s safety buffer: TV timeout orders get ~90s TTL (135-45), team timeout
        // orders get ~1s (30-45 clamped to 1) — effectively immediate, which is fine
        // since a 30s team timeout is nearly over by the time we even detect it.
        // Falls back to config TTL if break_started_at is not set (e.g. bot restart mid-break).
        signal.expiration_ts = game.break_expiration_ts();
        Some(signal)
    }
}
