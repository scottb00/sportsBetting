use std::sync::Arc;
use crate::engine::bot::{SharedState, SharedOrderBooks, SharedLogger};
use crate::espn::types::GamePhase;
use crate::kalshi::rest::KalshiRestClient;

/// Clean up internal state for finished games and backfill settlement PnL.
/// Kalshi auto-cancels orders when games settle, so order REST calls are not needed.
pub async fn cleanup_finished_games(
    state: &SharedState,
    order_books: &SharedOrderBooks,
    logger: &SharedLogger,
    kalshi_rest: &Arc<KalshiRestClient>,
) {
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
    let removed = count_before - s.game_state.games.len();
    drop(s);

    // Remove order books for finished tickers (separate lock)
    if !finished_tickers.is_empty() {
        let mut books = order_books.write().await;
        for ticker in &finished_tickers {
            books.remove(ticker);
        }
    }

    // Backfill settlement PnL for each finished ticker
    for ticker in &finished_tickers {
        match kalshi_rest.get_market(ticker).await {
            Ok(market) => {
                let result = market.result.as_deref().unwrap_or("");
                if result == "yes" || result == "no" {
                    let log = logger.lock().unwrap();
                    match log.settle_fills(ticker, result) {
                        Ok(n) if n > 0 => tracing::info!("Settled {} fills for {} (result={})", n, ticker, result),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("Failed to settle fills for {}: {}", ticker, e),
                    }
                }
            }
            Err(e) => tracing::warn!("Could not fetch market result for {}: {}", ticker, e),
        }
    }

    if removed > 0 {
        tracing::info!("Cleaned up {} finished games ({} tickers)", removed, finished_tickers.len());
    }
}
