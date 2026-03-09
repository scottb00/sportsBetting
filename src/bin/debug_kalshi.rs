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

    // Fetch sports events
    println!("=== Fetching sports events (category=sports, status=open, limit=100) ===");
    println!();

    match client
        .get_events(Some("sports"), Some("open"), None, Some(100))
        .await
    {
        Ok(resp) => {
            println!("Got {} events (cursor: {:?})", resp.events.len(), resp.cursor);
            println!();

            for (i, event) in resp.events.iter().enumerate() {
                println!("--- Event #{} ---", i + 1);
                println!("  event_ticker: {}", event.event_ticker);
                println!("  title:        {}", event.title);
                println!("  category:     {}", event.category);

                match &event.markets {
                    Some(markets) => {
                        println!("  markets ({}):", markets.len());
                        for m in markets {
                            println!(
                                "    [{}] {} | status={} yes_bid={:?} yes_ask={:?} vol={:?} oi={:?}",
                                m.ticker, m.title, m.status,
                                m.yes_bid, m.yes_ask, m.volume, m.open_interest,
                            );
                        }
                    }
                    None => println!("  markets: (none embedded)"),
                }
                println!();
            }
        }
        Err(e) => println!("ERROR fetching events: {:?}\n", e),
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
