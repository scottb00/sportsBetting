use std::collections::HashMap;

use crate::espn::types::GamePhase;
use crate::sportsbooks::types::SportsbookSpread;

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

    // Reference prices
    pub espn_home_win_prob: Option<f64>,
    /// Sportsbook-implied home win probabilities (devigged).
    /// Keys: "dk" (DraftKings via Odds API), "pinnacle", "fanduel", etc.
    pub sportsbook_probs: HashMap<String, f64>,
    /// Composite sportsbook spread (raw bid/ask from multiple books via The Odds API).
    pub sportsbook_spread: Option<SportsbookSpread>,

    /// Game start time as unix timestamp (seconds), from ESPN.
    pub start_time_ts: Option<i64>,
    /// Status detail from ESPN, e.g. "Halftime", "8:42 - 2nd Half"
    pub status_detail: String,
    /// Display clock from ESPN, e.g. "8:42" (minutes:seconds remaining in period)
    pub display_clock: Option<String>,
    /// Period number from ESPN (1 = 1st half, 2 = 2nd half for CBB; 3+ = OT)
    pub period: Option<i32>,
    /// Last play text, e.g. "Official TV Timeout"
    pub last_play: Option<String>,
    /// Last play type, e.g. "OfficialTVTimeOut"
    pub last_play_type: Option<String>,

    pub last_updated: std::time::Instant,
    /// Absolute unix timestamp (seconds) when break_ev orders should expire.
    /// Computed once on break entry (start + duration - safety buffer).
    /// All orders during the same break share this expiration.
    pub break_expires_at: Option<i64>,
    /// Unix timestamp (seconds) when the current break started.
    /// Used to filter sportsbook odds — only books updated after break start are relevant.
    pub break_started_at: Option<i64>,
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
            sportsbook_probs: HashMap::new(),
            sportsbook_spread: None,
            start_time_ts: None,
            status_detail: String::new(),
            display_clock: None,
            period: None,
            last_play: None,
            last_play_type: None,
            last_updated: std::time::Instant::now(),
            break_expires_at: None,
            break_started_at: None,
        }
    }

    /// Get the sportsbook spread aligned to a specific market's YES side.
    /// During breaks, only uses books updated after the break started.
    /// Returns (bid, offer) where bid < offer. Returns (None, None) if no spread data.
    pub fn spread_for_market(&self, market: &KalshiMarketState) -> (Option<f64>, Option<f64>) {
        let spread = match &self.sportsbook_spread {
            Some(s) => s,
            None => return (None, None),
        };

        // During breaks, filter to only post-break book updates
        if let Some(break_ts) = self.break_started_at {
            let (bid_home, offer_home, count) = spread.post_break_spread(break_ts);
            if count > 0 {
                return if market.is_home {
                    (bid_home, offer_home)
                } else {
                    (offer_home.map(|o| 1.0 - o), bid_home.map(|b| 1.0 - b))
                };
            }
            // No post-break books yet — fall through to regular spread
        }

        spread.aligned_spread(market.is_home)
    }

    /// Parse display_clock ("8:42") into minutes remaining as f64.
    pub fn minutes_remaining(&self) -> Option<f64> {
        let clock = self.display_clock.as_deref()?;
        let (m, s) = clock.split_once(':')?;
        let mins: f64 = m.parse().ok()?;
        let secs: f64 = s.parse().ok()?;
        Some(mins + secs / 60.0)
    }

    /// Returns true if we're in the 2nd half (or OT) with fewer than `threshold_mins` remaining.
    /// Used to suppress trading in the final minutes of the game.
    pub fn is_final_minutes(&self, threshold_mins: f64) -> bool {
        if self.period.unwrap_or(0) < 2 {
            return false;
        }
        self.minutes_remaining().is_some_and(|m| m < threshold_mins)
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

    /// True only for breaks worth trading: halftime and TV/media timeouts.
    /// Team timeouts (~30s) are too short — a passive order can't fill meaningfully.
    pub fn is_tradeable_break(&self) -> bool {
        match self.phase {
            GamePhase::Halftime => true,
            GamePhase::Break => {
                [self.last_play_type.as_deref(), self.last_play.as_deref()]
                    .iter()
                    .filter_map(|f| *f)
                    .any(|t| {
                        let l = t.to_lowercase();
                        l.contains("tv") || l.contains("official")
                    })
            }
            _ => false,
        }
    }

    /// Returns the absolute expiration timestamp (unix seconds) for break_ev orders.
    /// Computed once when the break starts; all orders during the same break share this value.
    pub fn break_expiration_ts(&self) -> Option<i64> {
        self.break_expires_at
    }

    /// Returns true if there's enough time left in the break for a new order to fill.
    pub fn break_has_time_for_order(&self, min_remaining_secs: i64) -> bool {
        match self.break_expires_at {
            Some(expires) => {
                let remaining = expires - chrono::Utc::now().timestamp();
                remaining >= min_remaining_secs
            }
            None => true, // unknown break timing (e.g. bot restart mid-break) — allow
        }
    }

    /// Update game state fields from an ESPN scoreboard poll.
    pub fn update_from_espn(&mut self, game: &crate::espn::types::GameInfo) {
        let was_break = self.phase.is_break();
        self.phase = game.game_phase.clone();
        // Compute absolute break expiration once on break entry
        if self.phase.is_break() && !was_break {
            let now = chrono::Utc::now().timestamp();
            self.break_started_at = Some(now);
            let (duration, safety_buffer): (i64, i64) = match self.phase {
                GamePhase::Halftime => (900, 60),
                GamePhase::Break => (135, 45),
                _ => (60, 30),
            };
            self.break_expires_at = Some(now + duration - safety_buffer);
        } else if !self.phase.is_break() {
            self.break_expires_at = None;
            self.break_started_at = None;
        }
        self.home_score = game.home_score;
        self.away_score = game.away_score;
        self.status_detail = game.status_detail.clone();
        self.display_clock = game.display_clock.clone();
        self.period = game.period;
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
