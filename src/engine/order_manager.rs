use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::espn::types::GamePhase;
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

/// An order together with metadata for lifecycle tracking.
struct TrackedOrder {
    order: Order,
    placed_at: Instant,
    /// Which strategy placed this order (e.g. "clv_hunter").
    strategy: String,
    /// The game phase when the order was placed.
    placed_phase: GamePhase,
    /// Unix timestamp (seconds) when this order expires on Kalshi.
    /// Used to proactively prune expired orders from local state.
    expiration_ts: Option<i64>,
}

/// Summary of a CLV-eligible resting order, returned for closing-line comparison.
#[derive(Debug)]
pub struct ClvOrderInfo {
    pub order_id: String,
    pub ticker: String,
    pub side: String,
    pub price_cents: i64,
}

/// Tracks our open orders and manages the order lifecycle.
///
/// This is the single source of truth for what we believe is resting on Kalshi.
/// It guards against duplicate orders by tracking:
/// - Live orders (placed on Kalshi, confirmed via REST response)
/// - Pending orders (signal accepted, API call in flight)
/// - Dry-run orders (not placed, but counted to prevent re-signaling)
///
/// On each tick, expired orders are pruned so strategies can re-evaluate.
pub struct OrderManager {
    /// Order ID -> tracked order (order + placement time)
    open_orders: HashMap<String, TrackedOrder>,
    /// Tracks (ticker, strategy) pairs that have a pending or dry-run order
    /// this session. Prevents re-firing the same signal every tick.
    /// Key: (ticker, strategy_name). Value: when the intent was recorded.
    order_intents: HashMap<(String, String), OrderIntent>,
}

/// Records that we intend to have an order on this ticker/strategy.
struct OrderIntent {
    /// When this intent expires (order's expiration or a fallback TTL).
    /// After this, the strategy is free to re-evaluate.
    expires_at: Instant,
    /// Whether a real order was placed (vs dry-run).
    is_live: bool,
}

