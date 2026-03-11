use std::sync::Arc;

use crate::engine::bot::SharedState;
use crate::kalshi::rest::KalshiRestClient;

/// Reconcile local risk positions with Kalshi API (source of truth).
/// Logs discrepancies and auto-corrects to prevent drift from missed fills.
pub async fn reconcile_positions(state: &SharedState, kalshi_rest: &Arc<KalshiRestClient>) {
    let positions = match kalshi_rest.get_positions().await {
        Ok(resp) => resp.market_positions,
        Err(e) => {
            tracing::warn!("Position reconciliation failed: {:?}", e);
            return;
        }
    };

    let mut s = state.lock().await;
    let mut corrections = 0;

    for pos in &positions {
        let local_net = s.risk.net_position(&pos.ticker);
        if local_net == pos.position {
            continue;
        }

        tracing::warn!(
            "POSITION MISMATCH: {} local={} kalshi={} — reseeding",
            pos.ticker, local_net, pos.position,
        );

        if pos.position > 0 {
            let avg_price = (pos.market_exposure as f64 / pos.position as f64) as i64;
            s.risk.reseed_position(&pos.ticker, "yes", avg_price, pos.position);
        } else if pos.position < 0 {
            let abs_pos = pos.position.abs();
            let avg_price = (pos.market_exposure as f64 / abs_pos as f64) as i64;
            s.risk.reseed_position(&pos.ticker, "no", avg_price, abs_pos);
        } else {
            // Position is zero — clear any local entries
            s.risk.reseed_position(&pos.ticker, "yes", 0, 0);
        }
        corrections += 1;
    }

    if corrections > 0 {
        tracing::info!("Position reconciliation: corrected {} mismatches", corrections);
    }
}
