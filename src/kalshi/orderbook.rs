use std::collections::BTreeMap;

use super::types::{OrderBookDelta, OrderBookSnapshot, PriceLevel};

/// Local order book replica maintained from WebSocket deltas.
/// Kalshi books are bids only — a YES bid at price X implies a NO ask at (100 - X).
#[derive(Debug, Clone)]
pub struct LocalOrderBook {
    pub market_ticker: String,
    /// YES side: price (cents) -> quantity
    pub yes_levels: BTreeMap<i64, i64>,
    /// NO side: price (cents) -> quantity
    pub no_levels: BTreeMap<i64, i64>,
    pub last_seq: Option<i64>,
}

impl LocalOrderBook {
    pub fn new(market_ticker: String) -> Self {
        Self {
            market_ticker,
            yes_levels: BTreeMap::new(),
            no_levels: BTreeMap::new(),
            last_seq: None,
        }
    }

    /// Initialize from a full snapshot.
    pub fn apply_snapshot(&mut self, snapshot: &OrderBookSnapshot) {
        self.yes_levels.clear();
        self.no_levels.clear();

        for level in &snapshot.yes {
            if level.quantity > 0 {
                self.yes_levels.insert(level.price, level.quantity);
            }
        }
        for level in &snapshot.no {
            if level.quantity > 0 {
                self.no_levels.insert(level.price, level.quantity);
            }
        }
    }

    /// Apply a single delta update.
    pub fn apply_delta(&mut self, delta: &OrderBookDelta) {
        let levels = match delta.side.as_str() {
            "yes" => &mut self.yes_levels,
            "no" => &mut self.no_levels,
            _ => return,
        };

        let new_qty = levels.get(&delta.price).unwrap_or(&0) + delta.delta;
        if new_qty <= 0 {
            levels.remove(&delta.price);
        } else {
            levels.insert(delta.price, new_qty);
        }
    }

    /// Best YES bid (highest price someone will pay for YES).
    pub fn best_yes_bid(&self) -> Option<PriceLevel> {
        self.yes_levels.iter().next_back().map(|(&price, &quantity)| PriceLevel { price, quantity })
    }

    /// Best YES ask (derived: lowest NO bid implies YES ask at 100 - price).
    pub fn best_yes_ask(&self) -> Option<PriceLevel> {
        self.no_levels.iter().next_back().map(|(&no_price, &quantity)| PriceLevel {
            price: 100 - no_price,
            quantity,
        })
    }

    /// Best NO bid (highest price someone will pay for NO).
    pub fn best_no_bid(&self) -> Option<PriceLevel> {
        self.no_levels.iter().next_back().map(|(&price, &quantity)| PriceLevel { price, quantity })
    }

    /// Best NO ask (derived: lowest YES bid implies NO ask at 100 - price).
    pub fn best_no_ask(&self) -> Option<PriceLevel> {
        self.yes_levels.iter().next_back().map(|(&yes_price, &quantity)| PriceLevel {
            price: 100 - yes_price,
            quantity,
        })
    }

    /// YES midpoint price (average of best bid and ask), or None if no two-sided market.
    pub fn yes_mid(&self) -> Option<f64> {
        match (self.best_yes_bid(), self.best_yes_ask()) {
            (Some(bid), Some(ask)) => Some((bid.price as f64 + ask.price as f64) / 2.0),
            _ => None,
        }
    }

    /// YES implied probability (mid / 100).
    pub fn yes_implied_prob(&self) -> Option<f64> {
        self.yes_mid().map(|mid| mid / 100.0)
    }

    /// Spread in cents.
    pub fn spread(&self) -> Option<i64> {
        match (self.best_yes_bid(), self.best_yes_ask()) {
            (Some(bid), Some(ask)) => Some(ask.price - bid.price),
            _ => None,
        }
    }

    /// Total resting quantity on YES side.
    pub fn yes_total_size(&self) -> i64 {
        self.yes_levels.values().sum()
    }

    /// Total resting quantity on NO side.
    pub fn no_total_size(&self) -> i64 {
        self.no_levels.values().sum()
    }

    /// Order book imbalance: (yes_size - no_size) / (yes_size + no_size).
    /// Ranges from -1 (all NO) to +1 (all YES).
    pub fn imbalance(&self) -> f64 {
        let yes = self.yes_total_size() as f64;
        let no = self.no_total_size() as f64;
        let total = yes + no;
        if total == 0.0 {
            0.0
        } else {
            (yes - no) / total
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_and_delta() {
        let mut book = LocalOrderBook::new("TEST-MARKET".to_string());

        let snapshot = OrderBookSnapshot {
            yes: vec![
                PriceLevel { price: 45, quantity: 100 },
                PriceLevel { price: 44, quantity: 50 },
            ],
            no: vec![
                PriceLevel { price: 58, quantity: 80 },
                PriceLevel { price: 57, quantity: 30 },
            ],
        };

        book.apply_snapshot(&snapshot);

        assert_eq!(book.best_yes_bid().unwrap().price, 45);
        assert_eq!(book.best_no_bid().unwrap().price, 58);
        // YES ask = 100 - best NO bid = 100 - 58 = 42
        assert_eq!(book.best_yes_ask().unwrap().price, 42);

        // Apply a delta: add 20 contracts at YES 46
        book.apply_delta(&OrderBookDelta {
            market_ticker: "TEST-MARKET".to_string(),
            price: 46,
            delta: 20,
            side: "yes".to_string(),
            ts: None,
        });

        assert_eq!(book.best_yes_bid().unwrap().price, 46);
        assert_eq!(book.best_yes_bid().unwrap().quantity, 20);

        // Remove the level by delta = -20
        book.apply_delta(&OrderBookDelta {
            market_ticker: "TEST-MARKET".to_string(),
            price: 46,
            delta: -20,
            side: "yes".to_string(),
            ts: None,
        });

        assert_eq!(book.best_yes_bid().unwrap().price, 45);
    }

    #[test]
    fn test_midpoint_and_spread() {
        let mut book = LocalOrderBook::new("TEST".to_string());
        book.apply_snapshot(&OrderBookSnapshot {
            yes: vec![PriceLevel { price: 45, quantity: 100 }],
            no: vec![PriceLevel { price: 52, quantity: 100 }],
        });

        // YES bid = 45, YES ask = 100 - 52 = 48
        assert_eq!(book.spread(), Some(3));
        assert!((book.yes_mid().unwrap() - 46.5).abs() < 0.01);
    }
}
