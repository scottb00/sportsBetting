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
    s.order_manager.apply_synced_orders(orders);
}
