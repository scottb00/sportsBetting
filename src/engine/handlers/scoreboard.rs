use std::sync::Arc;

use crate::engine::bot::{SharedState, SharedLogger, StrategyRegistry, BotState, fetch_and_apply_summary};
use crate::engine::executor::{evaluate_strategies, execute_signal};
use crate::engine::notifier::Notifier;
use crate::espn::poller::{EspnPoller, GameTracker};
use crate::kalshi::rest::KalshiRestClient;

/// Handle an ESPN scoreboard poll tick.
pub async fn handle_scoreboard_tick(
    espn_poller: &EspnPoller,
    state: &SharedState,
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
    let new_breaks = game_tracker.update(&games);
    let mut s = state.lock().await;

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
    validate_clv_orders(&s, logger, &pregame_to_live);

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

    // Re-acquire lock for strategy evaluation and logging
    let mut s = state.lock().await;
    // Prune stale in-flight entries (older than 60s) instead of blanket clearing,
    // so API calls still in progress from a previous tick keep their guard.
    s.order_manager.prune_stale_in_flight(std::time::Duration::from_secs(60));
    log_game_summary(&s);

    let signals = evaluate_strategies(&mut s, registry);
    drop(s);

    let mut placed = Vec::new();
    for signal in signals {
        if let Some(order) = execute_signal(
            signal, state, logger, kalshi_rest, dry_run,
            &registry.live_strategies,
            registry.max_contracts_per_game,
        ).await {
            placed.push(order);
        }
    }

    // Send notifications for break_ev orders only
    if let Some(n) = notifier {
        let break_ev_orders: Vec<_> = placed.into_iter()
            .filter(|o| o.strategy == "break_ev")
            .collect();
        n.notify_orders_batch(&break_ev_orders).await;
    }
}

/// Validate CLV orders when games transition from pre-game to live.
fn validate_clv_orders(s: &BotState, logger: &SharedLogger, pregame_to_live: &[String]) {
    for event_id in pregame_to_live {
        if let Some(gs) = s.game_state.get(event_id) {
            let tickers: Vec<&str> = gs.kalshi_tickers();
            let clv_orders = s.order_manager.clv_orders_for_tickers(&tickers);
            for clv_order in clv_orders {
                let closing_mid = s.order_books
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
