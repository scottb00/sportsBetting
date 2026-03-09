use crate::engine::bot::SharedState;
use crate::polymarket::client::PolymarketEvent;

/// Handle a Polymarket WebSocket event.
pub async fn handle_polymarket_event(event: PolymarketEvent, _state: &SharedState) {
    match event {
        PolymarketEvent::PriceUpdate { asset_id, best_bid, best_ask } => {
            tracing::debug!(
                "Polymarket price: {} bid={:?} ask={:?}",
                asset_id, best_bid, best_ask,
            );
        }
        PolymarketEvent::TradeUpdate { .. } => {}
        PolymarketEvent::Connected => tracing::info!("Polymarket WebSocket connected"),
        PolymarketEvent::Disconnected => tracing::warn!("Polymarket WebSocket disconnected"),
    }
}
