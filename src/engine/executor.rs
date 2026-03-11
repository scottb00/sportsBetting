use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::engine::bot::{BreakEvalLog, BreakMarketEval, SharedState, SharedLogger, StrategyRegistry};
use crate::engine::game_state::GameState;
use crate::engine::market_prep::book_prices;
use crate::engine::order_manager::{OrderManager, OrderSignal};
use crate::engine::risk::RiskManager;
use crate::kalshi::orderbook::LocalOrderBook;
use crate::kalshi::rest::KalshiRestClient;
use crate::strategies::common::compute_edge_and_alo;

/// Snapshot of state needed for strategy evaluation (taken under lock, evaluated without lock).
pub struct EvalSnapshot {
    pub games: HashMap<String, GameState>,
    pub risk: RiskManager,
    /// Set of tickers that have resting orders or in-flight API calls.
    pub resting_tickers: std::collections::HashSet<String>,
    /// Pre-computed resting contract counts per ticker.
    pub resting_contracts: HashMap<String, i64>,
}

/// Build an EvalSnapshot from the current BotState (call while holding the lock).
pub fn build_eval_snapshot(
    state: &crate::engine::bot::BotState,
) -> EvalSnapshot {
    let games = state.game_state.games.clone();
    let risk = state.risk.clone();

    // Pre-compute resting status and resting contract counts for all game tickers
    let mut resting_tickers = std::collections::HashSet::new();
    let mut resting_contracts = HashMap::new();
    for game in games.values() {
        for market in &game.kalshi_markets {
            let ticker = &market.ticker;
            if state.order_manager.has_resting_order(ticker) {
                resting_tickers.insert(ticker.clone());
            }
            let resting = state.order_manager.resting_contracts_for_tickers(&[ticker.as_str()]);
            if resting > 0 {
                resting_contracts.insert(ticker.clone(), resting);
            }
        }
    }

    EvalSnapshot { games, risk, resting_tickers, resting_contracts }
}

/// Availability of contracts for a game: how many can be added (regular) and how many
/// can be used to reduce existing exposure (reduce_cap).
struct ContractAvailability {
    /// Contracts available for new (risk-adding) orders. Zero if at cap.
    regular_remaining: i64,
    /// Max contracts for a reduce order: the absolute signed net game risk.
    reduce_cap: i64,
}

