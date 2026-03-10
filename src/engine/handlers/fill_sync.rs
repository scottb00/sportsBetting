use std::sync::Arc;

use crate::engine::bot::{SharedState, SharedLogger};
use crate::kalshi::rest::KalshiRestClient;

/// Sync fills from Kalshi REST API into the local SQLite database.
/// Uses a high-water mark (latest fill timestamp) to only fetch new fills.
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

    if fills.is_empty() {
        return;
    }

    // Process fills under state lock — log to SQLite, update order manager
    let new_count = {
        let mut s = state.lock().await;
        let log = logger.lock().unwrap();
        let mut new_count = 0;

        for fill in &fills {
            let price_cents = if fill.side == "yes" {
                fill.yes_price
            } else {
                fill.no_price
            };
            let fee_cents = fill.fee_cost.unwrap_or(0.0);
            let game_info = crate::engine::logger::GameInfo::from_game_state(&s.game_state, &fill.ticker);
            let strategy = s.order_manager.get_strategy(&fill.order_id).map(|s| s.to_string());

            // Backfill stub order row
            let _ = log.log_order_if_missing(
                &fill.order_id,
                &fill.ticker,
                &fill.action,
                &fill.side,
                price_cents,
                fill.count,
                "filled",
                Some(&fill.created_time),
                game_info.as_ref(),
            );
            if let Some(ref strat) = strategy {
                let _ = log.update_order_strategy(&fill.order_id, strat);
            }

            // INSERT OR IGNORE: returns true if new row inserted, false if duplicate
            match log.log_fill(
                &fill.trade_id,
                &fill.order_id,
                &fill.ticker,
                &fill.side,
                &fill.action,
                price_cents,
                fill.count,
                fee_cents,
                Some(&fill.created_time),
            ) {
                Ok(true) => {
                    new_count += 1;
                    s.order_manager.record_fill(&fill.order_id, fill.count);
                    s.risk.record_fill(
                        &fill.ticker, &fill.action, &fill.side,
                        price_cents, fill.count,
                    );
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("Failed to log fill {}: {:?}", fill.trade_id, e);
                }
            }

            let order_status = if s.order_manager.has_resting_order(&fill.ticker) {
                "partial_fill"
            } else {
                "filled"
            };
            let _ = log.update_order_status(&fill.order_id, order_status);
        }

        // Update high-water mark
        if let Some(latest) = fills.iter().map(|f| &f.created_time).max()
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(latest)
        {
            s.order_manager.set_last_fill_sync_ts(dt.timestamp());
        }

        new_count
    };

    if new_count > 0 {
        tracing::info!("Fill sync: logged {} new fills from Kalshi REST", new_count);
    }
}
