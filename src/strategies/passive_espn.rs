use std::collections::HashMap;

use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::espn::types::{GamePhase};
use crate::kalshi::orderbook::LocalOrderBook;

use super::Strategy;
use super::common::evaluate_edge;

/// Passive ESPN-referenced strategy.
///
/// Trades during PreGame and breaks (TV timeouts / halftime):
/// - **PreGame**: Posts where Kalshi diverges from ESPN/DK, expires at game start
/// - **Break**: Posts during TV timeouts / halftime, expires when break ends
/// - Closes existing positions via `is_close` flag when edge disappears in any active phase
pub struct PassiveEspn {
    pub pregame_min_edge: f64,
    pub break_min_edge: f64,
    pub contracts_per_pct_edge: f64,
    pub min_trade_contracts: i64,
    pub max_contracts_per_order: i64,
    pub max_contracts_per_game: i64,
}

impl PassiveEspn {
    pub fn new(
        pregame_min_edge: f64,
        break_min_edge: f64,
        contracts_per_pct_edge: f64,
        min_trade_contracts: i64,
        max_contracts_per_order: i64,
        max_contracts_per_game: i64,
    ) -> Self {
        Self {
            pregame_min_edge,
            break_min_edge,
            contracts_per_pct_edge,
            min_trade_contracts,
            max_contracts_per_order,
            max_contracts_per_game,
        }
    }

    fn min_edge_for_phase(&self, phase: &GamePhase) -> f64 {
        match phase {
            GamePhase::PreGame => self.pregame_min_edge,
            GamePhase::Break | GamePhase::Halftime => self.break_min_edge,
            _ => 1.0, // unreachable due to can_evaluate guard
        }
    }

    fn phase_label(phase: &GamePhase) -> &'static str {
        match phase {
            GamePhase::PreGame => "pregame",
            _ => "break_ev",
        }
    }
}

impl Strategy for PassiveEspn {
    fn name(&self) -> &str {
        "passive_espn"
    }

    fn can_evaluate(&self, game: &GameState) -> bool {
        match game.phase {
            GamePhase::PreGame => true,
            GamePhase::Break | GamePhase::Halftime => {
                game.is_tradeable_break() && !game.is_final_minutes(5.0)
            }
            _ => false,
        }
    }

    fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
        order_books: &HashMap<String, LocalOrderBook>,
    ) -> Option<OrderSignal> {
        let min_edge = self.min_edge_for_phase(&game.phase);
        let label = Self::phase_label(&game.phase);

        let mut signal = evaluate_edge(
            game, risk, current_game_exposure, min_edge, label, order_books,
            self.contracts_per_pct_edge, self.min_trade_contracts, self.max_contracts_per_order,
            self.max_contracts_per_game,
        )?;

        // Phase-specific expiration
        match game.phase {
            GamePhase::PreGame => {
                signal.expiration_ts = game.start_time_ts;
            }
            GamePhase::Break | GamePhase::Halftime => {
                signal.expiration_ts = game.break_expiration_ts();
            }
            _ => {}
        }

        Some(signal)
    }
}
