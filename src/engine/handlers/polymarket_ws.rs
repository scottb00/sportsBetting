use crate::engine::bot::{PolymarketBook, SharedPolyBooks};
use crate::polymarket::client::PolymarketEvent;

/// Convert a Polymarket decimal price (0.0–1.0) to cents (1–99).
fn decimal_to_cents(price: f64) -> Option<i64> {
    if price <= 0.0 || price >= 1.0 {
        return None;
    }
    Some((price * 100.0).round() as i64)
}

/// Handle a Polymarket WebSocket event by updating SharedPolyBooks.
pub async fn handle_polymarket_event(event: PolymarketEvent, poly_books: &SharedPolyBooks) {
    match event {
        PolymarketEvent::PriceUpdate { asset_id, best_bid, best_ask } => {
            let bid_cents = best_bid.and_then(decimal_to_cents);
            let ask_cents = best_ask.and_then(decimal_to_cents);

            if bid_cents.is_some() || ask_cents.is_some() {
                let mut books = poly_books.write().await;
                let book = books.entry(asset_id.clone()).or_insert_with(|| PolymarketBook {
                    best_bid: None,
                    best_ask: None,
                    last_updated: std::time::Instant::now(),
                });
                if bid_cents.is_some() {
                    book.best_bid = bid_cents;
                }
                if ask_cents.is_some() {
                    book.best_ask = ask_cents;
                }
                book.last_updated = std::time::Instant::now();

                tracing::debug!(
                    "Polymarket book: {} bid={}c ask={}c",
                    asset_id,
                    bid_cents.map_or("?".to_string(), |v| v.to_string()),
                    ask_cents.map_or("?".to_string(), |v| v.to_string()),
                );
            }
        }
        PolymarketEvent::TradeUpdate { .. } => {}
        PolymarketEvent::Connected => tracing::info!("Polymarket WebSocket connected"),
        PolymarketEvent::Disconnected => tracing::warn!("Polymarket WebSocket disconnected"),
    }
}
