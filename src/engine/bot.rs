use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::engine::game_state::{GameStateManager, KalshiMarketState};
use crate::engine::logger::TradeLogger;
use crate::engine::market_mapper::MarketMapper;
use crate::engine::order_manager::OrderManager;
use crate::engine::risk::RiskManager;
use crate::espn::poller::EspnPoller;
use crate::espn::types::GameInfo;
use crate::kalshi::orderbook::LocalOrderBook;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::types::GetEventsResponse;
use crate::strategies::Strategy;

/// All shared mutable state for the bot.
pub struct BotState {
    pub game_state: GameStateManager,
    pub market_mapper: MarketMapper,
    pub order_books: HashMap<String, LocalOrderBook>,
    pub risk: RiskManager,
    pub order_manager: OrderManager,
    pub logger: TradeLogger,
}

pub type SharedState = Arc<Mutex<BotState>>;

/// Holds registered strategies and shared filter parameters.
pub struct StrategyRegistry {
    pub strategies: Vec<Box<dyn Strategy>>,
    /// Which strategies are allowed to place real orders.
    pub live_strategies: Vec<String>,
    pub min_volume: i64,
    pub min_price_cents: f64,
    pub max_price_cents: f64,
    pub order_ttl: Duration,
}

/// Build initial BotState from config.
pub fn create_bot_state(config: &Config) -> Result<BotState> {
    let logger = TradeLogger::new(&config.logging.db_path)?;
    tracing::info!("Trade logger initialized at {}", config.logging.db_path);

    let market_mapper = MarketMapper::new(&config.logging.cache_path);

    Ok(BotState {
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
    })
}

/// Build the strategy registry from config.
pub fn create_strategies(config: &Config) -> StrategyRegistry {
    use crate::strategies::arb_scanner::ArbScanner;
    use crate::strategies::break_ev::BreakEvQuoter;
    use crate::strategies::clv_hunter::ClvHunter;

    let strategies: Vec<Box<dyn Strategy>> = vec![
        Box::new(BreakEvQuoter::new(config.strategy.break_ev_min_edge)),
        Box::new(ArbScanner::new(config.strategy.arb_scanner_min_edge)),
        Box::new(ClvHunter::new(config.strategy.clv_hunter_min_edge)),
    ];

    tracing::info!("Live strategies: {:?}", config.strategy.live_strategies);

    StrategyRegistry {
        strategies,
        live_strategies: config.strategy.live_strategies.clone(),
        min_volume: config.strategy.min_volume,
        min_price_cents: config.strategy.min_price_cents,
        max_price_cents: config.strategy.max_price_cents,
        order_ttl: Duration::from_secs(config.strategy.order_ttl_secs),
    }
}

/// Populate game states from ESPN data, optionally setting Kalshi volume.
pub fn populate_game_states(
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
pub async fn fetch_summaries_for_games(espn_poller: &EspnPoller, state: &SharedState) {
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

/// Convert "2026-03-09" to Kalshi ticker date format "26MAR09".
pub fn kalshi_date_tag(date_str: &str) -> String {
    let dt = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .expect("invalid date format");
    dt.format("%y%b%d").to_string().to_uppercase()
}

/// All KXNCAAMB* series we want to fetch from Kalshi.
/// KXNCAAMBGAME = individual game markets, the rest are conference tournament series
/// where the championship game markets serve as game-winner markets.
const CBB_SERIES: &[&str] = &[
    "KXNCAAMBGAME",
    "KXNCAAMBSOCON",
    "KXNCAAMBSBELT",
    "KXNCAAMBACC",
    "KXNCAAMBSEC",
    "KXNCAAMBSWAC",
    "KXNCAAMBWCC",
    "KXNCAAMBCAA",
    "KXNCAAMBBSKY",
    "KXNCAAMBNEC",
    "KXNCAAMBAE",
    "KXNCAAMBWAC",
    "KXNCAAMBMEAC",
    "KXNCAAMBPAT",
    "KXNCAAMBB12",
    "KXNCAAMBBTEN",
    "KXNCAAMBBE",
    "KXNCAAMBHL",
    "KXNCAAMBMWC",
    "KXNCAAMBAAC",
    "KXNCAAMBSLAND",
    "KXNCAAMBOVC",
    "KXNCAAMBMAAC",
    "KXNCAAMBASUN",
    "KXNCAAMBCUSA",
    "KXNCAAMBMVC",
];

/// Fetch all CBB events from Kalshi across multiple series.
pub async fn fetch_all_kalshi_cbb_events(
    kalshi_rest: &KalshiRestClient,
) -> GetEventsResponse {
    let mut all_events = Vec::new();

    for series in CBB_SERIES {
        match kalshi_rest
            .get_events_with_series(None, Some(series), Some("open"), None, Some(200))
            .await
        {
            Ok(resp) => {
                if !resp.events.is_empty() {
                    tracing::debug!("Fetched {} events from series {}", resp.events.len(), series);
                    all_events.extend(resp.events);
                }
            }
            Err(e) => {
                tracing::debug!("Failed to fetch series {}: {:?}", series, e);
            }
        }
    }

    GetEventsResponse {
        events: all_events,
        cursor: None,
    }
}

/// For conference tournament events, filter markets to only active finalists
/// and synthesize a game-style title ("TeamA vs TeamB") for matching.
/// Returns (event_ticker, synthetic_title, active_markets).
pub fn normalize_conference_tournament_event(
    event: &crate::kalshi::types::Event,
) -> Option<(String, String, Vec<(String, String)>)> {
    let markets = event.markets.as_ref()?;

    // Find active markets with real bids (the finalists)
    let active: Vec<_> = markets
        .iter()
        .filter(|m| m.status == "active" && m.yes_bid.unwrap_or(0) > 0)
        .collect();

    if active.len() != 2 {
        return None; // Not a championship matchup (or already resolved)
    }

    // Extract team names from yes_sub_title or market title
    let team_a = active[0].yes_sub_title.as_ref()
        .cloned()
        .unwrap_or_else(|| extract_team_from_title(&active[0].title));
    let team_b = active[1].yes_sub_title.as_ref()
        .cloned()
        .unwrap_or_else(|| extract_team_from_title(&active[1].title));

    if team_a.is_empty() || team_b.is_empty() {
        return None;
    }

    // Synthesize a title that fuzzy_game_match can handle: "TeamA at TeamB"
    let synthetic_title = format!("{} at {}", team_a, team_b);

    let market_entries = vec![
        (active[0].ticker.clone(), team_a),
        (active[1].ticker.clone(), team_b),
    ];

    Some((event.event_ticker.clone(), synthetic_title, market_entries))
}

/// Extract team name from tournament market title like
/// "Will East Tennessee St. be the Southern Conference tournament champions?"
fn extract_team_from_title(title: &str) -> String {
    let s = title.strip_prefix("Will ").unwrap_or(title);
    if let Some(idx) = s.find(" be the ") {
        s[..idx].to_string()
    } else {
        String::new()
    }
}

/// Extract bid/ask/mid from a local order book.
pub fn extract_book_prices(book: &LocalOrderBook) -> (Option<f64>, Option<f64>, Option<f64>) {
    (
        book.best_yes_bid().map(|l| l.price as f64),
        book.best_yes_ask().map(|l| l.price as f64),
        book.yes_mid(),
    )
}
