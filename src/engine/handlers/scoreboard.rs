use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::bot::{SharedState, SharedOrderBooks, SharedBreakLog, SharedLogger, StrategyRegistry, BotState, fetch_and_apply_summary};
use crate::engine::executor::{build_eval_snapshot, evaluate_strategies, execute_signal};
use crate::engine::notifier::Notifier;
use crate::espn::poller::{EspnPoller, GameTracker};
use crate::kalshi::orderbook::LocalOrderBook;
use crate::kalshi::rest::KalshiRestClient;

/// Handle an ESPN scoreboard poll tick.
#[allow(clippy::too_many_arguments)]
pub async fn handle_scoreboard_tick(
    espn_poller: &EspnPoller,
    state: &SharedState,
    order_books: &SharedOrderBooks,
    break_log: &SharedBreakLog,
    logger: &SharedLogger,
    game_tracker: &mut GameTracker,
    registry: &StrategyRegistry,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
    notifier: Option<&Notifier>,
) {
    let games = match espn_poller.fetch_scoreboard().await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("ESPN scoreboard fetch failed: {:?}", e);
            return;
        }
    };

    // Detect phase transitions BEFORE updating the tracker (needs previous phases)
    let pregame_to_live = game_tracker.pregame_to_live(&games);
    let breaks_ended = game_tracker.breaks_ended(&games);
    let new_breaks = game_tracker.update(&games);
    let mut s = state.lock().await;
    s.last_espn_update = std::time::Instant::now();

    // Update game states (no volume update on polls — volume is set at startup)
    for game in &games {
        let gs = s.game_state.upsert(
            game.event_id.clone(),
            game.home_team.clone(),
            game.away_team.clone(),
        );
        gs.update_from_espn(game);
        // Update win prob from play-by-play during live/break (more current than summary)
        if game.last_play_home_win_prob.is_some() && game.game_phase.is_live_or_break() {
            gs.espn_home_win_prob = game.last_play_home_win_prob;
        }
    }

    // Log game-start transitions
    for event_id in &pregame_to_live {
        if let Some(gs) = s.game_state.get(event_id) {
            tracing::info!(
                "GAME STARTED: {} v {} | espn_hp={:?}",
                gs.away_team, gs.home_team, gs.espn_home_win_prob,
            );
        }
    }

    // CLV validation: check pre-game orders when game goes live
    {
        let books = order_books.read().await;
        validate_clv_orders(&s, &books, logger, &pregame_to_live);
    }

    // Log break detection with context
    for event_id in &new_breaks {
        if let Some(gs) = s.game_state.get(event_id) {
            tracing::info!(
                "BREAK DETECTED: {} v {} | score: {}-{} | phase={:?} | detail={:?} | last_play={:?} | espn_hp={:?}",
                gs.away_team, gs.home_team,
                gs.away_score.unwrap_or(0), gs.home_score.unwrap_or(0),
                gs.phase, gs.status_detail, gs.last_play, gs.espn_home_win_prob,
            );
        }
    }

    // Release lock before async ESPN summary fetches to avoid blocking event loop
    drop(s);

    // Fetch summary on new breaks (for updated win probs) — lock released
    for event_id in &new_breaks {
        fetch_and_apply_summary(espn_poller, state, event_id, "Break ").await;
    }

    // Cancel ALL resting orders on games transitioning from break -> live.
    // Cancels all orders (not just break_ev tagged) so orders surviving a bot restart
    // (where order_strategies is lost) are also cleaned up.
    cancel_break_orders(state, &breaks_ended, kalshi_rest, dry_run).await;

    // Cancel CLV orders that are no longer at the top of book (market moved away).
    // Runs before snapshot so the re-evaluation below can re-place at the new price.
    refresh_stale_clv_orders(state, order_books, kalshi_rest, dry_run).await;

    // Re-acquire lock for mutable pre-work, then snapshot and release before evaluation.
    // This minimizes lock hold time: WS fill processing isn't blocked during strategy evaluation.
    let snapshot = {
        let mut s = state.lock().await;
        // Prune stale in-flight entries (older than 60s) instead of blanket clearing,
        // so API calls still in progress from a previous tick keep their guard.
        s.order_manager.clear_all_in_flight();
        log_game_summary(&s);
        build_eval_snapshot(&s)
    };
    // State lock is now released — strategies evaluate on the snapshot
    let signals = {
        let books = order_books.read().await;
        let mut blog = break_log.lock().unwrap();
        evaluate_strategies(&snapshot, &books, &mut blog, registry)
    };

    let mut placed = Vec::new();
    for signal in signals {
        if let Some(order) = execute_signal(
            signal, state, order_books, logger, kalshi_rest, dry_run,
            &registry.live_strategies,
            registry.max_contracts_per_game,
        ).await {
            placed.push(order);
        }
    }

    // Send notifications for break_ev trades and any risk-reducing trades
    if let Some(n) = notifier {
        let notify_orders: Vec<_> = placed.into_iter()
            .filter(|o| o.strategy == "break_ev" || o.reduce_only)
            .collect();
        n.notify_orders_batch(&notify_orders).await;
    }
}

