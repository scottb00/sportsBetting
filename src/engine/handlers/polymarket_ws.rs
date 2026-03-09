use crate::engine::bot::SharedState;
use crate::polymarket::client::PolymarketEvent;

/// Handle a Polymarket WebSocket event.
// TODO: Integrate Polymarket prices as secondary reference for arb_scanner.
// When implemented, update GameState with polymarket bid/ask for cross-market comparison.
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
