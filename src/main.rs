use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use sports_betting::config::Config;
use sports_betting::engine::bot::{
    self, SharedState, SharedOrderBooks, SharedBreakLog, SharedLogger, SharedMapper,
    create_bot_state, create_strategies, populate_game_states, fetch_summaries_for_games,
};
use sports_betting::engine::dashboard;
use sports_betting::engine::handlers;
use sports_betting::engine::market_mapper::MarketMapper;
use sports_betting::engine::market_prep::{
    self, build_espn_for_matching, build_kalshi_for_matching,
    build_kalshi_volume, filter_events_for_dates, today_and_tomorrow_tags,
};
use sports_betting::engine::notifier::Notifier;
use sports_betting::espn::poller::{EspnPoller, GameTracker};
use sports_betting::kalshi::auth::KalshiAuth;
use sports_betting::kalshi::rest::KalshiRestClient;
use sports_betting::kalshi::websocket::{KalshiWsClient, KalshiWsHandle};
use sports_betting::polymarket::client::PolymarketClient;
use sports_betting::sportsbooks::odds_api::OddsApiClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("sports_betting=info".parse()?),
        )
        .init();

    tracing::info!("Sports betting bot starting...");

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let config = Config::load(&config_path)?;
    tracing::info!("Config loaded from {}", config_path);
    if config.kalshi.dry_run {
        tracing::info!("*** DRY RUN MODE — no real orders will be placed ***");
    }

    let auth = KalshiAuth::from_file(config.kalshi.api_key_id.clone(), &config.kalshi.private_key_path)?;
    let kalshi_rest = Arc::new(KalshiRestClient::new(auth.clone(), config.kalshi.demo));
    let espn_poller = EspnPoller::new();
    let poly_client = PolymarketClient::new();
    let odds_api_client = config.odds_api.as_ref().map(|cfg| {
        tracing::info!("The Odds API enabled — multi-book spread data active");
        OddsApiClient::new(cfg.api_key.clone())
    });

    let notifier = config.notify.as_ref().map(|nc| {
        tracing::info!("Notifications enabled via Telegram bot");
        Notifier::new(nc)
    });

    let (mut bot_state, trade_logger, mut market_mapper) = create_bot_state(&config)?;
    let logger: SharedLogger = Arc::new(std::sync::Mutex::new(trade_logger));
    let strategies = create_strategies(&config);

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    if !market_mapper.load_cache(&today) {
        tracing::info!("No cached mappings for today, will resolve after fetching markets");
    }

    // --- Initial market discovery & mapping ---
    fetch_initial_markets(
        &espn_poller, &poly_client, &kalshi_rest, &today,
        &mut bot_state, &mut market_mapper,
    ).await?;

    let state: SharedState = Arc::new(Mutex::new(bot_state));
    let mapper: SharedMapper = Arc::new(Mutex::new(market_mapper));
    let order_books: SharedOrderBooks = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let poly_books: sports_betting::engine::bot::SharedPolyBooks = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let break_log: SharedBreakLog = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));

    // Start dashboard web server
    {
        let dashboard_state = state.clone();
        let dashboard_books = order_books.clone();
        let dashboard_poly_books = poly_books.clone();
        let dashboard_logger = logger.clone();
        let dashboard_break_log = break_log.clone();
        let db_path = config.logging.db_path.clone();
        let dashboard_dry_run = config.kalshi.dry_run;
        tokio::spawn(async move {
            if let Err(e) = dashboard::serve(dashboard_state, dashboard_books, dashboard_poly_books, dashboard_logger, dashboard_break_log, &db_path, 3030, dashboard_dry_run).await {
                tracing::error!("Dashboard server failed: {:?}", e);
            }
        });
    }

    // Fetch initial ESPN win probs
    fetch_summaries_for_games(&espn_poller, &state).await;

    // --- Load existing positions and sync resting orders from Kalshi on startup ---
    {
        // Fetch positions outside the lock first
        let positions = match kalshi_rest.get_positions().await {
            Ok(resp) => resp.market_positions,
            Err(e) => {
                tracing::warn!("Failed to load positions from Kalshi: {:?}", e);
                vec![]
            }
        };
        let mut s = state.lock().await;
        for pos in &positions {
            if pos.total_traded > 0 {
                tracing::info!(
                    "Position: {} | traded={} position={} exposure={} pnl={}",
                    pos.ticker, pos.total_traded, pos.position,
                    pos.market_exposure, pos.realized_pnl,
                );
            }
            // Seed risk manager with current positions for PnL tracking
            if pos.position > 0 {
                // Positive position = YES contracts
                let avg_price = (pos.market_exposure as f64 / pos.position as f64) as i64;
                s.risk.seed_positions(&pos.ticker, "yes", avg_price, pos.position);
            } else if pos.position < 0 {
                // Negative position = NO contracts
                let abs_pos = pos.position.abs();
                let avg_price = (pos.market_exposure as f64 / abs_pos as f64) as i64;
                s.risk.seed_positions(&pos.ticker, "no", avg_price, abs_pos);
            }
        }
        tracing::info!("Loaded positions from Kalshi, seeded risk manager");

        // Set fill sync watermark to "now" so first maintenance tick only fetches
        // truly new fills. Positions are already correct from seed_positions() above —
        // running sync_fills with no watermark would re-apply ALL historical fills
        // on top of seeded positions, doubling them.
        s.order_manager.set_last_fill_sync_ts(chrono::Utc::now().timestamp());
    }
    handlers::sync_orders(&state, &logger, &kalshi_rest).await;

    // --- Re-settle all historical fills with corrected PnL formula ---
    {
        let log = logger.lock().unwrap();
        let reset_count = log.reset_all_fill_pnl().unwrap_or(0);
        if reset_count > 0 {
            tracing::info!("Reset {} fill PnL values for re-settlement", reset_count);
        }
        let unsettled = log.unsettled_tickers().unwrap_or_default();
        drop(log);

        let mut settled_total = 0usize;
        for ticker in &unsettled {
            match kalshi_rest.get_market(ticker).await {
                Ok(market) => {
                    let result = market.result.as_deref().unwrap_or("");
                    if result == "yes" || result == "no" {
                        let log = logger.lock().unwrap();
                        match log.settle_fills(ticker, result) {
                            Ok(n) => settled_total += n,
                            Err(e) => tracing::warn!("Re-settle failed for {}: {}", ticker, e),
                        }
                    }
                }
                Err(e) => tracing::warn!("Could not fetch market {} for re-settlement: {}", ticker, e),
            }
        }
        if settled_total > 0 {
            tracing::info!("Re-settled {} fills across {} tickers at startup", settled_total, unsettled.len());
        }
    }

    // --- Connect WebSockets ---
    let (mut kalshi_rx, kalshi_ws_handle) = connect_kalshi_ws(&auth, &config, &mapper).await?;
    let mut poly_rx = connect_poly_ws(&state).await?;

    // --- Seed orderbooks via REST for all initial tickers ---
    // WS snapshots may be delayed; REST provides an immediate baseline.
    {
        let tickers: Vec<String> = {
            let s = state.lock().await;
            s.game_state.games.values()
                .flat_map(|g| g.kalshi_markets.iter().map(|m| m.ticker.clone()))
                .collect()
        };
        let mut seeded = 0usize;
        for ticker in &tickers {
            match kalshi_rest.get_orderbook(ticker).await {
                Ok(snapshot) => {
                    let mut books = order_books.write().await;
                    let book = books
                        .entry(ticker.clone())
                        .or_insert_with(|| sports_betting::kalshi::orderbook::LocalOrderBook::new(ticker.clone()));
                    book.apply_snapshot(&snapshot, None);
                    seeded += 1;
                }
                Err(e) => {
                    tracing::warn!("Failed to seed orderbook for {}: {:?}", ticker, e);
                }
            }
        }
        if seeded > 0 {
            tracing::info!("Seeded {} orderbooks via REST at startup", seeded);
        }
    }

    // --- Seed Polymarket books via REST ---
    // WS only sends updates on trades/order changes; REST gives immediate baseline prices.
    {
        let poly_tokens: Vec<String> = {
            let s = state.lock().await;
            s.game_state.games.values()
                .filter_map(|g| g.polymarket_token_id.clone())
                .collect()
        };
        if !poly_tokens.is_empty() {
            let poly_client = sports_betting::polymarket::client::PolymarketClient::new();
            let mut seeded = 0usize;
            for token_id in &poly_tokens {
                match poly_client.fetch_book(token_id).await {
                    Ok((best_bid, best_ask)) => {
                        let bid_cents = best_bid.filter(|&p| p > 0.0 && p < 1.0).map(|p| (p * 100.0).round() as i64);
                        let ask_cents = best_ask.filter(|&p| p > 0.0 && p < 1.0).map(|p| (p * 100.0).round() as i64);
                        if bid_cents.is_some() || ask_cents.is_some() {
                            let mut books = poly_books.write().await;
                            let book = books.entry(token_id.clone()).or_insert_with(|| {
                                sports_betting::engine::bot::PolymarketBook {
                                    best_bid: None,
                                    best_ask: None,
                                    last_updated: std::time::Instant::now(),
                                }
                            });
                            book.best_bid = bid_cents;
                            book.best_ask = ask_cents;
                            book.last_updated = std::time::Instant::now();
                            seeded += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to seed Polymarket book for {}: {:?}", token_id, e);
                    }
                }
            }
            if seeded > 0 {
                tracing::info!("Seeded {} Polymarket books via REST at startup", seeded);
            }
        }
    }

    // --- Main event loop ---
    let mut game_tracker = GameTracker::new();
    let mut scoreboard_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(config.polling.scoreboard_interval_secs));
    let mut maintenance_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(config.intervals.maintenance_interval_secs));
    let mut current_day = today;
    let dry_run = config.kalshi.dry_run;

    tracing::info!("Entering main event loop");

    loop {
        tokio::select! {
            _ = scoreboard_interval.tick() => {
                let now_day = chrono::Local::now().format("%Y-%m-%d").to_string();
                if now_day != current_day {
                    tracing::info!("New trading day: {} -> {}", current_day, now_day);
                    let mut s = state.lock().await;
                    s.risk.reset_daily();
                    s.position_corrections = 0;
                    current_day = now_day;
                }
                handlers::handle_scoreboard_tick(
                    &espn_poller, &state, &order_books, &break_log, &logger,
                    &mut game_tracker, &strategies, &kalshi_rest, dry_run,
                    notifier.as_ref(),
                ).await;
            }
            _ = maintenance_interval.tick() => {
                handlers::handle_maintenance_tick(
                    &state, &order_books, &poly_books, &logger, &kalshi_rest, &espn_poller,
                    &mapper, kalshi_ws_handle.as_ref(), notifier.as_ref(),
                    odds_api_client.as_ref(),
                ).await;
            }
            Some(event) = async {
                match &mut kalshi_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handlers::handle_kalshi_event(event, &state, &order_books, &logger, &kalshi_rest, notifier.as_ref()).await;
            }
            Some(event) = async {
                match &mut poly_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handlers::handle_polymarket_event(event, &poly_books).await;
            }
        }
    }
}

