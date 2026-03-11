use std::sync::Arc;

use crate::engine::bot::{SharedState, SharedLogger};
use crate::kalshi::rest::KalshiRestClient;

/// Sync resting orders from Kalshi API into the local OrderManager cache.
/// Fetches orders OUTSIDE the lock, then applies under the lock to avoid
/// blocking the event loop during the HTTP call.
pub async fn sync_orders(state: &SharedState, logger: &SharedLogger, kalshi_rest: &Arc<KalshiRestClient>) {
    // Fetch from Kalshi without holding the lock
    let orders = match kalshi_rest.get_orders(None).await {
        Ok(resp) => resp.orders,
        Err(e) => {
            tracing::warn!("Order sync failed: {:?}", e);
            return;
        }
    };

    // Collect data for logging under state lock (avoids holding state during SQLite writes)
    type LogEntry = (String, String, String, String, i64, i64, String, String,
                     Option<crate::engine::logger::GameInfo>, Option<String>);
    let log_entries: Vec<LogEntry> = {
        let s = state.lock().await;
        orders.iter().map(|order| {
            let price_cents = match order.side {
                crate::kalshi::types::OrderSide::Yes => order.yes_price.unwrap_or(0),
                crate::kalshi::types::OrderSide::No => order.no_price.unwrap_or(0),
            };
            let game_info = crate::engine::logger::GameInfo::from_game_state(&s.game_state, &order.ticker);
            let strategy = s.order_manager.get_strategy(&order.order_id).map(|s| s.to_string());
            (
                order.order_id.clone(), order.ticker.clone(),
                format!("{:?}", order.action), format!("{:?}", order.side),
                price_cents, order.remaining_count,
                order.status.clone(), order.created_time.clone(),
                game_info, strategy,
            )
        }).collect()
    };

    // Log under logger lock (no state lock held)
    {
        let log = logger.lock().unwrap();
        for (order_id, ticker, action, side, price_cents, remaining_count, status, created_time, game_info, strategy) in &log_entries {
            let _ = log.log_order_if_missing(
                order_id, ticker, action, side,
                *price_cents, *remaining_count, status,
                Some(created_time.as_str()), game_info.as_ref(),
            );
            if let Some(strat) = strategy {
                let _ = log.update_order_strategy(order_id, strat);
            }
        }
    }

    // Apply to order manager under state lock
    let mut s = state.lock().await;
    s.order_manager.apply_synced_orders(orders);
}
