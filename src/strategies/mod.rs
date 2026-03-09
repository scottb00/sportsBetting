pub mod arb_scanner;
pub mod break_ev;
pub mod clv_hunter;
pub mod common;

use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;

/// Trait for all trading strategies.
///
/// Each strategy decides (1) whether it can evaluate a given game,
/// and (2) what order signal to emit.  The engine loops through
/// registered strategies, calls `evaluate` on eligible games,
/// and picks the best signal per game.
pub trait Strategy: Send + Sync {
    /// Human-readable name used for logging and order tagging.
    fn name(&self) -> &str;

    /// Fast pre-filter: can this strategy produce a signal for `game`
    /// given its current phase?
    fn can_evaluate(&self, game: &GameState) -> bool;

    /// Evaluate the game and optionally return an order signal.
    /// Implementations should call `can_evaluate` internally as a guard,
    /// so `evaluate` is safe to call directly without checking first.
    fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
    ) -> Option<OrderSignal>;
}
