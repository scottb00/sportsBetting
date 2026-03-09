use anyhow::Result;
use sports_betting::config::Config;
use sports_betting::kalshi::auth::KalshiAuth;
use sports_betting::kalshi::rest::KalshiRestClient;

#[tokio::main]
async fn main() -> Result<()> {
    // Load config
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let config = Config::load(&config_path)?;
    println!("Config loaded from {}", config_path);
    println!("Demo mode: {}", config.kalshi.demo);
    println!();

    // Create auth and client
    let auth = KalshiAuth::from_file(
        config.kalshi.api_key_id.clone(),
        &config.kalshi.private_key_path,
    )?;
    let client = KalshiRestClient::new(auth, config.kalshi.demo);

    // -------------------------------------------------------
    // 1. Fetch sports events (open, limit 100)
    // -------------------------------------------------------
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
                                m.ticker,
                                m.title,
                                m.status,
                                m.yes_bid,
                                m.yes_ask,
                                m.volume,
                                m.open_interest,
                            );
                        }
                    }
                    None => {
                        println!("  markets: (none embedded)");
                    }
                }
                println!();
            }
        }
        Err(e) => {
            println!("ERROR fetching events: {:?}", e);
            println!();
        }
    }

    // -------------------------------------------------------
    // 2. Search markets for "basketball"
    // -------------------------------------------------------
    println!("=== Searching markets: query=\"basketball\" ===");
    println!();

    match client.search_markets("basketball").await {
        Ok(markets) => {
            println!("Found {} markets for 'basketball'", markets.len());
            for m in &markets {
                println!(
                    "  [{}] {} | event={} status={} yes_bid={:?} yes_ask={:?} vol={:?} oi={:?}",
                    m.ticker,
                    m.title,
                    m.event_ticker,
                    m.status,
                    m.yes_bid,
                    m.yes_ask,
                    m.volume,
                    m.open_interest,
                );
            }
            println!();
        }
        Err(e) => {
            println!("ERROR searching 'basketball': {:?}", e);
            println!();
        }
    }

    // -------------------------------------------------------
    // 3. Search markets for "NCAA"
    // -------------------------------------------------------
    println!("=== Searching markets: query=\"NCAA\" ===");
    println!();

    match client.search_markets("NCAA").await {
        Ok(markets) => {
            println!("Found {} markets for 'NCAA'", markets.len());
            for m in &markets {
                println!(
                    "  [{}] {} | event={} status={} yes_bid={:?} yes_ask={:?} vol={:?} oi={:?}",
                    m.ticker,
                    m.title,
                    m.event_ticker,
                    m.status,
                    m.yes_bid,
                    m.yes_ask,
                    m.volume,
                    m.open_interest,
                );
            }
            println!();
        }
        Err(e) => {
            println!("ERROR searching 'NCAA': {:?}", e);
            println!();
        }
    }

    // -------------------------------------------------------
    // 4. Search markets for "CBB"
    // -------------------------------------------------------
    println!("=== Searching markets: query=\"CBB\" ===");
    println!();

    match client.search_markets("CBB").await {
        Ok(markets) => {
            println!("Found {} markets for 'CBB'", markets.len());
            for m in &markets {
                println!(
                    "  [{}] {} | event={} status={} yes_bid={:?} yes_ask={:?} vol={:?} oi={:?}",
                    m.ticker,
                    m.title,
                    m.event_ticker,
                    m.status,
                    m.yes_bid,
                    m.yes_ask,
                    m.volume,
                    m.open_interest,
                );
            }
            println!();
        }
        Err(e) => {
            println!("ERROR searching 'CBB': {:?}", e);
            println!();
        }
    }

    // -------------------------------------------------------
    // 5. Search markets for "March Madness"
    // -------------------------------------------------------
    println!("=== Searching markets: query=\"March Madness\" ===");
    println!();

    match client.search_markets("March Madness").await {
        Ok(markets) => {
            println!("Found {} markets for 'March Madness'", markets.len());
            for m in &markets {
                println!(
                    "  [{}] {} | event={} status={} yes_bid={:?} yes_ask={:?} vol={:?} oi={:?}",
                    m.ticker,
                    m.title,
                    m.event_ticker,
                    m.status,
                    m.yes_bid,
                    m.yes_ask,
                    m.volume,
                    m.open_interest,
                );
            }
            println!();
        }
        Err(e) => {
            println!("ERROR searching 'March Madness': {:?}", e);
            println!();
        }
    }

    println!("=== Done ===");
    Ok(())
}
