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

        if dry_run {
            for (order_id, ticker) in &orders_to_cancel {
                tracing::info!("DRY RUN: would cancel order {} on finished {}", order_id, ticker);
            }
        } else {
            let futs: Vec<_> = orders_to_cancel.iter().map(|(order_id, ticker)| {
                let rest = kalshi_rest.clone();
                let oid = order_id.clone();
                let t = ticker.clone();
                async move {
                    match rest.cancel_order(&oid).await {
                        Ok(()) => tracing::info!("Cancelled order {} (game finished: {})", oid, t),
                        Err(e) => tracing::warn!("Failed to cancel order {}: {:?}", oid, e),
                    }
                }
            }).collect();
            futures_util::future::join_all(futs).await;
        }

        let mut s = state.lock().await;
        for (order_id, _) in &orders_to_cancel {
            s.order_manager.remove_order(order_id);
        }
        s.game_state.cleanup_finished();
        s.order_manager.clear_committed_contracts(&finished_tickers);
        for ticker in &finished_tickers {
            s.order_books.remove(ticker);
        }
    } else {
        let count_before = s.game_state.games.len();
        s.game_state.cleanup_finished();
        s.order_manager.clear_committed_contracts(&finished_tickers);
        let removed = count_before - s.game_state.games.len();
        if removed > 0 {
            tracing::info!("Cleaned up {} finished games", removed);
        }
    }
}