impl Default for OrderManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderManager {
    pub fn new() -> Self {
        Self {
            open_orders: HashMap::new(),
            order_intents: HashMap::new(),
        }
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
            expiration_ts: signal.expiration_ts, // Kalshi expects unix seconds
        }
    }

    /// Record that we intend to place (or have placed as dry-run) an order.
    /// This prevents the same strategy from re-firing on the same ticker
    /// until the intent expires.
    pub fn record_intent(&mut self, signal: &OrderSignal, is_live: bool) {
        let ttl = if let Some(exp_ts) = signal.expiration_ts {
            let now_ts = chrono::Utc::now().timestamp();
            let secs_remaining = (exp_ts - now_ts).max(0) as u64;
            Duration::from_secs(secs_remaining)
        } else {
            // Fallback: intents without expiration last 5 minutes
            Duration::from_secs(300)
        };

        let key = (signal.kalshi_ticker.clone(), signal.strategy.clone());
        self.order_intents.insert(key, OrderIntent {
            expires_at: Instant::now() + ttl,
            is_live,
        });
    }

    /// Track a new open order with strategy and phase metadata.
    pub fn track_order(
        &mut self,
        order: Order,
        strategy: String,
        placed_phase: GamePhase,
        expiration_ts: Option<i64>,
    ) {
        tracing::info!(
            "Tracking order {}: {:?} {:?} {} @ {:?}/{:?} (strategy={}, phase={:?}, expires={:?})",
            order.order_id,
            order.action,
            order.side,
            order.remaining_count,
            order.yes_price,
            order.no_price,
            strategy,
            placed_phase,
            expiration_ts,
        );
        let order_id = order.order_id.clone();
        self.open_orders.insert(order_id, TrackedOrder {
            order,
            placed_at: Instant::now(),
            strategy,
            placed_phase,
            expiration_ts,
        });
    }

    /// Handle a fill — update or remove the order.
    pub fn handle_fill(&mut self, fill: &Fill) {
        if let Some(tracked) = self.open_orders.get_mut(&fill.order_id) {
            tracked.order.remaining_count -= fill.count;
            if tracked.order.remaining_count <= 0 {
                let ticker = tracked.order.ticker.clone();
                let strategy = tracked.strategy.clone();
                self.open_orders.remove(&fill.order_id);
                // Clear intent so strategy can re-evaluate after full fill
                self.order_intents.remove(&(ticker, strategy));
                tracing::info!("Order {} fully filled", fill.order_id);
            }
        }
    }

    /// Remove a cancelled order and clear its intent.
    pub fn handle_cancel(&mut self, order_id: &str) {
        if let Some(tracked) = self.open_orders.remove(order_id) {
            let key = (tracked.order.ticker.clone(), tracked.strategy.clone());
            self.order_intents.remove(&key);
            tracing::info!("Order {} cancelled", order_id);
        }
    }

    /// Prune orders that have expired based on their expiration_ts.
    /// Kalshi auto-cancels these, but we never get a WS notification,
    /// so we must clean them up locally.
    /// Also prunes expired intents so strategies can re-fire.
    pub fn prune_expired(&mut self) {
        let now_ts = chrono::Utc::now().timestamp();
        let now = Instant::now();

        // Prune expired open orders
        let expired_ids: Vec<String> = self.open_orders
            .iter()
            .filter(|(_, t)| {
                t.expiration_ts.is_some_and(|exp| now_ts >= exp)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in &expired_ids {
            if let Some(tracked) = self.open_orders.remove(id) {
                tracing::info!(
                    "Pruned expired order {} on {} (strategy={})",
                    id, tracked.order.ticker, tracked.strategy,
                );
            }
        }

        // Prune expired intents
        self.order_intents.retain(|key, intent| {
            if now >= intent.expires_at {
                tracing::debug!(
                    "Intent expired: {} / {} (was_live={})",
                    key.0, key.1, intent.is_live
                );
                false
            } else {
                true
            }
        });
    }

    /// Check if there's already a resting order OR active intent from a given
    /// strategy on a ticker. This is the main dedup guard.
    pub fn has_strategy_order(&self, ticker: &str, strategy: &str) -> bool {
        // Check live orders
        let has_open = self.open_orders.values().any(|t| {
            t.order.ticker == ticker && t.strategy == strategy
        });
        if has_open {
            return true;
        }

        // Check intents (includes dry-run orders and in-flight orders)
        let key = (ticker.to_string(), strategy.to_string());
        self.order_intents.contains_key(&key)
    }

    /// Get all open orders for a market ticker.
    pub fn orders_for_market(&self, ticker: &str) -> Vec<&Order> {
        self.open_orders
            .values()
            .filter(|t| t.order.ticker == ticker)
            .map(|t| &t.order)
            .collect()
    }

    /// Get total resting exposure in dollars for a market.
    pub fn market_exposure(&self, ticker: &str) -> f64 {
        self.open_orders
            .values()
            .filter(|t| t.order.ticker == ticker)
            .map(|t| {
                let price = t.order.yes_price.or(t.order.no_price).unwrap_or(50) as f64 / 100.0;
                t.order.remaining_count as f64 * price
            })
            .sum()
    }

    pub fn open_order_count(&self) -> usize {
        self.open_orders.len()
    }

    pub fn intent_count(&self) -> usize {
        self.order_intents.len()
    }

    /// Get order IDs for a market ticker (for bulk cancellation).
    pub fn order_ids_for_market(&self, ticker: &str) -> Vec<String> {
        self.open_orders
            .values()
            .filter(|t| t.order.ticker == ticker)
            .map(|t| t.order.order_id.clone())
            .collect()
    }

    /// Return order IDs that have been resting longer than `ttl`.
    pub fn stale_orders(&self, ttl: Duration) -> Vec<String> {
        let now = Instant::now();
        self.open_orders
            .values()
            .filter(|t| now.duration_since(t.placed_at) > ttl)
            .map(|t| t.order.order_id.clone())
            .collect()
    }

    /// Return order IDs for orders on any of the given tickers.
    pub fn order_ids_for_tickers(&self, tickers: &[String]) -> Vec<String> {
        self.open_orders
            .values()
            .filter(|t| tickers.iter().any(|tk| tk == &t.order.ticker))
            .map(|t| t.order.order_id.clone())
            .collect()
    }

    /// Return CLV-eligible orders: placed by "clv_hunter" during PreGame, still resting.
    pub fn clv_orders_for_tickers(&self, tickers: &[&str]) -> Vec<ClvOrderInfo> {
        self.open_orders
            .values()
            .filter(|t| {
                t.strategy == "clv_hunter"
                    && t.placed_phase == GamePhase::PreGame
                    && tickers.contains(&t.order.ticker.as_str())
            })
            .map(|t| {
                let price_cents = t.order.yes_price.or(t.order.no_price).unwrap_or(0);
                ClvOrderInfo {
                    order_id: t.order.order_id.clone(),
                    ticker: t.order.ticker.clone(),
                    side: format!("{:?}", t.order.side),
                    price_cents,
                }
            })
            .collect()
    }

    /// Clear all intents for tickers belonging to finished games.
    pub fn clear_intents_for_tickers(&mut self, tickers: &[String]) {
        self.order_intents.retain(|key, _| {
            !tickers.iter().any(|t| t == &key.0)
        });
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

    fn make_signal(ticker: &str, strategy: &str) -> OrderSignal {
        OrderSignal {
            strategy: strategy.to_string(),
            kalshi_ticker: ticker.to_string(),
            side: OrderSide::Yes,
            action: OrderAction::Buy,
            price_cents: 55,
            size_dollars: 10.0,
            post_only: true,
            expiration_ts: None,
            edge_after_fees: 0.05,
        }
    }

    #[test]
    fn prune_expired_orders() {
        let mut om = OrderManager::new();
        let past_ts = chrono::Utc::now().timestamp() - 10;
        om.track_order(
            make_order("o1", "TICKER-A"),
            "clv_hunter".to_string(),
            GamePhase::PreGame,
            Some(past_ts),
        );
        assert_eq!(om.open_order_count(), 1);

        om.prune_expired();
        assert_eq!(om.open_order_count(), 0);
    }

    #[test]
    fn non_expired_orders_survive_prune() {
        let mut om = OrderManager::new();
        let future_ts = chrono::Utc::now().timestamp() + 3600;
        om.track_order(
            make_order("o1", "TICKER-A"),
            "clv_hunter".to_string(),
            GamePhase::PreGame,
            Some(future_ts),
        );

        om.prune_expired();
        assert_eq!(om.open_order_count(), 1);
    }

    #[test]
    fn intent_blocks_has_strategy_order() {
        let mut om = OrderManager::new();
        let signal = make_signal("TICKER-A", "break_ev");
        om.record_intent(&signal, false);

        assert!(om.has_strategy_order("TICKER-A", "break_ev"));
        assert!(!om.has_strategy_order("TICKER-A", "clv_hunter"));
        assert!(!om.has_strategy_order("TICKER-B", "break_ev"));
    }

    #[test]
    fn expired_intent_pruned() {
        let mut om = OrderManager::new();
        // Create a signal with expiration in the past
        let mut signal = make_signal("TICKER-A", "break_ev");
        signal.expiration_ts = Some(chrono::Utc::now().timestamp() - 1);
        om.record_intent(&signal, false);

        // Intent has 0 TTL remaining, but Instant math might keep it for a moment
        // Sleep briefly to ensure expiration
        std::thread::sleep(std::time::Duration::from_millis(10));

        om.prune_expired();
        assert!(!om.has_strategy_order("TICKER-A", "break_ev"));
    }

    #[test]
    fn clear_intents_for_tickers() {
        let mut om = OrderManager::new();
        om.record_intent(&make_signal("TICKER-A", "break_ev"), false);
        om.record_intent(&make_signal("TICKER-B", "clv_hunter"), false);
        om.record_intent(&make_signal("TICKER-C", "arb_scanner"), false);

        assert_eq!(om.intent_count(), 3);

        om.clear_intents_for_tickers(&["TICKER-A".to_string(), "TICKER-C".to_string()]);
        assert_eq!(om.intent_count(), 1);
        assert!(om.has_strategy_order("TICKER-B", "clv_hunter"));
        assert!(!om.has_strategy_order("TICKER-A", "break_ev"));
    }

    #[test]
    fn cancel_clears_intent() {
        let mut om = OrderManager::new();
        let signal = make_signal("TICKER-A", "clv_hunter");
        om.record_intent(&signal, true);
        om.track_order(
            make_order("o1", "TICKER-A"),
            "clv_hunter".to_string(),
            GamePhase::PreGame,
            None,
        );

        assert!(om.has_strategy_order("TICKER-A", "clv_hunter"));
        om.handle_cancel("o1");
        assert!(!om.has_strategy_order("TICKER-A", "clv_hunter"));
    }

    #[test]
    fn stale_orders_detected() {
        let mut om = OrderManager::new();
        om.track_order(
            make_order("o1", "TICKER-A"),
            "clv_hunter".to_string(),
            GamePhase::PreGame,
            None,
        );

        // No stale orders with a generous TTL
        assert!(om.stale_orders(Duration::from_secs(3600)).is_empty());
        // All orders are stale with a zero TTL
        assert_eq!(om.stale_orders(Duration::from_secs(0)).len(), 1);
    }
}