/// Fetch ESPN, Kalshi, and Polymarket events; run market mapping; populate game states.
async fn fetch_initial_markets(
    espn_poller: &EspnPoller,
    poly_client: &PolymarketClient,
    kalshi_rest: &Arc<KalshiRestClient>,
    today: &str,
    bot_state: &mut bot::BotState,
    market_mapper: &mut MarketMapper,
) -> Result<()> {
    tracing::info!("Fetching initial market data...");
    let espn_games = espn_poller.fetch_scoreboard().await?;
    tracing::info!("Found {} ESPN games", espn_games.len());

    let poly_events = poly_client.fetch_cbb_events().await.unwrap_or_else(|e| {
        tracing::warn!("Failed to fetch Polymarket events: {:?}", e);
        vec![]
    });
    tracing::info!("Found {} Polymarket events", poly_events.len());

    let (_, tomorrow, date_tags) = today_and_tomorrow_tags();
    let mut kalshi_events = market_prep::fetch_all_kalshi_cbb_events(kalshi_rest).await;
    let before = kalshi_events.events.len();
    // Build volume map BEFORE filtering so cached mapper entries from yesterday
    // (still-live games) also get volume populated.
    let kalshi_volume = build_kalshi_volume(&kalshi_events);
    filter_events_for_dates(&mut kalshi_events, &date_tags);
    tracing::info!(
        "Found {} Kalshi CBB events ({} total, filtered to {:?} + conference tournaments)",
        kalshi_events.events.len(), before, date_tags,
    );

    let espn_for_matching = build_espn_for_matching(&espn_games);
    let kalshi_for_matching = build_kalshi_for_matching(&kalshi_events);

    let is_today_or_tomorrow = |s: &str| s.starts_with(today) || s.starts_with(&tomorrow);
    let poly_for_matching: Vec<(String, String)> = poly_events
        .iter()
        .filter(|e| {
            e.event_date.as_deref().map_or_else(
                || e.markets.iter().any(|m| m.game_start_time.as_deref().is_some_and(is_today_or_tomorrow)),
                is_today_or_tomorrow,
            )
        })
        .flat_map(|e| {
            e.markets.iter().filter_map(|m| {
                if m.sports_market_type.as_deref() != Some("moneyline") { return None; }
                let token_id = m.parsed_token_ids().map(|(yes, _)| yes)?;
                Some((token_id, m.question.clone()))
            })
        })
        .collect();
    tracing::info!(
        "Matching: {} ESPN, {} Kalshi events, {} Poly moneylines (filtered to {}/{})",
        espn_for_matching.len(), kalshi_for_matching.len(), poly_for_matching.len(), today, tomorrow
    );

    if let Err(e) = market_mapper.resolve_deterministic(
        &espn_for_matching,
        &kalshi_for_matching,
        &poly_for_matching,
        today,
    ) {
        tracing::error!("Market mapping failed: {:?}", e);
    }

    populate_game_states(bot_state, market_mapper, &espn_games, Some(&kalshi_volume));

    Ok(())
}