/// Cancel all resting break_ev orders for games where a break just ended.
///
/// Called whenever ESPN reports a phase transition from Break/Halftime -> Live.
/// Cancels ALL resting orders on these games (not just break_ev tagged) to handle
/// bot restarts where order_strategies metadata is lost.
async fn cancel_break_orders(
    state: &SharedState,
    ended_event_ids: &[String],
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
) {
    if ended_event_ids.is_empty() {
        return;
    }

    // Collect (order_id, ticker) pairs for ALL resting orders on ended games
    let to_cancel: Vec<(String, String)> = {
        let s = state.lock().await;
        ended_event_ids.iter()
            .filter_map(|event_id| s.game_state.get(event_id))
            .flat_map(|gs| {
                gs.kalshi_tickers().into_iter()
                    .flat_map(|ticker| {
                        let ticker_owned = ticker.to_string();
                        s.order_manager.order_ids_for_market(ticker)
                            .into_iter()
                            .map(move |oid| (oid, ticker_owned.clone()))
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };

    if to_cancel.is_empty() {
        return;
    }

    for (order_id, ticker) in &to_cancel {
        tracing::info!(
            "Break ended: cancelling order {} on {}",
            order_id, ticker,
        );

        if dry_run {
            continue;
        }

        match kalshi_rest.cancel_order(order_id).await {
            Ok(()) => {
                let mut s = state.lock().await;
                s.order_manager.remove_order(order_id);
                tracing::info!("Break ended: cancelled {} on {}", order_id, ticker);
            }
            Err(e) => {
                tracing::warn!(
                    "Break ended: failed to cancel {} on {}: {:?}",
                    order_id, ticker, e,
                );
            }
        }
    }
}

/// Cancel CLV orders that are no longer at the top of the order book.
///
/// When the market moves up (someone outbids our resting order), we're no longer
/// the best bid and unlikely to fill. Cancel the stale order so the strategy can
/// re-evaluate and re-place at the current top of book if edge still exists.
async fn refresh_stale_clv_orders(
    state: &SharedState,
    order_books: &SharedOrderBooks,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
) {
    // Collect stale CLV orders under locks (no async work inside)
    let stale: Vec<(String, String, i64)> = {
        let s = state.lock().await;
        let books = order_books.read().await;
        let all = s.order_manager.all_clv_orders();
        if all.is_empty() {
            return;
        }
        let mut result = Vec::new();
        for info in all {
            let best_bid = match info.side.as_str() {
                "Yes" => books.get(&info.ticker).and_then(|b| b.best_yes_bid()).map(|b| b.price),
                "No"  => books.get(&info.ticker).and_then(|b| b.best_no_bid()).map(|b| b.price),
                _     => None,
            };
            let stale = best_bid.is_some_and(|bid| bid > info.price_cents);
            tracing::info!(
                "CLV refresh: {} {} bid={}c best_{}_bid={:?} stale={}",
                info.ticker, info.side, info.price_cents, info.side, best_bid, stale,
            );
            if stale {
                result.push((info.order_id.clone(), info.ticker.clone(), info.price_cents));
            }
        }
        result
    };

    if stale.is_empty() {
        return;
    }

    for (order_id, ticker, price_cents) in &stale {
        tracing::info!(
            "CLV stale: {} on {} (our bid={}c outbid by market) — cancelling",
            order_id, ticker, price_cents,
        );

        if dry_run {
            continue;
        }

        match kalshi_rest.cancel_order(order_id).await {
            Ok(()) => {
                let mut s = state.lock().await;
                s.order_manager.remove_order(order_id);
                // Mark in-flight after cancel so the strategy cannot immediately re-place
                // on the same tick. The 60s prune window acts as a cooldown.
                s.order_manager.mark_in_flight(ticker);
                tracing::info!("CLV stale: cancelled {} on {}, will re-evaluate", order_id, ticker);
            }
            Err(e) => {
                tracing::warn!("CLV stale: failed to cancel {} on {}: {:?}", order_id, ticker, e);
            }
        }
    }
}

/// Validate CLV orders when games transition from pre-game to live.
fn validate_clv_orders(
    s: &BotState,
    order_books: &HashMap<String, LocalOrderBook>,
    logger: &SharedLogger,
    pregame_to_live: &[String],
) {
    for event_id in pregame_to_live {
        if let Some(gs) = s.game_state.get(event_id) {
            let tickers: Vec<&str> = gs.kalshi_tickers();
            let clv_orders = s.order_manager.clv_orders_for_tickers(&tickers);
            for clv_order in clv_orders {
                let closing_mid = order_books
                    .get(&clv_order.ticker)
                    .and_then(|book| book.yes_mid())
                    .map(|mid| mid as i64);

                if let Some(mid) = closing_mid {
                    let clv = if clv_order.side == "Yes" {
                        mid - clv_order.price_cents
                    } else {
                        clv_order.price_cents - mid
                    };
                    let captured = if clv > 0 { "CAPTURED" } else { "MISSED" };
                    tracing::info!(
                        "CLV check: {} order {} at {}c, closing mid {}c, CLV = {}c [{}]",
                        clv_order.ticker, clv_order.order_id,
                        clv_order.price_cents, mid, clv, captured,
                    );
                    let log = logger.lock().unwrap();
                    let _ = log.log_clv_check(
                        &clv_order.order_id,
                        &clv_order.ticker,
                        &clv_order.side,
                        clv_order.price_cents,
                        mid,
                        clv,
                    );
                } else {
                    tracing::warn!(
                        "CLV check: no closing mid for {} (order {}), skipping",
                        clv_order.ticker, clv_order.order_id,
                    );
                }
            }
        }
    }
}

/// Log a summary of current game states (single pass).
fn log_game_summary(s: &BotState) {
    use crate::espn::types::GamePhase;

    let mut live_count = 0usize;
    let mut break_count = 0usize;
    let mut pre_count = 0usize;
    let mut with_kalshi = 0usize;
    let mut with_fair = 0usize;

    for gs in s.game_state.games.values() {
        match gs.phase {
            GamePhase::Live => live_count += 1,
            GamePhase::Halftime | GamePhase::Break => break_count += 1,
            GamePhase::PreGame => pre_count += 1,
            _ => {}
        }
        if gs.has_kalshi() { with_kalshi += 1; }
        if gs.espn_home_win_prob.is_some() { with_fair += 1; }

        // Per-game detail logging
        if gs.has_kalshi() && gs.espn_home_win_prob.is_some() {
            let tickers: Vec<&str> = gs.kalshi_tickers();
            if gs.phase.is_live_or_break() {
                tracing::info!(
                    "  {} v {} | {}-{} | phase={:?} | last_play={:?} | espn_hp={:?} | kalshi={:?}",
                    gs.away_team, gs.home_team,
                    gs.away_score.unwrap_or(0), gs.home_score.unwrap_or(0),
                    gs.phase, gs.last_play,
                    gs.espn_home_win_prob, tickers,
                );
            } else {
                tracing::info!(
                    "  {} v {} | phase={:?} | espn_hp={:?} | kalshi={:?}",
                    gs.away_team, gs.home_team, gs.phase,
                    gs.espn_home_win_prob, tickers,
                );
            }
        }
    }

    tracing::info!(
        "Games: {} live, {} break, {} pre | {} w/Kalshi, {} w/fair_value | orders={} in_flight={}",
        live_count, break_count, pre_count, with_kalshi, with_fair,
        s.order_manager.open_order_count(), s.order_manager.in_flight_count(),
    );
}
