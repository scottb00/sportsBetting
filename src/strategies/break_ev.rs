use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::kalshi::types::{OrderAction, OrderSide};

/// Break-Based +EV Quoter.
///
/// When a game enters a break (halftime, TV timeout), compares consensus fair value
/// (ESPN win prob, DK odds, Polymarket) against Kalshi book price.
/// If dislocation exceeds fee-adjusted threshold, posts passive limit orders at top of book.
pub struct BreakEvQuoter {
    /// Minimum edge (after fees) required to post an order. In probability terms (0.0 - 1.0).
    pub min_edge: f64,
}

impl BreakEvQuoter {
    pub fn new(min_edge: f64) -> Self {
        Self { min_edge }
    }

    /// Evaluate a game on break and potentially emit an order signal.
    pub fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
    ) -> Option<OrderSignal> {
        if !game.phase.is_break() {
            return None;
        }

        let kalshi_ticker = game.kalshi_ticker.as_ref()?;
        let kalshi_mid = game.kalshi_yes_mid? / 100.0; // convert cents to probability

        // Determine direction using mid-based reference
        let mid_fair = game.kalshi_aligned_fair_value()?;
        let buying_yes = mid_fair > kalshi_mid;

        // Use conservative reference (adverse Polymarket side) for actual edge
        let fair_value = game.kalshi_aligned_fair_value_conservative(buying_yes)?;
        let edge = fair_value - kalshi_mid;
        let edge_abs = edge.abs();

        // Post passive limit: improve the best bid by 1 cent (maker pricing)
        let price_cents = if edge > 0.0 {
            // Kalshi is cheap — buy YES, post at best_bid + 1 (but not above ask - 1)
            let bid = game.kalshi_yes_bid.unwrap_or(0.0) as i64;
            let ask = game.kalshi_yes_ask.unwrap_or(100.0) as i64;
            (bid + 1).min(ask - 1).max(1)
        } else {
            // Kalshi is expensive — buy NO at (100 - yes_ask + 1), i.e. improve no bid
            let yes_ask = game.kalshi_yes_ask.unwrap_or(100.0) as i64;
            let yes_bid = game.kalshi_yes_bid.unwrap_or(0.0) as i64;
            // no_bid = 100 - yes_ask, so improve by 1: 100 - yes_ask + 1
            (100 - yes_ask + 1).min(100 - yes_bid - 1).max(1)
        };

        let fee_per_contract = RiskManager::maker_fee(1, price_cents) / 100.0;
        let edge_after_fees = edge_abs - fee_per_contract;

        if edge_after_fees < self.min_edge {
            tracing::debug!(
                "BreakEV: {} edge {:.4} after fees {:.4} < min {:.4}, skip",
                kalshi_ticker,
                edge_abs,
                edge_after_fees,
                self.min_edge
            );
            return None;
        }

        // Kelly sizing
        let size = risk.kelly_size(fair_value, price_cents as f64, current_game_exposure);
        if size <= 0.0 {
            return None;
        }

        let (side, action) = if edge > 0.0 {
            (OrderSide::Yes, OrderAction::Buy)
        } else {
            (OrderSide::No, OrderAction::Buy)
        };

        tracing::info!(
            "BreakEV signal: {} {:?} {:?} at {} cents, size ${:.2}, edge {:.4}, fair {:.4}, kalshi_mid {:.4}",
            kalshi_ticker, action, side, price_cents, size, edge_after_fees, fair_value, kalshi_mid
        );

        Some(OrderSignal {
            strategy: "break_ev".to_string(),
            kalshi_ticker: kalshi_ticker.clone(),
            side,
            action,
            price_cents,
            size_dollars: size,
            post_only: true, // Always maker
        })
    }
}
