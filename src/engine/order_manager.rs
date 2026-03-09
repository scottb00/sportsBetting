use std::collections::HashMap;

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
}

/// Tracks our open orders and manages the order lifecycle.
pub struct OrderManager {
    /// Order ID -> order details
    open_orders: HashMap<String, Order>,
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
        }
    }

    /// Track a new open order.
    pub fn track_order(&mut self, order: Order) {
        tracing::info!(
            "Tracking order {}: {:?} {:?} {} @ {:?}/{:?}",
            order.order_id,
            order.action,
            order.side,
            order.remaining_count,
            order.yes_price,
            order.no_price
        );
        self.open_orders.insert(order.order_id.clone(), order);
    }

    /// Handle a fill — update or remove the order.
    pub fn handle_fill(&mut self, fill: &Fill) {
        if let Some(order) = self.open_orders.get_mut(&fill.order_id) {
            order.remaining_count -= fill.count;
            if order.remaining_count <= 0 {
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
            .filter(|o| o.ticker == ticker)
            .collect()
    }

    /// Get total resting exposure in dollars for a market.
    pub fn market_exposure(&self, ticker: &str) -> f64 {
        self.open_orders
            .values()
            .filter(|o| o.ticker == ticker)
            .map(|o| {
                let price = o.yes_price.or(o.no_price).unwrap_or(50) as f64 / 100.0;
                o.remaining_count as f64 * price
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
            .filter(|o| o.ticker == ticker)
            .map(|o| o.order_id.clone())
            .collect()
    }
}
