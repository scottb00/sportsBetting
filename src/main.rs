use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use sports_betting::config::Config;
use sports_betting::engine::game_state::GameStateManager;
use sports_betting::engine::logger::TradeLogger;
use sports_betting::engine::market_mapper::MarketMapper;
use sports_betting::engine::order_manager::{OrderManager, OrderSignal};
use sports_betting::engine::risk::RiskManager;
use sports_betting::espn::poller::{EspnPoller, GameTracker};
use sports_betting::espn::types::GamePhase;
use sports_betting::kalshi::auth::KalshiAuth;
use sports_betting::kalshi::orderbook::LocalOrderBook;
use sports_betting::kalshi::rest::KalshiRestClient;
use sports_betting::kalshi::websocket::{KalshiWsClient, KalshiWsEvent};
use sports_betting::polymarket::client::{PolymarketClient, PolymarketEvent};
use sports_betting::strategies::arb_scanner::ArbScanner;
use sports_betting::strategies::break_ev::BreakEvQuoter;
use sports_betting::strategies::clv_hunter::ClvHunter;

type SharedState = Arc<Mutex<BotState>>;

struct BotState {
    game_state: GameStateManager,
    market_mapper: MarketMapper,
    order_books: HashMap<String, LocalOrderBook>, // kalshi ticker -> book
    risk: RiskManager,
    order_manager: OrderManager,
    logger: TradeLogger,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("sports_betting=info".parse()?),
        )
        .init();

    tracing::info!("Sports betting bot starting...");

    // Load config
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let config = Config::load(&config_path)?;
    tracing::info!("Config loaded from {}", config_path);
    if config.kalshi.dry_run {
        tracing::info!("*** DRY RUN MODE — no real orders will be placed ***");
    }

    // Initialize components
    let auth = KalshiAuth::from_file(config.kalshi.api_key_id.clone(), &config.kalshi.private_key_path)?;
    let kalshi_rest = Arc::new(KalshiRestClient::new(auth.clone(), config.kalshi.demo));
    let espn_poller = EspnPoller::new();
    let poly_client = PolymarketClient::new();

    let logger = TradeLogger::new(&config.logging.db_path)?;
    tracing::info!("Trade logger initialized at {}", config.logging.db_path);

    let mut market_mapper = MarketMapper::new(&config.logging.cache_path);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Try loading cached mappings
    if !market_mapper.load_cache(&today) {
        tracing::info!("No cached mappings for today, will resolve after fetching markets");
    }

    // Initialize shared state
    let state = Arc::new(Mutex::new(BotState {
        game_state: GameStateManager::new(),
        market_mapper,
        order_books: HashMap::new(),
        risk: RiskManager::new(
            config.risk.max_position_per_game,
            config.risk.max_total_exposure,
            config.risk.daily_loss_limit,
            config.risk.kelly_fraction,
            config.risk.min_edge_threshold,
        ),
        order_manager: OrderManager::new(),
        logger,
    }));

    // Strategies
    let break_ev = BreakEvQuoter::new(config.strategy.break_ev_min_edge);
    let arb_scanner = ArbScanner::new(config.strategy.arb_scanner_min_edge);
    let clv_hunter = ClvHunter::new(config.strategy.clv_hunter_min_edge);

    // --- Initial market discovery & mapping ---
    tracing::info!("Fetching initial market data...");
    let espn_games = espn_poller.fetch_scoreboard().await?;
    tracing::info!("Found {} ESPN games", espn_games.len());

    let poly_events = poly_client.fetch_cbb_events().await.unwrap_or_else(|e| {
        tracing::warn!("Failed to fetch Polymarket events: {:?}", e);
        vec![]
    });
    tracing::info!("Found {} Polymarket events", poly_events.len());

    // Fetch Kalshi CBB game moneyline events (series_ticker=KXNCAAMBGAME)
    let kalshi_events = kalshi_rest
        .get_events_with_series(None, Some("KXNCAAMBGAME"), Some("open"), None, Some(100))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to fetch Kalshi events: {:?}", e);
            sports_betting::kalshi::types::GetEventsResponse {
                events: vec![],
                cursor: None,
            }
        });
    tracing::info!("Found {} Kalshi CBB events", kalshi_events.events.len());

    // Build lists for LLM matching
    let espn_for_llm: Vec<(String, String)> = espn_games
        .iter()
        .map(|g| (g.event_id.clone(), format!("{} @ {}", g.away_team, g.home_team)))
        .collect();

    let empty_markets = vec![];
    let kalshi_for_llm: Vec<(String, String)> = kalshi_events
        .events
        .iter()
        .flat_map(|e| {
            e.markets
                .as_ref()
                .unwrap_or(&empty_markets)
                .iter()
                .map(|m| (m.ticker.clone(), m.title.clone()))
        })
        .collect();

    // Filter Poly events to today/tomorrow only, then extract moneyline markets
    let tomorrow = (chrono::Local::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let poly_for_llm: Vec<(String, String)> = poly_events
        .iter()
        .filter(|e| {
            // Keep events whose date matches today or tomorrow
            match &e.event_date {
                Some(d) => d.starts_with(&today) || d.starts_with(&tomorrow),
                None => {
                    // No event_date — check game_start_time on markets
                    e.markets.iter().any(|m| {
                        m.game_start_time.as_ref().map_or(false, |t| {
                            t.starts_with(&today) || t.starts_with(&tomorrow)
                        })
                    })
                }
            }
        })
        .flat_map(|e| {
            e.markets.iter().filter_map(|m| {
                // Only include moneyline markets, not spreads/totals
                if m.sports_market_type.as_deref() != Some("moneyline") {
                    return None;
                }
                let token_id = m.parsed_token_ids().map(|(yes, _)| yes)?;
                Some((token_id, m.question.clone()))
            })
        })
        .collect();
    tracing::info!(
        "LLM input: {} ESPN, {} Kalshi, {} Poly moneylines (filtered to {}/{})",
        espn_for_llm.len(), kalshi_for_llm.len(), poly_for_llm.len(), today, tomorrow
    );

    // Resolve market mappings deterministically
    {
        let mut s = state.lock().await;

        if let Err(e) = s.market_mapper.resolve_deterministic(
            &espn_for_llm,
            &kalshi_for_llm,
            &poly_for_llm,
            &today,
        ) {
            tracing::error!("Market mapping failed: {:?}", e);
        }

        // Build volume lookup from Kalshi markets
        let kalshi_volume: HashMap<String, i64> = kalshi_events
            .events
            .iter()
            .flat_map(|e| e.markets.as_ref().unwrap_or(&empty_markets).iter())
            .filter_map(|m| m.volume.map(|v| (m.ticker.clone(), v)))
            .collect();

        // Populate game state from ESPN
        for game in &espn_games {
            let kalshi_ticker = s
                .market_mapper
                .kalshi_ticker(&game.event_id)
                .map(|t| t.to_string());
            let kalshi_is_home = s.market_mapper.kalshi_is_home_team(&game.event_id);
            let poly_token = s
                .market_mapper
                .polymarket_token(&game.event_id)
                .map(|t| t.to_string());
            let poly_is_home = s.market_mapper.polymarket_is_home_team(&game.event_id);

            let gs = s.game_state.upsert(
                game.event_id.clone(),
                game.home_team.clone(),
                game.away_team.clone(),
            );
            gs.phase = game.game_phase.clone();
            gs.home_score = game.home_score;
            gs.away_score = game.away_score;
            gs.kalshi_volume = kalshi_ticker.as_ref().and_then(|t| kalshi_volume.get(t).copied());
            gs.kalshi_ticker = kalshi_ticker;
            gs.kalshi_is_home = kalshi_is_home;
            gs.polymarket_token_id = poly_token;
            gs.polymarket_is_home = poly_is_home;
        }
    }

    // --- Fetch initial DK odds + win probs for all mapped games ---
    {
        let mapped_event_ids: Vec<String> = {
            let s = state.lock().await;
            s.game_state.games.keys().cloned().collect()
        };
        tracing::info!("Fetching initial summaries for {} games", mapped_event_ids.len());
        for event_id in &mapped_event_ids {
            match espn_poller.fetch_summary(event_id).await {
                Ok(summary) => {
                    let mut s = state.lock().await;
                    if let Some(gs) = s.game_state.get_mut(event_id) {
                        gs.espn_home_win_prob = EspnPoller::latest_win_prob(&summary);
                        if let Some((home_ml, _away_ml)) = EspnPoller::extract_dk_moneyline(&summary) {
                            gs.dk_home_implied_prob = Some(EspnPoller::moneyline_to_prob(home_ml));
                        }
                        tracing::info!(
                            "Initial {}: {} v {} | espn_hp={:?} dk_hp={:?}",
                            event_id, gs.away_team, gs.home_team,
                            gs.espn_home_win_prob, gs.dk_home_implied_prob,
                        );
                    }
                }
                Err(e) => tracing::warn!("Failed initial summary for {}: {:?}", event_id, e),
            }
        }
    }

    // --- Connect to Kalshi WebSocket ---
    let kalshi_tickers: Vec<String> = {
        let s = state.lock().await;
        s.market_mapper.all_mapped_kalshi_tickers()
    };

    let kalshi_ws = KalshiWsClient::new(auth.clone(), config.kalshi.demo);
    let mut kalshi_rx = if !kalshi_tickers.is_empty() {
        tracing::info!(
            "Connecting Kalshi WS for {} markets",
            kalshi_tickers.len()
        );
        Some(kalshi_ws.connect(kalshi_tickers).await?)
    } else {
        tracing::warn!("No Kalshi markets mapped, skipping WS connection");
        None
    };

    // --- Connect to Polymarket WebSocket ---
    let poly_tokens: Vec<String> = {
        let s = state.lock().await;
        s.game_state
            .games
            .values()
            .filter_map(|g| g.polymarket_token_id.clone())
            .collect()
    };

    let mut poly_rx = if !poly_tokens.is_empty() {
        tracing::info!(
            "Connecting Polymarket WS for {} tokens",
            poly_tokens.len()
        );
        Some(PolymarketClient::connect_ws(poly_tokens).await?)
    } else {
        tracing::warn!("No Polymarket tokens mapped, skipping WS connection");
        None
    };

    // --- Main event loop ---
    let mut game_tracker = GameTracker::new();
    let mut scoreboard_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(config.polling.scoreboard_interval_secs));

    tracing::info!("Entering main event loop");

    loop {
        tokio::select! {
            // ESPN scoreboard poll
            _ = scoreboard_interval.tick() => {
                match espn_poller.fetch_scoreboard().await {
                    Ok(games) => {
                        let new_breaks = game_tracker.update(&games);
                        let mut s = state.lock().await;

                        // Update game states
                        for game in &games {
                            let gs = s.game_state.upsert(
                                game.event_id.clone(),
                                game.home_team.clone(),
                                game.away_team.clone(),
                            );
                            gs.phase = game.game_phase.clone();
                            gs.home_score = game.home_score;
                            gs.away_score = game.away_score;
                            gs.last_updated = std::time::Instant::now();
                        }

                        // On new breaks, fetch summary for win prob + DK odds
                        for event_id in &new_breaks {
                            match espn_poller.fetch_summary(event_id).await {
                                Ok(summary) => {
                                    if let Some(gs) = s.game_state.get_mut(event_id) {
                                        gs.espn_home_win_prob = EspnPoller::latest_win_prob(&summary);
                                        if let Some((home_ml, _away_ml)) = EspnPoller::extract_dk_moneyline(&summary) {
                                            gs.dk_home_implied_prob = Some(EspnPoller::moneyline_to_prob(home_ml));
                                        }
                                        tracing::info!(
                                            "Updated {} win_prob={:?} dk_prob={:?}",
                                            event_id,
                                            gs.espn_home_win_prob,
                                            gs.dk_home_implied_prob,
                                        );
                                    }
                                }
                                Err(e) => tracing::warn!("Failed to fetch summary for {}: {:?}", event_id, e),
                            }
                        }

                        // Log game state summary
                        let live_count = s.game_state.live_games().len();
                        let break_count = s.game_state.games_on_break().len();
                        let pre_count = s.game_state.pre_game_games().len();
                        let with_kalshi = s.game_state.games.values().filter(|g| g.kalshi_ticker.is_some()).count();
                        let with_fair = s.game_state.games.values().filter(|g| g.consensus_fair_value().is_some()).count();
                        tracing::info!(
                            "Games: {} live, {} break, {} pre | {} w/Kalshi, {} w/fair_value",
                            live_count, break_count, pre_count, with_kalshi, with_fair
                        );

                        // Log per-game detail for mapped games with fair values
                        for gs in s.game_state.games.values() {
                            if gs.kalshi_ticker.is_some() && gs.consensus_fair_value().is_some() {
                                tracing::info!(
                                    "  {} v {} | phase={:?} | espn_hp={:?} dk_hp={:?} poly_hp={:?} | kalshi_mid={:?} is_home={} | aligned_fair={:?}",
                                    gs.away_team, gs.home_team, gs.phase,
                                    gs.espn_home_win_prob, gs.dk_home_implied_prob, gs.polymarket_home_prob,
                                    gs.kalshi_yes_mid, gs.kalshi_is_home,
                                    gs.kalshi_aligned_fair_value()
                                );
                            }
                        }

                        // --- Run strategies ---
                        let signals = evaluate_strategies(&s, &break_ev, &arb_scanner, &clv_hunter);
                        drop(s);

                        // Execute signals
                        for signal in signals {
                            execute_signal(signal, &state, &kalshi_rest, config.kalshi.dry_run).await;
                        }
                    }
                    Err(e) => tracing::warn!("ESPN scoreboard fetch failed: {:?}", e),
                }
            }

            // Kalshi WebSocket events
            Some(event) = async {
                match &mut kalshi_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let mut s = state.lock().await;
                match event {
                    KalshiWsEvent::OrderBookSnapshot { market_ticker, snapshot } => {
                        let book = s.order_books
                            .entry(market_ticker.clone())
                            .or_insert_with(|| LocalOrderBook::new(market_ticker.clone()));
                        book.apply_snapshot(&snapshot);
                        let prices = (book.best_yes_bid().map(|l| l.price as f64),
                                      book.best_yes_ask().map(|l| l.price as f64),
                                      book.yes_mid());
                        update_kalshi_prices_from(&mut s.game_state, &market_ticker, prices);
                        tracing::debug!("Book snapshot for {}", market_ticker);
                    }
                    KalshiWsEvent::OrderBookDelta(delta) => {
                        let ticker = delta.market_ticker.clone();
                        if let Some(book) = s.order_books.get_mut(&ticker) {
                            book.apply_delta(&delta);
                            let prices = (book.best_yes_bid().map(|l| l.price as f64),
                                          book.best_yes_ask().map(|l| l.price as f64),
                                          book.yes_mid());
                            update_kalshi_prices_from(&mut s.game_state, &ticker, prices);
                        }
                    }
                    KalshiWsEvent::Fill(fill) => {
                        tracing::info!(
                            "FILL: {} {:?} {} contracts @ {} yes_price",
                            fill.market_ticker, fill.action, fill.count, fill.yes_price
                        );
                        s.order_manager.handle_fill(&fill);
                        let _ = s.logger.log_fill(
                            &fill.trade_id, &fill.order_id, &fill.market_ticker,
                            &fill.side, &fill.action, fill.yes_price, fill.count, 0.0,
                        );
                    }
                    KalshiWsEvent::Trade(trade) => {
                        tracing::debug!(
                            "Trade: {} {} contracts @ {} taker={}",
                            trade.market_ticker, trade.count, trade.yes_price, trade.taker_side
                        );
                    }
                    KalshiWsEvent::Connected => {
                        tracing::info!("Kalshi WebSocket connected");
                    }
                    KalshiWsEvent::Disconnected => {
                        tracing::warn!("Kalshi WebSocket disconnected");
                    }
                    KalshiWsEvent::Error(e) => {
                        tracing::error!("Kalshi WebSocket error: {}", e);
                    }
                }
            }

            // Polymarket WebSocket events
            Some(event) = async {
                match &mut poly_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match event {
                    PolymarketEvent::PriceUpdate { asset_id, best_bid, best_ask } => {
                        let mut s = state.lock().await;
                        // Find the game with this token and update price
                        for gs in s.game_state.games.values_mut() {
                            if gs.polymarket_token_id.as_deref() == Some(&asset_id) {
                                if let (Some(bid), Some(ask)) = (best_bid, best_ask) {
                                    let yes_mid = (bid + ask) / 2.0;
                                    // Convert to home prob: flip if YES token is for away team
                                    gs.polymarket_home_prob = Some(if gs.polymarket_is_home {
                                        yes_mid
                                    } else {
                                        1.0 - yes_mid
                                    });
                                }
                                break;
                            }
                        }
                    }
                    PolymarketEvent::TradeUpdate { .. } => {}
                    PolymarketEvent::Connected => {
                        tracing::info!("Polymarket WebSocket connected");
                    }
                    PolymarketEvent::Disconnected => {
                        tracing::warn!("Polymarket WebSocket disconnected");
                    }
                }
            }
        }
    }
}

