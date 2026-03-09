use std::sync::Arc;

use crate::engine::bot::SharedState;
use crate::kalshi::rest::KalshiRestClient;

/// Sync resting orders from Kalshi API into the local OrderManager cache.
pub async fn sync_orders(state: &SharedState, kalshi_rest: &Arc<KalshiRestClient>) {
    let mut s = state.lock().await;
    if let Err(e) = s.order_manager.sync_from_kalshi(kalshi_rest).await {
        tracing::warn!("Order sync failed: {:?}", e);
    }
}
