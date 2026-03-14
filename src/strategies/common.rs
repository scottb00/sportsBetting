use std::collections::HashMap;

use crate::engine::game_state::{GameState, KalshiMarketState};
use crate::engine::market_prep::extract_book_prices;
use crate::engine::order_manager::OrderSignal;
use crate::engine::risk::RiskManager;
use crate::engine::venue::KALSHI_FEE_RATE;
use crate::espn::types::GamePhase;
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
/// `fee_rate` is the maker fee rate (e.g. 0.0175 for Kalshi, -0.001 for Polymarket rebate).
/// Returns `None` if the book is invalid or there is no positive raw edge.
/// Does NOT apply any minimum-edge threshold — callers decide that.
pub fn compute_edge_and_alo(yes_bid: i64, yes_ask: i64, fair_value: f64, contracts: i64, fee_rate: f64) -> Option<EdgeResult> {
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

    let n = contracts.max(1);
    let fee_per_contract = RiskManager::venue_fee(fee_rate, n, alo_price) / n as f64 / 100.0;
    let edge_after_fees = edge_raw - fee_per_contract;

    Some(EdgeResult {
        buying_yes,
        alo_price,
        edge_raw,
        edge_after_fees,
    })
}

/// Conviction-based sizing parameters, extracted from config.
#[derive(Debug, Clone)]
pub struct ConvictionConfig {
    pub max_contracts: i64,
    /// (min_score_threshold, contracts) — evaluated highest-threshold-first.
    pub long_tiers: Vec<(f64, i64)>,
    pub short_tiers: Vec<(f64, i64)>,
}

/// Map a conviction score to a contract count using a tier table.
/// Tiers are (min_score, contracts) pairs. We find the highest threshold that the score meets.
fn conviction_to_contracts(score: f64, tiers: &[(f64, i64)], max_contracts: i64) -> i64 {
    let mut best = 0i64;
    for &(threshold, contracts) in tiers {
        if score >= threshold {
            best = best.max(contracts);
        }
    }
    best.min(max_contracts)
}

