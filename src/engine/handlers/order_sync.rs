use std::sync::Arc;

use crate::engine::bot::SharedState;
use crate::kalshi::rest::KalshiRestClient;

/// Sync resting orders from Kalshi API into the local OrderManager cache.
/// Fetches orders OUTSIDE the lock, then applies under the lock to avoid
/// blocking the event loop during the HTTP call.
pub async fn sync_orders(state: &SharedState, kalshi_rest: &Arc<KalshiRestClient>) {
    // Fetch from Kalshi without holding the lock
    let orders = match kalshi_rest.get_orders(None).await {
        Ok(resp) => resp.orders,
        Err(e) => {
            tracing::warn!("Order sync failed: {:?}", e);
            return;
        }
    };

    // Apply under lock (fast, no I/O)
    let mut s = state.lock().await;

    // Persist resting orders to SQLite so the dashboard can show them
    // (uses INSERT OR IGNORE to avoid overwriting orders with real strategy/edge data)
    for order in &orders {
        let price_cents = order.yes_price.or(order.no_price).unwrap_or(0);
        let _ = s.logger.log_order_if_missing(
            &order.order_id,
            &order.ticker,
            &format!("{:?}", order.action),
            &format!("{:?}", order.side),
            price_cents,
            order.remaining_count,
            &order.status,
            Some(&order.created_time),
        );
    }

    s.order_manager.apply_synced_orders(orders);
}

/// Sync positions from Kalshi API into BotState for dashboard display.
pub async fn sync_positions(state: &SharedState, kalshi_rest: &Arc<KalshiRestClient>) {
    let positions = match kalshi_rest.get_positions().await {
        Ok(resp) => resp.market_positions,
        Err(e) => {
            tracing::warn!("Position sync failed: {:?}", e);
            return;
        }
    };

    let with_holdings: Vec<_> = positions.iter()
        .filter(|p| p.position != 0)
        .collect();
    if !with_holdings.is_empty() {
        for p in &with_holdings {
            tracing::info!(
                "Position holding: {} | position={} exposure={} traded={}",
                p.ticker, p.position, p.market_exposure, p.total_traded
            );
        }
    }

    let mut s = state.lock().await;
    s.positions.clear();
    for pos in positions {
        s.positions.insert(pos.ticker.clone(), pos);
    }
}