/// Update Kalshi bid/ask/mid in game state from pre-extracted prices.
fn update_kalshi_prices_from(
    game_state: &mut GameStateManager,
    kalshi_ticker: &str,
    prices: (Option<f64>, Option<f64>, Option<f64>),
) {
    for gs in game_state.games.values_mut() {
        if gs.kalshi_ticker.as_deref() == Some(kalshi_ticker) {
            gs.kalshi_yes_bid = prices.0;
            gs.kalshi_yes_ask = prices.1;
            gs.kalshi_yes_mid = prices.2;
            break;
        }
    }
}

/// Run all strategies and collect order signals.
/// Only keeps the best (highest edge) signal per ticker to avoid doubling up.
fn evaluate_strategies(
    state: &BotState,
    break_ev: &BreakEvQuoter,
    arb_scanner: &ArbScanner,
    clv_hunter: &ClvHunter,
) -> Vec<OrderSignal> {
    let mut signals = Vec::new();

    if state.risk.is_halted() {
        return signals;
    }

    for game in state.game_state.games.values() {
        let ticker = match &game.kalshi_ticker {
            Some(t) => t,
            None => continue,
        };

        // Skip low-volume markets (< 20k contracts)
        if game.kalshi_volume.unwrap_or(0) < 20_000 {
            continue;
        }

        // Skip extreme prices (< 10c or > 90c)
        if let Some(mid) = game.kalshi_yes_mid {
            if mid < 10.0 || mid > 90.0 {
                continue;
            }
        }

        let current_exposure = state.order_manager.market_exposure(ticker);
        let mut best_signal: Option<OrderSignal> = None;

        // Break EV — only during breaks
        if game.phase.is_break() {
            if let Some(signal) = break_ev.evaluate(game, &state.risk, current_exposure) {
                best_signal = Some(signal);
            }
        }

        // Arb Scanner — during live games (prefer over break_ev if better size)
        if matches!(game.phase, GamePhase::Live | GamePhase::Halftime | GamePhase::Break) {
            if let Some(signal) = arb_scanner.evaluate(game, &state.risk, current_exposure) {
                if best_signal.as_ref().map_or(true, |b| signal.size_dollars > b.size_dollars) {
                    best_signal = Some(signal);
                }
            }
        }

        // CLV Hunter — pre-game only
        if game.phase == GamePhase::PreGame {
            if let Some(signal) = clv_hunter.evaluate(game, &state.risk, current_exposure) {
                if best_signal.as_ref().map_or(true, |b| signal.size_dollars > b.size_dollars) {
                    best_signal = Some(signal);
                }
            }
        }

        if let Some(signal) = best_signal {
            signals.push(signal);
        }
    }

    signals
}

