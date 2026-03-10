use crate::engine::bot::{SharedState, SharedLogger};
use crate::kalshi::websocket::KalshiWsEvent;

/// Handle a Kalshi WebSocket event.
pub async fn handle_kalshi_event(
    event: KalshiWsEvent,
    state: &SharedState,
    logger: &SharedLogger,
) {
    let mut s = state.lock().await;
    match event {
        KalshiWsEvent::OrderBookSnapshot { market_ticker, snapshot } => {
            let book = s.order_books
                .entry(market_ticker.clone())
                .or_insert_with(|| crate::kalshi::orderbook::LocalOrderBook::new(market_ticker.clone()));
            book.apply_snapshot(&snapshot);
            tracing::debug!("Book snapshot for {}", market_ticker);
        }
        KalshiWsEvent::OrderBookDelta(delta) => {
            let ticker = delta.market_ticker.clone();
            if let Some(book) = s.order_books.get_mut(&ticker) {
                book.apply_delta(&delta);
            }
        }
        KalshiWsEvent::Fill(fill) => {
            tracing::info!(
                "FILL: {} {} {} {} contracts @ yes={} no={}",
                fill.market_ticker, fill.action, fill.side, fill.count,
                fill.yes_price, fill.no_price,
            );
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
            drop(s);
            // Log under logger lock (separate from state lock)
            {
                let log = logger.lock().unwrap();
                let _ = log.update_order_status(&order_id, new_status);
                let _ = log.log_fill(
                    &trade_id, &order_id, &ticker,
                    &side, &action, price_cents, count, 0.0,
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
