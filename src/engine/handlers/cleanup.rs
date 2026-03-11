use crate::engine::bot::{SharedState, SharedOrderBooks, SharedLogger};
use crate::engine::logger::GameInfo;
use crate::espn::types::GamePhase;

/// Clean up internal state for finished games.
/// Kalshi auto-cancels orders when games settle, so no REST calls needed.
pub async fn cleanup_finished_games(state: &SharedState, order_books: &SharedOrderBooks, logger: &SharedLogger) {
    let mut s = state.lock().await;

    // Persist settlement prices and game info before removing games
    let settlements: Vec<(String, i64)> = s.game_state.games.values()
        .filter(|g| g.phase == GamePhase::Final)
        .flat_map(|g| {
            let home_won = match (g.home_score, g.away_score) {
                (Some(h), Some(a)) => h > a,
                _ => return vec![],
            };
            g.kalshi_markets.iter().map(move |m| {
                // Settlement: 100 if YES side won, 0 if YES side lost
                let yes_won = if m.is_home { home_won } else { !home_won };
                (m.ticker.clone(), if yes_won { 100 } else { 0 })
            }).collect::<Vec<_>>()
        })
        .collect();

    // Build game info map for Final games (before state is removed)
    let game_info_map: std::collections::HashMap<String, GameInfo> = s.game_state.games.values()
        .filter(|g| g.phase == GamePhase::Final)
        .flat_map(|g| {
            g.kalshi_markets.iter().map(move |m| {
                (m.ticker.clone(), GameInfo {
                    game_name: format!("{} vs {}", g.away_team, g.home_team),
                    home_team: g.home_team.clone(),
                    away_team: g.away_team.clone(),
                    is_home: m.is_home,
                    edge_bps: None,
                    espn_fair: g.fair_value_for_market(m),
                })
            })
        })
        .collect();

    if !settlements.is_empty() || !game_info_map.is_empty() {
        let log = logger.lock().unwrap();
        for (ticker, settlement) in &settlements {
            if let Err(e) = log.record_settlement(ticker, *settlement) {
                tracing::warn!("Failed to record settlement for {}: {:?}", ticker, e);
            }
        }
        if !game_info_map.is_empty() {
            match log.backfill_game_info(&game_info_map) {
                Ok(n) if n > 0 => tracing::info!("Backfilled game info for {} order rows during cleanup", n),
                Ok(_) => {}
                Err(e) => tracing::warn!("Failed to backfill game info during cleanup: {:?}", e),
            }
        }
    }

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
