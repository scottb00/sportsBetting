use std::sync::Arc;

use crate::engine::bot::SharedState;
use crate::espn::types::GamePhase;
use crate::kalshi::rest::KalshiRestClient;

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
        // Drop lock before async REST calls to avoid holding mutex across I/O
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