/// Connect to Kalshi WebSocket for all mapped market tickers.
async fn connect_kalshi_ws(
    auth: &KalshiAuth,
    config: &Config,
    mapper: &SharedMapper,
) -> Result<(Option<tokio::sync::mpsc::UnboundedReceiver<sports_betting::kalshi::websocket::KalshiWsEvent>>, Option<KalshiWsHandle>)> {
    let kalshi_tickers: Vec<String> = {
        let m = mapper.lock().await;
        m.all_mapped_kalshi_tickers()
    };

    let kalshi_ws = KalshiWsClient::new(auth.clone(), config.kalshi.demo);
    if !kalshi_tickers.is_empty() {
        tracing::info!("Connecting Kalshi WS for {} markets", kalshi_tickers.len());
        let (rx, handle) = kalshi_ws.connect(kalshi_tickers)?;
        Ok((Some(rx), Some(handle)))
    } else {
        tracing::warn!("No Kalshi markets mapped, skipping WS connection");
        Ok((None, None))
    }
}

/// Connect to Polymarket WebSocket for all mapped tokens.
async fn connect_poly_ws(
    state: &SharedState,
) -> Result<Option<tokio::sync::mpsc::UnboundedReceiver<sports_betting::polymarket::client::PolymarketEvent>>> {
    let poly_tokens: Vec<String> = {
        let s = state.lock().await;
        s.game_state.games.values()
            .filter_map(|g| g.polymarket_token_id.clone())
            .collect()
    };

    if !poly_tokens.is_empty() {
        tracing::info!("Connecting Polymarket WS for {} tokens", poly_tokens.len());
        Ok(Some(PolymarketClient::connect_ws(poly_tokens)?))
    } else {
        tracing::warn!("No Polymarket tokens mapped, skipping WS connection");
        Ok(None)
    }
}
