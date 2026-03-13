use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::engine::bot::{BreakEvalLog, BreakMarketEval, SharedOrderBooks, SharedState, SharedLogger, StrategyRegistry};
use crate::engine::game_state::GameState;
use crate::engine::market_prep::{book_prices, extract_book_prices};
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
            None => {
                // Log skipped break games so dashboard shows why they were filtered
                if game.phase.is_break() && game.has_kalshi() {
                    let reason = if game.kalshi_total_volume() < registry.min_volume {
                        format!("low volume ({})", game.kalshi_total_volume())
                    } else {
                        "at contract cap or no tradeable mid".to_string()
                    };
                    break_log.push_front(BreakEvalLog {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        away_team: game.away_team.clone(),
                        home_team: game.home_team.clone(),
                        score: format!("{}-{}", game.away_score.unwrap_or(0), game.home_score.unwrap_or(0)),
                        status: game.status_detail.clone(),
                        period: game.period,
                        display_clock: game.display_clock.clone(),
                        markets: Vec::new(),
                        result: format!("NO_SIGNAL: {}", reason),
                    });
                    if break_log.len() > 100 { break_log.pop_back(); }
                }
                continue;
            }
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

                // Determine skip reason for this market
                let skip_reason = if fair.is_none() {
                    Some("no fair value".to_string())
                } else if prices.bid.is_none() || prices.ask.is_none() {
                    Some("no book".to_string())
                } else if edge_raw.is_none() {
                    Some("no raw edge (bad book or fair=mid)".to_string())
                } else if edge_after_fees.is_some_and(|e| e < 0.0) {
                    Some(format!("fees exceed edge ({:.1}% raw)", edge_raw.unwrap_or(0.0) * 100.0))
                } else {
                    None // has edge — may still be filtered by min_edge or position logic
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
                    skip_reason,
                });
            }
        }

        let mut best_signal: Option<OrderSignal> = None;
        // Track why signals were blocked (for break logging)
        let mut blocked_reason: Option<String> = None;

        for strategy in &registry.strategies {
            if !strategy.can_evaluate(game) {
                if is_break {
                    if !game.is_tradeable_break() {
                        blocked_reason = Some("not tradeable break (team timeout?)".to_string());
                    } else if game.is_final_minutes(5.0) {
                        blocked_reason = Some("final 5 minutes — skipped".to_string());
                    }
                }
                continue;
            }

            if let Some(mut signal) = strategy.evaluate(game, &snapshot.risk, current_exposure, order_books) {
                // Skip break_ev ADD orders when not enough time remains in break
                if signal.strategy == "break_ev" && !signal.is_close && !game.break_has_time_for_order(30) {
                    tracing::info!(
                        "BREAK SKIP: {} v {} | break_ev ADD skipped — insufficient time remaining in break",
                        game.away_team, game.home_team,
                    );
                    blocked_reason = Some("insufficient break time (<30s remaining)".to_string());
                    continue;
                }

                // Determine if this signal reduces existing game-level exposure.
                let is_reduce = snapshot.risk.is_reduce_order(
                    &game.kalshi_markets, &signal.kalshi_ticker, &signal.side,
                );

                // Resting-order check: block if the SAME ticker already has a resting order.
                // The per-game contract cap (position + resting across all tickers) prevents
                // over-exposure, so we don't need to block across different tickers.
                let blocked_by_resting = snapshot.resting_tickers.contains(&signal.kalshi_ticker);
                if blocked_by_resting {
                    if is_break {
                        tracing::info!(
                            "BREAK SKIP: {} v {} | {} blocked by resting order (reduce={})",
                            game.away_team, game.home_team, strategy.name(), is_reduce,
                        );
                        blocked_reason = Some(format!("blocked by resting order on {}", signal.kalshi_ticker));
                    }
                    continue;
                }

                // Apply contract cap: reduce orders use reduce_cap, regular use regular_remaining.
                let max_contracts = if is_reduce { avail.reduce_cap } else { avail.regular_remaining };
                if max_contracts == 0 {
                    if is_break {
                        blocked_reason = Some("at contract cap".to_string());
                    }
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
                    period: game.period,
                    display_clock: game.display_clock.clone(),
                    markets: market_evals,
                    result: format!("SIGNAL: {} {:?} {}c", signal.kalshi_ticker, signal.side, signal.price_cents),
                });
                if break_log.len() > 100 { break_log.pop_back(); }
            }
            signals.push(signal);
        } else if is_break {
            // Determine the most informative reason for no signal
            let no_signal_reason = if let Some(reason) = blocked_reason {
                reason
            } else if market_evals.iter().all(|m| m.fair.is_none()) {
                "no fair value from ESPN".to_string()
            } else if market_evals.iter().all(|m| m.bid.is_none() || m.ask.is_none()) {
                "no order book data".to_string()
            } else if market_evals.iter().all(|m| m.edge_after_fees.is_none() || m.edge_after_fees.unwrap_or(0.0) <= 0.0) {
                // Find the best edge across markets for context
                let best_edge = market_evals.iter()
                    .filter_map(|m| m.edge_after_fees)
                    .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .unwrap_or(0.0);
                let best_raw = market_evals.iter()
                    .filter_map(|m| m.edge_raw)
                    .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .unwrap_or(0.0);
                if best_raw > 0.0 {
                    format!("fees exceed edge (best raw {:.1}%, after fees {:.1}%)", best_raw * 100.0, best_edge * 100.0)
                } else {
                    "no edge (fair ≈ market price)".to_string()
                }
            } else {
                // Had positive edge-after-fees but still no signal — likely below min_edge or position already at target
                let best_net = market_evals.iter()
                    .filter_map(|m| m.edge_after_fees)
                    .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    .unwrap_or(0.0);
                format!("edge below threshold or position at target (best {:.1}%)", best_net * 100.0)
            };

            tracing::info!(
                "BREAK RESULT: {} v {} | no signal: {}",
                game.away_team, game.home_team, no_signal_reason,
            );
            break_log.push_front(BreakEvalLog {
                timestamp: chrono::Utc::now().to_rfc3339(),
                away_team: game.away_team.clone(),
                home_team: game.home_team.clone(),
                score: format!("{}-{}", game.away_score.unwrap_or(0), game.home_score.unwrap_or(0)),
                status: game.status_detail.clone(),
                period: game.period,
                display_clock: game.display_clock.clone(),
                markets: market_evals,
                result: format!("NO_SIGNAL: {}", no_signal_reason),
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
    /// Score at time of order, e.g. "42-38" (Away-Home)
    pub score: String,
    /// Game clock/status, e.g. "Halftime" or "2nd Half, 8:23"
    pub clock: String,
    /// True if this order reduces existing game-level risk exposure.
    pub reduce_only: bool,
    /// Team whose win resolves YES on this ticker (e.g. "Duke")
    pub yes_team: String,
    /// Signed YES-position before the order (+= YES contracts, -= NO contracts)
    pub current_pos: i64,
    /// Signed YES-position after the order (if filled)
    pub target_pos: i64,
}

/// Execute an order signal via Kalshi REST API (or log if dry_run).
/// Returns placement info if the order was successfully placed (for batch notification).
///
/// Re-checks contract limits at execution time (under lock) to prevent TOCTOU races
/// between evaluate_strategies and the actual API call.
///
/// For break_ev orders, cross-checks the WS-derived orderbook against a fresh REST fetch
/// to detect stale books and reprice if needed.
pub async fn execute_signal(
    signal: OrderSignal,
    state: &SharedState,
    order_books: &SharedOrderBooks,
    logger: &SharedLogger,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
    live_strategies: &[String],
    max_contracts_per_game: i64,
) -> Option<PlacedOrder> {
    // Re-check risk AND contract limits under the lock right before building the order.
    // This closes the TOCTOU gap: state may have changed since evaluate_strategies ran.
    let (mut signal, is_reduce) = {
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

    let mut order_req = match OrderManager::signal_to_order(&signal) {
        Some(req) => req,
        None => {
            tracing::info!(
                "Skipping {} signal on {}: contract cap is zero",
                signal.strategy, signal.kalshi_ticker
            );
            return None;
        }
    };

    // Sanity checks — block orders that should never be placed
    let price = order_req.yes_price.or(order_req.no_price).unwrap_or(0);
    if !(1..=99).contains(&price) {
        tracing::warn!(
            "ORDER BLOCKED: {} {} price={}c outside [1,99]",
            signal.kalshi_ticker, signal.strategy, price,
        );
        return None;
    }
    if signal.edge_after_fees < 0.0 {
        tracing::warn!(
            "ORDER BLOCKED: {} {} negative edge ({:.4}) — refusing to place",
            signal.kalshi_ticker, signal.strategy, signal.edge_after_fees,
        );
        return None;
    }

    // REST orderbook cross-check for break_ev orders: fetch fresh book via REST API
    // and compare to the WS-derived book used during evaluation. If the book has drifted
    // significantly, re-compute edge with the fresh data and update the order price.
    if signal.strategy == "break_ev" {
        match kalshi_rest.get_orderbook(&signal.kalshi_ticker).await {
            Ok(rest_snapshot) => {
                let mut rest_book = LocalOrderBook::new(signal.kalshi_ticker.clone());
                rest_book.apply_snapshot(&rest_snapshot, None);
                let rest_prices = extract_book_prices(&rest_book);

                let ws_price = order_req.yes_price.or(order_req.no_price).unwrap_or(0);

                if let (Some(rest_bid), Some(rest_ask)) = (rest_prices.bid, rest_prices.ask) {
                    let rest_bid_i = rest_bid as i64;
                    let rest_ask_i = rest_ask as i64;

                    // Compute what the ALO price would be from the REST book
                    let rest_alo = if matches!(signal.side, crate::kalshi::types::OrderSide::Yes) {
                        (rest_ask_i - 1).max(1)
                    } else {
                        ((100 - rest_bid_i) - 1).max(1)
                    };

                    let drift = (rest_alo - ws_price).abs();
                    if drift > 0 {
                        tracing::info!(
                            "Book drift: {} WS_alo={}c REST_alo={}c drift={}c (REST bid/ask={}/{})",
                            signal.kalshi_ticker, ws_price, rest_alo, drift,
                            rest_bid_i, rest_ask_i,
                        );
                    }

                    if drift >= 3 {
                        // Significant drift — re-evaluate edge with fresh REST book
                        if let Some(fair_cents) = signal.fair_value_cents {
                            let fair = fair_cents as f64 / 100.0;
                            match compute_edge_and_alo(rest_bid_i, rest_ask_i, fair) {
                                Some(result) if result.edge_after_fees > 0.0 => {
                                    tracing::info!(
                                        "Book drift: {} repricing from {}c to {}c (REST), edge {:.4} -> {:.4}",
                                        signal.kalshi_ticker, ws_price, result.alo_price,
                                        signal.edge_after_fees, result.edge_after_fees,
                                    );
                                    signal.price_cents = result.alo_price;
                                    signal.edge_after_fees = result.edge_after_fees;
                                    match OrderManager::signal_to_order(&signal) {
                                        Some(new_req) => { order_req = new_req; }
                                        None => {
                                            tracing::info!(
                                                "Book drift: {} repriced order has zero contracts — skipping",
                                                signal.kalshi_ticker,
                                            );
                                            return None;
                                        }
                                    }
                                }
                                _ => {
                                    tracing::info!(
                                        "Book drift: {} no edge at REST prices (bid={} ask={} fair={:.4}) — skipping order",
                                        signal.kalshi_ticker, rest_bid_i, rest_ask_i, fair_cents as f64 / 100.0,
                                    );
                                    let mut s = state.lock().await;
                                    s.order_manager.mark_in_flight(&signal.kalshi_ticker);
                                    return None;
                                }
                            }
                        } else {
                            tracing::warn!(
                                "Book drift: {} drift={}c but no fair_value_cents — skipping REST reprice",
                                signal.kalshi_ticker, drift,
                            );
                        }
                    }
                }

                // Update SharedOrderBooks with the fresh REST snapshot
                {
                    let mut books = order_books.write().await;
                    let book = books
                        .entry(signal.kalshi_ticker.clone())
                        .or_insert_with(|| LocalOrderBook::new(signal.kalshi_ticker.clone()));
                    book.apply_snapshot(&rest_snapshot, None);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Book drift check failed for {}: {:?} — proceeding with WS book",
                    signal.kalshi_ticker, e,
                );
            }
        }
    }

    // Strategy is live only if it's in the configured live_strategies list.
    // Close orders (risk-reducing) always bypass the live check — they don't need
    // to be explicitly enabled since they only unwind existing exposure.
    let strategy_is_live = signal.is_close || live_strategies.iter().any(|s| s == &signal.strategy);
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

            // Collect game_info, sportsbook odds, and display fields under state lock, then drop before logging
            let (game_info, sportsbook_odds_json, game_label, score, clock, yes_team, current_pos, target_pos) = {
                let s = state.lock().await;
                // Snapshot fresh sportsbook odds at order time
                let sb_json = s.game_state.get_by_kalshi_ticker(&signal.kalshi_ticker)
                    .and_then(|game| game.sportsbook_spread.as_ref().map(|spread| {
                        spread.fresh_books_json(game.break_started_at)
                    }));
                let gi = crate::engine::logger::GameInfo::from_game_state(&s.game_state, &signal.kalshi_ticker);
                let current = s.risk.net_position(&signal.kalshi_ticker);
                let signed_delta = if matches!(signal.side, crate::kalshi::types::OrderSide::Yes) {
                    order_req.count
                } else {
                    -order_req.count
                };
                let target = current + signed_delta;
                let (label, sc, cl, yes_t) = if let Some(game) = s.game_state.get_by_kalshi_ticker(&signal.kalshi_ticker) {
                    let score = if game.phase.is_live_or_break() {
                        format!("{}-{}", game.away_score.unwrap_or(0), game.home_score.unwrap_or(0))
                    } else {
                        String::new()
                    };
                    let cl = if game.phase == crate::espn::types::GamePhase::PreGame {
                        // For pre-game, show scheduled start time instead of generic "Scheduled"
                        if let Some(ts) = game.start_time_ts {
                            let eastern = chrono::FixedOffset::west_opt(5 * 3600).unwrap();
                            chrono::DateTime::from_timestamp(ts, 0)
                                .map(|dt| dt.with_timezone(&eastern).format("Starts %I:%M %p ET").to_string())
                                .unwrap_or_else(|| game.status_detail.clone())
                        } else {
                            game.status_detail.clone()
                        }
                    } else if game.phase.is_live_or_break() {
                        // For live/break, build clock from period + display_clock for richer detail
                        let period_label = match game.period {
                            Some(1) => "1st Half",
                            Some(2) => "2nd Half",
                            Some(p) if p > 2 => "OT",
                            _ => "",
                        };
                        match (period_label.is_empty(), &game.display_clock) {
                            (false, Some(dc)) => format!("{}, {}", period_label, dc),
                            (false, None) => period_label.to_string(),
                            (true, Some(dc)) => dc.clone(),
                            _ => game.status_detail.clone(),
                        }
                    } else {
                        game.status_detail.clone()
                    };
                    // Determine which team is YES on this ticker
                    let yes_t = if let Some(m) = game.kalshi_markets.iter().find(|m| m.ticker == signal.kalshi_ticker) {
                        if m.is_home { game.home_team.clone() } else { game.away_team.clone() }
                    } else {
                        String::new()
                    };
                    (
                        format!("{} at {}", game.away_team, game.home_team),
                        score,
                        cl,
                        yes_t,
                    )
                } else {
                    (signal.kalshi_ticker.clone(), String::new(), String::new(), String::new())
                };
                (gi, sb_json, label, sc, cl, yes_t, current, target)
            };

            // Log under logger lock only (no state lock held)
            {
                let log = logger.lock().unwrap();
                if let Err(e) = log.log_order(
                    &resp.order.order_id,
                    &signal.kalshi_ticker,
                    &signal.strategy,
                    &action_str,
                    &side_str,
                    price_cents,
                    order_req.count,
                    &resp.order.status,
                    edge_bps,
                    signal.fair_value_cents,
                    signal.is_close,
                    game_info.as_ref(),
                    sportsbook_odds_json.as_deref(),
                ) {
                    tracing::warn!("Failed to log order {}: {:?}", resp.order.order_id, e);
                }
            }

            // Apply state updates under state lock only (no logger lock)
            {
                let mut s = state.lock().await;

                // Track CLV orders for closing-line validation
                if signal.strategy == "pregame" {
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
                yes_team,
                current_pos,
                target_pos,
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
