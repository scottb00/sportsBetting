use std::collections::HashSet;
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

    // Remove tracked orders and positions for finished tickers
    for ticker in &finished_tickers {
        let order_ids = s.order_manager.order_ids_for_market(ticker);
        for oid in order_ids {
            s.order_manager.remove_order(&oid);
        }
    }
    s.risk.remove_positions_for_tickers(&finished_tickers);

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

/// Retry settlement for fills whose games were cleaned up but Kalshi hadn't settled yet.
/// Runs every maintenance tick; skips tickers still in active game state.
pub async fn settle_unsettled_fills(
    state: &SharedState,
    logger: &SharedLogger,
    kalshi_rest: &Arc<KalshiRestClient>,
) {
    let unsettled = {
        let log = logger.lock().unwrap();
        log.unsettled_tickers().unwrap_or_default()
    };
    if unsettled.is_empty() {
        return;
    }

    // Only retry tickers no longer in game_state (already cleaned up)
    let active_tickers: HashSet<String> = {
        let s = state.lock().await;
        s.game_state.games.values()
            .flat_map(|g| g.kalshi_tickers().into_iter().map(ToString::to_string))
            .collect()
    };

    for ticker in &unsettled {
        if active_tickers.contains(ticker) {
            continue;
        }
        match kalshi_rest.get_market(ticker).await {
            Ok(market) => {
                let result = market.result.as_deref().unwrap_or("");
                if result == "yes" || result == "no" {
                    let log = logger.lock().unwrap();
                    match log.settle_fills(ticker, result) {
                        Ok(n) if n > 0 => {
                            tracing::info!("Late-settled {} fills for {} (result={})", n, ticker, result);
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("Late-settle failed for {}: {}", ticker, e),
                    }
                }
            }
            Err(e) => tracing::warn!("Could not fetch market {} for late settlement: {}", ticker, e),
        }
    }
}
