use crate::engine::bot::SharedState;
use crate::engine::market_prep::extract_book_prices;
use crate::engine::notifier::Notifier;
use crate::kalshi::websocket::KalshiWsEvent;

/// Handle a Kalshi WebSocket event.
pub async fn handle_kalshi_event(
    event: KalshiWsEvent,
    state: &SharedState,
    notifier: Option<&Notifier>,
) {
    let mut s = state.lock().await;
    match event {
        KalshiWsEvent::OrderBookSnapshot { market_ticker, snapshot } => {
            let book = s.order_books
                .entry(market_ticker.clone())
                .or_insert_with(|| crate::kalshi::orderbook::LocalOrderBook::new(market_ticker.clone()));
            book.apply_snapshot(&snapshot);
            let prices = extract_book_prices(book);
            if let Some(gs) = s.game_state.get_mut_by_kalshi_ticker(&market_ticker)
                && let Some(market) = gs.kalshi_market_mut(&market_ticker)
            {
                market.update_prices(prices.bid, prices.ask, prices.mid);
            }
            tracing::debug!("Book snapshot for {}", market_ticker);
        }
        KalshiWsEvent::OrderBookDelta(delta) => {
            let ticker = delta.market_ticker.clone();
            if let Some(book) = s.order_books.get_mut(&ticker) {
                book.apply_delta(&delta);
                let prices = extract_book_prices(book);
                if let Some(gs) = s.game_state.get_mut_by_kalshi_ticker(&ticker)
                    && let Some(market) = gs.kalshi_market_mut(&ticker)
                {
                    market.update_prices(prices.bid, prices.ask, prices.mid);
                }
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
            // Update order status in the DB
            let new_status = if was_resting && !still_resting { "filled" } else { "partial_fill" };
            let _ = s.logger.update_order_status(&fill.order_id, new_status);
            let _ = s.logger.log_fill(
                &fill.trade_id, &fill.order_id, &fill.market_ticker,
                &fill.side, &fill.action, price_cents, fill.count, 0.0,
                None, // WS fills don't include timestamp; use current time
            );
            // Send fill notification (drop lock first since it's async)
            let ticker = fill.market_ticker.clone();
            let action = fill.action.clone();
            let count = fill.count;
            let yes_price = fill.yes_price;
            drop(s);
            if let Some(n) = notifier {
                n.notify_fill(&ticker, &action, count, yes_price).await;
            }
            // Lock already dropped above — early return to skip drop(s) at end
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
