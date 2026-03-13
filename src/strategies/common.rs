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
///
/// Target-position model:
/// 1. Compute target = floor(edge_pct * contracts_per_pct_edge) in the edge direction (0 if no edge).
/// 2. Compare to current net_position(ticker).
/// 3. If target > 0 and delta >= min_trade_contracts: emit add signal (capped at max_contracts_per_order).
/// 4. If target == 0 and net != 0: emit close signal scaled by close-direction edge.
fn evaluate_market(
    game: &GameState,
    market: &KalshiMarketState,
    order_books: &HashMap<String, LocalOrderBook>,
    risk: &RiskManager,
    min_edge: f64,
    strategy_name: &str,
    contracts_per_pct_edge: f64,
    min_trade_contracts: i64,
    max_contracts_per_order: i64,
    max_contracts_per_game: i64,
) -> Option<OrderSignal> {
    let fair_value = game.fair_value_for_market(market)?;

    // Derive prices from order book
    let prices = order_books.get(&market.ticker).map(extract_book_prices)?;
    let yes_bid = prices.bid? as i64;
    let yes_ask = prices.ask? as i64;

    // Step 1: compute target in signed YES-units (+ = hold YES, - = hold NO)
    // Capped at max_contracts_per_game to prevent unrealistic targets on thin/wide books.
    let target_signed: i64 = if let Some(r) = compute_edge_and_alo(yes_bid, yes_ask, fair_value) {
        if r.edge_after_fees >= min_edge {
            let n = ((r.edge_after_fees - min_edge) * 100.0 * contracts_per_pct_edge).floor().max(1.0) as i64;
            let n = n.min(max_contracts_per_game);
            if r.buying_yes { n } else { -n }
        } else {
            0
        }
    } else {
        0
    };

    // Step 2a: game-level net aligned to this market's YES direction.
    // Accounts for equivalent exposure on other markets (YES-DUKE and NO-UNC are the same bet).
    let net_game_aligned = risk.effective_net_for_market(&game.kalshi_markets, &market.ticker);

    // Step 3: decide action
    if target_signed != 0 {
        // Case A: we have a target — only ADD toward it, never trim.
        // delta = target_signed - net_game_aligned: positive = need more YES, negative = need more NO.
        // Only add if delta has same sign as target (we're short). If signs differ, we're already
        // at or past target via equivalent positions on another market — don't double-up.
        let delta = target_signed - net_game_aligned;
        if delta == 0 || delta.signum() != target_signed.signum() {
            return None; // at or past target, don't trim
        }
        if delta.abs() < min_trade_contracts {
            return None; // anti-scalp: delta too small to bother
        }

        // delta has same sign as target: add toward target direction
        let (side, action, alo, edge) = if target_signed > 0 {
            // Target is YES — buy YES
            let alo = (yes_ask - 1).max(1);
            let edge = if let Some(r) = compute_edge_and_alo(yes_bid, yes_ask, fair_value) {
                r.edge_after_fees
            } else {
                return None;
            };
            (OrderSide::Yes, OrderAction::Buy, alo, edge)
        } else {
            // Target is NO (target_signed < 0) — buy NO
            let alo = (100 - yes_bid - 1).max(1);
            let edge = if let Some(r) = compute_edge_and_alo(yes_bid, yes_ask, fair_value) {
                r.edge_after_fees
            } else {
                return None;
            };
            (OrderSide::No, OrderAction::Buy, alo, edge)
        };

        let contracts = delta.abs().min(max_contracts_per_order);
        let size_dollars = contracts as f64 * alo as f64 / 100.0;

        let mid = (yes_bid + yes_ask) as f64 / 2.0 / 100.0;
        let fair_for_log = if matches!(side, OrderSide::Yes) { fair_value } else { 1.0 - fair_value };
        let mid_for_log = if matches!(side, OrderSide::Yes) { mid } else { 1.0 - mid };
        tracing::info!(
            "{} ADD signal: {} {:?} at {}c, {} contracts (target={} net_game={}), edge {:.4}, fair {:.4}, mid {:.4}",
            strategy_name, market.ticker, side, alo, contracts,
            target_signed, net_game_aligned, edge, fair_for_log, mid_for_log
        );

        Some(OrderSignal {
            strategy: strategy_name.to_string(),
            kalshi_ticker: market.ticker.clone(),
            side,
            action,
            price_cents: alo,
            size_dollars,
            post_only: true,
            expiration_ts: None,
            edge_after_fees: edge,
            fair_value_cents: Some((fair_value * 100.0).round() as i64),
            is_close: false,
            max_contracts: None,
        })
    } else if net_game_aligned != 0 {
        // Case B: no edge in position direction, but game-level exposure exists — close it.
        // Uses game-aligned net (not ticker-specific) so we can close via ANY ticker in the game.
        // e.g., hold YES-DUKE → can close by buying NO-DUKE or buying YES-UNC (whichever has better edge).
        // evaluate_edge picks the best signal across all markets, so the best close price wins.
        let (side, action, alo) = if net_game_aligned > 0 {
            // Effectively long YES on this market → close by buying NO
            let alo = (100 - yes_bid - 1).max(1);
            (OrderSide::No, OrderAction::Buy, alo)
        } else {
            // Effectively long NO on this market → close by buying YES
            let alo = (yes_ask - 1).max(1);
            (OrderSide::Yes, OrderAction::Buy, alo)
        };

        let exposure_abs = net_game_aligned.unsigned_abs() as i64;

        // Compute edge in the close direction
        let close_edge = compute_edge_and_alo(yes_bid, yes_ask, fair_value)
            .filter(|r| {
                // Edge must be in the close direction (buying opposite of game-level exposure)
                (net_game_aligned > 0 && !r.buying_yes) || (net_game_aligned < 0 && r.buying_yes)
            })
            .map(|r| r.edge_after_fees.max(0.0))
            .unwrap_or(0.0);

        // Close as much as possible when +EV — only gate is positive close edge
        if close_edge <= 0.0 {
            return None;
        }

        let contracts = exposure_abs.min(max_contracts_per_order);
        let size_dollars = contracts as f64 * alo as f64 / 100.0;

        tracing::info!(
            "{} CLOSE signal: {} {:?} at {}c, {} contracts (net_game_aligned={}, exposure={}, close_edge={:.4}), fair {:.4}",
            strategy_name, market.ticker, side, alo, contracts, net_game_aligned, exposure_abs,
            close_edge, fair_value
        );

        Some(OrderSignal {
            strategy: strategy_name.to_string(),
            kalshi_ticker: market.ticker.clone(),
            side,
            action,
            price_cents: alo,
            size_dollars,
            post_only: true,
            expiration_ts: None,
            edge_after_fees: close_edge,
            fair_value_cents: Some((fair_value * 100.0).round() as i64),
            is_close: true,
            max_contracts: None,
        })
    } else {
        // Case C: no target, no position — nothing to do
        None
    }
}

/// Evaluate edge across ALL Kalshi markets for a game, return the best signal.
pub fn evaluate_edge(
    game: &GameState,
    risk: &RiskManager,
    _current_game_exposure: f64, // kept for trait compat; no longer used for sizing
    min_edge: f64,
    strategy_name: &str,
    order_books: &HashMap<String, LocalOrderBook>,
    contracts_per_pct_edge: f64,
    min_trade_contracts: i64,
    max_contracts_per_order: i64,
    max_contracts_per_game: i64,
) -> Option<OrderSignal> {
    let mut best: Option<OrderSignal> = None;

    for market in &game.kalshi_markets {
        if let Some(signal) = evaluate_market(
            game, market, order_books, risk,
            min_edge, strategy_name,
            contracts_per_pct_edge, min_trade_contracts, max_contracts_per_order,
            max_contracts_per_game,
        ) && best.as_ref().is_none_or(|b| signal.edge_after_fees > b.edge_after_fees)
        {
            best = Some(signal);
        }
    }

    best
}
