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

    // Collect data for logging under state lock, then log under logger lock
    struct OrderLogEntry {
        order_id: String,
        ticker: String,
        action: String,
        side: String,
        price_cents: i64,
        remaining_count: i64,
        status: String,
        created_time: String,
        game_info: Option<crate::engine::logger::GameInfo>,
        strategy: Option<String>,
    }

    let (log_entries, orders_for_cache) = {
        let s = state.lock().await;
        let entries: Vec<OrderLogEntry> = orders.iter().map(|order| {
            let price_cents = match order.side {
                crate::kalshi::types::OrderSide::Yes => order.yes_price.unwrap_or(0),
                crate::kalshi::types::OrderSide::No => order.no_price.unwrap_or(0),
            };
            let game_info = crate::engine::logger::GameInfo::from_game_state(&s.game_state, &order.ticker);
            let strategy = s.order_manager.get_strategy(&order.order_id).map(|s| s.to_string());
            OrderLogEntry {
                order_id: order.order_id.clone(),
                ticker: order.ticker.clone(),
                action: format!("{:?}", order.action),
                side: format!("{:?}", order.side),
                price_cents,
                remaining_count: order.remaining_count,
                status: order.status.clone(),
                created_time: order.created_time.clone(),
                game_info,
                strategy,
            }
        }).collect();
        (entries, orders)
    };

    // Log under logger lock (no state lock held)
    {
        let log = logger.lock().unwrap();
        for entry in &log_entries {
            let _ = log.log_order_if_missing(
                &entry.order_id,
                &entry.ticker,
                &entry.action,
                &entry.side,
                entry.price_cents,
                entry.remaining_count,
                &entry.status,
                Some(&entry.created_time),
                entry.game_info.as_ref(),
            );
            if let Some(strategy) = &entry.strategy {
                let _ = log.update_order_strategy(&entry.order_id, strategy);
            }
        }
    }

    // Apply to order manager under state lock
    let mut s = state.lock().await;
    s.order_manager.apply_synced_orders(orders_for_cache);
}
