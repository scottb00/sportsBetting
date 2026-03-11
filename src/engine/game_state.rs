use std::collections::HashMap;

use crate::espn::types::GamePhase;

/// Book state for a single Kalshi market ticker.
/// Prices (bid/ask/mid) live in LocalOrderBook, not here — use BotState::book_prices().
#[derive(Debug, Clone)]
pub struct KalshiMarketState {
    pub ticker: String,
    pub volume: Option<i64>,
    /// true if YES on this ticker = home team wins
    pub is_home: bool,
}

impl KalshiMarketState {
    pub fn new(ticker: String, is_home: bool) -> Self {
        Self {
            ticker,
            volume: None,
            is_home,
        }
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

    /// Game start time as unix timestamp (seconds), from ESPN.
    pub start_time_ts: Option<i64>,
    /// Status detail from ESPN, e.g. "Halftime", "8:42 - 2nd Half"
    pub status_detail: String,
    /// Last play text, e.g. "Official TV Timeout"
    pub last_play: Option<String>,
    /// Last play type, e.g. "OfficialTVTimeOut"
    pub last_play_type: Option<String>,

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
            start_time_ts: None,
            status_detail: String::new(),
            last_play: None,
            last_play_type: None,
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

    /// Update game state fields from an ESPN scoreboard poll.
    pub fn update_from_espn(&mut self, game: &crate::espn::types::GameInfo) {
        self.phase = game.game_phase.clone();
        self.home_score = game.home_score;
        self.away_score = game.away_score;
        self.status_detail = game.status_detail.clone();
        self.last_play = game.last_play.clone();
        self.last_play_type = game.last_play_type.clone();
        if game.start_time_ts.is_some() {
            self.start_time_ts = game.start_time_ts;
        }
        self.last_updated = std::time::Instant::now();
    }

    /// Update ESPN win probability from a summary response.
    pub fn update_from_espn_summary(&mut self, win_prob: Option<f64>) {
        self.espn_home_win_prob = win_prob;
    }
}

/// Manages all active game states.
pub struct GameStateManager {
    pub games: HashMap<String, GameState>, // keyed by ESPN event ID
    /// Reverse index: Kalshi ticker -> ESPN event ID (for O(1) ticker lookups).
    ticker_to_event: HashMap<String, String>,
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
            ticker_to_event: HashMap::new(),
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

    /// Register a Kalshi ticker in the reverse index.
    pub fn register_ticker(&mut self, ticker: &str, event_id: &str) {
        self.ticker_to_event.insert(ticker.to_string(), event_id.to_string());
    }

    /// Find game by any Kalshi ticker (O(1) via reverse index).
    pub fn get_by_kalshi_ticker(&self, ticker: &str) -> Option<&GameState> {
        let event_id = self.ticker_to_event.get(ticker)?;
        self.games.get(event_id)
    }

    /// Get all Kalshi tickers (as owned Strings) for the game containing the given ticker.
    pub fn game_tickers_for(&self, ticker: &str) -> Vec<String> {
        self.get_by_kalshi_ticker(ticker)
            .map(|g| g.kalshi_tickers().into_iter().map(|t| t.to_string()).collect())
            .unwrap_or_default()
    }

    /// Get all Kalshi market states for the game containing the given ticker.
    pub fn game_markets_for(&self, ticker: &str) -> Vec<KalshiMarketState> {
        self.get_by_kalshi_ticker(ticker)
            .map(|g| g.kalshi_markets.clone())
            .unwrap_or_default()
    }

    /// Find game mutably by any Kalshi ticker (O(1) via reverse index).
    pub fn get_mut_by_kalshi_ticker(&mut self, ticker: &str) -> Option<&mut GameState> {
        let event_id = self.ticker_to_event.get(ticker)?.clone();
        self.games.get_mut(&event_id)
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
        self.games.values().filter(|g| g.phase == GamePhase::PreGame).collect()
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
        // Remove ticker index entries for finished games
        let finished_tickers: Vec<String> = self.games.values()
            .filter(|g| g.phase == GamePhase::Final)
            .flat_map(|g| g.kalshi_markets.iter().map(|m| m.ticker.clone()))
            .collect();
        for ticker in &finished_tickers {
            self.ticker_to_event.remove(ticker);
        }
        self.games.retain(|_, g| g.phase != GamePhase::Final);
    }
}
