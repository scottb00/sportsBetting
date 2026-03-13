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
) {
    cleanup_finished_games(state, order_books, logger, kalshi_rest).await;
    discover_new_markets(kalshi_rest, espn_poller, state, order_books, mapper, ws_handle).await;
    refresh_missing_espn_probs(espn_poller, state).await;
    sync_orders(state, logger, kalshi_rest).await;
    sync_fills(state, logger, kalshi_rest).await;
    reconcile_positions(state, kalshi_rest, notifier).await;
    settle_unsettled_fills(state, logger, kalshi_rest).await;
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
