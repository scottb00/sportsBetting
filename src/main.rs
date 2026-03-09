use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use sports_betting::config::Config;
use sports_betting::engine::game_state::{GameStateManager, KalshiMarketState};
use sports_betting::engine::logger::TradeLogger;
use sports_betting::engine::market_mapper::MarketMapper;
use sports_betting::engine::order_manager::{OrderManager, OrderSignal};
use sports_betting::engine::risk::RiskManager;
use sports_betting::espn::poller::{EspnPoller, GameTracker};
use sports_betting::espn::types::{GameInfo, GamePhase};
use sports_betting::kalshi::auth::KalshiAuth;
use sports_betting::kalshi::orderbook::LocalOrderBook;
use sports_betting::kalshi::rest::KalshiRestClient;
use sports_betting::kalshi::websocket::{KalshiWsClient, KalshiWsEvent, KalshiWsHandle};
use sports_betting::polymarket::client::{PolymarketClient, PolymarketEvent};
use sports_betting::strategies::arb_scanner::ArbScanner;
use sports_betting::strategies::break_ev::BreakEvQuoter;
use sports_betting::strategies::clv_hunter::ClvHunter;

type SharedState = Arc<Mutex<BotState>>;

/// Convert "2026-03-09" to Kalshi ticker date format "26MAR09".
fn kalshi_date_tag(date_str: &str) -> String {
    let dt = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .expect("invalid date format");
    dt.format("%y%b%d").to_string().to_uppercase()
}

struct BotState {
    game_state: GameStateManager,
    market_mapper: MarketMapper,
    order_books: HashMap<String, LocalOrderBook>,
    risk: RiskManager,
    order_manager: OrderManager,
    logger: TradeLogger,
}

