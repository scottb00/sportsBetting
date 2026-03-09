use crate::engine::game_state::GameState;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::espn::types::GamePhase;
use crate::kalshi::types::{OrderAction, OrderSide};

/// CLV (Closing Line Value) Hunter.
///
/// Pre-game: compares Kalshi prices to DK / Polymarket reference lines.
/// If Kalshi diverges significantly, posts limit orders early — expects the market
/// to converge toward sharp lines by game time.
pub struct ClvHunter {
    /// Minimum edge to post a pre-game order.
    pub min_edge: f64,
}

impl ClvHunter {
    pub fn new(min_edge: f64) -> Self {
        Self { min_edge }
    }

    /// Evaluate a pre-game market for CLV opportunity.
    pub fn evaluate(
        &self,
        game: &GameState,
        risk: &RiskManager,
        current_game_exposure: f64,
    ) -> Option<OrderSignal> {
        if game.phase != GamePhase::PreGame {
            return None;
        }

        let kalshi_ticker = game.kalshi_ticker.as_ref()?;
        let kalshi_mid = game.kalshi_yes_mid? / 100.0;

        // Use aligned fair value (accounts for home/away ticker)
        let reference = game.kalshi_aligned_fair_value()?;

        let edge = reference - kalshi_mid;
        let edge_abs = edge.abs();

        // Post passive limit: improve the best bid by 1 cent (maker pricing)
        let price_cents = if edge > 0.0 {
            let bid = game.kalshi_yes_bid.unwrap_or(0.0) as i64;
            let ask = game.kalshi_yes_ask.unwrap_or(100.0) as i64;
            (bid + 1).min(ask - 1).max(1)
        } else {
            let yes_ask = game.kalshi_yes_ask.unwrap_or(100.0) as i64;
            let yes_bid = game.kalshi_yes_bid.unwrap_or(0.0) as i64;
            (100 - yes_ask + 1).min(100 - yes_bid - 1).max(1)
        };

        let fee_per_contract = RiskManager::maker_fee(1, price_cents) / 100.0;
        let edge_after_fees = edge_abs - fee_per_contract;

        if edge_after_fees < self.min_edge {
            return None;
        }

        let size = risk.kelly_size(reference, price_cents as f64, current_game_exposure);
        if size <= 0.0 {
            return None;
        }

        let (side, action) = if edge > 0.0 {
            (OrderSide::Yes, OrderAction::Buy)
        } else {
            (OrderSide::No, OrderAction::Buy)
        };

        tracing::info!(
            "CLV signal: {} {:?} {:?} at {} cents, size ${:.2}, edge {:.4}, ref {:.4}, kalshi {:.4}",
            kalshi_ticker, action, side, price_cents, size, edge_after_fees, reference, kalshi_mid
        );

        Some(OrderSignal {
            strategy: "clv_hunter".to_string(),
            kalshi_ticker: kalshi_ticker.clone(),
            side,
            action,
            price_cents,
            size_dollars: size,
            post_only: true,
        })
    }
}
