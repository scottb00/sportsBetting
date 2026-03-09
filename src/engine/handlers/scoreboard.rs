use std::sync::Arc;

use crate::engine::bot::{SharedState, StrategyRegistry, BotState};
use crate::engine::executor::{evaluate_strategies, execute_signal};
use crate::engine::notifier::Notifier;
use crate::espn::poller::{EspnPoller, GameTracker};
use crate::kalshi::rest::KalshiRestClient;

/// Handle an ESPN scoreboard poll tick.
pub async fn handle_scoreboard_tick(
    espn_poller: &EspnPoller,
    state: &SharedState,
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
    let _breaks_ended = game_tracker.breaks_ended(&games);
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
        gs.phase = game.game_phase.clone();
        gs.home_score = game.home_score;
        gs.away_score = game.away_score;
        gs.status_detail = game.status_detail.clone();
        gs.last_play = game.last_play.clone();
        gs.last_play_type = game.last_play_type.clone();
        // Update win prob from play-by-play during live/break (more current than summary)
        if game.last_play_home_win_prob.is_some() && game.game_phase.is_live_or_break() {
            gs.espn_home_win_prob = game.last_play_home_win_prob;
        }
        gs.last_updated = std::time::Instant::now();
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
    validate_clv_orders(&mut s, &pregame_to_live);

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

    // Fetch summary on new breaks (for updated win probs)
    for event_id in &new_breaks {
        match espn_poller.fetch_summary(event_id).await {
            Ok(summary) => {
                let win_prob = EspnPoller::latest_win_prob(&summary);
                let dk_ml = EspnPoller::extract_dk_moneyline(&summary).map(|(h, _)| h);
                if let Some(gs) = s.game_state.get_mut(event_id) {
                    let old_prob = gs.espn_home_win_prob;
                    gs.update_from_espn_summary(win_prob, dk_ml);
                    tracing::info!(
                        "Break summary: {} | win_prob {:?} -> {:?}",
                        event_id, old_prob, gs.espn_home_win_prob,
                    );
                }
            }
            Err(e) => tracing::warn!("Failed to fetch summary for {}: {:?}", event_id, e),
        }
    }

    log_game_summary(&s);

    let signals = evaluate_strategies(&s, registry);
    drop(s);

    for signal in signals {
        execute_signal(
            signal, state, kalshi_rest, dry_run,
            &registry.live_strategies, notifier,
        ).await;
    }
}

/// Validate CLV orders when games transition from pre-game to live.
fn validate_clv_orders(s: &mut BotState, pregame_to_live: &[String]) {
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
                    let _ = s.logger.log_clv_check(
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

/// Log a summary of current game states.
fn log_game_summary(s: &BotState) {
    let live_count = s.game_state.live_games().len();
    let break_count = s.game_state.games_on_break().len();
    let pre_count = s.game_state.pre_game_games().len();
    let with_kalshi = s.game_state.games.values().filter(|g| g.has_kalshi()).count();
    let with_fair = s.game_state.games.values().filter(|g| g.espn_home_win_prob.is_some()).count();
    tracing::info!(
        "Games: {} live, {} break, {} pre | {} w/Kalshi, {} w/fair_value | orders={} in_flight={}",
        live_count, break_count, pre_count, with_kalshi, with_fair,
        s.order_manager.open_order_count(), s.order_manager.in_flight_count(),
    );

    for gs in s.game_state.games.values() {
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
}