struct Strategies {
    break_ev: BreakEvQuoter,
    arb_scanner: ArbScanner,
    clv_hunter: ClvHunter,
    min_volume: i64,
    min_price_cents: f64,
    max_price_cents: f64,
    order_ttl: Duration,
}

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

    let logger = TradeLogger::new(&config.logging.db_path)?;
    tracing::info!("Trade logger initialized at {}", config.logging.db_path);

    let mut market_mapper = MarketMapper::new(&config.logging.cache_path);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    if !market_mapper.load_cache(&today) {
        tracing::info!("No cached mappings for today, will resolve after fetching markets");
    }

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

    let strategies = Strategies {
        break_ev: BreakEvQuoter::new(config.strategy.break_ev_min_edge),
        arb_scanner: ArbScanner::new(config.strategy.arb_scanner_min_edge),
        clv_hunter: ClvHunter::new(config.strategy.clv_hunter_min_edge),
        min_volume: config.strategy.min_volume,
        min_price_cents: config.strategy.min_price_cents,
        max_price_cents: config.strategy.max_price_cents,
        order_ttl: Duration::from_secs(config.strategy.order_ttl_secs),
    };

    // --- Initial market discovery & mapping ---
    tracing::info!("Fetching initial market data...");
    let espn_games = espn_poller.fetch_scoreboard().await?;
    tracing::info!("Found {} ESPN games", espn_games.len());

    let poly_events = poly_client.fetch_cbb_events().await.unwrap_or_else(|e| {
        tracing::warn!("Failed to fetch Polymarket events: {:?}", e);
        vec![]
    });
    tracing::info!("Found {} Polymarket events", poly_events.len());

    let kalshi_date_tag = kalshi_date_tag(&today);
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
    // Filter to today's events only (ticker contains date tag like "26MAR09")
    let kalshi_events = {
        let mut filtered = kalshi_events;
        let before = filtered.events.len();
        filtered.events.retain(|e| e.event_ticker.contains(&kalshi_date_tag));
        tracing::info!(
            "Found {} Kalshi CBB events ({} total, filtered to {})",
            filtered.events.len(), before, kalshi_date_tag,
        );
        filtered
    };

    // Build lists for matching
    let espn_for_matching: Vec<(String, String)> = espn_games
        .iter()
        .map(|g| (g.event_id.clone(), format!("{} @ {}", g.away_team, g.home_team)))
        .collect();

    // Build Kalshi events for matching: (event_ticker, title, Vec<(market_ticker, yes_sub_title)>)
    #[allow(clippy::type_complexity)]
    let kalshi_for_matching: Vec<(String, String, Vec<(String, String)>)> = kalshi_events
        .events
        .iter()
        .map(|e| {
            let markets: Vec<(String, String)> = e.markets.as_ref()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|m| {
                    let yes_sub = m.yes_sub_title.as_ref()?;
                    Some((m.ticker.clone(), yes_sub.clone()))
                })
                .collect();
            (e.event_ticker.clone(), e.title.clone(), markets)
        })
        .collect();

    // Build volume map across all market tickers
    let empty_markets = vec![];
    let kalshi_volume: HashMap<String, i64> = kalshi_events
        .events
        .iter()
        .flat_map(|e| e.markets.as_ref().unwrap_or(&empty_markets).iter())
        .filter_map(|m| m.volume.map(|v| (m.ticker.clone(), v)))
        .collect();

    let tomorrow = (chrono::Local::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let poly_for_matching: Vec<(String, String)> = poly_events
        .iter()
        .filter(|e| {
            match &e.event_date {
                Some(d) => d.starts_with(&today) || d.starts_with(&tomorrow),
                None => e.markets.iter().any(|m| {
                    m.game_start_time.as_ref().is_some_and(|t| {
                        t.starts_with(&today) || t.starts_with(&tomorrow)
                    })
                }),
            }
        })
        .flat_map(|e| {
            e.markets.iter().filter_map(|m| {
                if m.sports_market_type.as_deref() != Some("moneyline") {
                    return None;
                }
                let token_id = m.parsed_token_ids().map(|(yes, _)| yes)?;
                Some((token_id, m.question.clone()))
            })
        })
        .collect();
    tracing::info!(
        "Matching: {} ESPN, {} Kalshi events, {} Poly moneylines (filtered to {}/{})",
        espn_for_matching.len(), kalshi_for_matching.len(), poly_for_matching.len(), today, tomorrow
    );

    // Resolve market mappings and populate initial game state
    {
        let mut s = state.lock().await;

        if let Err(e) = s.market_mapper.resolve_deterministic(
            &espn_for_matching,
            &kalshi_for_matching,
            &poly_for_matching,
            &today,
        ) {
            tracing::error!("Market mapping failed: {:?}", e);
        }

        populate_game_states(&mut s, &espn_games, Some(&kalshi_volume));
    }

    // Fetch initial ESPN win probs
    fetch_summaries_for_games(&espn_poller, &state).await;

    // --- Connect WebSockets ---
    let kalshi_tickers: Vec<String> = {
        let s = state.lock().await;
        s.market_mapper.all_mapped_kalshi_tickers()
    };

    let kalshi_ws = KalshiWsClient::new(auth.clone(), config.kalshi.demo);
    let (mut kalshi_rx, kalshi_ws_handle): (Option<_>, Option<KalshiWsHandle>) = if !kalshi_tickers.is_empty() {
        tracing::info!("Connecting Kalshi WS for {} markets", kalshi_tickers.len());
        let (rx, handle) = kalshi_ws.connect(kalshi_tickers).await?;
        (Some(rx), Some(handle))
    } else {
        tracing::warn!("No Kalshi markets mapped, skipping WS connection");
        (None, None)
    };

    let poly_tokens: Vec<String> = {
        let s = state.lock().await;
        s.game_state.games.values()
            .filter_map(|g| g.polymarket_token_id.clone())
            .collect()
    };

    let mut poly_rx = if !poly_tokens.is_empty() {
        tracing::info!("Connecting Polymarket WS for {} tokens", poly_tokens.len());
        Some(PolymarketClient::connect_ws(poly_tokens).await?)
    } else {
        tracing::warn!("No Polymarket tokens mapped, skipping WS connection");
        None
    };

    // --- Main event loop ---
    let mut game_tracker = GameTracker::new();
    let mut scoreboard_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(config.polling.scoreboard_interval_secs));
    let mut cleanup_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(300));
    let mut discovery_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(300));
    let mut current_day = chrono::Local::now().format("%Y-%m-%d").to_string();

    tracing::info!("Entering main event loop");

    loop {
        tokio::select! {
            _ = scoreboard_interval.tick() => {
                // Check for daily reset (new trading day)
                let now_day = chrono::Local::now().format("%Y-%m-%d").to_string();
                if now_day != current_day {
                    tracing::info!("New trading day: {} -> {}", current_day, now_day);
                    let mut s = state.lock().await;
                    s.risk.reset_daily();
                    current_day = now_day;
                }

                handle_scoreboard_tick(
                    &espn_poller, &state, &mut game_tracker,
                    &strategies, &kalshi_rest, config.kalshi.dry_run,
                ).await;
            }

            _ = cleanup_interval.tick() => {
                cleanup_finished_games(&state, &kalshi_rest, config.kalshi.dry_run).await;
            }

            _ = discovery_interval.tick() => {
                discover_new_markets(
                    &kalshi_rest, &espn_poller, &state,
                    kalshi_ws_handle.as_ref(), config.kalshi.demo,
                ).await;
            }

            Some(event) = async {
                match &mut kalshi_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handle_kalshi_event(event, &state).await;
            }

            Some(event) = async {
                match &mut poly_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handle_polymarket_event(event, &state).await;
            }
        }
    }
}