/// Format conviction details as a JSON array string for DB logging.
pub fn format_conviction_details(details: &[(String, crate::sportsbooks::types::BookConviction, f64)]) -> String {
    use crate::sportsbooks::types::BookConviction;
    let entries: Vec<String> = details.iter().map(|(book, conv, weight)| {
        let label = match conv {
            BookConviction::Agree => "agree",
            BookConviction::Disagree => "disagree",
            BookConviction::NoOpinion => "no_opinion",
        };
        format!(r#"{{"book":"{}","conviction":"{}","weight":{:.1}}}"#, book, label, weight)
    }).collect();
    format!("[{}]", entries.join(","))
}

/// Evaluate edge for a specific Kalshi market within a game.
///
/// Target-position model:
/// 1. Compute target from conviction-based sizing in the edge direction (0 if no edge).
/// 2. Compare to current net_position(ticker).
/// 3. If target > 0 and delta >= min_trade_contracts: emit add signal.
/// 4. If target == 0 and net != 0: emit close signal gated by conviction (same veto + tier sizing as adds).
fn evaluate_market(
    game: &GameState,
    market: &KalshiMarketState,
    order_books: &HashMap<String, LocalOrderBook>,
    risk: &RiskManager,
    min_edge: f64,
    strategy_name: &str,
    min_trade_contracts: i64,
    max_close_contracts: i64,
    max_contracts_per_game: i64,
    conviction_config: &ConvictionConfig,
) -> Option<OrderSignal> {
    let fair_value = game.fair_value_for_market(market)?;

    // Derive prices from order book
    let prices = order_books.get(&market.ticker).map(extract_book_prices)?;
    let yes_bid = prices.bid? as i64;
    let yes_ask = prices.ask? as i64;

    // Step 1: compute target in signed YES-units (+ = hold YES, - = hold NO)
    // Capped at max_contracts_per_game to prevent unrealistic targets on thin/wide books.
    // Also capture conviction data for logging on the signal.
    let mut signal_conviction_score: Option<f64> = None;
    let mut signal_conviction_details: Option<String> = None;

    let target_signed: i64 = if let Some(r) = compute_edge_and_alo(yes_bid, yes_ask, fair_value, 1, KALSHI_FEE_RATE) {
        if r.edge_after_fees >= min_edge {
            // Conviction-based sizing: sportsbook consensus tiers
            let n = if let Some(spread) = &game.sportsbook_spread {
                let short_break = matches!(game.phase, GamePhase::Break);
                let conv = spread.conviction_score(
                    r.alo_price,
                    r.buying_yes,
                    market.is_home,
                    short_break,
                    game.break_started_at,
                );

                // Capture conviction data for the signal
                signal_conviction_score = Some(conv.score);
                signal_conviction_details = Some(format_conviction_details(&conv.details));

                // Any disagreement → hard veto, no trade
                if conv.any_disagree {
                    tracing::info!(
                        "Conviction VETO {}: {} — {}",
                        strategy_name, market.ticker, conv
                    );
                    return None;
                }

                let tiers = if short_break { &conviction_config.short_tiers } else { &conviction_config.long_tiers };
                let contracts = conviction_to_contracts(conv.score, tiers, conviction_config.max_contracts);

                tracing::info!(
                    "Conviction {}: {} → {} contracts — {}",
                    strategy_name, market.ticker, contracts, conv
                );

                contracts
            } else {
                // No sportsbook data — treat as 0 conviction
                signal_conviction_score = Some(0.0);
                let short_break = matches!(game.phase, GamePhase::Break);
                let tiers = if short_break { &conviction_config.short_tiers } else { &conviction_config.long_tiers };
                let contracts = conviction_to_contracts(0.0, tiers, conviction_config.max_contracts);

                tracing::info!(
                    "Conviction {}: {} → {} contracts — no sportsbook data (score=0)",
                    strategy_name, market.ticker, contracts,
                );

                contracts
            };
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
            let edge = if let Some(r) = compute_edge_and_alo(yes_bid, yes_ask, fair_value, delta.abs(), KALSHI_FEE_RATE) {
                r.edge_after_fees
            } else {
                return None;
            };
            (OrderSide::Yes, OrderAction::Buy, alo, edge)
        } else {
            // Target is NO (target_signed < 0) — buy NO
            let alo = (100 - yes_bid - 1).max(1);
            let edge = if let Some(r) = compute_edge_and_alo(yes_bid, yes_ask, fair_value, delta.abs(), KALSHI_FEE_RATE) {
                r.edge_after_fees
            } else {
                return None;
            };
            (OrderSide::No, OrderAction::Buy, alo, edge)
        };

        let contracts = delta.abs();
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
            conviction_score: signal_conviction_score,
            conviction_details: signal_conviction_details,
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
        let close_edge = compute_edge_and_alo(yes_bid, yes_ask, fair_value, exposure_abs, KALSHI_FEE_RATE)
            .filter(|r| {
                // Edge must be in the close direction (buying opposite of game-level exposure)
                (net_game_aligned > 0 && !r.buying_yes) || (net_game_aligned < 0 && r.buying_yes)
            })
            .map(|r| r.edge_after_fees.max(0.0))
            .unwrap_or(0.0);

        // Close requires positive edge
        if close_edge <= 0.0 {
            return None;
        }

        // Conviction gating + tier-based sizing for closes (same as adds)
        let buying_yes = matches!(side, OrderSide::Yes);
        let short_break = matches!(game.phase, GamePhase::Break);

        let (contracts, close_conviction_score, close_conviction_details) = if let Some(spread) = &game.sportsbook_spread {
            let conv = spread.conviction_score(
                alo, buying_yes, market.is_home, short_break, game.break_started_at,
            );

            let score = conv.score;
            let details = format_conviction_details(&conv.details);

            // Any disagreement → hard veto, no close
            if conv.any_disagree {
                tracing::info!(
                    "Conviction VETO CLOSE {}: {} — {}",
                    strategy_name, market.ticker, conv
                );
                return None;
            }

            let tiers = if short_break { &conviction_config.short_tiers } else { &conviction_config.long_tiers };
            let tier_contracts = conviction_to_contracts(score, tiers, conviction_config.max_contracts);
            // Cap at exposure (can't close more than we have) and max_close_contracts
            let n = tier_contracts.min(exposure_abs).min(max_close_contracts);

            tracing::info!(
                "Conviction CLOSE {}: {} → {} contracts (tier={}, exposure={}) — {}",
                strategy_name, market.ticker, n, tier_contracts, exposure_abs, conv
            );

            (n, Some(score), Some(details))
        } else {
            // No sportsbook data — treat as 0 conviction
            let tiers = if short_break { &conviction_config.short_tiers } else { &conviction_config.long_tiers };
            let tier_contracts = conviction_to_contracts(0.0, tiers, conviction_config.max_contracts);
            let n = tier_contracts.min(exposure_abs).min(max_close_contracts);

            tracing::info!(
                "Conviction CLOSE {}: {} → {} contracts — no sportsbook data (score=0)",
                strategy_name, market.ticker, n,
            );

            (n, Some(0.0), None)
        };

        if contracts == 0 {
            return None;
        }

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
            conviction_score: close_conviction_score,
            conviction_details: close_conviction_details,
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
    min_trade_contracts: i64,
    max_close_contracts: i64,
    max_contracts_per_game: i64,
    conviction_config: &ConvictionConfig,
) -> Option<OrderSignal> {
    let mut best: Option<OrderSignal> = None;

    for market in &game.kalshi_markets {
        if let Some(signal) = evaluate_market(
            game, market, order_books, risk,
            min_edge, strategy_name,
            min_trade_contracts, max_close_contracts,
            max_contracts_per_game, conviction_config,
        ) && best.as_ref().is_none_or(|b| signal.edge_after_fees > b.edge_after_fees)
        {
            best = Some(signal);
        }
    }

    best
}
