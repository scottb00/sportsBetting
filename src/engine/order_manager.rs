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
    /// Strategy that placed each order (order_id -> strategy name).
    /// Populated when the executor places orders; used by sync to backfill the DB.
    order_strategies: HashMap<String, String>,
    /// Tracks total contracts sent per ticker (resting + filled).
    /// This persists across order fills to prevent re-ordering the same game.
    committed_contracts: HashMap<String, i64>,
    /// When we last synced with Kalshi.
    pub last_sync: Option<Instant>,
    /// High-water mark for fill sync: latest fill created_time as unix seconds.
    last_fill_sync_ts: Option<i64>,
    /// Double-buffer fill dedup: current active set and previous (rotated when current hits 500).
    /// Prevents double-counting fills discovered by both WS and REST sync.
    processed_fills_current: HashSet<String>,
    processed_fills_prev: HashSet<String>,
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
            order_strategies: HashMap::new(),
            committed_contracts: HashMap::new(),
            last_sync: None,
            last_fill_sync_ts: None,
            processed_fills_current: HashSet::new(),
            processed_fills_prev: HashSet::new(),
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
        self.last_sync = Some(Instant::now());

        // Clean up CLV and strategy entries for orders no longer resting
        self.clv_orders.retain(|oid, _| self.resting_orders.contains_key(oid));
        self.order_strategies.retain(|oid, _| self.resting_orders.contains_key(oid));

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

    /// Clear all in-flight tickers. Called at the start of each scoreboard tick
    /// since in_flight only needs to prevent double-sends within a single tick.
    pub fn clear_all_in_flight(&mut self) {
        self.in_flight.clear();
    }

    /// Record a newly placed order into the cache (avoids waiting for next sync).
    /// Also tracks committed contracts so filled orders still count toward limits.
    pub fn record_placed_order(&mut self, order: Order, contracts_sent: i64, strategy: &str) {
        self.order_strategies.insert(order.order_id.clone(), strategy.to_string());
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

    /// Return (order_id, ticker) pairs for resting break_ev orders on the given tickers.
    pub fn break_ev_order_ids_for_tickers(&self, tickers: &[&str]) -> Vec<(String, String)> {
        tickers.iter()
            .flat_map(|ticker| {
                self.orders_by_ticker
                    .get(*ticker)
                    .into_iter()
                    .flatten()
                    .filter(|id| {
                        self.order_strategies.get(*id).map(|s| s == "break_ev").unwrap_or(false)
                    })
                    .map(|id| (id.clone(), (*ticker).to_string()))
            })
            .collect()
    }

    /// Get order IDs for a market ticker (for cancellation).
    pub fn order_ids_for_market(&self, ticker: &str) -> Vec<String> {
        self.orders_by_ticker
            .get(ticker)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Return all CLV-eligible resting orders.
    pub fn all_clv_orders(&self) -> Vec<&ClvOrderInfo> {
        self.clv_orders
            .values()
            .filter(|info| self.resting_orders.contains_key(&info.order_id))
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

    /// Get the strategy that placed an order (if known from this session).
    pub fn get_strategy(&self, order_id: &str) -> Option<&str> {
        self.order_strategies.get(order_id).map(|s| s.as_str())
    }

    pub fn open_order_count(&self) -> usize {
        self.resting_orders.len()
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Get the fill sync high-water mark (unix seconds).
    pub fn last_fill_sync_ts(&self) -> Option<i64> {
        self.last_fill_sync_ts
    }

    /// Set the fill sync high-water mark (unix seconds).
    pub fn set_last_fill_sync_ts(&mut self, ts: i64) {
        self.last_fill_sync_ts = Some(ts);
    }

    /// Check if a fill has already been processed (dedup check).
    pub fn is_fill_processed(&self, trade_id: &str) -> bool {
        self.processed_fills_current.contains(trade_id)
            || self.processed_fills_prev.contains(trade_id)
    }

    /// Mark a fill as processed. Rotates the double-buffer when current hits 500 entries.
    pub fn mark_fill_processed(&mut self, trade_id: &str) {
        const MAX_SIZE: usize = 500;
        if self.processed_fills_current.len() >= MAX_SIZE {
            // Rotate: prev = current, current = fresh
            std::mem::swap(&mut self.processed_fills_prev, &mut self.processed_fills_current);
            self.processed_fills_current.clear();
        }
        self.processed_fills_current.insert(trade_id.to_string());
    }

    /// Recover CLV order metadata from resting orders after a restart.
    /// Resting orders placed in a previous session won't have clv_orders entries —
    /// this method rebuilds them from order_strategies for CLV tracking to work post-restart.
    pub fn recover_clv_from_resting(&mut self) {
        for (order_id, order) in &self.resting_orders {
            // Skip if already tracked in clv_orders
            if self.clv_orders.contains_key(order_id) {
                continue;
            }
            // Only recover CLV orders (placed by clv_hunter strategy)
            if self.order_strategies.get(order_id).map(|s| s.as_str()) != Some("clv_hunter") {
                continue;
            }
            let price_cents = order.yes_price.or(order.no_price).unwrap_or(0);
            let side = format!("{:?}", order.side);
            self.clv_orders.insert(order_id.clone(), ClvOrderInfo {
                order_id: order_id.clone(),
                ticker: order.ticker.clone(),
                side,
                price_cents,
            });
        }
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
        if let Some(order) = self.resting_orders.remove(order_id)
            && let Some(ids) = self.orders_by_ticker.get_mut(&order.ticker)
        {
            ids.remove(order_id);
            if ids.is_empty() {
                self.orders_by_ticker.remove(&order.ticker);
            }
        }
        self.clv_orders.remove(order_id);
    }

    /// Remove committed_contracts entries for finished tickers (prevents unbounded growth).
    pub fn clear_committed_contracts(&mut self, tickers: &[String]) {
        for ticker in tickers {
            self.committed_contracts.remove(ticker.as_str());
        }
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
        om.record_placed_order(make_order("o1", "TICKER-A"), 10, "test");

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
        om.record_placed_order(make_order("o1", "TICKER-A"), 10, "test");

        assert!(om.has_resting_order("TICKER-A"));
        assert_eq!(om.in_flight_count(), 0);
    }

    #[test]
    fn committed_contracts_tracked() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"), 10, "test");
        assert_eq!(om.committed_contracts("TICKER-A"), 10);
        assert_eq!(om.committed_contracts("TICKER-B"), 0);
    }

    #[test]
    fn clv_orders_tracked() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"), 10, "test");
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
        om.record_placed_order(make_order("o1", "TICKER-A"), 10, "test");
        om.record_clv_order("o1", "TICKER-A", "Yes", 55);

        assert!(om.has_resting_order("TICKER-A"));
        om.remove_order("o1");
        assert!(!om.has_resting_order("TICKER-A"));
    }

    #[test]
    fn partial_fill_keeps_order_resting() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"), 10, "test");
        assert!(om.has_resting_order("TICKER-A"));

        // Partial fill: 3 of 10 contracts
        om.record_fill("o1", 3);
        assert!(om.has_resting_order("TICKER-A"), "Order should still be resting after partial fill");
        assert_eq!(om.resting_contracts_for_tickers(&["TICKER-A"]), 7);
    }

    #[test]
    fn full_fill_removes_order() {
        let mut om = OrderManager::new();
        om.record_placed_order(make_order("o1", "TICKER-A"), 10, "test");

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