/// Populate game states from ESPN data, optionally setting Kalshi volume.
fn populate_game_states(
    s: &mut BotState,
    games: &[GameInfo],
    kalshi_volume: Option<&HashMap<String, i64>>,
) {
    for game in games {
        let kalshi_market_infos: Vec<_> = s.market_mapper.kalshi_markets_for_game(&game.event_id).to_vec();
        let kalshi_title = s.market_mapper.kalshi_title(&game.event_id).map(|t| t.to_string());
        let poly_token = s.market_mapper.polymarket_token(&game.event_id).map(|t| t.to_string());
        let poly_is_home = s.market_mapper.polymarket_is_home_team(&game.event_id);

        let gs = s.game_state.upsert(
            game.event_id.clone(),
            game.home_team.clone(),
            game.away_team.clone(),
        );
        gs.phase = game.game_phase.clone();
        gs.home_score = game.home_score;
        gs.away_score = game.away_score;
        if game.start_time_ts.is_some() {
            gs.start_time_ts = game.start_time_ts;
        }
        gs.status_detail = game.status_detail.clone();
        gs.last_updated = std::time::Instant::now();

        // Set up Kalshi markets if not already present
        if gs.kalshi_markets.is_empty() && !kalshi_market_infos.is_empty() {
            let title = kalshi_title.as_deref().unwrap_or("");
            for info in &kalshi_market_infos {
                let is_home = MarketMapper::market_is_home_team(title, &info.yes_sub_title);
                let mut market = KalshiMarketState::new(info.ticker.clone(), is_home);
                if let Some(vol_map) = kalshi_volume {
                    market.volume = vol_map.get(&info.ticker).copied();
                }
                gs.kalshi_markets.push(market);
            }
        }

        gs.polymarket_token_id = poly_token;
        gs.polymarket_is_home = poly_is_home;
    }
}

/// Fetch ESPN summaries (win prob) for all games in state.
async fn fetch_summaries_for_games(espn_poller: &EspnPoller, state: &SharedState) {
    let event_ids: Vec<String> = {
        let s = state.lock().await;
        s.game_state.games.keys().cloned().collect()
    };
    tracing::info!("Fetching summaries for {} games", event_ids.len());

    for event_id in &event_ids {
        match espn_poller.fetch_summary(event_id).await {
            Ok(summary) => {
                let win_prob = EspnPoller::latest_win_prob(&summary);
                let dk_ml = EspnPoller::extract_dk_moneyline(&summary).map(|(h, _)| h);
                let mut s = state.lock().await;
                if let Some(gs) = s.game_state.get_mut(event_id) {
                    gs.update_from_espn_summary(win_prob, dk_ml);
                    tracing::info!(
                        "{}: {} v {} | espn_hp={:?}",
                        event_id, gs.away_team, gs.home_team,
                        gs.espn_home_win_prob,
                    );
                }
            }
            Err(e) => tracing::warn!("Failed summary for {}: {:?}", event_id, e),
        }
    }
}

