use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::engine::bot::{BreakEvalLog, BreakMarketEval, SharedState, SharedLogger, StrategyRegistry};
use crate::engine::game_state::GameState;
use crate::engine::market_prep::book_prices;
use crate::engine::order_manager::{OrderManager, OrderSignal};
use crate::engine::risk::RiskManager;
use crate::kalshi::orderbook::LocalOrderBook;
use crate::kalshi::rest::KalshiRestClient;

/// Snapshot of state needed for strategy evaluation (taken under lock, evaluated without lock).
pub struct EvalSnapshot {
    pub games: HashMap<String, GameState>,
    pub risk: RiskManager,
    /// Pre-computed committed contracts per ticker.
    pub committed_contracts: HashMap<String, i64>,
    /// Set of tickers that have resting orders or in-flight API calls.
    pub resting_tickers: std::collections::HashSet<String>,
}

/// Build an EvalSnapshot from the current BotState (call while holding the lock).
pub fn build_eval_snapshot(
    state: &crate::engine::bot::BotState,
) -> EvalSnapshot {
    let games = state.game_state.games.clone();
    let risk = state.risk.clone();

    // Pre-compute committed contracts and resting status for all game tickers
    let mut committed_contracts = HashMap::new();
    let mut resting_tickers = std::collections::HashSet::new();
    for game in games.values() {
        for market in &game.kalshi_markets {
            committed_contracts.insert(
                market.ticker.clone(),
                state.order_manager.committed_contracts(&market.ticker),
            );
            if state.order_manager.has_resting_order(&market.ticker) {
                resting_tickers.insert(market.ticker.clone());
            }
        }
    }

    EvalSnapshot { games, risk, committed_contracts, resting_tickers }
}

