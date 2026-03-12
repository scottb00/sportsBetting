use std::sync::Arc;

use crate::engine::bot::{SharedState, SharedLogger};
use crate::kalshi::rest::KalshiRestClient;

/// Sync fills from Kalshi REST API into the local SQLite database.
/// Uses a high-water mark (latest fill timestamp) to only fetch new fills.
///
/// Lock strategy: state and logger locks are never held simultaneously.
/// Phase 1: read metadata + mark fills processed under state lock
/// Phase 2: log fills + order statuses under logger lock only
/// Phase 3: apply risk/order updates under state lock only
pub async fn sync_fills(state: &SharedState, logger: &SharedLogger, kalshi_rest: &Arc<KalshiRestClient>) {
    // Read high-water mark from OrderManager (stored as unix seconds)
    let min_ts = {
        let s = state.lock().await;
        s.order_manager.last_fill_sync_ts().map(|ts| ts.to_string())
    };

    let fills = match kalshi_rest
        .get_fills(None, min_ts.as_deref(), Some(100))
        .await
    {
        Ok(resp) => resp.fills,
        Err(e) => {
            tracing::warn!("Fill sync failed: {:?}", e);
            return;
        }
    };

    // Mark successful sync even if no new fills
    {
        let mut s = state.lock().await;
        s.last_kalshi_sync = std::time::Instant::now();
    }

    if fills.is_empty() {
        return;
    }

    // Phase 1: Collect metadata from state, under state lock
    let fill_data: Vec<(
        usize,                                      // index
        i64,                                        // price_cents
        f64,                                        // fee_dollars
        Option<crate::engine::logger::GameInfo>,    // game_info
        Option<String>,                             // strategy
    )> = {
        let s = state.lock().await;
        fills.iter().enumerate().map(|(i, fill)| {
            let price_cents = if fill.side == "yes" { fill.yes_price } else { fill.no_price };
            let fee_dollars = fill.fee_cost.unwrap_or(0.0); // Kalshi API fee_cost is in dollars
            let game_info: Option<crate::engine::logger::GameInfo> =
                crate::engine::logger::GameInfo::from_game_state(&s.game_state, &fill.ticker);
            let strategy = s.order_manager.get_strategy(&fill.order_id).map(ToString::to_string);
            (i, price_cents, fee_dollars, game_info, strategy)
        }).collect()
    };
    // state lock released

    let new_count = fills.len();

    // Phase 2: Log to SQLite and update order statuses under logger lock only
    {
        let log = logger.lock().unwrap();
        for (i, price_cents, fee_dollars, game_info, strategy) in &fill_data {
            let fill = &fills[*i];

            // Backfill stub order row
            let _ = log.log_order_if_missing(
                &fill.order_id,
                &fill.ticker,
                &fill.action,
                &fill.side,
                *price_cents,
                fill.count,
                "filled",
                Some(&fill.created_time),
                game_info.as_ref(),
            );
            if let Some(strat) = strategy {
                let _ = log.update_order_strategy(&fill.order_id, strat);
            }

            // INSERT OR IGNORE: log the fill
            if let Err(e) = log.log_fill(
                &fill.trade_id,
                &fill.order_id,
                &fill.ticker,
                &fill.side,
                &fill.action,
                *price_cents,
                fill.count,
                *fee_dollars,
                Some(&fill.created_time),
            ) {
                tracing::warn!("Failed to log fill {}: {:?}", fill.trade_id, e);
            }
        }
    }
    // logger lock released

    // Phase 3: Apply state updates under state lock only (no logger lock)
    {
        let mut s = state.lock().await;
        for (i, _price_cents, _fee_dollars, _game_info, _strategy) in &fill_data {
            let fill = &fills[*i];
            s.order_manager.record_fill(&fill.order_id, fill.count);
            // Update risk manager for fills discovered via REST
            let price_cents = if fill.side == "yes" { fill.yes_price } else { fill.no_price };
            s.risk.record_fill(&fill.ticker, &fill.action, &fill.side, price_cents, fill.count);
        }

        // Update high-water mark
        if let Some(latest) = fills.iter().map(|f| &f.created_time).max()
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(latest)
        {
            s.order_manager.set_last_fill_sync_ts(dt.timestamp());
        }

        // Update order statuses in logger
        let order_statuses: Vec<(String, String)> = fills.iter().map(|fill| {
            let status = if s.order_manager.has_resting_order(&fill.ticker) {
                "partial_fill"
            } else {
                "filled"
            };
            (fill.order_id.clone(), status.to_string())
        }).collect();

        drop(s);
        // Write order statuses under logger lock
        let log = logger.lock().unwrap();
        for (order_id, status) in &order_statuses {
            let _ = log.update_order_status(order_id, status);
        }
    }

    if new_count > 0 {
        tracing::info!("Fill sync: logged {} new fills from Kalshi REST", new_count);
    }
}
