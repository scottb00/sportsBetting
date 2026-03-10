use anyhow::Result;
use sports_betting::config::Config;
use sports_betting::kalshi::auth::KalshiAuth;
use sports_betting::kalshi::rest::KalshiRestClient;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load("config.toml")?;
    let auth = KalshiAuth::from_file(config.kalshi.api_key_id.clone(), &config.kalshi.private_key_path)?;
    let client = KalshiRestClient::new(auth, config.kalshi.demo);

    // Get all resting orders
    let orders_resp = client.get_orders(None).await?;
    println!("Found {} resting orders", orders_resp.orders.len());

    for order in &orders_resp.orders {
        println!(
            "  {} | {:?} {:?} {} @ yes={:?} no={:?} | remaining={}",
            order.order_id, order.action, order.side, order.ticker,
            order.yes_price, order.no_price, order.remaining_count
        );
    }

    if orders_resp.orders.is_empty() {
        println!("No orders to cancel.");
    }

    println!("\nCancelling all {} orders...", orders_resp.orders.len());
    for order in &orders_resp.orders {
        match client.cancel_order(&order.order_id).await {
            Ok(()) => println!("  Cancelled {}", order.order_id),
            Err(e) => println!("  Failed to cancel {}: {:?}", order.order_id, e),
        }
    }

    // Also show positions
    println!("\n--- Current Positions ---");
    let positions = match client.get_positions().await {
        Ok(p) => p,
        Err(e) => {
            println!("  Failed to fetch positions: {:?}", e);
            let balance = client.get_balance().await?;
            println!("\nBalance: ${:.2}", balance.balance as f64 / 100.0);
            println!("Done.");
            return Ok(());
        }
    };
    for pos in &positions.market_positions {
        if pos.position != 0 {
            let side = if pos.position > 0 { "YES" } else { "NO" };
            println!(
                "  {} | {} {} contracts | exposure={} | pnl={}",
                pos.ticker, pos.position.abs(), side,
                pos.market_exposure, pos.realized_pnl
            );
        }
    }
    if positions.market_positions.iter().all(|p| p.position == 0) {
        println!("  No active positions.");
    }

    // Show balance
    let balance = client.get_balance().await?;
    println!("\nBalance: ${:.2}", balance.balance as f64 / 100.0);

    println!("Done.");
    Ok(())
}
