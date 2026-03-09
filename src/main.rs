use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use sports_betting::config::Config;
use sports_betting::engine::bot::{
    self, SharedState, create_bot_state, create_strategies, kalshi_date_tag,
    populate_game_states, fetch_summaries_for_games,
};
use sports_betting::engine::handlers;
use sports_betting::engine::notifier::Notifier;
use sports_betting::espn::poller::{EspnPoller, GameTracker};
use sports_betting::kalshi::auth::KalshiAuth;
use sports_betting::kalshi::rest::KalshiRestClient;
use sports_betting::kalshi::websocket::{KalshiWsClient, KalshiWsHandle};
use sports_betting::polymarket::client::PolymarketClient;

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

    let notifier = config.notify.as_ref().map(|nc| {
        tracing::info!("Notifications enabled via ntfy.sh topic: {}", nc.ntfy_topic);
        Notifier::new(nc)
    });

    let mut bot_state = create_bot_state(&config)?;
    let strategies = create_strategies(&config);

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    if !bot_state.market_mapper.load_cache(&today) {
        tracing::info!("No cached mappings for today, will resolve after fetching markets");
    }

    // --- Initial market discovery & mapping ---
    let (_espn_games, _kalshi_volume) = fetch_initial_markets(
        &espn_poller, &poly_client, &kalshi_rest, &today, &mut bot_state,
    ).await?;

    let state: SharedState = Arc::new(Mutex::new(bot_state));

    // Fetch initial ESPN win probs
    fetch_summaries_for_games(&espn_poller, &state).await;

    // --- Connect WebSockets ---
    let (mut kalshi_rx, kalshi_ws_handle) = connect_kalshi_ws(&auth, &config, &state).await?;
    let mut poly_rx = connect_poly_ws(&state).await?;

    // --- Main event loop ---
    let mut game_tracker = GameTracker::new();
    let mut scoreboard_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(config.polling.scoreboard_interval_secs));
    let mut cleanup_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(config.intervals.cleanup_secs));
    let mut discovery_interval =
        tokio::time::interval(tokio::time::Duration::from_secs(config.intervals.discovery_secs));
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
                    current_day = now_day;
                }
                handlers::handle_scoreboard_tick(
                    &espn_poller, &state, &mut game_tracker,
                    &strategies, &kalshi_rest, dry_run,
                    notifier.as_ref(),
                ).await;
            }
            _ = cleanup_interval.tick() => {
                handlers::cleanup_finished_games(&state, &kalshi_rest, dry_run).await;
            }
            _ = discovery_interval.tick() => {
                handlers::discover_new_markets(
                    &kalshi_rest, &espn_poller, &state,
                    kalshi_ws_handle.as_ref(),
                ).await;
            }
            Some(event) = async {
                match &mut kalshi_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handlers::handle_kalshi_event(event, &state, notifier.as_ref()).await;
            }
            Some(event) = async {
                match &mut poly_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                handlers::handle_polymarket_event(event, &state).await;
            }
        }
    }
}

/// Fetch ESPN, Kalshi, and Polymarket events; run market mapping; populate game states.
/// Returns ESPN games and Kalshi volume map for later use.
async fn fetch_initial_markets(
    espn_poller: &EspnPoller,
    poly_client: &PolymarketClient,
    kalshi_rest: &Arc<KalshiRestClient>,
    today: &str,
    bot_state: &mut bot::BotState,
) -> Result<(Vec<sports_betting::espn::types::GameInfo>, HashMap<String, i64>)> {
    tracing::info!("Fetching initial market data...");
    let espn_games = espn_poller.fetch_scoreboard().await?;
    tracing::info!("Found {} ESPN games", espn_games.len());

    let poly_events = poly_client.fetch_cbb_events().await.unwrap_or_else(|e| {
        tracing::warn!("Failed to fetch Polymarket events: {:?}", e);
        vec![]
    });
    tracing::info!("Found {} Polymarket events", poly_events.len());

    let date_tag = kalshi_date_tag(today);
    let kalshi_events = bot::fetch_all_kalshi_cbb_events(kalshi_rest).await;
    let kalshi_events = {
        let mut filtered = kalshi_events;
        let before = filtered.events.len();
        // Keep KXNCAAMBGAME events matching today's date tag,
        // plus all conference tournament events (already filtered to status=open)
        filtered.events.retain(|e| {
            e.event_ticker.contains(&date_tag)
                || !e.event_ticker.starts_with("KXNCAAMBGAME")
        });
        tracing::info!(
            "Found {} Kalshi CBB events ({} total, filtered to {} + conference tournaments)",
            filtered.events.len(), before, date_tag,
        );
        filtered
    };

    // Build matching data
    let espn_for_matching: Vec<(String, String)> = espn_games
        .iter()
        .map(|g| (g.event_id.clone(), format!("{} @ {}", g.away_team, g.home_team)))
        .collect();

    #[allow(clippy::type_complexity)]
    let kalshi_for_matching: Vec<(String, String, Vec<(String, String)>)> = kalshi_events
        .events
        .iter()
        .filter_map(|e| {
            // Conference tournament events need special handling
            if !e.event_ticker.starts_with("KXNCAAMBGAME") {
                return bot::normalize_conference_tournament_event(e);
            }
            let markets: Vec<(String, String)> = e.markets.as_ref()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|m| {
                    let yes_sub = m.yes_sub_title.as_ref()?;
                    Some((m.ticker.clone(), yes_sub.clone()))
                })
                .collect();
            Some((e.event_ticker.clone(), e.title.clone(), markets))
        })
        .collect();

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
                Some(d) => d.starts_with(today) || d.starts_with(&tomorrow),
                None => e.markets.iter().any(|m| {
                    m.game_start_time.as_ref().is_some_and(|t| {
                        t.starts_with(today) || t.starts_with(&tomorrow)
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

    if let Err(e) = bot_state.market_mapper.resolve_deterministic(
        &espn_for_matching,
        &kalshi_for_matching,
        &poly_for_matching,
        today,
    ) {
        tracing::error!("Market mapping failed: {:?}", e);
    }

    populate_game_states(bot_state, &espn_games, Some(&kalshi_volume));

    Ok((espn_games, kalshi_volume))
}

/// Connect to Kalshi WebSocket for all mapped market tickers.
async fn connect_kalshi_ws(
    auth: &KalshiAuth,
    config: &Config,
    state: &SharedState,
) -> Result<(Option<tokio::sync::mpsc::UnboundedReceiver<sports_betting::kalshi::websocket::KalshiWsEvent>>, Option<KalshiWsHandle>)> {
    let kalshi_tickers: Vec<String> = {
        let s = state.lock().await;
        s.market_mapper.all_mapped_kalshi_tickers()
    };

    let kalshi_ws = KalshiWsClient::new(auth.clone(), config.kalshi.demo);
    if !kalshi_tickers.is_empty() {
        tracing::info!("Connecting Kalshi WS for {} markets", kalshi_tickers.len());
        let (rx, handle) = kalshi_ws.connect(kalshi_tickers).await?;
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
        Ok(Some(PolymarketClient::connect_ws(poly_tokens).await?))
    } else {
        tracing::warn!("No Polymarket tokens mapped, skipping WS connection");
        Ok(None)
    }
}
