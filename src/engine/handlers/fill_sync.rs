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

    if fills.is_empty() {
        return;
    }

    // Phase 1: Collect metadata from state and determine which fills are new, under state lock
    let fill_data: Vec<(
        usize,                                      // index
        i64,                                        // price_cents
        f64,                                        // fee_dollars
        Option<crate::engine::logger::GameInfo>,    // game_info
        Option<String>,                             // strategy
        bool,                                       // is_new (not already processed)
    )> = {
        let mut s = state.lock().await;
        fills.iter().enumerate().map(|(i, fill)| {
            let price_cents = if fill.side == "yes" { fill.yes_price } else { fill.no_price };
            let fee_dollars = fill.fee_cost.unwrap_or(0.0); // Kalshi API fee_cost is in dollars
            let mut game_info = crate::engine::logger::GameInfo::from_game_state(&s.game_state, &fill.ticker);
            // Compute edge from live fair value
            if let Some(ref mut gi) = game_info {
                if let Some(fv) = gi.espn_fair {
                    let order_prob = price_cents as f64 / 100.0;
                    let is_yes = fill.side.eq_ignore_ascii_case("yes");
                    let fair_for_side = if is_yes { fv } else { 1.0 - fv };
                    let is_buy = fill.action.eq_ignore_ascii_case("buy");
                    let raw_edge = fair_for_side - order_prob;
                    gi.edge_bps = Some((if is_buy { raw_edge } else { -raw_edge }) * 10000.0);
                }
            }
            let strategy = s.order_manager.get_strategy(&fill.order_id).map(ToString::to_string);
            let is_new = !s.order_manager.is_fill_processed(&fill.trade_id);
            if is_new {
                s.order_manager.mark_fill_processed(&fill.trade_id);
            }
            (i, price_cents, fee_dollars, game_info, strategy, is_new)
        }).collect()
    };
    // state lock released

    let new_count: usize = fill_data.iter().filter(|d| d.5).count();

    // Phase 2: Log to SQLite and update order statuses under logger lock only
    {
        let log = logger.lock().unwrap();
        for (i, price_cents, fee_dollars, game_info, strategy, _is_new) in &fill_data {
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
        for (i, _price_cents, _fee_dollars, _game_info, _strategy, is_new) in &fill_data {
            if !is_new {
                continue;
            }
            let fill = &fills[*i];
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
