use std::sync::Arc;
use crate::engine::bot::{SharedState, SharedOrderBooks, SharedLogger};
use crate::engine::logger::GameInfo;
use crate::engine::notifier::Notifier;
use crate::engine::risk::RiskManager;
use crate::kalshi::orderbook::DeltaResult;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::websocket::KalshiWsEvent;

/// Minimum interval between REST resnapshots for the same ticker.
const RESNAPSHOT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10);

/// Handle a Kalshi WebSocket event.
pub async fn handle_kalshi_event(
    event: KalshiWsEvent,
    state: &SharedState,
    order_books: &SharedOrderBooks,
    logger: &SharedLogger,
    kalshi_rest: &Arc<KalshiRestClient>,
    notifier: Option<&Notifier>,
) {
    match event {
        KalshiWsEvent::OrderBookSnapshot { market_ticker, snapshot, seq } => {
            let mut books = order_books.write().await;
            let book = books
                .entry(market_ticker.clone())
                .or_insert_with(|| crate::kalshi::orderbook::LocalOrderBook::new(market_ticker.clone()));
            book.apply_snapshot(&snapshot, seq);
            tracing::debug!("Book snapshot for {}", market_ticker);
        }
        KalshiWsEvent::OrderBookDelta(delta) => {
            let ticker = delta.market_ticker.clone();
            let needs_resnapshot = {
                let mut books = order_books.write().await;
                if let Some(book) = books.get_mut(&ticker) {
                    match book.apply_delta(&delta) {
                        DeltaResult::NeedsResnapshot(reason) => {
                            // Check cooldown to avoid resnapshot storms
                            let should = book.last_resnapshot_at
                                .is_none_or(|t| t.elapsed() > RESNAPSHOT_COOLDOWN);
                            if should {
                                book.last_resnapshot_at = Some(std::time::Instant::now());
                                Some(reason)
                            } else {
                                tracing::debug!("Resnapshot cooldown active for {}, skipping", ticker);
                                None
                            }
                        }
                        DeltaResult::Ok => None,
                    }
                } else {
                    None
                }
            };

            if let Some(reason) = needs_resnapshot {
                tracing::warn!("Book resnapshot needed for {}: {}", ticker, reason);
                let rest = kalshi_rest.clone();
                let books = order_books.clone();
                let t = ticker.clone();
                tokio::spawn(async move {
                    match rest.get_orderbook(&t).await {
                        Ok(snapshot) => {
                            let mut books = books.write().await;
                            if let Some(book) = books.get_mut(&t) {
                                book.apply_snapshot(&snapshot, None);
                                tracing::info!("Book resnapshot applied for {} (reason: {})", t, reason);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to resnapshot {}: {:?}", t, e);
                        }
                    }
                });
            }
        }
        KalshiWsEvent::Fill(fill) => {
            tracing::info!(
                "FILL: {} {} {} {} contracts @ yes={} no={}",
                fill.market_ticker, fill.action, fill.side, fill.count,
                fill.yes_price, fill.no_price,
            );
            let price_cents = if fill.side == "yes" { fill.yes_price } else { fill.no_price };
            // Calculate maker fee locally (WS fills don't include fee_cost)
            let fee_dollars = RiskManager::maker_fee(fill.count, price_cents) / 100.0;
            // Extract strategy and game_info while holding state lock
            let (new_status, strategy, game_info, is_new) = {
                let mut s = state.lock().await;
                let was_resting = s.order_manager.has_resting_order(&fill.market_ticker);
                let strat = s.order_manager.get_strategy(&fill.order_id)
                    .unwrap_or_default().to_string();
                let gi = GameInfo::from_game_state(&s.game_state, &fill.market_ticker);
                let is_new = s.apply_fill_if_new(
                    &fill.trade_id, &fill.market_ticker, &fill.order_id,
                    &fill.action, &fill.side, price_cents, fill.count,
                );
                let still_resting = s.order_manager.has_resting_order(&fill.market_ticker);
                let status = if was_resting && !still_resting { "filled" } else { "partial_fill" };
                (status, strat, gi, is_new)
            };
            if !is_new {
                tracing::debug!("WS fill {} already processed — skipping", fill.trade_id);
                return;
            }
            // Log under logger lock (separate from state lock)
            {
                let log = logger.lock().unwrap();
                let _ = log.update_order_status(&fill.order_id, new_status);
                // Backfill order stub with strategy and game_info
                let _ = log.log_order_if_missing(
                    &fill.order_id, &fill.market_ticker, &fill.action,
                    &fill.side, price_cents, fill.count, new_status,
                    None, game_info.as_ref(),
                );
                if !strategy.is_empty() {
                    let _ = log.update_order_strategy(&fill.order_id, &strategy);
                }
                let _ = log.log_fill(
                    &fill.trade_id, &fill.order_id, &fill.market_ticker,
                    &fill.side, &fill.action, price_cents, fill.count, fee_dollars,
                    None, // WS fills don't include timestamp; use current time
                );
            }
            // Fill notifications disabled — only max-conviction order placements notify via Telegram
            let _ = notifier;
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
