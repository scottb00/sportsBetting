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

    // Collect game_info and strategy per order under state lock (minimal data needed for logging)
    let order_extras: Vec<(i64, Option<crate::engine::logger::GameInfo>, Option<String>)> = {
        let s = state.lock().await;
        orders.iter().map(|order| {
            let price_cents = match order.side {
                crate::kalshi::types::OrderSide::Yes => order.yes_price.unwrap_or(0),
                crate::kalshi::types::OrderSide::No => order.no_price.unwrap_or(0),
            };
            let mut game_info = crate::engine::logger::GameInfo::from_game_state(&s.game_state, &order.ticker);
            // Compute edge from live fair value
            if let Some(ref mut gi) = game_info {
                if let Some(fv) = gi.espn_fair {
                    let is_yes = matches!(order.side, crate::kalshi::types::OrderSide::Yes);
                    let order_prob = price_cents as f64 / 100.0;
                    let fair_for_side = if is_yes { fv } else { 1.0 - fv };
                    let is_buy = matches!(order.action, crate::kalshi::types::OrderAction::Buy);
                    let raw_edge = fair_for_side - order_prob;
                    gi.edge_bps = Some((if is_buy { raw_edge } else { -raw_edge }) * 10000.0);
                }
            }
            let strategy = s.order_manager.get_strategy(&order.order_id).map(|s| s.to_string());
            (price_cents, game_info, strategy)
        }).collect()
    };

    // Log under logger lock (no state lock held)
    {
        let log = logger.lock().unwrap();
        for (order, (price_cents, game_info, strategy)) in orders.iter().zip(order_extras.iter()) {
            let _ = log.log_order_if_missing(
                &order.order_id,
                &order.ticker,
                &format!("{:?}", order.action),
                &format!("{:?}", order.side),
                *price_cents,
                order.remaining_count,
                &order.status,
                Some(&order.created_time),
                game_info.as_ref(),
            );
            if let Some(strategy) = strategy {
                let _ = log.update_order_strategy(&order.order_id, strategy);
            }
        }
    }

    // Apply to order manager under state lock
    let mut s = state.lock().await;
    s.order_manager.apply_synced_orders(orders);
}
