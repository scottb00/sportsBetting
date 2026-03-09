use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;

use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::types::*;

/// Signal emitted by a strategy to request an order.
#[derive(Debug, Clone)]
pub struct OrderSignal {
    pub strategy: String,
    pub kalshi_ticker: String,
    pub side: OrderSide,
    pub action: OrderAction,
    pub price_cents: i64,
    pub size_dollars: f64, // will be converted to contracts
    pub post_only: bool,
    /// Optional expiration timestamp (unix seconds) — Kalshi API takes milliseconds.
    pub expiration_ts: Option<i64>,
    /// Edge after fees (used for comparing signals across markets).
    pub edge_after_fees: f64,
}

/// Summary of a CLV-eligible resting order, returned for closing-line comparison.
#[derive(Debug)]
pub struct ClvOrderInfo {
    pub order_id: String,
    pub ticker: String,
    pub side: String,
    pub price_cents: i64,
}

/// Thin order manager that uses Kalshi's API as the source of truth.
///
/// Instead of tracking the full order lifecycle locally, we periodically
/// fetch resting orders from Kalshi and cache them. The only local state
/// is:
/// - Cached resting orders (refreshed via `sync_from_kalshi`)
/// - In-flight tickers (short-lived guard against double-sends)
/// - CLV order metadata (which orders were placed by clv_hunter pre-game)
pub struct OrderManager {
    /// Cached resting orders from Kalshi, keyed by order_id.
    resting_orders: HashMap<String, Order>,
    /// Tickers with an API call currently in flight (prevents double-sends).
    /// Cleared on next sync.
    in_flight: HashSet<String>,
    /// CLV order metadata: order_id -> info. Populated when we place CLV orders.
    /// Used for closing-line validation when games go live.
    clv_orders: HashMap<String, ClvOrderInfo>,
    /// When we last synced with Kalshi.
    pub last_sync: Option<Instant>,
}

impl Default for OrderManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderManager {
    pub fn new() -> Self {
        Self {
            resting_orders: HashMap::new(),
            in_flight: HashSet::new(),
            clv_orders: HashMap::new(),
            last_sync: None,
        }
    }

    /// Fetch all resting orders from Kalshi and replace our cache.
    pub async fn sync_from_kalshi(&mut self, kalshi_rest: &Arc<KalshiRestClient>) -> Result<()> {
        let resp = kalshi_rest.get_orders(None).await?;
        let old_count = self.resting_orders.len();
        self.resting_orders.clear();
        for order in resp.orders {
            self.resting_orders.insert(order.order_id.clone(), order);
        }
        self.in_flight.clear();
        self.last_sync = Some(Instant::now());

        // Clean up CLV entries for orders no longer resting
        self.clv_orders.retain(|oid, _| self.resting_orders.contains_key(oid));

        let new_count = self.resting_orders.len();
        if old_count != new_count || old_count > 0 || new_count > 0 {
            tracing::info!(
                "Order sync: {} resting orders (was {}), {} CLV tracked",
                new_count, old_count, self.clv_orders.len(),
            );
        }
        Ok(())
    }

    /// Check if there's already a resting order on a given ticker.
    pub fn has_resting_order(&self, ticker: &str) -> bool {
        if self.in_flight.contains(ticker) {
            return true;
        }
        self.resting_orders.values().any(|o| o.ticker == ticker)
    }

    /// Mark a ticker as in-flight (API call about to be sent).
    pub fn mark_in_flight(&mut self, ticker: &str) {
        self.in_flight.insert(ticker.to_string());
    }

    /// Clear in-flight status for a ticker (e.g. after API call completes).
    pub fn clear_in_flight(&mut self, ticker: &str) {
        self.in_flight.remove(ticker);
    }

    /// Record a newly placed order into the cache (avoids waiting for next sync).
    pub fn record_placed_order(&mut self, order: Order) {
        self.in_flight.remove(&order.ticker);
        self.resting_orders.insert(order.order_id.clone(), order);
    }

    /// Record CLV metadata for a newly placed CLV order.
    pub fn record_clv_order(&mut self, order_id: &str, ticker: &str, side: &str, price_cents: i64) {
        self.clv_orders.insert(order_id.to_string(), ClvOrderInfo {
            order_id: order_id.to_string(),
            ticker: ticker.to_string(),
            side: side.to_string(),
            price_cents,
        });
    }

