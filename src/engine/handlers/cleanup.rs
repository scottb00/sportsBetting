use crate::engine::bot::{SharedState, SharedOrderBooks};
use crate::espn::types::GamePhase;

/// Clean up internal state for finished games.
/// Kalshi auto-cancels orders when games settle, so no REST calls needed.
pub async fn cleanup_finished_games(state: &SharedState, order_books: &SharedOrderBooks) {
    let mut s = state.lock().await;

    let finished_tickers: Vec<String> = s.game_state.games.values()
        .filter(|g| g.phase == GamePhase::Final)
        .flat_map(|g| g.kalshi_tickers().into_iter().map(ToString::to_string))
        .collect();

    // Remove tracked orders for finished tickers
    for ticker in &finished_tickers {
        let order_ids = s.order_manager.order_ids_for_market(ticker);
        for oid in order_ids {
            s.order_manager.remove_order(&oid);
        }
    }

    let count_before = s.game_state.games.len();
    s.game_state.cleanup_finished();
    s.order_manager.clear_committed_contracts(&finished_tickers);
    s.order_manager.cleanup_stale_strategies();
    let removed = count_before - s.game_state.games.len();
    drop(s);

    // Remove order books for finished tickers (separate lock)
    if !finished_tickers.is_empty() {
        let mut books = order_books.write().await;
        for ticker in &finished_tickers {
            books.remove(ticker);
        }
    }

    if removed > 0 {
        tracing::info!("Cleaned up {} finished games ({} tickers)", removed, finished_tickers.len());
    }
}
