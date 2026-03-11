use std::collections::HashMap;

use crate::engine::game_state::{GameState, KalshiMarketState};
use crate::engine::market_prep::extract_book_prices;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::kalshi::orderbook::LocalOrderBook;
use crate::kalshi::types::{OrderAction, OrderSide};

/// Result of computing edge and ALO price from book prices and fair value.
#[derive(Debug, Clone)]
pub struct EdgeResult {
    /// Whether the signal is to buy YES (true) or buy NO (false).
    pub buying_yes: bool,
    /// ALO price in cents (most aggressive passive price).
    pub alo_price: i64,
    /// Raw edge before fees.
    pub edge_raw: f64,
    /// Edge after subtracting maker fees.
    pub edge_after_fees: f64,
}

/// Compute edge and ALO price given book prices (YES bid/ask) and a fair value.
///
/// Returns `None` if the book is invalid or there is no positive raw edge.
/// Does NOT apply any minimum-edge threshold — callers decide that.
pub fn compute_edge_and_alo(yes_bid: i64, yes_ask: i64, fair_value: f64) -> Option<EdgeResult> {
    if yes_bid <= 0 || yes_ask >= 100 || yes_bid >= yes_ask {
        return None;
    }

    // Determine direction: compare fair to mid
    let mid = (yes_bid + yes_ask) as f64 / 2.0 / 100.0;
    let buying_yes = fair_value > mid;

    // ALO pricing: most aggressive passive price
    let alo_price = if buying_yes {
        // Buy YES: best passive price is ask - 1
        (yes_ask - 1).max(1)
    } else {
        // Buy NO: NO ask = 100 - yes_bid. Most aggressive passive NO = (100 - yes_bid) - 1
        let no_ask = 100 - yes_bid;
        (no_ask - 1).max(1)
    };

    // Edge from actual order price, not mid
    let order_prob = alo_price as f64 / 100.0;
    let edge_raw = if buying_yes {
        fair_value - order_prob
    } else {
        (1.0 - fair_value) - order_prob // fair NO prob - NO price
    };

    if edge_raw <= 0.0 {
        return None;
    }

    let fee_per_contract = RiskManager::maker_fee(1, alo_price) / 100.0;
    let edge_after_fees = edge_raw - fee_per_contract;

    Some(EdgeResult {
        buying_yes,
        alo_price,
        edge_raw,
        edge_after_fees,
    })
}

/// Evaluate edge for a specific Kalshi market within a game.
/// Returns an OrderSignal if edge exceeds threshold after fees.
///
/// Logic:
/// 1. Get ESPN fair value aligned with this market's YES side
/// 2. Determine direction (buy YES or buy NO)
/// 3. Price ALO: most aggressive passive price (ask-1 for YES, best NO ask-1 for NO)
/// 4. Edge = |fair_value - order_price| (edge from actual fill price, not mid)
/// 5. Subtract maker fees
/// 6. Kelly-size the order
fn evaluate_market(
    game: &GameState,
    market: &KalshiMarketState,
    order_books: &HashMap<String, LocalOrderBook>,
    risk: &RiskManager,
    current_game_exposure: f64,
    min_edge: f64,
    strategy_name: &str,
) -> Option<OrderSignal> {
    let fair_value = game.fair_value_for_market(market)?;

    // Derive prices from order book (single source of truth)
    let prices = order_books.get(&market.ticker).map(extract_book_prices)?;
    let yes_bid = prices.bid? as i64;
    let yes_ask = prices.ask? as i64;

    let result = compute_edge_and_alo(yes_bid, yes_ask, fair_value)?;

    if result.edge_after_fees < min_edge {
        if result.edge_after_fees > 0.0 {
            // Near-miss: positive edge after fees but below threshold
            tracing::info!(
                "{} near-miss: {} edge_after_fees={:.4} < min_edge={:.4} (edge={:.4}, fair={:.4}, price={}c)",
                strategy_name, market.ticker, result.edge_after_fees, min_edge, result.edge_raw, fair_value, result.alo_price
            );
        } else {
            // Fees eat the edge entirely
            let fee_per_contract = RiskManager::maker_fee(1, result.alo_price) / 100.0;
            tracing::info!(
                "{} fees-eaten: {} edge={:.4} but edge_after_fees={:.4} (fee={:.4}, fair={:.4}, price={}c)",
                strategy_name, market.ticker, result.edge_raw, result.edge_after_fees, fee_per_contract, fair_value, result.alo_price
            );
        }
        return None;
    }

    let (side, action) = if result.buying_yes {
        (OrderSide::Yes, OrderAction::Buy)
    } else {
        (OrderSide::No, OrderAction::Buy)
    };

    // Kelly sizing: use the fair prob for the side we're buying
    let fair_for_side = if result.buying_yes { fair_value } else { 1.0 - fair_value };
    let size = risk.kelly_size(fair_for_side, result.alo_price as f64, current_game_exposure);
    if size <= 0.0 {
        return None;
    }

    let mid = (yes_bid + yes_ask) as f64 / 2.0 / 100.0;
    let mid_for_side = if result.buying_yes { mid } else { 1.0 - mid };
    tracing::info!(
        "{} signal: {} {:?} {:?} at {}c, size ${:.2}, edge {:.4} (after fees), fair {:.4}, mid {:.4}",
        strategy_name, market.ticker, action, side, result.alo_price, size, result.edge_after_fees, fair_for_side, mid_for_side
    );

    Some(OrderSignal {
        strategy: strategy_name.to_string(),
        kalshi_ticker: market.ticker.clone(),
        side,
        action,
        price_cents: result.alo_price,
        size_dollars: size,
        post_only: true,
        expiration_ts: None,
        edge_after_fees: result.edge_after_fees,
        max_contracts: None, // set by executor based on per-game limit
    })
}

/// Evaluate edge across ALL Kalshi markets for a game, return the best signal.
pub fn evaluate_edge(
    game: &GameState,
    risk: &RiskManager,
    current_game_exposure: f64,
    min_edge: f64,
    strategy_name: &str,
    order_books: &HashMap<String, LocalOrderBook>,
) -> Option<OrderSignal> {
    let mut best: Option<OrderSignal> = None;

    for market in &game.kalshi_markets {
        if let Some(signal) = evaluate_market(game, market, order_books, risk, current_game_exposure, min_edge, strategy_name)
            && best.as_ref().is_none_or(|b| signal.edge_after_fees > b.edge_after_fees)
        {
            best = Some(signal);
        }
    }

    best
}
