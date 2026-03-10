use std::sync::Arc;

use crate::engine::bot::{SharedState, populate_game_states, fetch_and_apply_summary};
use crate::engine::market_prep::{
    kalshi_date_tag, build_espn_for_matching, build_kalshi_for_matching,
    build_kalshi_volume, filter_events_for_dates, fetch_all_kalshi_cbb_events,
};
use crate::espn::poller::EspnPoller;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::websocket::KalshiWsHandle;

/// Discover new Kalshi markets that appeared after startup.
pub async fn discover_new_markets(
    kalshi_rest: &Arc<KalshiRestClient>,
    espn_poller: &EspnPoller,
    state: &SharedState,
    ws_handle: Option<&KalshiWsHandle>,
) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tomorrow = (chrono::Local::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let date_tags = vec![kalshi_date_tag(&today), kalshi_date_tag(&tomorrow)];
    let mut kalshi_events = fetch_all_kalshi_cbb_events(kalshi_rest).await;
    filter_events_for_dates(&mut kalshi_events, &date_tags);

    let kalshi_for_matching = build_kalshi_for_matching(&kalshi_events);
    let kalshi_volume = build_kalshi_volume(&kalshi_events);

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

    let espn_for_matching = build_espn_for_matching(&espn_games);

    // Lock for mapping — drop before async ESPN summary fetches
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

    // Fetch ESPN summaries for new games (async I/O — lock released above)
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
            fetch_and_apply_summary(espn_poller, state, event_id, "Discovery: ").await;
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