/// Handle an ESPN scoreboard poll tick.
async fn handle_scoreboard_tick(
    espn_poller: &EspnPoller,
    state: &SharedState,
    game_tracker: &mut GameTracker,
    strategies: &Strategies,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
) {
    let games = match espn_poller.fetch_scoreboard().await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("ESPN scoreboard fetch failed: {:?}", e);
            return;
        }
    };

    // Detect phase transitions BEFORE updating the tracker (needs previous phases)
    let _breaks_ended = game_tracker.breaks_ended(&games);
    let pregame_to_live = game_tracker.pregame_to_live(&games);
    let new_breaks = game_tracker.update(&games);
    let mut s = state.lock().await;

    // Update game states (no volume update on polls — volume is set at startup)
    for game in &games {
        let gs = s.game_state.upsert(
            game.event_id.clone(),
            game.home_team.clone(),
            game.away_team.clone(),
        );
        gs.phase = game.game_phase.clone();
        gs.home_score = game.home_score;
        gs.away_score = game.away_score;
        gs.status_detail = game.status_detail.clone();
        gs.last_updated = std::time::Instant::now();
    }

    // --- CLV validation: check pre-game orders when game goes live ---
    // (CLV orders auto-expire via Kalshi expiration_ts — no manual cancellation needed)
    for event_id in &pregame_to_live {
        if let Some(gs) = s.game_state.get(event_id) {
            let tickers: Vec<&str> = gs.kalshi_tickers();
            let clv_orders = s.order_manager.clv_orders_for_tickers(&tickers);
            for clv_order in &clv_orders {
                let closing_mid = s.order_books
                    .get(&clv_order.ticker)
                    .and_then(|book| book.yes_mid())
                    .map(|mid| mid as i64);

                if let Some(mid) = closing_mid {
                    let clv = if clv_order.side == "Yes" {
                        mid - clv_order.price_cents
                    } else {
                        clv_order.price_cents - mid
                    };
                    let captured = if clv > 0 { "CAPTURED" } else { "MISSED" };
                    tracing::info!(
                        "CLV check: {} order {} at {}c, closing mid {}c, CLV = {}c [{}]",
                        clv_order.ticker, clv_order.order_id,
                        clv_order.price_cents, mid, clv, captured,
                    );
                    let _ = s.logger.log_clv_check(
                        &clv_order.order_id,
                        &clv_order.ticker,
                        &clv_order.side,
                        clv_order.price_cents,
                        mid,
                        clv,
                    );
                } else {
                    tracing::warn!(
                        "CLV check: no closing mid for {} (order {}), skipping",
                        clv_order.ticker, clv_order.order_id,
                    );
                }
            }
        }
    }

    // --- Fetch summary on new breaks (for updated win probs) ---
    for event_id in &new_breaks {
        match espn_poller.fetch_summary(event_id).await {
            Ok(summary) => {
                let win_prob = EspnPoller::latest_win_prob(&summary);
                let dk_ml = EspnPoller::extract_dk_moneyline(&summary).map(|(h, _)| h);
                if let Some(gs) = s.game_state.get_mut(event_id) {
                    gs.update_from_espn_summary(win_prob, dk_ml);
                    tracing::info!(
                        "Updated {} win_prob={:?}",
                        event_id, gs.espn_home_win_prob,
                    );
                }
            }
            Err(e) => tracing::warn!("Failed to fetch summary for {}: {:?}", event_id, e),
        }
    }

    log_game_summary(&s);

    // Orders use Kalshi's native expiration_ts — no manual TTL cancellation needed
    let signals = evaluate_strategies(&s, strategies);
    drop(s);

    for signal in signals {
        execute_signal(signal, state, kalshi_rest, dry_run).await;
    }
}

