use std::sync::Arc;

use crate::engine::bot::{SharedState, SharedLogger};
use crate::kalshi::rest::KalshiRestClient;

/// Sync fills from Kalshi REST API into the local SQLite database.
/// Uses a high-water mark (latest fill timestamp) to only fetch new fills.
///
/// Lock strategy: state and logger locks are never held simultaneously.
/// Phase 1: read metadata under state lock
/// Phase 2: log to SQLite under logger lock only
/// Phase 3: apply state updates under state lock only
/// Phase 4: collect order statuses under state lock, then write under logger lock
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

    // Phase 1: Collect metadata from state (game info, strategies) under state lock
    struct FillContext {
        game_info: Option<crate::engine::logger::GameInfo>,
        strategy: Option<String>,
        price_cents: i64,
        fee_cents: f64,
    }

    let fill_contexts: Vec<FillContext> = {
        let s = state.lock().await;
        fills.iter().map(|fill| {
            let price_cents = if fill.side == "yes" { fill.yes_price } else { fill.no_price };
            let fee_cents = fill.fee_cost.unwrap_or(0.0);
            let game_info = crate::engine::logger::GameInfo::from_game_state(&s.game_state, &fill.ticker);
            let strategy = s.order_manager.get_strategy(&fill.order_id).map(|s| s.to_string());
            FillContext { game_info, strategy, price_cents, fee_cents }
        }).collect()
    };
    // state lock released

    // Phase 2: Log to SQLite under logger lock only (no state lock held)
    let new_fill_indices: Vec<usize> = {
        let log = logger.lock().unwrap();
        let mut new_indices = Vec::new();

        for (i, (fill, ctx)) in fills.iter().zip(fill_contexts.iter()).enumerate() {
            // Backfill stub order row
            let _ = log.log_order_if_missing(
                &fill.order_id,
                &fill.ticker,
                &fill.action,
                &fill.side,
                ctx.price_cents,
                fill.count,
                "filled",
                Some(&fill.created_time),
                ctx.game_info.as_ref(),
            );
            if let Some(ref strat) = ctx.strategy {
                let _ = log.update_order_strategy(&fill.order_id, strat);
            }

            // INSERT OR IGNORE: returns true if new row inserted, false if duplicate
            match log.log_fill(
                &fill.trade_id,
                &fill.order_id,
                &fill.ticker,
                &fill.side,
                &fill.action,
                ctx.price_cents,
                fill.count,
                ctx.fee_cents,
                Some(&fill.created_time),
            ) {
                Ok(true) => {
                    new_indices.push(i);
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("Failed to log fill {}: {:?}", fill.trade_id, e);
                }
            }
        }

        new_indices
    };
    // logger lock released

    // Phase 3: Apply state updates under state lock only (no logger lock)
    // Also collect order statuses for Phase 4
    let order_statuses: Vec<(String, String)>;
    {
        let mut s = state.lock().await;
        for &i in &new_fill_indices {
            let fill = &fills[i];
            let _ctx = &fill_contexts[i];

            // In-memory dedup: skip if WS handler already processed this fill
            if s.order_manager.is_fill_processed(&fill.trade_id) {
                continue;
            }
            s.order_manager.mark_fill_processed(&fill.trade_id);

            s.order_manager.record_fill(&fill.order_id, fill.count);
            // NOTE: Do NOT call s.risk.record_fill() here. Risk state (exposure, positions)
            // is already seeded correctly from Kalshi positions API at startup via seed_positions().
            // Calling record_fill here would double-count historical fills on top of the seed.
        }

        // Update high-water mark
        if let Some(latest) = fills.iter().map(|f| &f.created_time).max()
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(latest)
        {
            s.order_manager.set_last_fill_sync_ts(dt.timestamp());
        }

        // Collect order statuses while we hold the state lock
        order_statuses = fills.iter().map(|fill| {
            let status = if s.order_manager.has_resting_order(&fill.ticker) {
                "partial_fill"
            } else {
                "filled"
            };
            (fill.order_id.clone(), status.to_string())
        }).collect();
    }
    // state lock released

    // Phase 4: Write order statuses under logger lock only (no state lock)
    {
        let log = logger.lock().unwrap();
        for (order_id, status) in &order_statuses {
            let _ = log.update_order_status(order_id, status);
        }
    }

    if !new_fill_indices.is_empty() {
        tracing::info!("Fill sync: logged {} new fills from Kalshi REST", new_fill_indices.len());
    }
}
