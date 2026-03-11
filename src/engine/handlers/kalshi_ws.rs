use crate::engine::bot::{SharedState, SharedOrderBooks, SharedLogger};
use crate::engine::logger::GameInfo;
use crate::engine::risk::RiskManager;
use crate::kalshi::websocket::KalshiWsEvent;

/// Handle a Kalshi WebSocket event.
///
/// Orderbook updates (snapshots/deltas) only lock order_books — not state.
/// Fill processing locks state for risk/order updates, then logger separately.
pub async fn handle_kalshi_event(
    event: KalshiWsEvent,
    state: &SharedState,
    order_books: &SharedOrderBooks,
    logger: &SharedLogger,
) {
    match event {
        KalshiWsEvent::OrderBookSnapshot { market_ticker, snapshot } => {
            let mut books = order_books.write().await;
            let book = books
                .entry(market_ticker.clone())
                .or_insert_with(|| crate::kalshi::orderbook::LocalOrderBook::new(market_ticker.clone()));
            book.apply_snapshot(&snapshot);
            tracing::debug!("Book snapshot for {}", market_ticker);
        }
        KalshiWsEvent::OrderBookDelta(delta) => {
            let ticker = delta.market_ticker.clone();
            let mut books = order_books.write().await;
            if let Some(book) = books.get_mut(&ticker) {
                book.apply_delta(&delta);
            } else {
                tracing::debug!("Delta for unknown ticker {} (no snapshot yet)", ticker);
            }
        }
        KalshiWsEvent::Fill(fill) => {
            tracing::info!(
                "FILL: {} {} {} {} contracts @ yes={} no={}",
                fill.market_ticker, fill.action, fill.side, fill.count,
                fill.yes_price, fill.no_price,
            );

            let mut s = state.lock().await;

            // In-memory dedup: skip if this fill was already processed (e.g. by REST fill sync)
            if s.order_manager.is_fill_processed(&fill.trade_id) {
                tracing::debug!("Skipping duplicate WS fill {}", fill.trade_id);
                return;
            }
            s.order_manager.mark_fill_processed(&fill.trade_id);

            // Use the price for the side we're trading
            let price_cents = if fill.side == "yes" { fill.yes_price } else { fill.no_price };
            s.risk.record_fill(
                &fill.market_ticker, &fill.action, &fill.side,
                price_cents, fill.count,
            );
            // Decrement remaining count; only removes order when fully filled
            let was_resting = s.order_manager.has_resting_order(&fill.market_ticker);
            s.order_manager.record_fill(&fill.order_id, fill.count);
            let still_resting = s.order_manager.has_resting_order(&fill.market_ticker);
            let new_status = if was_resting && !still_resting { "filled" } else { "partial_fill" };
            // Extract data needed for logging before dropping state lock
            let order_id = fill.order_id.clone();
            let trade_id = fill.trade_id.clone();
            let ticker = fill.market_ticker.clone();
            let side = fill.side.clone();
            let action = fill.action.clone();
            let count = fill.count;
            let fee_dollars = RiskManager::maker_fee(count, price_cents) / 100.0;
            let strategy = s.order_manager.get_strategy(&order_id).map(ToString::to_string).unwrap_or_default();
            let mut game_info = GameInfo::from_game_state(&s.game_state, &ticker);
            // Compute edge from live fair value
            if let Some(ref mut gi) = game_info {
                if let Some(fv) = gi.espn_fair {
                    let order_prob = price_cents as f64 / 100.0;
                    let is_yes = side.eq_ignore_ascii_case("yes");
                    let fair_for_side = if is_yes { fv } else { 1.0 - fv };
                    let is_buy = action.eq_ignore_ascii_case("buy");
                    let raw_edge = fair_for_side - order_prob;
                    gi.edge_bps = Some((if is_buy { raw_edge } else { -raw_edge }) * 10000.0);
                }
            }
            drop(s);
            // Log under logger lock (separate from state lock)
            {
                let log = logger.lock().unwrap();
                // Ensure order stub exists with strategy and game info
                let _ = log.log_order_if_missing(
                    &order_id, &ticker, &action, &side,
                    price_cents, count, new_status,
                    None, game_info.as_ref(),
                );
                if !strategy.is_empty() {
                    let _ = log.update_order_strategy(&order_id, &strategy);
                }
                let _ = log.log_fill(
                    &trade_id, &order_id, &ticker,
                    &side, &action, price_cents, count, fee_dollars,
                    None, // WS fills don't include timestamp; use current time
                );
            }
        }
        KalshiWsEvent::Trade(_) => {} // Ignored; public trades not used
        KalshiWsEvent::Connected => tracing::info!("Kalshi WebSocket connected"),
        KalshiWsEvent::Disconnected => tracing::warn!("Kalshi WebSocket disconnected"),
        KalshiWsEvent::Error(e) => tracing::error!("Kalshi WebSocket error: {}", e),
    }
}