/// Log a summary of current game states.
fn log_game_summary(s: &BotState) {
    let live_count = s.game_state.live_games().len();
    let break_count = s.game_state.games_on_break().len();
    let pre_count = s.game_state.pre_game_games().len();
    let with_kalshi = s.game_state.games.values().filter(|g| g.has_kalshi()).count();
    let with_fair = s.game_state.games.values().filter(|g| g.espn_home_win_prob.is_some()).count();
    tracing::info!(
        "Games: {} live, {} break, {} pre | {} w/Kalshi, {} w/fair_value",
        live_count, break_count, pre_count, with_kalshi, with_fair
    );

    for gs in s.game_state.games.values() {
        if gs.has_kalshi() && gs.espn_home_win_prob.is_some() {
            let tickers: Vec<&str> = gs.kalshi_tickers();
            tracing::info!(
                "  {} v {} | phase={:?} | espn_hp={:?} | kalshi={:?}",
                gs.away_team, gs.home_team, gs.phase,
                gs.espn_home_win_prob, tickers,
            );
        }
    }
}

/// Handle a Kalshi WebSocket event.
async fn handle_kalshi_event(event: KalshiWsEvent, state: &SharedState) {
    let mut s = state.lock().await;
    match event {
        KalshiWsEvent::OrderBookSnapshot { market_ticker, snapshot } => {
            let book = s.order_books
                .entry(market_ticker.clone())
                .or_insert_with(|| LocalOrderBook::new(market_ticker.clone()));
            book.apply_snapshot(&snapshot);
            let prices = extract_book_prices(book);
            if let Some(gs) = s.game_state.get_mut_by_kalshi_ticker(&market_ticker)
                && let Some(market) = gs.kalshi_market_mut(&market_ticker)
            {
                market.update_prices(prices.0, prices.1, prices.2);
            }
            tracing::debug!("Book snapshot for {}", market_ticker);
        }
        KalshiWsEvent::OrderBookDelta(delta) => {
            let ticker = delta.market_ticker.clone();
            if let Some(book) = s.order_books.get_mut(&ticker) {
                book.apply_delta(&delta);
                let prices = extract_book_prices(book);
                if let Some(gs) = s.game_state.get_mut_by_kalshi_ticker(&ticker)
                    && let Some(market) = gs.kalshi_market_mut(&ticker)
                {
                    market.update_prices(prices.0, prices.1, prices.2);
                }
            }
        }
        KalshiWsEvent::Fill(fill) => {
            tracing::info!(
                "FILL: {} {:?} {} contracts @ {} yes_price",
                fill.market_ticker, fill.action, fill.count, fill.yes_price
            );
            // Record fill for PnL tracking
            let price = fill.yes_price.max(fill.no_price) as f64 / 100.0;
            let exposure_change = fill.count as f64 * price;
            s.risk.record_fill(exposure_change, 0.0); // PnL realized at settlement
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
        KalshiWsEvent::Connected => tracing::info!("Kalshi WebSocket connected"),
        KalshiWsEvent::Disconnected => tracing::warn!("Kalshi WebSocket disconnected"),
        KalshiWsEvent::Error(e) => tracing::error!("Kalshi WebSocket error: {}", e),
    }
}

/// Handle a Polymarket WebSocket event.
async fn handle_polymarket_event(event: PolymarketEvent, _state: &SharedState) {
    match event {
        PolymarketEvent::PriceUpdate { asset_id, best_bid, best_ask } => {
            if let (Some(_bid), Some(_ask)) = (best_bid, best_ask) {
                // Polymarket prices still streamed but not used for fair value.
                // Kept for future reference / logging.
                let _ = (asset_id, _bid, _ask);
            }
        }
        PolymarketEvent::TradeUpdate { .. } => {}
        PolymarketEvent::Connected => tracing::info!("Polymarket WebSocket connected"),
        PolymarketEvent::Disconnected => tracing::warn!("Polymarket WebSocket disconnected"),
    }
}

/// Cancel orders for finished games and clean up state.
async fn cleanup_finished_games(
    state: &SharedState,
    kalshi_rest: &Arc<KalshiRestClient>,
    dry_run: bool,
) {
    let mut s = state.lock().await;

    // Find all tickers for finished games
    let finished_tickers: Vec<String> = s.game_state.games.values()
        .filter(|g| g.phase == GamePhase::Final)
        .flat_map(|g| g.kalshi_tickers().into_iter().map(|t| t.to_string()))
        .collect();

    // Collect order IDs to cancel
    let mut orders_to_cancel = Vec::new();
    for ticker in &finished_tickers {
        let order_ids = s.order_manager.order_ids_for_market(ticker);
        for oid in order_ids {
            orders_to_cancel.push((oid, ticker.clone()));
        }
    }

    // Cancel orders (release lock during API calls)
    if !orders_to_cancel.is_empty() {
        tracing::info!("Cancelling {} orders for finished games", orders_to_cancel.len());
        drop(s);

        for (order_id, ticker) in &orders_to_cancel {
            if dry_run {
                tracing::info!("DRY RUN: would cancel order {} on finished {}", order_id, ticker);
            } else {
                match kalshi_rest.cancel_order(order_id).await {
                    Ok(()) => tracing::info!("Cancelled order {} (game finished: {})", order_id, ticker),
                    Err(e) => tracing::warn!("Failed to cancel order {}: {:?}", order_id, e),
                }
            }
        }

        // Update local state
        let mut s = state.lock().await;
        for (order_id, _) in &orders_to_cancel {
            s.order_manager.handle_cancel(order_id);
        }
        s.game_state.cleanup_finished();
        for ticker in &finished_tickers {
            s.order_books.remove(ticker);
        }
    } else {
        let count_before = s.game_state.games.len();
        s.game_state.cleanup_finished();
        let removed = count_before - s.game_state.games.len();
        if removed > 0 {
            tracing::info!("Cleaned up {} finished games", removed);
        }
    }
}

/// Discover new Kalshi markets that appeared after startup.
/// Re-fetches Kalshi events, maps any new ones, populates game states,
/// and subscribes to new tickers on the live WS connection.
async fn discover_new_markets(
    kalshi_rest: &Arc<KalshiRestClient>,
    espn_poller: &EspnPoller,
    state: &SharedState,
    ws_handle: Option<&KalshiWsHandle>,
    _demo: bool,
) {
    // Re-fetch Kalshi events (filtered to today)
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let date_tag = kalshi_date_tag(&today);
    let kalshi_events = match kalshi_rest
        .get_events_with_series(None, Some("KXNCAAMBGAME"), Some("open"), None, Some(100))
        .await
    {
        Ok(mut events) => {
            events.events.retain(|e| e.event_ticker.contains(&date_tag));
            events
        }
        Err(e) => {
            tracing::debug!("Discovery: failed to fetch Kalshi events: {:?}", e);
            return;
        }
    };

    // Build Kalshi matching data
    #[allow(clippy::type_complexity)]
    let kalshi_for_matching: Vec<(String, String, Vec<(String, String)>)> = kalshi_events
        .events
        .iter()
        .map(|e| {
            let markets: Vec<(String, String)> = e.markets.as_ref()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|m| {
                    let yes_sub = m.yes_sub_title.as_ref()?;
                    Some((m.ticker.clone(), yes_sub.clone()))
                })
                .collect();
            (e.event_ticker.clone(), e.title.clone(), markets)
        })
        .collect();

    // Build volume map
    let empty_markets = vec![];
    let kalshi_volume: HashMap<String, i64> = kalshi_events
        .events
        .iter()
        .flat_map(|e| e.markets.as_ref().unwrap_or(&empty_markets).iter())
        .filter_map(|m| m.volume.map(|v| (m.ticker.clone(), v)))
        .collect();

    // Check which Kalshi event tickers are already mapped
    let already_mapped: Vec<String> = {
        let s = state.lock().await;
        s.market_mapper.all_mapped_kalshi_tickers()
    };

    // Filter to only truly new events (any event with at least one unmapped market ticker)
    let new_events: Vec<_> = kalshi_for_matching
        .iter()
        .filter(|(_, _, markets)| {
            markets.iter().any(|(ticker, _)| !already_mapped.contains(ticker))
        })
        .collect();

    if new_events.is_empty() {
        return;
    }

    tracing::info!("Discovery: found {} new Kalshi events to map", new_events.len());

    // Re-fetch ESPN to get fresh game list for matching
    let espn_games = match espn_poller.fetch_scoreboard().await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("Discovery: ESPN fetch failed: {:?}", e);
            return;
        }
    };

    let espn_for_matching: Vec<(String, String)> = espn_games
        .iter()
        .map(|g| (g.event_id.clone(), format!("{} @ {}", g.away_team, g.home_team)))
        .collect();

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Run mapping (with empty polymarket since we don't re-discover poly mid-session)
    let mut s = state.lock().await;
    let tickers_before: std::collections::HashSet<String> =
        s.market_mapper.all_mapped_kalshi_tickers().into_iter().collect();

    if let Err(e) = s.market_mapper.resolve_deterministic(
        &espn_for_matching,
        &kalshi_for_matching,
        &[], // no new polymarket discovery
        &today,
    ) {
        tracing::warn!("Discovery: mapping failed: {:?}", e);
        return;
    }

    let tickers_after: std::collections::HashSet<String> =
        s.market_mapper.all_mapped_kalshi_tickers().into_iter().collect();

    let new_tickers: Vec<String> = tickers_after
        .difference(&tickers_before)
        .cloned()
        .collect();

    if new_tickers.is_empty() {
        return;
    }

    tracing::info!("Discovery: {} new Kalshi tickers mapped: {:?}", new_tickers.len(), new_tickers);

    // Populate game states for newly mapped games
    populate_game_states(&mut s, &espn_games, Some(&kalshi_volume));
    drop(s);

    // Fetch ESPN summaries for new games
    {
        let s = state.lock().await;
        let new_event_ids: Vec<String> = new_tickers
            .iter()
            .filter_map(|ticker| {
                s.game_state.games.values()
                    .find(|g| g.kalshi_markets.iter().any(|m| m.ticker == *ticker))
                    .map(|g| g.espn_event_id.clone())
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        drop(s);

        for event_id in &new_event_ids {
            match espn_poller.fetch_summary(event_id).await {
                Ok(summary) => {
                    let win_prob = EspnPoller::latest_win_prob(&summary);
                    let dk_ml = EspnPoller::extract_dk_moneyline(&summary).map(|(h, _)| h);
                    let mut s = state.lock().await;
                    if let Some(gs) = s.game_state.get_mut(event_id) {
                        gs.update_from_espn_summary(win_prob, dk_ml);
                        tracing::info!(
                            "Discovery: {} {} v {} | espn_hp={:?}",
                            event_id, gs.away_team, gs.home_team, gs.espn_home_win_prob,
                        );
                    }
                }
                Err(e) => tracing::warn!("Discovery: summary fetch failed for {}: {:?}", event_id, e),
            }
        }
    }

    // Subscribe to new tickers on the live WS connection
    if let Some(handle) = ws_handle {
        let count = handle.subscribe_additional(new_tickers);
        if count > 0 {
            tracing::info!("Discovery: subscribed to {} new tickers on WS", count);
        }
    } else {
        tracing::warn!("Discovery: no WS handle — new markets won't receive book updates");
    }
}

/// Extract bid/ask/mid from a local order book.
fn extract_book_prices(book: &LocalOrderBook) -> (Option<f64>, Option<f64>, Option<f64>) {
    (
        book.best_yes_bid().map(|l| l.price as f64),
        book.best_yes_ask().map(|l| l.price as f64),
        book.yes_mid(),
    )
}

/// Run all strategies and collect order signals.
/// Evaluates across ALL markets per game, picks the best signal per game.
fn evaluate_strategies(
    state: &BotState,
    strategies: &Strategies,
) -> Vec<OrderSignal> {
    let mut signals = Vec::new();

    if state.risk.is_halted() {
        return signals;
    }

    for game in state.game_state.games.values() {
        if !game.has_kalshi() {
            continue;
        }

        // Skip low-volume games
        if game.kalshi_total_volume() < strategies.min_volume {
            continue;
        }

        // Skip extreme prices — check if any market has mid in tradeable range
        let has_tradeable_mid = game.kalshi_markets.iter().any(|m| {
            m.yes_mid.is_some_and(|mid| (strategies.min_price_cents..=strategies.max_price_cents).contains(&mid))
        });
        if !has_tradeable_mid {
            continue;
        }

        // Compute exposure across all markets for this game
        let current_exposure: f64 = game.kalshi_markets.iter()
            .map(|m| state.order_manager.market_exposure(&m.ticker))
            .sum();

        let mut best_signal: Option<OrderSignal> = None;

        // Detailed break logging for debugging
        if game.phase.is_break() && game.has_kalshi() {
            let home_score = game.home_score.unwrap_or(0);
            let away_score = game.away_score.unwrap_or(0);
            for market in &game.kalshi_markets {
                let fair = game.fair_value_for_market(market);
                let kalshi_mid = market.yes_mid.map(|m| m / 100.0);
                tracing::info!(
                    "BREAK: {} v {} | {} | score {}-{} | {} YES={} bid={:?} ask={:?} mid={:?} | espn_fair={:?} | vol={:?}",
                    game.away_team, game.home_team, game.status_detail,
                    away_score, home_score,
                    market.ticker,
                    if market.is_home { "home" } else { "away" },
                    market.yes_bid, market.yes_ask, kalshi_mid,
                    fair, market.volume,
                );
            }
        }

        // Helper: check if any market in this game already has a resting order from the given strategy
        let has_resting_order = |strategy: &str| -> bool {
            game.kalshi_markets.iter().any(|m| {
                state.order_manager.has_strategy_order(&m.ticker, strategy)
            })
        };

        if game.phase.is_break()
            && !has_resting_order("break_ev")
            && let Some(mut signal) = strategies.break_ev.evaluate(game, &state.risk, current_exposure)
        {
            // Set expiration to now + TTL so Kalshi auto-expires when break likely ends
            let expire_at = chrono::Utc::now().timestamp() + strategies.order_ttl.as_secs() as i64;
            signal.expiration_ts = Some(expire_at);
            best_signal = Some(signal);
        }

        if matches!(game.phase, GamePhase::Live | GamePhase::Halftime | GamePhase::Break)
            && !has_resting_order("arb_scanner")
            && let Some(mut signal) = strategies.arb_scanner.evaluate(game, &state.risk, current_exposure)
            && best_signal.as_ref().is_none_or(|b| signal.size_dollars > b.size_dollars)
        {
            let expire_at = chrono::Utc::now().timestamp() + strategies.order_ttl.as_secs() as i64;
            signal.expiration_ts = Some(expire_at);
            best_signal = Some(signal);
        }

        if game.phase == GamePhase::PreGame
            && !has_resting_order("clv_hunter")
            && let Some(signal) = strategies.clv_hunter.evaluate(game, &state.risk, current_exposure)
            && best_signal.as_ref().is_none_or(|b| signal.size_dollars > b.size_dollars)
        {
            best_signal = Some(signal);
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

    // Only CLV orders go live; other strategies stay in dry-run mode for now
    let effective_dry_run = dry_run || signal.strategy != "clv_hunter";

    if effective_dry_run {
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
            // Look up current game phase for this ticker
            let placed_phase = s.game_state
                .get_mut_by_kalshi_ticker(&signal.kalshi_ticker)
                .map(|gs| gs.phase.clone())
                .unwrap_or(GamePhase::Unknown);
            s.order_manager.track_order(resp.order, signal.strategy.clone(), placed_phase);
            tracing::info!("Order placed successfully");
        }
        Err(e) => {
            tracing::error!("Failed to place order: {:?}", e);
        }
    }
}
