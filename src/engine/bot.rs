use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
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
use crate::strategies::Strategy;

/// One row in the break evaluation log.
#[derive(Clone, serde::Serialize)]
pub struct BreakEvalLog {
    pub timestamp: String,
    pub away_team: String,
    pub home_team: String,
    pub score: String,
    pub status: String,
    /// Per-market eval details
    pub markets: Vec<BreakMarketEval>,
    /// Overall result for this game: "PLACED", "DRY_RUN", "NO_SIGNAL", or skip reason
    pub result: String,
}

#[derive(Clone, serde::Serialize)]
pub struct BreakMarketEval {
    pub ticker: String,
    pub is_home: bool,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub mid: Option<f64>,
    pub fair: Option<f64>,
    pub alo_price: Option<i64>,
    pub edge_raw: Option<f64>,
    pub edge_after_fees: Option<f64>,
    pub side: Option<String>,
}

/// Core mutable state: game data, risk, and order tracking.
/// Order books and break eval log are stored in separate locks to reduce contention.
pub struct BotState {
    pub game_state: GameStateManager,
    pub risk: RiskManager,
    pub order_manager: OrderManager,
    pub started_at: DateTime<Utc>,
}

pub type SharedState = Arc<Mutex<BotState>>;

/// Order books in a RwLock: WS deltas write, strategies/dashboard read concurrently.
/// Separated from BotState so orderbook updates (highest frequency) don't block strategy evaluation.
pub type SharedOrderBooks = Arc<tokio::sync::RwLock<HashMap<String, LocalOrderBook>>>;

/// Break evaluation log for the dashboard. Separated from BotState since it's dashboard-only.
pub type SharedBreakLog = Arc<std::sync::Mutex<VecDeque<BreakEvalLog>>>;

/// Thread-safe trade logger, independent of BotState to reduce lock contention.
/// Uses std::sync::Mutex since all operations are fast (single SQLite INSERT).
pub type SharedLogger = Arc<std::sync::Mutex<TradeLogger>>;

/// Thread-safe market mapper, independent of BotState.
/// Uses tokio::sync::Mutex since discovery may hold the lock across async calls.
pub type SharedMapper = Arc<Mutex<MarketMapper>>;

/// Holds registered strategies and shared filter parameters.
pub struct StrategyRegistry {
    pub strategies: Vec<Box<dyn Strategy>>,
    /// Which strategies are allowed to place real orders.
    pub live_strategies: Vec<String>,
    pub min_volume: i64,
    pub min_price_cents: f64,
    pub max_price_cents: f64,
    pub order_ttl: Duration,
    /// Hard cap on total contracts per game (across all tickers/orders).
    pub max_contracts_per_game: i64,
}

/// Build initial BotState, TradeLogger, and MarketMapper from config.
/// Logger and mapper are returned separately so they can be wrapped in their own locks.
pub fn create_bot_state(config: &Config) -> Result<(BotState, TradeLogger, MarketMapper)> {
    let logger = TradeLogger::new(&config.logging.db_path)?;
    tracing::info!("Trade logger initialized at {}", config.logging.db_path);

    let market_mapper = MarketMapper::new(&config.logging.cache_path);

    let state = BotState {
        game_state: GameStateManager::new(),
        risk: RiskManager::new(),
        order_manager: OrderManager::new(),
        started_at: Utc::now(),
    };
    Ok((state, logger, market_mapper))
}

/// Build the strategy registry from config.
pub fn create_strategies(config: &Config) -> StrategyRegistry {
    use crate::strategies::break_ev::BreakEvQuoter;
    use crate::strategies::clv_hunter::ClvHunter;

    let strategies: Vec<Box<dyn Strategy>> = vec![
        Box::new(BreakEvQuoter::new(
            config.strategy.break_ev_min_edge,
            config.strategy.contracts_per_pct_edge,
            config.strategy.min_trade_contracts,
        )),
        Box::new(ClvHunter::new(
            config.strategy.clv_hunter_min_edge,
            config.strategy.contracts_per_pct_edge,
            config.strategy.min_trade_contracts,
        )),
    ];

    tracing::info!("Live strategies: {:?}", config.strategy.live_strategies);

    StrategyRegistry {
        strategies,
        live_strategies: config.strategy.live_strategies.clone(),
        min_volume: config.strategy.min_volume,
        min_price_cents: config.strategy.min_price_cents,
        max_price_cents: config.strategy.max_price_cents,
        order_ttl: Duration::from_secs(config.strategy.order_ttl_secs),
        max_contracts_per_game: config.strategy.max_contracts_per_game,
    }
}

/// Populate game states from ESPN data, optionally setting Kalshi volume.
pub fn populate_game_states(
    s: &mut BotState,
    mapper: &MarketMapper,
    games: &[GameInfo],
    kalshi_volume: Option<&HashMap<String, i64>>,
) {
    for game in games {
        let kalshi_market_infos: Vec<_> = mapper.kalshi_markets_for_game(&game.event_id).to_vec();
        let poly_token = mapper.polymarket_token(&game.event_id).map(ToString::to_string);
        let poly_is_home = mapper.polymarket_is_home_team(&game.event_id);

        let gs = s.game_state.upsert(
            game.event_id.clone(),
            game.home_team.clone(),
            game.away_team.clone(),
        );
        gs.update_from_espn(game);

        // Set up Kalshi markets if not already present
        if gs.kalshi_markets.is_empty() && !kalshi_market_infos.is_empty() {
            for info in &kalshi_market_infos {
                let is_home = MarketMapper::yes_is_home_team(&game.home_team, &info.yes_sub_title);
                let mut market = KalshiMarketState::new(info.ticker.clone(), is_home);
                if let Some(vol_map) = kalshi_volume {
                    market.volume = vol_map.get(&info.ticker).copied();
                }
                gs.kalshi_markets.push(market);
            }
        }

        gs.polymarket_token_id = poly_token;
        gs.polymarket_is_home = poly_is_home;

        // Register tickers in reverse index for O(1) lookups
        for info in &kalshi_market_infos {
            s.game_state.register_ticker(&info.ticker, &game.event_id);
        }
    }
}

/// Fetch an ESPN summary and update the game state's win probability.
/// Acquires and releases the state lock around the update (not during the HTTP call).
pub async fn fetch_and_apply_summary(
    espn_poller: &EspnPoller,
    state: &SharedState,
    event_id: &str,
    log_prefix: &str,
) {
    match espn_poller.fetch_summary(event_id).await {
        Ok(summary) => {
            let win_prob = EspnPoller::latest_win_prob(&summary);
            let mut s = state.lock().await;
            if let Some(gs) = s.game_state.get_mut(event_id) {
                let old_prob = gs.espn_home_win_prob;
                gs.update_from_espn_summary(win_prob);
                tracing::info!(
                    "{}{}: {} v {} | espn_hp={:?} (was {:?})",
                    log_prefix, event_id, gs.away_team, gs.home_team,
                    gs.espn_home_win_prob, old_prob,
                );
            }
        }
        Err(e) => tracing::warn!("{}Failed summary for {}: {:?}", log_prefix, event_id, e),
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
        fetch_and_apply_summary(espn_poller, state, event_id, "").await;
    }
}