/// Check whether a game should be skipped for strategy evaluation.
/// Returns `Some(ContractAvailability)` if the game is eligible, `None` if it should be skipped.
fn game_contracts_remaining(
    game: &GameState,
    snapshot: &EvalSnapshot,
    order_books: &HashMap<String, LocalOrderBook>,
    registry: &StrategyRegistry,
) -> Option<ContractAvailability> {
    if !game.has_kalshi() {
        return None;
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
        return None;
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
        return None;
    }

    // Compute signed net game risk (home-team aligned) and absolute committed contracts.
    let net_game_risk = snapshot.risk.net_game_home_risk(&game.kalshi_markets);
    let reduce_cap = net_game_risk.unsigned_abs() as i64;

    let game_committed: i64 = game.kalshi_markets.iter()
        .map(|m| {
            let position = snapshot.risk.net_position(&m.ticker).unsigned_abs() as i64;
            let resting = snapshot.resting_contracts.get(&m.ticker).copied().unwrap_or(0);
            position + resting
        })
        .sum();
    let regular_remaining = (registry.max_contracts_per_game - game_committed).max(0);

    // Skip only when there's nothing to trade (no cap space AND nothing to reduce).
    if regular_remaining == 0 && reduce_cap == 0 {
        return None;
    }

    Some(ContractAvailability { regular_remaining, reduce_cap })
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

    for game in snapshot.games.values() {
        let avail = match game_contracts_remaining(game, snapshot, order_books, registry) {
            Some(a) => a,
            None => continue,
        };

        // Compute exposure for Kelly sizing from position + resting orders
        let current_exposure: f64 = game.kalshi_markets.iter()
            .map(|m| {
                let position = snapshot.risk.net_position(&m.ticker).unsigned_abs() as i64;
                let resting = snapshot.resting_contracts.get(&m.ticker).copied().unwrap_or(0);
                let contracts = position + resting;
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
                    if let (Some(fv), Some(bid), Some(ask)) = (fair, prices.bid, prices.ask) {
                        match compute_edge_and_alo(bid as i64, ask as i64, fv) {
                            Some(r) => (
                                Some(r.alo_price),
                                Some(r.edge_raw),
                                Some(r.edge_after_fees),
                                Some(if r.buying_yes { "YES" } else { "NO" }.to_string()),
                            ),
                            None => (None, None, None, None),
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

            if let Some(mut signal) = strategy.evaluate(game, &snapshot.risk, current_exposure, order_books) {
                // Determine if this signal reduces existing game-level exposure.
                let is_reduce = snapshot.risk.is_reduce_order(
                    &game.kalshi_markets, &signal.kalshi_ticker, &signal.side,
                );

                // Resting-order check:
                // - Regular orders: blocked if ANY market in the game has a resting order.
                // - Reduce orders: only blocked if the SAME ticker already has a resting order
                //   (a resting order on the other team's market doesn't conflict).
                let blocked_by_resting = if is_reduce {
                    snapshot.resting_tickers.contains(&signal.kalshi_ticker)
                } else {
                    game.kalshi_markets.iter().any(|m| snapshot.resting_tickers.contains(&m.ticker))
                };
                if blocked_by_resting {
                    if is_break {
                        tracing::info!(
                            "BREAK SKIP: {} v {} | {} blocked by resting order (reduce={})",
                            game.away_team, game.home_team, strategy.name(), is_reduce,
                        );
                    }
                    continue;
                }

                // Apply contract cap: reduce orders use reduce_cap, regular use regular_remaining.
                let max_contracts = if is_reduce { avail.reduce_cap } else { avail.regular_remaining };
                if max_contracts == 0 {
                    continue;
                }

                // Set expiration for non-CLV strategies (CLV sets its own in evaluate())
                if signal.expiration_ts.is_none() {
                    let expire_at = chrono::Utc::now().timestamp() + registry.order_ttl.as_secs() as i64;
                    signal.expiration_ts = Some(expire_at);
                }

                signal.max_contracts = Some(max_contracts);

                if best_signal.as_ref().is_none_or(|b| signal.edge_after_fees > b.edge_after_fees) {
                    best_signal = Some(signal);
                }
            }
        }

        if let Some(signal) = best_signal {
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
    pub count: i64,
    pub price_cents: i64,
    pub size_dollars: f64,
    pub edge_after_fees: f64,
    /// Human-readable game label, e.g. "Duke at UNC"
    pub game_label: String,
    /// Score at time of order, e.g. "42-38 (Away-Home)"
    pub score: String,
    /// Game clock/status, e.g. "Halftime" or "2nd Half, 8:23"
    pub clock: String,
    /// True if this order reduces existing game-level risk exposure.
    pub reduce_only: bool,
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
    let (signal, is_reduce) = {
        let s = state.lock().await;

        // Get game markets for reduce detection and cap computation.
        let game_markets = s.game_state.game_markets_for(&signal.kalshi_ticker);
        let is_reduce = s.risk.is_reduce_order(&game_markets, &signal.kalshi_ticker, &signal.side);

        // Re-check resting order (a fill may have been partially processed).
        // For reduce orders, only block if the same ticker has a resting order (same-side conflict).
        // For regular orders, block on any same-ticker resting order (same behaviour as before).
        if s.order_manager.has_resting_order(&signal.kalshi_ticker) {
            tracing::info!(
                "Skipping {} signal on {}: resting order appeared since evaluation",
                signal.strategy, signal.kalshi_ticker
            );
            return None;
        }

        // Re-compute contract cap.
        let net_game_risk = s.risk.net_game_home_risk(&game_markets);
        let reduce_cap = net_game_risk.unsigned_abs() as i64;
        let game_committed: i64 = game_markets.iter()
            .map(|m| {
                let position = s.risk.net_position(&m.ticker).unsigned_abs() as i64;
                let resting = s.order_manager.resting_contracts_for_tickers(&[m.ticker.as_str()]);
                position + resting
            })
            .sum();
        let regular_remaining = (max_contracts_per_game - game_committed).max(0);

        let contracts_remaining = if is_reduce { reduce_cap } else { regular_remaining };
        if contracts_remaining <= 0 {
            tracing::info!(
                "Skipping {} signal on {}: contract limit reached (reduce={}, reduce_cap={}, regular_remaining={})",
                signal.strategy, signal.kalshi_ticker, is_reduce, reduce_cap, regular_remaining
            );
            return None;
        }

        // Update max_contracts with the freshest cap
        let mut signal = signal;
        signal.max_contracts = Some(contracts_remaining);
        (signal, is_reduce)
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

            // Collect game_info and display fields under state lock, then drop before logging
            let (game_info, game_label, score, clock) = {
                let s = state.lock().await;
                let gi = crate::engine::logger::GameInfo::from_game_state(&s.game_state, &signal.kalshi_ticker);
                let (label, sc, cl) = if let Some(game) = s.game_state.get_by_kalshi_ticker(&signal.kalshi_ticker) {
                    let score = if game.phase.is_live_or_break() {
                        format!("{}-{}", game.away_score.unwrap_or(0), game.home_score.unwrap_or(0))
                    } else {
                        String::new()
                    };
                    (
                        format!("{} at {}", game.away_team, game.home_team),
                        score,
                        game.status_detail.clone(),
                    )
                } else {
                    (signal.kalshi_ticker.clone(), String::new(), String::new())
                };
                (gi, label, sc, cl)
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
                count: order_req.count,
                price_cents,
                size_dollars: signal.size_dollars,
                edge_after_fees: signal.edge_after_fees,
                game_label,
                score,
                clock,
                reduce_only: is_reduce,
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
