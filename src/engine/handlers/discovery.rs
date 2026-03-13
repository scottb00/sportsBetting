use std::sync::Arc;

use crate::engine::bot::{SharedState, SharedOrderBooks, SharedMapper, populate_game_states, fetch_and_apply_summary};
use crate::engine::market_prep::{
    build_espn_for_matching, build_kalshi_for_matching,
    build_kalshi_volume, filter_events_for_dates, fetch_all_kalshi_cbb_events,
    today_and_tomorrow_tags,
};
use crate::espn::poller::EspnPoller;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::websocket::KalshiWsHandle;

/// Discover new Kalshi markets that appeared after startup.
pub async fn discover_new_markets(
    kalshi_rest: &Arc<KalshiRestClient>,
    espn_poller: &EspnPoller,
    state: &SharedState,
    order_books: &SharedOrderBooks,
    mapper: &SharedMapper,
    ws_handle: Option<&KalshiWsHandle>,
) {
    let (today, _, date_tags) = today_and_tomorrow_tags();
    let mut kalshi_events = fetch_all_kalshi_cbb_events(kalshi_rest).await;
    // Build volume map BEFORE filtering so cached mapper entries from yesterday get volume.
    let kalshi_volume = build_kalshi_volume(&kalshi_events);
    filter_events_for_dates(&mut kalshi_events, &date_tags);

    let kalshi_for_matching = build_kalshi_for_matching(&kalshi_events);

    // Snapshot already-mapped tickers before acquiring the mapper write lock
    let already_mapped: std::collections::HashSet<String> = {
        let m = mapper.lock().await;
        m.all_mapped_kalshi_tickers().into_iter().collect()
    };

    let has_new = kalshi_for_matching
        .iter()
        .any(|(_, _, markets)| markets.iter().any(|(ticker, _)| !already_mapped.contains(ticker)));

    // Refresh volume for all existing markets (data already fetched)
    {
        let mut s = state.lock().await;
        for game in s.game_state.games.values_mut() {
            for market in &mut game.kalshi_markets {
                if let Some(&vol) = kalshi_volume.get(&market.ticker) {
                    market.volume = Some(vol);
                }
            }
        }
    }

    if !has_new {
        return;
    }

    tracing::info!("Discovery: found new Kalshi events to map");

    let espn_games = match espn_poller.fetch_scoreboard().await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("Discovery: ESPN fetch failed: {:?}", e);
            return;
        }
    };

    let espn_for_matching = build_espn_for_matching(&espn_games);

    // Lock mapper first, then state (consistent ordering to prevent deadlocks)
    let mut m = mapper.lock().await;

    if let Err(e) = m.resolve_deterministic(
        &espn_for_matching,
        &kalshi_for_matching,
        &[],
        &today,
    ) {
        tracing::warn!("Discovery: mapping failed: {:?}", e);
        return;
    }

    let new_tickers: Vec<String> = m.all_mapped_kalshi_tickers()
        .into_iter()
        .filter(|t| !already_mapped.contains(t))
        .collect();

    if new_tickers.is_empty() {
        return;
    }

    tracing::info!("Discovery: {} new Kalshi tickers mapped: {:?}", new_tickers.len(), new_tickers);

    {
        let mut s = state.lock().await;
        populate_game_states(&mut s, &m, &espn_games, Some(&kalshi_volume));
    }
    // Drop mapper lock before async ESPN calls
    drop(m);

    // Fetch ESPN summaries for new games (async I/O — locks released above)
    let new_event_ids: Vec<String> = {
        let s = state.lock().await;
        new_tickers.iter()
            .filter_map(|ticker| s.game_state.get_by_kalshi_ticker(ticker).map(|g| g.espn_event_id.clone()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    };
    for event_id in &new_event_ids {
        fetch_and_apply_summary(espn_poller, state, event_id, "Discovery: ").await;
    }

    // Subscribe to new tickers on the live WS connection
    if let Some(handle) = ws_handle {
        let count = handle.subscribe_additional(new_tickers.clone());
        if count > 0 {
            tracing::info!("Discovery: subscribed to {} new tickers on WS", count);
        }
    } else {
        tracing::warn!("Discovery: no WS handle — new markets won't receive book updates");
    }

    // Seed orderbooks via REST for new tickers (WS snapshot may not arrive for mid-session subs)
    for ticker in &new_tickers {
        match kalshi_rest.get_orderbook(ticker).await {
            Ok(snapshot) => {
                let mut books = order_books.write().await;
                let book = books
                    .entry(ticker.clone())
                    .or_insert_with(|| crate::kalshi::orderbook::LocalOrderBook::new(ticker.clone()));
                book.apply_snapshot(&snapshot, None);
                tracing::info!("Discovery: seeded orderbook for {} via REST", ticker);
            }
            Err(e) => {
                tracing::warn!("Discovery: failed to fetch orderbook for {}: {:?}", ticker, e);
            }
        }
    }
}