/// Execute an order signal via Kalshi REST API (or log if dry_run).
async fn execute_signal(
    signal: OrderSignal,
    state: &SharedState,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
) {
    let s = state.lock().await;
    if !s.risk.can_trade(signal.size_dollars) {
        tracing::warn!(
            "Risk check failed for {} signal on {}",
            signal.strategy,
            signal.kalshi_ticker
        );
        return;
    }
    drop(s);

    let order_req = OrderManager::signal_to_order(&signal);

    if dry_run {
        tracing::info!(
            "DRY RUN: {} {:?} {:?} {} contracts @ {:?}/{:?} | size=${:.2} | strategy={}",
            signal.kalshi_ticker,
            order_req.action,
            order_req.side,
            order_req.count,
            order_req.yes_price,
            order_req.no_price,
            signal.size_dollars,
            signal.strategy,
        );
        return;
    }

    tracing::info!(
        "Placing order: {} {:?} {:?} {} @ {:?}/{:?} ({})",
        signal.kalshi_ticker,
        order_req.action,
        order_req.side,
        order_req.count,
        order_req.yes_price,
        order_req.no_price,
        signal.strategy,
    );

    match kalshi_rest.create_order(&order_req).await {
        Ok(resp) => {
            let mut s = state.lock().await;
            let _ = s.logger.log_order(
                &resp.order.order_id,
                &signal.kalshi_ticker,
                &signal.strategy,
                &format!("{:?}", order_req.action),
                &format!("{:?}", order_req.side),
                order_req.yes_price.or(order_req.no_price).unwrap_or(0),
                order_req.count,
                &resp.order.status,
            );
            s.order_manager.track_order(resp.order);
            tracing::info!("Order placed successfully");
        }
        Err(e) => {
            tracing::error!("Failed to place order: {:?}", e);
        }
    }
}
