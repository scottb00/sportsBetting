use std::sync::Arc;

use crate::engine::bot::{BotState, SharedState, StrategyRegistry};
use crate::engine::order_manager::{OrderManager, OrderSignal};
use crate::kalshi::rest::KalshiRestClient;

/// Run all strategies and collect order signals.
/// Evaluates across ALL markets per game, picks the best signal per game.
pub fn evaluate_strategies(
    state: &BotState,
    registry: &StrategyRegistry,
) -> Vec<OrderSignal> {
    let mut signals = Vec::new();

    if state.risk.is_halted() {
        return signals;
    }

    for game in state.game_state.games.values() {
        if !game.has_kalshi() {
            continue;
        }

        // Skip low-volume games
        if game.kalshi_total_volume() < registry.min_volume {
            continue;
        }

        // Skip extreme prices — check if any market has mid in tradeable range
        let has_tradeable_mid = game.kalshi_markets.iter().any(|m| {
            m.yes_mid.is_some_and(|mid| {
                (registry.min_price_cents..=registry.max_price_cents).contains(&mid)
            })
        });
        if !has_tradeable_mid {
            continue;
        }

        // Compute exposure across all markets for this game
        let current_exposure: f64 = game.kalshi_markets.iter()
            .map(|m| state.order_manager.market_exposure(&m.ticker))
            .sum();

        // Detailed break logging
        if game.phase.is_break() {
            let home_score = game.home_score.unwrap_or(0);
            let away_score = game.away_score.unwrap_or(0);
            for market in &game.kalshi_markets {
                let fair = game.fair_value_for_market(market);
                let kalshi_mid = market.yes_mid.map(|m| m / 100.0);
                tracing::info!(
                    "BREAK: {} v {} | {} | score {}-{} | {} YES={} bid={:?} ask={:?} mid={:?} | espn_fair={:?} | vol={:?}",
                    game.away_team, game.home_team, game.status_detail,
                    away_score, home_score,
                    market.ticker,
                    if market.is_home { "home" } else { "away" },
                    market.yes_bid, market.yes_ask, kalshi_mid,
                    fair, market.volume,
                );
            }
        }

        let mut best_signal: Option<OrderSignal> = None;

        for strategy in &registry.strategies {
            if !strategy.can_evaluate(game) {
                continue;
            }

            // Skip if any market in this game already has a resting order or in-flight
            let has_resting = game.kalshi_markets.iter().any(|m| {
                state.order_manager.has_resting_order(&m.ticker)
            });
            if has_resting {
                continue;
            }

            if let Some(mut signal) = strategy.evaluate(game, &state.risk, current_exposure) {
                // Set expiration for non-CLV strategies (CLV sets its own in evaluate())
                if signal.expiration_ts.is_none() {
                    let expire_at = chrono::Utc::now().timestamp() + registry.order_ttl.as_secs() as i64;
                    signal.expiration_ts = Some(expire_at);
                }

                if best_signal.as_ref().is_none_or(|b| signal.size_dollars > b.size_dollars) {
                    best_signal = Some(signal);
                }
            }
        }

        if let Some(signal) = best_signal {
            signals.push(signal);
        }
    }

    signals
}

/// Info about a successfully placed order, used for batch notifications.
pub struct PlacedOrder {
    pub strategy: String,
    pub ticker: String,
    pub side: String,
    pub action: String,
    pub count: i64,
    pub price_cents: i64,
    pub size_dollars: f64,
    pub edge_after_fees: f64,
}

/// Execute an order signal via Kalshi REST API (or log if dry_run).
/// Returns placement info if the order was successfully placed (for batch notifications).
pub async fn execute_signal(
    signal: OrderSignal,
    state: &SharedState,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
    live_strategies: &[String],
) -> Option<PlacedOrder> {
    let s = state.lock().await;
    if !s.risk.can_trade(signal.size_dollars) {
        tracing::warn!(
            "Risk check failed for {} signal on {}",
            signal.strategy,
            signal.kalshi_ticker
        );
        return None;
    }
    drop(s);

    let order_req = OrderManager::signal_to_order(&signal);

    // Strategy is live only if it's in the configured live_strategies list
    let strategy_is_live = live_strategies.iter().any(|s| s == &signal.strategy);
    let effective_dry_run = dry_run || !strategy_is_live;

    if effective_dry_run {
        tracing::info!(
            "DRY RUN: {} {:?} {:?} {} contracts @ {:?}/{:?} | size=${:.2} | strategy={}",
            signal.kalshi_ticker,
            order_req.action,
            order_req.side,
            order_req.count,
            order_req.yes_price,
            order_req.no_price,
            signal.size_dollars,
            signal.strategy,
        );
        // Mark in-flight so we don't double-fire on the same ticker this tick
        let mut s = state.lock().await;
        s.order_manager.mark_in_flight(&signal.kalshi_ticker);
        return None;
    }

    tracing::info!(
        "Placing order: {} {:?} {:?} {} @ {:?}/{:?} ({})",
        signal.kalshi_ticker,
        order_req.action,
        order_req.side,
        order_req.count,
        order_req.yes_price,
        order_req.no_price,
        signal.strategy,
    );

    // Mark in-flight before the API call to prevent duplicate signals
    {
        let mut s = state.lock().await;
        s.order_manager.mark_in_flight(&signal.kalshi_ticker);
    }

    match kalshi_rest.create_order(&order_req).await {
        Ok(resp) => {
            {
                let mut s = state.lock().await;
                let _ = s.logger.log_order(
                    &resp.order.order_id,
                    &signal.kalshi_ticker,
                    &signal.strategy,
                    &format!("{:?}", order_req.action),
                    &format!("{:?}", order_req.side),
                    order_req.yes_price.or(order_req.no_price).unwrap_or(0),
                    order_req.count,
                    &resp.order.status,
                );

                // Track CLV orders for closing-line validation
                if signal.strategy == "clv_hunter" {
                    let price = order_req.yes_price.or(order_req.no_price).unwrap_or(0);
                    s.order_manager.record_clv_order(
                        &resp.order.order_id,
                        &signal.kalshi_ticker,
                        &format!("{:?}", order_req.side),
                        price,
                    );
                }

                s.order_manager.record_placed_order(resp.order);
            } // lock dropped

            tracing::info!("Order placed successfully");

            let price_cents = order_req.yes_price.or(order_req.no_price).unwrap_or(0);
            Some(PlacedOrder {
                strategy: signal.strategy,
                ticker: signal.kalshi_ticker,
                side: format!("{:?}", order_req.side),
                action: format!("{:?}", order_req.action),
                count: order_req.count,
                price_cents,
                size_dollars: signal.size_dollars,
                edge_after_fees: signal.edge_after_fees,
            })
        }
        Err(e) => {
            tracing::error!("Failed to place order: {:?}", e);
            // Clear in-flight so the strategy can retry next tick
            let mut s = state.lock().await;
            s.order_manager.clear_in_flight(&signal.kalshi_ticker);
            None
        }
    }
}
