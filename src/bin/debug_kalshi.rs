use anyhow::Result;
use sports_betting::config::Config;
use sports_betting::kalshi::auth::KalshiAuth;
use sports_betting::kalshi::rest::KalshiRestClient;

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let config = Config::load(&config_path)?;
    println!("Config loaded from {}", config_path);
    println!("Demo mode: {}", config.kalshi.demo);
    println!();

    let auth = KalshiAuth::from_file(
        config.kalshi.api_key_id.clone(),
        &config.kalshi.private_key_path,
    )?;
    let client = KalshiRestClient::new(auth, config.kalshi.demo);

    // Try multiple KXNCAAMB* series to discover all CBB conference tournaments
    let series_list = vec![
        "KXNCAAMBGAME", "KXNCAAMBSOCON", "KXNCAAMBSBELT", "KXNCAAMBACC",
        "KXNCAAMBB12", "KXNCAAMBBTEN", "KXNCAAMBSEC", "KXNCAAMBBE",
        "KXNCAAMBSWAC", "KXNCAAMBWCC", "KXNCAAMBCAA", "KXNCAAMBHL",
        "KXNCAAMBMWC", "KXNCAAMBAAC", "KXNCAAMBSLAND", "KXNCAAMBBSKY",
        "KXNCAAMBNEC", "KXNCAAMBAE", "KXNCAAMBOVC", "KXNCAAMBMAAC",
        "KXNCAAMBASUN", "KXNCAAMBCUSA", "KXNCAAMBMVC", "KXNCAAMBWAC",
        "KXNCAAMBMEAC", "KXNCAAMBSCON", "KXNCAAMBBSTH", "KXNCAAMBAEAST",
        "KXNCAAMBIV", "KXNCAAMBPAT", "KXNCAAMBBW",
    ];

    for series in &series_list {
        match client
            .get_events_with_series(None, Some(series), Some("open"), None, Some(200))
            .await
        {
            Ok(resp) => {
                if !resp.events.is_empty() {
                    println!("=== {} — {} events ===", series, resp.events.len());
                    for event in &resp.events {
                        let market_count = event.markets.as_ref().map(|m| m.len()).unwrap_or(0);
                        println!("  {} | {} | markets={}", event.event_ticker, event.title, market_count);
                        if let Some(markets) = &event.markets {
                            for m in markets {
                                println!("    [{}] {} | status={} yes_bid={:?} yes_ask={:?} vol={:?}",
                                    m.ticker, m.title, m.status, m.yes_bid, m.yes_ask, m.volume);
                            }
                        }
                    }
                    println!();
                }
            }
            Err(_) => {}
        }
    }

    // Search markets for various queries
    for query in &["basketball", "NCAA", "CBB", "March Madness"] {
        println!("=== Searching markets: query=\"{}\" ===", query);
        println!();
        match client.search_markets(query).await {
            Ok(markets) => {
                println!("Found {} markets for '{}'", markets.len(), query);
                for m in &markets {
                    println!(
                        "  [{}] {} | event={} status={} yes_bid={:?} yes_ask={:?} vol={:?} oi={:?}",
                        m.ticker, m.title, m.event_ticker, m.status,
                        m.yes_bid, m.yes_ask, m.volume, m.open_interest,
                    );
                }
                println!();
            }
            Err(e) => println!("ERROR searching '{}': {:?}\n", query, e),
        }
    }

    println!("=== Done ===");
    Ok(())
}
