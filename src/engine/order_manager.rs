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
}

/// An order together with metadata for lifecycle tracking.
struct TrackedOrder {
    order: Order,
    placed_at: Instant,
    /// Which strategy placed this order (e.g. "clv_hunter").
    strategy: String,
    /// The game phase when the order was placed.
    placed_phase: GamePhase,
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
pub struct OrderManager {
    /// Order ID -> tracked order (order + placement time)
    open_orders: HashMap<String, TrackedOrder>,
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

    /// Track a new open order with strategy and phase metadata.
    pub fn track_order(&mut self, order: Order, strategy: String, placed_phase: GamePhase) {
        tracing::info!(
            "Tracking order {}: {:?} {:?} {} @ {:?}/{:?} (strategy={}, phase={:?})",
            order.order_id,
            order.action,
            order.side,
            order.remaining_count,
            order.yes_price,
            order.no_price,
            strategy,
            placed_phase,
        );
        let order_id = order.order_id.clone();
        self.open_orders.insert(order_id, TrackedOrder {
            order,
            placed_at: Instant::now(),
            strategy,
            placed_phase,
        });
    }

    /// Handle a fill — update or remove the order.
    pub fn handle_fill(&mut self, fill: &Fill) {
        if let Some(tracked) = self.open_orders.get_mut(&fill.order_id) {
            tracked.order.remaining_count -= fill.count;
            if tracked.order.remaining_count <= 0 {
                self.open_orders.remove(&fill.order_id);
                tracing::info!("Order {} fully filled", fill.order_id);
            }
        }
    }

    /// Remove a cancelled order.
    pub fn handle_cancel(&mut self, order_id: &str) {
        if self.open_orders.remove(order_id).is_some() {
            tracing::info!("Order {} cancelled", order_id);
        }
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

    /// Get order IDs for a market ticker (for bulk cancellation).
    pub fn order_ids_for_market(&self, ticker: &str) -> Vec<String> {
        self.open_orders
            .values()
            .filter(|t| t.order.ticker == ticker)
            .map(|t| t.order.order_id.clone())
            .collect()
    }

    /// Check if there's already a resting order from a given strategy on a ticker.
    pub fn has_strategy_order(&self, ticker: &str, strategy: &str) -> bool {
        self.open_orders.values().any(|t| t.order.ticker == ticker && t.strategy == strategy)
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
}
