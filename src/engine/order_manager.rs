use std::collections::{HashMap, HashSet};
use std::time::Instant;

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
    /// Hard cap on contracts for this signal (from per-game limit).
    pub max_contracts: Option<i64>,
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
    /// Reverse index: ticker -> set of order_ids (for O(1) ticker lookups).
    orders_by_ticker: HashMap<String, HashSet<String>>,
    /// Tickers with an API call currently in flight (prevents double-sends).
    /// Cleared on next sync.
    in_flight: HashSet<String>,
    /// CLV order metadata: order_id -> info. Populated when we place CLV orders.
    /// Used for closing-line validation when games go live.
    clv_orders: HashMap<String, ClvOrderInfo>,
    /// Tracks total contracts sent per ticker (resting + filled).
    /// This persists across order fills to prevent re-ordering the same game.
    committed_contracts: HashMap<String, i64>,
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
            orders_by_ticker: HashMap::new(),
            in_flight: HashSet::new(),
            clv_orders: HashMap::new(),
            committed_contracts: HashMap::new(),
            last_sync: None,
        }
    }

    /// Seed committed_contracts for a single ticker (call on startup from pre-fetched data).
    pub fn record_startup_position(&mut self, ticker: &str, total_traded: i64) {
        self.committed_contracts.insert(ticker.to_string(), total_traded);
    }

    /// Apply pre-fetched orders from Kalshi into the local cache.
    /// Use this instead of sync_from_kalshi to avoid holding the lock during HTTP calls.
    pub fn apply_synced_orders(&mut self, orders: Vec<Order>) {
        let old_count = self.resting_orders.len();
        self.resting_orders.clear();
        self.orders_by_ticker.clear();
        for order in orders {
            self.orders_by_ticker
                .entry(order.ticker.clone())
                .or_default()
                .insert(order.order_id.clone());
            self.resting_orders.insert(order.order_id.clone(), order);
        }
        self.in_flight.clear();
        self.last_sync = Some(Instant::now());

        // Clean up CLV entries for orders no longer resting
        self.clv_orders.retain(|oid, _| self.resting_orders.contains_key(oid));

        let new_count = self.resting_orders.len();
        if old_count != 0 || new_count != 0 {
            tracing::info!(
                "Order sync: {} resting orders (was {}), {} CLV tracked",
                new_count, old_count, self.clv_orders.len(),
            );
        }
    }

    /// Check if there's already a resting order on a given ticker.
    pub fn has_resting_order(&self, ticker: &str) -> bool {
        if self.in_flight.contains(ticker) {
            return true;
        }
        self.orders_by_ticker
            .get(ticker)
            .is_some_and(|ids| !ids.is_empty())
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
    /// Also tracks committed contracts so filled orders still count toward limits.
    pub fn record_placed_order(&mut self, order: Order, contracts_sent: i64) {
        *self.committed_contracts.entry(order.ticker.clone()).or_default() += contracts_sent;
        tracing::info!(
            "Committed {} contracts on {} (total: {})",
            contracts_sent, order.ticker,
            self.committed_contracts.get(&order.ticker).unwrap_or(&0),
        );
        self.in_flight.remove(&order.ticker);
        self.orders_by_ticker
            .entry(order.ticker.clone())
            .or_default()
            .insert(order.order_id.clone());
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

    /// Convert a signal to a CreateOrderRequest. Returns None if the contract
    /// cap would be zero (caller should skip this signal).
    pub fn signal_to_order(signal: &OrderSignal) -> Option<CreateOrderRequest> {
        // Reject signals where the cap is explicitly zero
        if signal.max_contracts == Some(0) {
            return None;
        }

        let (yes_price, no_price) = match signal.side {
            OrderSide::Yes => (Some(signal.price_cents), None),
            OrderSide::No => (None, Some(signal.price_cents)),
        };

        // Convert dollar size to contracts: each contract pays $1 on resolution
        let mut count = (signal.size_dollars / (signal.price_cents as f64 / 100.0)).floor() as i64;
        count = count.max(1);
        // Apply hard contract cap from per-game limit
        if let Some(max) = signal.max_contracts {
            count = count.min(max);
        }

        Some(CreateOrderRequest {
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
        })
    }

    /// Get total contracts committed for a market (resting + filled).
    /// This prevents re-ordering after fills exhaust the per-game limit.
    pub fn committed_contracts(&self, ticker: &str) -> i64 {
        self.committed_contracts.get(ticker).copied().unwrap_or(0)
    }

    /// Get order IDs for a market ticker (for cancellation).
    pub fn order_ids_for_market(&self, ticker: &str) -> Vec<String> {
        self.orders_by_ticker
            .get(ticker)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
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

    /// Record a partial fill: decrement remaining_count on the order.
    /// If fully filled (remaining hits 0), remove the order from cache.
    pub fn record_fill(&mut self, order_id: &str, filled_count: i64) {
        if let Some(order) = self.resting_orders.get_mut(order_id) {
            order.remaining_count -= filled_count;
            if order.remaining_count <= 0 {
                // Fully filled — remove from cache
                let ticker = order.ticker.clone();
                self.resting_orders.remove(order_id);
                if let Some(ids) = self.orders_by_ticker.get_mut(&ticker) {
                    ids.remove(order_id);
                    if ids.is_empty() {
                        self.orders_by_ticker.remove(&ticker);
                    }
                }
                self.clv_orders.remove(order_id);
            }
        } else {
            // Order not in local cache (maybe already removed) — no-op
            self.clv_orders.remove(order_id);
        }
    }

    /// Remove an order from local cache (e.g. after cancel).
    pub fn remove_order(&mut self, order_id: &str) {
        if let Some(order) = self.resting_orders.remove(order_id) {
            if let Some(ids) = self.orders_by_ticker.get_mut(&order.ticker) {
                ids.remove(order_id);
                if ids.is_empty() {
                    self.orders_by_ticker.remove(&order.ticker);
                }
            }
        }
        self.clv_orders.remove(order_id);
    }

    /// Get total committed contracts across multiple tickers (game-level check).
    pub fn committed_contracts_for_tickers(&self, tickers: &[&str]) -> i64 {
        tickers.iter()
            .map(|t| self.committed_contracts.get(*t).copied().unwrap_or(0))
            .sum()
    }

    /// Count resting contracts across multiple tickers.
    pub fn resting_contracts_for_tickers(&self, tickers: &[&str]) -> i64 {
        tickers.iter()
            .flat_map(|t| {
                self.orders_by_ticker.get(*t)
                    .into_iter()
                    .flat_map(|ids| ids.iter())
                    .filter_map(|id| self.resting_orders.get(id))
                    .map(|o| o.remaining_count)
            })
            .sum()
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
        om.record_placed_order(make_order("o1", "TICKER-A"), 10);

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
        om.record_placed_order(make_order("o1", "TICKER-A"), 10);

        assert!(om.has_resting_order("TICKER-A"));
        assert_eq!(om.in_flight_count(), 0);
    }

    #[test]
    fn committed_contracts_tracked() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"), 10);
        assert_eq!(om.committed_contracts("TICKER-A"), 10);
        assert_eq!(om.committed_contracts("TICKER-B"), 0);
    }

    #[test]
    fn clv_orders_tracked() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"), 10);
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
        om.record_placed_order(make_order("o1", "TICKER-A"), 10);
        om.record_clv_order("o1", "TICKER-A", "Yes", 55);

        assert!(om.has_resting_order("TICKER-A"));
        om.remove_order("o1");
        assert!(!om.has_resting_order("TICKER-A"));
    }

    #[test]
    fn partial_fill_keeps_order_resting() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"), 10);
        assert!(om.has_resting_order("TICKER-A"));

        // Partial fill: 3 of 10 contracts
        om.record_fill("o1", 3);
        assert!(om.has_resting_order("TICKER-A"), "Order should still be resting after partial fill");
        assert_eq!(om.resting_contracts_for_tickers(&["TICKER-A"]), 7);
    }

    #[test]
    fn full_fill_removes_order() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"), 10);

        // Full fill
        om.record_fill("o1", 10);
        assert!(!om.has_resting_order("TICKER-A"), "Order should be gone after full fill");
    }

    #[test]
    fn signal_to_order_returns_none_on_zero_cap() {
        let signal = OrderSignal {
            strategy: "test".into(),
            kalshi_ticker: "TICKER-A".into(),
            side: OrderSide::Yes,
            action: OrderAction::Buy,
            price_cents: 50,
            size_dollars: 10.0,
            post_only: true,
            expiration_ts: None,
            edge_after_fees: 0.05,
            max_contracts: Some(0),
        };
        assert!(OrderManager::signal_to_order(&signal).is_none());
    }
}