    /// Convert a signal to a CreateOrderRequest.
    pub fn signal_to_order(signal: &OrderSignal) -> CreateOrderRequest {
        let (yes_price, no_price) = match signal.side {
            OrderSide::Yes => (Some(signal.price_cents), None),
            OrderSide::No => (None, Some(signal.price_cents)),
        };

        // Convert dollar size to contracts: each contract pays $1 on resolution
        let count = (signal.size_dollars / (signal.price_cents as f64 / 100.0)).floor() as i64;
        let count = count.max(1);

        CreateOrderRequest {
            ticker: signal.kalshi_ticker.clone(),
            action: signal.action.clone(),
            side: signal.side.clone(),
            count,
            order_type: "limit".to_string(),
            yes_price,
            no_price,
            time_in_force: Some(TimeInForce::GoodTillCanceled),
            post_only: Some(signal.post_only),
            expiration_ts: signal.expiration_ts,
        }
    }

    /// Get total resting exposure in dollars for a market.
    pub fn market_exposure(&self, ticker: &str) -> f64 {
        self.resting_orders
            .values()
            .filter(|o| o.ticker == ticker)
            .map(|o| {
                let price = o.yes_price.or(o.no_price).unwrap_or(50) as f64 / 100.0;
                o.remaining_count as f64 * price
            })
            .sum()
    }

    /// Get order IDs for a market ticker (for cancellation).
    pub fn order_ids_for_market(&self, ticker: &str) -> Vec<String> {
        self.resting_orders
            .values()
            .filter(|o| o.ticker == ticker)
            .map(|o| o.order_id.clone())
            .collect()
    }

    /// Return CLV-eligible orders for the given tickers.
    pub fn clv_orders_for_tickers(&self, tickers: &[&str]) -> Vec<&ClvOrderInfo> {
        self.clv_orders
            .values()
            .filter(|info| {
                tickers.contains(&info.ticker.as_str())
                    && self.resting_orders.contains_key(&info.order_id)
            })
            .collect()
    }

    pub fn open_order_count(&self) -> usize {
        self.resting_orders.len()
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Remove an order from local cache (e.g. after cancel or fill).
    pub fn remove_order(&mut self, order_id: &str) {
        self.resting_orders.remove(order_id);
        self.clv_orders.remove(order_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_order(id: &str, ticker: &str) -> Order {
        Order {
            order_id: id.to_string(),
            ticker: ticker.to_string(),
            action: OrderAction::Buy,
            side: OrderSide::Yes,
            order_type: "limit".to_string(),
            status: "resting".to_string(),
            yes_price: Some(55),
            no_price: Some(45),
            remaining_count: 10,
            created_time: "2026-03-09T18:00:00Z".to_string(),
        }
    }

    #[test]
    fn has_resting_order_from_cache() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"));

        assert!(om.has_resting_order("TICKER-A"));
        assert!(!om.has_resting_order("TICKER-B"));
    }

    #[test]
    fn in_flight_blocks_duplicate() {
        let mut om = OrderManager::new();
        om.mark_in_flight("TICKER-A");

        assert!(om.has_resting_order("TICKER-A"));
        assert!(!om.has_resting_order("TICKER-B"));
    }

    #[test]
    fn record_placed_clears_in_flight() {
        let mut om = OrderManager::new();
        om.mark_in_flight("TICKER-A");
        om.record_placed_order(make_order("o1", "TICKER-A"));

        assert!(om.has_resting_order("TICKER-A"));
        assert_eq!(om.in_flight_count(), 0);
    }

    #[test]
    fn market_exposure() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"));
        // 10 contracts at 55 cents = $5.50
        let exp = om.market_exposure("TICKER-A");
        assert!((exp - 5.5).abs() < 0.01);
    }

    #[test]
    fn clv_orders_tracked() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"));
        om.record_clv_order("o1", "TICKER-A", "Yes", 55);

        let clv = om.clv_orders_for_tickers(&["TICKER-A"]);
        assert_eq!(clv.len(), 1);
        assert_eq!(clv[0].price_cents, 55);

        // Remove order -> CLV entry gone on next query
        om.remove_order("o1");
        let clv = om.clv_orders_for_tickers(&["TICKER-A"]);
        assert_eq!(clv.len(), 0);
    }

    #[test]
    fn remove_order_clears_all() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"));
        om.record_clv_order("o1", "TICKER-A", "Yes", 55);

        assert!(om.has_resting_order("TICKER-A"));
        om.remove_order("o1");
        assert!(!om.has_resting_order("TICKER-A"));
    }
}
