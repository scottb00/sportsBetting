mod cleanup;
mod discovery;
mod fill_sync;
mod kalshi_ws;
mod order_sync;
mod polymarket_ws;
mod position_sync;
mod scoreboard;

pub use cleanup::{cleanup_finished_games, settle_unsettled_fills};
pub use discovery::discover_new_markets;
pub use fill_sync::sync_fills;
pub use kalshi_ws::handle_kalshi_event;
pub use order_sync::sync_orders;
pub use polymarket_ws::handle_polymarket_event;
pub use position_sync::reconcile_positions;
pub use scoreboard::handle_scoreboard_tick;

use std::sync::Arc;
use crate::engine::bot::{SharedState, SharedOrderBooks, SharedMapper, SharedLogger, fetch_and_apply_summary};
use crate::engine::notifier::Notifier;
use crate::espn::poller::EspnPoller;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::websocket::KalshiWsHandle;
use crate::sportsbooks::odds_api::OddsApiClient;

/// Combined maintenance tick: cleanup finished games, discover new markets,
/// then sync orders, fills, and reconcile positions from Kalshi REST.
pub async fn handle_maintenance_tick(
    state: &SharedState,
    order_books: &SharedOrderBooks,
    logger: &SharedLogger,
    kalshi_rest: &Arc<KalshiRestClient>,
    espn_poller: &EspnPoller,
    mapper: &SharedMapper,
    ws_handle: Option<&KalshiWsHandle>,
    notifier: Option<&Notifier>,
    odds_api_client: Option<&OddsApiClient>,
) {
    cleanup_finished_games(state, order_books, logger, kalshi_rest).await;
    discover_new_markets(kalshi_rest, espn_poller, state, order_books, mapper, ws_handle).await;
    refresh_missing_espn_probs(espn_poller, state).await;
    if let Some(odds_api) = odds_api_client {
        refresh_odds_api(odds_api, state).await;
    }
    sync_orders(state, logger, kalshi_rest).await;
    sync_fills(state, logger, kalshi_rest, notifier).await;
    reconcile_positions(state, kalshi_rest, notifier).await;
    settle_unsettled_fills(state, logger, kalshi_rest).await;
}

/// Fetch odds from The Odds API (multi-bookmaker) and store composite spread + devigged probs.
async fn refresh_odds_api(client: &OddsApiClient, state: &SharedState) {
    match client.fetch_ncaab_odds().await {
        Ok(api_games) => {
            let games = {
                let s = state.lock().await;
                s.game_state.games.clone()
            };
            let matches = crate::sportsbooks::matcher::match_odds_api_to_games(&api_games, &games);
            {
                let mut s = state.lock().await;
                for (event_id, spread) in &matches {
                    if let Some(gs) = s.game_state.get_mut(event_id) {
                        // Store devigged midpoints per bookmaker for dashboard compat
                        for book in &spread.books {
                            gs.sportsbook_probs.insert(book.bookmaker.clone(), book.home_devigged());
                        }
                        gs.sportsbook_spread = Some(spread.clone());
                    }
                }
            }
            tracing::debug!(
                "Odds API: matched {}/{} games, {} books avg",
                matches.len(),
                api_games.len(),
                matches.values().map(|s| s.books.len()).sum::<usize>().checked_div(matches.len().max(1)).unwrap_or(0),
            );
        }
        Err(e) => tracing::warn!("Odds API fetch failed: {:?}", e),
    }
}

/// Re-fetch ESPN summaries for games that have Kalshi markets but no win probability.
/// ESPN BPI projections aren't always available at startup — this retries until they appear.
async fn refresh_missing_espn_probs(espn_poller: &EspnPoller, state: &SharedState) {
    let event_ids: Vec<String> = {
        let s = state.lock().await;
        s.game_state.games.values()
            .filter(|gs| gs.has_kalshi() && gs.espn_home_win_prob.is_none())
            .map(|gs| gs.espn_event_id.clone())
            .collect()
    };

    for event_id in &event_ids {
        fetch_and_apply_summary(espn_poller, state, event_id, "RefreshProb ").await;
    }
}