/// Run all strategies and collect order signals.
/// Evaluates across ALL markets per game, picks the best signal per game.
///
/// Operates on an EvalSnapshot taken from BotState, so the state lock is NOT held
/// during evaluation. This allows WS fill processing to proceed concurrently.
pub fn evaluate_strategies(
    snapshot: &EvalSnapshot,
    order_books: &HashMap<String, LocalOrderBook>,
    break_log: &mut VecDeque<BreakEvalLog>,
    registry: &StrategyRegistry,
) -> Vec<OrderSignal> {
    let mut signals = Vec::new();

    if snapshot.risk.is_halted() {
        return signals;
    }

    for game in snapshot.games.values() {
        if !game.has_kalshi() {
            continue;
        }

        // Skip low-volume games
        let total_vol = game.kalshi_total_volume();
        if total_vol < registry.min_volume {
            if game.phase.is_live_or_break() {
                tracing::debug!(
                    "SKIP (volume): {} v {} | vol={} < min={}",
                    game.away_team, game.home_team, total_vol, registry.min_volume,
                );
            }
            continue;
        }

        // Skip extreme prices — check if any market has mid in tradeable range
        let has_tradeable_mid = game.kalshi_markets.iter().any(|m| {
            book_prices(order_books, &m.ticker).mid.is_some_and(|mid| {
                (registry.min_price_cents..=registry.max_price_cents).contains(&mid)
            })
        });
        if !has_tradeable_mid {
            let mids: Vec<_> = game.kalshi_markets.iter()
                .map(|m| format!("{}={:?}", m.ticker, book_prices(order_books, &m.ticker).mid))
                .collect();
            if game.phase.is_live_or_break() || game.phase == crate::espn::types::GamePhase::PreGame {
                tracing::debug!(
                    "SKIP (no tradeable mid): {} v {} | mids={:?}",
                    game.away_team, game.home_team, mids,
                );
            }
            continue;
        }

        // Check hard contract cap across all markets for this game
        let game_committed: i64 = game.kalshi_markets.iter()
            .map(|m| snapshot.committed_contracts.get(&m.ticker).copied().unwrap_or(0))
            .sum();
        if game_committed >= registry.max_contracts_per_game {
            continue;
        }
        let contracts_remaining = registry.max_contracts_per_game - game_committed;

        // Compute exposure for Kelly sizing from committed contracts across all game tickers
        let current_exposure: f64 = game.kalshi_markets.iter()
            .map(|m| {
                let contracts = snapshot.committed_contracts.get(&m.ticker).copied().unwrap_or(0);
                let avg_price = book_prices(order_books, &m.ticker).mid.map(|mid| mid / 100.0).unwrap_or(0.50);
                contracts as f64 * avg_price
            })
            .sum();

        // Detailed break logging + build market eval data for dashboard
        let is_break = game.phase.is_break();
        let mut market_evals: Vec<BreakMarketEval> = Vec::new();
        if is_break {
            let home_score = game.home_score.unwrap_or(0);
            let away_score = game.away_score.unwrap_or(0);
            for market in &game.kalshi_markets {
                let fair = game.fair_value_for_market(market);
                let prices = book_prices(order_books, &market.ticker);
                let kalshi_mid = prices.mid.map(|m| m / 100.0);
                tracing::info!(
                    "BREAK: {} v {} | {} | score {}-{} | {} YES={} bid={:?} ask={:?} mid={:?} | espn_fair={:?} | vol={:?}",
                    game.away_team, game.home_team, game.status_detail,
                    away_score, home_score,
                    market.ticker,
                    if market.is_home { "home" } else { "away" },
                    prices.bid, prices.ask, kalshi_mid,
                    fair, market.volume,
                );

                // Compute ALO price and edge for the eval log
                let (alo_price, edge_raw, edge_after_fees, side) =
                    if let (Some(fv), Some(bid), Some(ask), Some(mid)) = (fair, prices.bid, prices.ask, prices.mid) {
                        let bid_i = bid as i64;
                        let ask_i = ask as i64;
                        let mid_prob = mid / 100.0;
                        if bid_i > 0 && ask_i < 100 && bid_i < ask_i {
                            let buying_yes = fv > mid_prob;
                            let price = if buying_yes { (ask_i - 1).max(1) } else { (100 - bid_i - 1).max(1) };
                            let order_prob = price as f64 / 100.0;
                            let raw = if buying_yes { fv - order_prob } else { (1.0 - fv) - order_prob };
                            let fee = RiskManager::maker_fee(1, price) / 100.0;
                            let net = if raw > 0.0 { raw - fee } else { raw };
                            let side_str = if buying_yes { "YES" } else { "NO" };
                            (Some(price), Some(raw), Some(net), Some(side_str.to_string()))
                        } else {
                            (None, None, None, None)
                        }
                    } else {
                        (None, None, None, None)
                    };

                market_evals.push(BreakMarketEval {
                    ticker: market.ticker.clone(),
                    is_home: market.is_home,
                    bid: prices.bid,
                    ask: prices.ask,
                    mid: prices.mid,
                    fair,
                    alo_price,
                    edge_raw,
                    edge_after_fees,
                    side,
                });
            }
        }

        let mut best_signal: Option<OrderSignal> = None;

        for strategy in &registry.strategies {
            if !strategy.can_evaluate(game) {
                continue;
            }

            // Skip if any market in this game already has a resting order or in-flight
            let has_resting = game.kalshi_markets.iter().any(|m| {
                snapshot.resting_tickers.contains(&m.ticker)
            });
            if has_resting {
                if is_break {
                    tracing::info!(
                        "BREAK SKIP: {} v {} | {} blocked by resting order",
                        game.away_team, game.home_team, strategy.name(),
                    );
                }
                continue;
            }

            if let Some(mut signal) = strategy.evaluate(game, &snapshot.risk, current_exposure, order_books) {
                // Set expiration for non-CLV strategies (CLV sets its own in evaluate())
                if signal.expiration_ts.is_none() {
                    let expire_at = chrono::Utc::now().timestamp() + registry.order_ttl.as_secs() as i64;
                    signal.expiration_ts = Some(expire_at);
                }

                if best_signal.as_ref().is_none_or(|b| signal.edge_after_fees > b.edge_after_fees) {
                    best_signal = Some(signal);
                }
            }
        }

        if let Some(mut signal) = best_signal {
            signal.max_contracts = Some(contracts_remaining);
            if is_break {
                break_log.push_front(BreakEvalLog {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    away_team: game.away_team.clone(),
                    home_team: game.home_team.clone(),
                    score: format!("{}-{}", game.away_score.unwrap_or(0), game.home_score.unwrap_or(0)),
                    status: game.status_detail.clone(),
                    markets: market_evals,
                    result: format!("SIGNAL: {} {:?} {}c", signal.kalshi_ticker, signal.side, signal.price_cents),
                });
                if break_log.len() > 100 { break_log.pop_back(); }
            }
            signals.push(signal);
        } else if is_break {
            tracing::info!(
                "BREAK RESULT: {} v {} | no signal generated",
                game.away_team, game.home_team,
            );
            break_log.push_front(BreakEvalLog {
                timestamp: chrono::Utc::now().to_rfc3339(),
                away_team: game.away_team.clone(),
                home_team: game.home_team.clone(),
                score: format!("{}-{}", game.away_score.unwrap_or(0), game.home_score.unwrap_or(0)),
                status: game.status_detail.clone(),
                markets: market_evals,
                result: "NO_SIGNAL".to_string(),
            });
            if break_log.len() > 100 { break_log.pop_back(); }
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
///
/// Re-checks contract limits at execution time (under lock) to prevent TOCTOU races
/// between evaluate_strategies and the actual API call.
pub async fn execute_signal(
    signal: OrderSignal,
    state: &SharedState,
    logger: &SharedLogger,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
    live_strategies: &[String],
    max_contracts_per_game: i64,
) -> Option<PlacedOrder> {
    // Re-check risk AND contract limits under the lock right before building the order.
    // This closes the TOCTOU gap: state may have changed since evaluate_strategies ran.
    let signal = {
        let s = state.lock().await;
        if !s.risk.can_trade(signal.size_dollars) {
            tracing::warn!(
                "Risk check failed for {} signal on {}",
                signal.strategy,
                signal.kalshi_ticker
            );
            return None;
        }

        // Re-check has_resting_order (a fill may have been partially processed)
        if s.order_manager.has_resting_order(&signal.kalshi_ticker) {
            tracing::info!(
                "Skipping {} signal on {}: resting order appeared since evaluation",
                signal.strategy, signal.kalshi_ticker
            );
            return None;
        }

        // Re-compute contract cap across ALL tickers for this game (not just the signal's ticker)
        let game_tickers = s.game_state.game_tickers_for(&signal.kalshi_ticker);
        let game_committed: i64 = game_tickers.iter()
            .map(|t| s.order_manager.committed_contracts(t))
            .sum();
        let contracts_remaining = (max_contracts_per_game - game_committed).max(0);
        if contracts_remaining <= 0 {
            tracing::info!(
                "Skipping {} signal on {}: contract limit reached ({}/{})",
                signal.strategy, signal.kalshi_ticker,
                game_committed, max_contracts_per_game
            );
            return None;
        }

        // Update max_contracts with the freshest cap
        let mut signal = signal;
        signal.max_contracts = Some(contracts_remaining);
        signal
    };

    let order_req = match OrderManager::signal_to_order(&signal) {
        Some(req) => req,
        None => {
            tracing::info!(
                "Skipping {} signal on {}: contract cap is zero",
                signal.strategy, signal.kalshi_ticker
            );
            return None;
        }
    };

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
            let price_cents = order_req.yes_price.or(order_req.no_price).unwrap_or(0);
            let edge_bps = Some(signal.edge_after_fees * 10000.0);
            let action_str = format!("{:?}", order_req.action);
            let side_str = format!("{:?}", order_req.side);

            // Collect game_info under state lock, then drop before logging
            let game_info = {
                let s = state.lock().await;
                crate::engine::logger::GameInfo::from_game_state(&s.game_state, &signal.kalshi_ticker)
            };

            // Log under logger lock only (no state lock held)
            {
                let log = logger.lock().unwrap();
                let _ = log.log_order(
                    &resp.order.order_id,
                    &signal.kalshi_ticker,
                    &signal.strategy,
                    &action_str,
                    &side_str,
                    price_cents,
                    order_req.count,
                    &resp.order.status,
                    edge_bps,
                    game_info.as_ref(),
                );
            }

            // Apply state updates under state lock only (no logger lock)
            {
                let mut s = state.lock().await;

                // Track CLV orders for closing-line validation
                if signal.strategy == "clv_hunter" {
                    s.order_manager.record_clv_order(
                        &resp.order.order_id,
                        &signal.kalshi_ticker,
                        &side_str,
                        price_cents,
                    );
                }

                s.order_manager.record_placed_order(resp.order, order_req.count, &signal.strategy);
            }

            tracing::info!("Order placed successfully");

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
