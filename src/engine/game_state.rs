use std::collections::HashMap;

use crate::espn::types::GamePhase;

/// Book state for a single Kalshi market ticker.
#[derive(Debug, Clone)]
pub struct KalshiMarketState {
    pub ticker: String,
    pub yes_bid: Option<f64>,
    pub yes_ask: Option<f64>,
    pub yes_mid: Option<f64>,
    pub volume: Option<i64>,
    /// true if YES on this ticker = home team wins
    pub is_home: bool,
}

impl KalshiMarketState {
    pub fn new(ticker: String, is_home: bool) -> Self {
        Self {
            ticker,
            yes_bid: None,
            yes_ask: None,
            yes_mid: None,
            volume: None,
            is_home,
        }
    }

    pub fn update_prices(&mut self, bid: Option<f64>, ask: Option<f64>, mid: Option<f64>) {
        self.yes_bid = bid;
        self.yes_ask = ask;
        self.yes_mid = mid;
    }
}

/// Unified game state combining data from all sources.
#[derive(Debug, Clone)]
pub struct GameState {
    pub espn_event_id: String,
    /// All Kalshi markets for this game (typically 2: one per team)
    pub kalshi_markets: Vec<KalshiMarketState>,
    pub polymarket_token_id: Option<String>,
    pub polymarket_is_home: bool, // true if polymarket YES token is for the home team

    pub home_team: String,
    pub away_team: String,
    pub home_score: Option<i32>,
    pub away_score: Option<i32>,
    pub phase: GamePhase,

    // Reference price — ESPN only
    pub espn_home_win_prob: Option<f64>,

    pub last_updated: std::time::Instant,
}

impl GameState {
    pub fn new(espn_event_id: String, home_team: String, away_team: String) -> Self {
        Self {
            espn_event_id,
            kalshi_markets: Vec::new(),
            polymarket_token_id: None,
            polymarket_is_home: false,
            home_team,
            away_team,
            home_score: None,
            away_score: None,
            phase: GamePhase::Unknown,
            espn_home_win_prob: None,
            last_updated: std::time::Instant::now(),
        }
    }

    /// Get fair value aligned with a specific Kalshi market's YES side.
    /// If the market's YES = home team, return home prob directly.
    /// If YES = away team, return 1 - home prob.
    pub fn fair_value_for_market(&self, market: &KalshiMarketState) -> Option<f64> {
        let home_fair = self.espn_home_win_prob?;
        if market.is_home {
            Some(home_fair)
        } else {
            Some(1.0 - home_fair)
        }
    }

    /// Get mutable Kalshi market state for a given ticker.
    pub fn kalshi_market_mut(&mut self, ticker: &str) -> Option<&mut KalshiMarketState> {
        self.kalshi_markets.iter_mut().find(|m| m.ticker == ticker)
    }

    /// Get all Kalshi tickers for this game.
    pub fn kalshi_tickers(&self) -> Vec<&str> {
        self.kalshi_markets.iter().map(|m| m.ticker.as_str()).collect()
    }

    /// Check if any Kalshi market is mapped.
    pub fn has_kalshi(&self) -> bool {
        !self.kalshi_markets.is_empty()
    }

    /// Get total volume across all Kalshi markets for this game.
    pub fn kalshi_total_volume(&self) -> i64 {
        self.kalshi_markets.iter().filter_map(|m| m.volume).sum()
    }

    /// Update ESPN win probability from a summary response.
    pub fn update_from_espn_summary(&mut self, win_prob: Option<f64>, _dk_home_moneyline: Option<f64>) {
        self.espn_home_win_prob = win_prob;
    }
}

/// Manages all active game states.
pub struct GameStateManager {
    pub games: HashMap<String, GameState>, // keyed by ESPN event ID
}

impl Default for GameStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl GameStateManager {
    pub fn new() -> Self {
        Self {
            games: HashMap::new(),
        }
    }

    pub fn get(&self, event_id: &str) -> Option<&GameState> {
        self.games.get(event_id)
    }

    pub fn get_mut(&mut self, event_id: &str) -> Option<&mut GameState> {
        self.games.get_mut(event_id)
    }

    pub fn upsert(&mut self, event_id: String, home_team: String, away_team: String) -> &mut GameState {
        self.games
            .entry(event_id.clone())
            .or_insert_with(|| GameState::new(event_id, home_team, away_team))
    }

    /// Find game by any Kalshi ticker (searches across all markets per game).
    pub fn get_mut_by_kalshi_ticker(&mut self, ticker: &str) -> Option<&mut GameState> {
        self.games.values_mut().find(|g| {
            g.kalshi_markets.iter().any(|m| m.ticker == ticker)
        })
    }

    /// Find game by Polymarket token ID.
    pub fn get_mut_by_polymarket_token(&mut self, token_id: &str) -> Option<&mut GameState> {
        self.games.values_mut().find(|g| g.polymarket_token_id.as_deref() == Some(token_id))
    }

    /// Get all games currently in a break state.
    pub fn games_on_break(&self) -> Vec<&GameState> {
        self.games.values().filter(|g| g.phase.is_break()).collect()
    }

    /// Get all pre-game games (for CLV).
    pub fn pre_game_games(&self) -> Vec<&GameState> {
        self.games
            .values()
            .filter(|g| g.phase == GamePhase::PreGame)
            .collect()
    }

    /// Get all live games (for arb scanning).
    pub fn live_games(&self) -> Vec<&GameState> {
        self.games
            .values()
            .filter(|g| matches!(g.phase, GamePhase::Live | GamePhase::Halftime | GamePhase::Break))
            .collect()
    }

    /// Remove finished games.
    pub fn cleanup_finished(&mut self) {
        self.games.retain(|_, g| g.phase != GamePhase::Final);
    }
}
