use std::sync::Arc;

use crate::engine::bot::SharedState;
use crate::kalshi::rest::KalshiRestClient;

/// Convert an ISO timestamp to a unix timestamp string for the Kalshi API.
fn iso_to_unix_ts(iso: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp().to_string())
}

/// Sync fills from Kalshi REST API into the local SQLite database.
/// Uses a high-water mark (latest fill timestamp) to only fetch new fills.
pub async fn sync_fills(state: &SharedState, kalshi_rest: &Arc<KalshiRestClient>) {
    // Read high-water mark from state, convert ISO to unix ts for Kalshi API
    let min_ts = {
        let s = state.lock().await;
        s.last_fill_sync_ts.as_deref().and_then(iso_to_unix_ts)
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

    let mut s = state.lock().await;
    let mut new_count = 0;

    for fill in &fills {
        // Determine the price for the side being traded
        let price_cents = if fill.side == "yes" {
            fill.yes_price
        } else {
            fill.no_price
        };
        let fee_cents = fill.fee_cost.unwrap_or(0.0);

        // Backfill a stub order row if this order isn't in the DB yet
        // (e.g. placed in a previous session before current DB was created)
        let _ = s.logger.log_order_if_missing(
            &fill.order_id,
            &fill.ticker,
            &fill.action,
            &fill.side,
            price_cents,
            fill.count,
            "filled",
            Some(&fill.created_time),
        );

        // INSERT OR IGNORE: returns true if new row inserted, false if duplicate
        match s.logger.log_fill(
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
                // Update in-memory order tracking (decrement remaining qty)
                s.order_manager.record_fill(&fill.order_id, fill.count);
            }
            Ok(false) => {} // duplicate trade_id — already logged
            Err(e) => tracing::warn!("Failed to log fill {}: {:?}", fill.trade_id, e),
        }

        // Update order status: check if order is fully filled or partial
        let status = if s.order_manager.has_resting_order(&fill.ticker) {
            "partial_fill"
        } else {
            "filled"
        };
        let _ = s.logger.update_order_status(&fill.order_id, status);
    }

    // Update high-water mark to the latest fill's created_time
    if let Some(latest) = fills.iter().map(|f| &f.created_time).max() {
        s.last_fill_sync_ts = Some(latest.clone());
    }

    if new_count > 0 {
        tracing::info!("Fill sync: logged {} new fills from Kalshi REST", new_count);
    }
}
