use crate::engine::bot::{SharedState, SharedOrderBooks, SharedLogger};
use crate::kalshi::websocket::KalshiWsEvent;

/// Handle a Kalshi WebSocket event.
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
            }
        }
        KalshiWsEvent::Fill(fill) => {
            tracing::info!(
                "FILL: {} {} {} {} contracts @ yes={} no={}",
                fill.market_ticker, fill.action, fill.side, fill.count,
                fill.yes_price, fill.no_price,
            );
            let price_cents = if fill.side == "yes" { fill.yes_price } else { fill.no_price };
            let new_status = {
                let mut s = state.lock().await;
                s.risk.record_fill(
                    &fill.market_ticker, &fill.action, &fill.side,
                    price_cents, fill.count,
                );
                // Decrement remaining count; only removes order when fully filled
                let was_resting = s.order_manager.has_resting_order(&fill.market_ticker);
                s.order_manager.record_fill(&fill.order_id, fill.count);
                let still_resting = s.order_manager.has_resting_order(&fill.market_ticker);
                if was_resting && !still_resting { "filled" } else { "partial_fill" }
            };
            // Log under logger lock (separate from state lock)
            {
                let log = logger.lock().unwrap();
                let _ = log.update_order_status(&fill.order_id, new_status);
                let _ = log.log_fill(
                    &fill.trade_id, &fill.order_id, &fill.market_ticker,
                    &fill.side, &fill.action, price_cents, fill.count, 0.0,
                    None, // WS fills don't include timestamp; use current time
                );
            }
            // Fill notifications are handled by scoreboard handler (break_ev orders only)
        }
        KalshiWsEvent::Trade(trade) => {
            tracing::debug!(
                "Trade: {} {} contracts @ {} taker={}",
                trade.market_ticker, trade.count, trade.yes_price, trade.taker_side
            );
        }
        KalshiWsEvent::Connected => tracing::info!("Kalshi WebSocket connected"),
        KalshiWsEvent::Disconnected => tracing::warn!("Kalshi WebSocket disconnected"),
        KalshiWsEvent::Error(e) => tracing::error!("Kalshi WebSocket error: {}", e),
    }
}
