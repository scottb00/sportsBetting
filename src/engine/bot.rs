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
    use crate::strategies::break_ev::BreakEvQuoter;
    use crate::strategies::clv_hunter::ClvHunter;

    let strategies: Vec<Box<dyn Strategy>> = vec![
        Box::new(BreakEvQuoter::new(config.strategy.break_ev_min_edge)),
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
        let _kalshi_title = s.market_mapper.kalshi_title(&game.event_id).map(|t| t.to_string());
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
