mod cleanup;
mod discovery;
mod fill_sync;
mod kalshi_ws;
mod order_sync;
mod polymarket_ws;
mod scoreboard;

pub use cleanup::cleanup_finished_games;
pub use discovery::discover_new_markets;
pub use fill_sync::sync_fills;
pub use kalshi_ws::handle_kalshi_event;
pub use order_sync::sync_orders;
pub use polymarket_ws::handle_polymarket_event;
pub use scoreboard::handle_scoreboard_tick;

use std::sync::Arc;
use crate::engine::bot::{SharedState, SharedMapper, SharedLogger};
use crate::espn::poller::EspnPoller;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::websocket::KalshiWsHandle;

/// Combined maintenance tick: cleanup finished games, discover new markets,
/// then sync orders and fills from Kalshi REST.
pub async fn handle_maintenance_tick(
    state: &SharedState,
    logger: &SharedLogger,
    kalshi_rest: &Arc<KalshiRestClient>,
    espn_poller: &EspnPoller,
    mapper: &SharedMapper,
    ws_handle: Option<&KalshiWsHandle>,
) {
    cleanup_finished_games(state).await;
    discover_new_markets(kalshi_rest, espn_poller, state, mapper, ws_handle).await;
    sync_orders(state, logger, kalshi_rest).await;
    sync_fills(state, logger, kalshi_rest).await;
}
