use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::bot::{
    SharedState, StrategyRegistry, extract_book_prices, kalshi_date_tag,
    populate_game_states, BotState, fetch_all_kalshi_cbb_events,
    normalize_conference_tournament_event,
};
use crate::engine::executor::{evaluate_strategies, execute_signal};
use crate::engine::notifier::Notifier;
use crate::espn::poller::{EspnPoller, GameTracker};
use crate::espn::types::GamePhase;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::websocket::{KalshiWsEvent, KalshiWsHandle};
use crate::polymarket::client::PolymarketEvent;

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

    // Prune orders/intents that have expired since last tick
    s.order_manager.prune_expired();

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
        gs.last_updated = std::time::Instant::now();
    }

    // CLV validation: check pre-game orders when game goes live
    validate_clv_orders(&mut s, &pregame_to_live);

    // Fetch summary on new breaks (for updated win probs)
    for event_id in &new_breaks {
        match espn_poller.fetch_summary(event_id).await {
            Ok(summary) => {
                let win_prob = EspnPoller::latest_win_prob(&summary);
                let dk_ml = EspnPoller::extract_dk_moneyline(&summary).map(|(h, _)| h);
                if let Some(gs) = s.game_state.get_mut(event_id) {
                    gs.update_from_espn_summary(win_prob, dk_ml);
                    tracing::info!(
                        "Updated {} win_prob={:?}",
                        event_id, gs.espn_home_win_prob,
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
            for clv_order in &clv_orders {
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
        "Games: {} live, {} break, {} pre | {} w/Kalshi, {} w/fair_value | orders={} intents={}",
        live_count, break_count, pre_count, with_kalshi, with_fair,
        s.order_manager.open_order_count(), s.order_manager.intent_count(),
    );

    for gs in s.game_state.games.values() {
        if gs.has_kalshi() && gs.espn_home_win_prob.is_some() {
            let tickers: Vec<&str> = gs.kalshi_tickers();
            tracing::info!(
                "  {} v {} | phase={:?} | espn_hp={:?} | kalshi={:?}",
                gs.away_team, gs.home_team, gs.phase,
                gs.espn_home_win_prob, tickers,
            );
        }
    }
}

/// Handle a Kalshi WebSocket event.
pub async fn handle_kalshi_event(
    event: KalshiWsEvent,
    state: &SharedState,
    notifier: Option<&Notifier>,
) {
    let mut s = state.lock().await;
    match event {
        KalshiWsEvent::OrderBookSnapshot { market_ticker, snapshot } => {
            let book = s.order_books
                .entry(market_ticker.clone())
                .or_insert_with(|| crate::kalshi::orderbook::LocalOrderBook::new(market_ticker.clone()));
            book.apply_snapshot(&snapshot);
            let prices = extract_book_prices(book);
            if let Some(gs) = s.game_state.get_mut_by_kalshi_ticker(&market_ticker)
                && let Some(market) = gs.kalshi_market_mut(&market_ticker)
            {
                market.update_prices(prices.0, prices.1, prices.2);
            }
            tracing::debug!("Book snapshot for {}", market_ticker);
        }
        KalshiWsEvent::OrderBookDelta(delta) => {
            let ticker = delta.market_ticker.clone();
            if let Some(book) = s.order_books.get_mut(&ticker) {
                book.apply_delta(&delta);
                let prices = extract_book_prices(book);
                if let Some(gs) = s.game_state.get_mut_by_kalshi_ticker(&ticker)
                    && let Some(market) = gs.kalshi_market_mut(&ticker)
                {
                    market.update_prices(prices.0, prices.1, prices.2);
                }
            }
        }
        KalshiWsEvent::Fill(fill) => {
            tracing::info!(
                "FILL: {} {:?} {} contracts @ {} yes_price",
                fill.market_ticker, fill.action, fill.count, fill.yes_price
            );
            let price = fill.yes_price.max(fill.no_price) as f64 / 100.0;
            let exposure_change = fill.count as f64 * price;
            s.risk.record_fill(exposure_change, 0.0);
            s.order_manager.handle_fill(&fill);
            let _ = s.logger.log_fill(
                &fill.trade_id, &fill.order_id, &fill.market_ticker,
                &fill.side, &fill.action, fill.yes_price, fill.count, 0.0,
            );
            // Send fill notification (drop lock first since it's async)
            let ticker = fill.market_ticker.clone();
            let action = fill.action.clone();
            let count = fill.count;
            let yes_price = fill.yes_price;
            drop(s);
            if let Some(n) = notifier {
                n.notify_fill(&ticker, &action, count, yes_price).await;
            }
            return; // already dropped lock
        }
        KalshiWsEvent::Trade(trade) => {
            tracing::debug!(
                "Trade: {} {} contracts @ {} taker={}",
                trade.market_ticker, trade.count, trade.yes_price, trade.taker_side
            );
        }
        KalshiWsEvent::Connected => tracing::info!("Kalshi WebSocket connected"),
        KalshiWsEvent::Disconnected => tracing::warn!("Kalshi WebSocket disconnected"),
        KalshiWsEvent::Error(e) => tracing::error!("Kalshi WebSocket error: {}", e),
    }
}

/// Handle a Polymarket WebSocket event.
pub async fn handle_polymarket_event(event: PolymarketEvent, _state: &SharedState) {
    match event {
        PolymarketEvent::PriceUpdate { asset_id, best_bid, best_ask } => {
            if let (Some(_bid), Some(_ask)) = (best_bid, best_ask) {
                let _ = (asset_id, _bid, _ask);
            }
        }
        PolymarketEvent::TradeUpdate { .. } => {}
        PolymarketEvent::Connected => tracing::info!("Polymarket WebSocket connected"),
        PolymarketEvent::Disconnected => tracing::warn!("Polymarket WebSocket disconnected"),
    }
}

/// Cancel orders for finished games and clean up state.
pub async fn cleanup_finished_games(
    state: &SharedState,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
) {
    let mut s = state.lock().await;

    let finished_tickers: Vec<String> = s.game_state.games.values()
        .filter(|g| g.phase == GamePhase::Final)
        .flat_map(|g| g.kalshi_tickers().into_iter().map(|t| t.to_string()))
        .collect();

    let mut orders_to_cancel = Vec::new();
    for ticker in &finished_tickers {
        let order_ids = s.order_manager.order_ids_for_market(ticker);
        for oid in order_ids {
            orders_to_cancel.push((oid, ticker.clone()));
        }
    }

    if !orders_to_cancel.is_empty() {
        tracing::info!("Cancelling {} orders for finished games", orders_to_cancel.len());
        drop(s);

        for (order_id, ticker) in &orders_to_cancel {
            if dry_run {
                tracing::info!("DRY RUN: would cancel order {} on finished {}", order_id, ticker);
            } else {
                match kalshi_rest.cancel_order(order_id).await {
                    Ok(()) => tracing::info!("Cancelled order {} (game finished: {})", order_id, ticker),
                    Err(e) => tracing::warn!("Failed to cancel order {}: {:?}", order_id, e),
                }
            }
        }

        let mut s = state.lock().await;
        for (order_id, _) in &orders_to_cancel {
            s.order_manager.handle_cancel(order_id);
        }
        s.order_manager.clear_intents_for_tickers(&finished_tickers);
        s.game_state.cleanup_finished();
        for ticker in &finished_tickers {
            s.order_books.remove(ticker);
        }
    } else {
        let count_before = s.game_state.games.len();
        s.order_manager.clear_intents_for_tickers(&finished_tickers);
        s.game_state.cleanup_finished();
        let removed = count_before - s.game_state.games.len();
        if removed > 0 {
            tracing::info!("Cleaned up {} finished games", removed);
        }
    }
}

/// Discover new Kalshi markets that appeared after startup.
pub async fn discover_new_markets(
    kalshi_rest: &Arc<KalshiRestClient>,
    espn_poller: &EspnPoller,
    state: &SharedState,
    ws_handle: Option<&KalshiWsHandle>,
) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let date_tag = kalshi_date_tag(&today);
    let mut kalshi_events = fetch_all_kalshi_cbb_events(kalshi_rest).await;
    kalshi_events.events.retain(|e| {
        e.event_ticker.contains(&date_tag)
            || !e.event_ticker.starts_with("KXNCAAMBGAME")
    });

    #[allow(clippy::type_complexity)]
    let kalshi_for_matching: Vec<(String, String, Vec<(String, String)>)> = kalshi_events
        .events
        .iter()
        .filter_map(|e| {
            if !e.event_ticker.starts_with("KXNCAAMBGAME") {
                return normalize_conference_tournament_event(e);
            }
            let markets: Vec<(String, String)> = e.markets.as_ref()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|m| {
                    let yes_sub = m.yes_sub_title.as_ref()?;
                    Some((m.ticker.clone(), yes_sub.clone()))
                })
                .collect();
            Some((e.event_ticker.clone(), e.title.clone(), markets))
        })
        .collect();

    let empty_markets = vec![];
    let kalshi_volume: HashMap<String, i64> = kalshi_events
        .events
        .iter()
        .flat_map(|e| e.markets.as_ref().unwrap_or(&empty_markets).iter())
        .filter_map(|m| m.volume.map(|v| (m.ticker.clone(), v)))
        .collect();

    let already_mapped: Vec<String> = {
        let s = state.lock().await;
        s.market_mapper.all_mapped_kalshi_tickers()
    };

    let new_events: Vec<_> = kalshi_for_matching
        .iter()
        .filter(|(_, _, markets)| {
            markets.iter().any(|(ticker, _)| !already_mapped.contains(ticker))
        })
        .collect();

    if new_events.is_empty() {
        return;
    }

    tracing::info!("Discovery: found {} new Kalshi events to map", new_events.len());

    let espn_games = match espn_poller.fetch_scoreboard().await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("Discovery: ESPN fetch failed: {:?}", e);
            return;
        }
    };

    let espn_for_matching: Vec<(String, String)> = espn_games
        .iter()
        .map(|g| (g.event_id.clone(), format!("{} @ {}", g.away_team, g.home_team)))
        .collect();

    let mut s = state.lock().await;
    let tickers_before: std::collections::HashSet<String> =
        s.market_mapper.all_mapped_kalshi_tickers().into_iter().collect();

    if let Err(e) = s.market_mapper.resolve_deterministic(
        &espn_for_matching,
        &kalshi_for_matching,
        &[],
        &today,
    ) {
        tracing::warn!("Discovery: mapping failed: {:?}", e);
        return;
    }

    let tickers_after: std::collections::HashSet<String> =
        s.market_mapper.all_mapped_kalshi_tickers().into_iter().collect();

    let new_tickers: Vec<String> = tickers_after
        .difference(&tickers_before)
        .cloned()
        .collect();

    if new_tickers.is_empty() {
        return;
    }

    tracing::info!("Discovery: {} new Kalshi tickers mapped: {:?}", new_tickers.len(), new_tickers);

    populate_game_states(&mut s, &espn_games, Some(&kalshi_volume));
    drop(s);

    // Fetch ESPN summaries for new games
    {
        let s = state.lock().await;
        let new_event_ids: Vec<String> = new_tickers
            .iter()
            .filter_map(|ticker| {
                s.game_state.games.values()
                    .find(|g| g.kalshi_markets.iter().any(|m| m.ticker == *ticker))
                    .map(|g| g.espn_event_id.clone())
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        drop(s);

        for event_id in &new_event_ids {
            match espn_poller.fetch_summary(event_id).await {
                Ok(summary) => {
                    let win_prob = EspnPoller::latest_win_prob(&summary);
                    let dk_ml = EspnPoller::extract_dk_moneyline(&summary).map(|(h, _)| h);
                    let mut s = state.lock().await;
                    if let Some(gs) = s.game_state.get_mut(event_id) {
                        gs.update_from_espn_summary(win_prob, dk_ml);
                        tracing::info!(
                            "Discovery: {} {} v {} | espn_hp={:?}",
                            event_id, gs.away_team, gs.home_team, gs.espn_home_win_prob,
                        );
                    }
                }
                Err(e) => tracing::warn!("Discovery: summary fetch failed for {}: {:?}", event_id, e),
            }
        }
    }

    // Subscribe to new tickers on the live WS connection
    if let Some(handle) = ws_handle {
        let count = handle.subscribe_additional(new_tickers);
        if count > 0 {
            tracing::info!("Discovery: subscribed to {} new tickers on WS", count);
        }
    } else {
        tracing::warn!("Discovery: no WS handle — new markets won't receive book updates");
    }
}
