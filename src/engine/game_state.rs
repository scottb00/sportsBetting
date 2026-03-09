use std::collections::HashMap;

use crate::espn::types::GamePhase;

/// Unified game state combining data from all sources.
#[derive(Debug, Clone)]
pub struct GameState {
    pub espn_event_id: String,
    pub kalshi_ticker: Option<String>,
    pub kalshi_is_home: bool, // true if kalshi ticker is for the home team
    pub polymarket_token_id: Option<String>,
    pub polymarket_is_home: bool, // true if polymarket YES token is for the home team

    pub home_team: String,
    pub away_team: String,
    pub home_score: Option<i32>,
    pub away_score: Option<i32>,
    pub phase: GamePhase,

    // Reference prices
    pub espn_home_win_prob: Option<f64>,
    pub dk_home_implied_prob: Option<f64>,
    pub polymarket_home_prob: Option<f64>, // mid (for logging / initial direction)
    pub polymarket_home_bid: Option<f64>,  // best bid converted to home-team prob
    pub polymarket_home_ask: Option<f64>,  // best ask converted to home-team prob

    // Kalshi book state
    pub kalshi_yes_bid: Option<f64>,
    pub kalshi_yes_ask: Option<f64>,
    pub kalshi_yes_mid: Option<f64>,
    pub kalshi_volume: Option<i64>,

    pub last_updated: std::time::Instant,
}

impl GameState {
    pub fn new(espn_event_id: String, home_team: String, away_team: String) -> Self {
        Self {
            espn_event_id,
            kalshi_ticker: None,
            kalshi_is_home: true,
            polymarket_token_id: None,
            polymarket_is_home: false, // default: Poly YES = first listed = away
            home_team,
            away_team,
            home_score: None,
            away_score: None,
            phase: GamePhase::Unknown,
            espn_home_win_prob: None,
            dk_home_implied_prob: None,
            polymarket_home_prob: None,
            polymarket_home_bid: None,
            polymarket_home_ask: None,
            kalshi_yes_bid: None,
            kalshi_yes_ask: None,
            kalshi_yes_mid: None,
            kalshi_volume: None,
            last_updated: std::time::Instant::now(),
        }
    }

    /// Maximum Polymarket spread (in probability) to include in consensus.
    /// Markets wider than this are too illiquid to be reliable references.
    const POLY_MAX_SPREAD: f64 = 0.15;

    /// Compute consensus fair value from available reference prices.
    /// Uses polymarket mid. Requires 2+ sources.
    pub fn consensus_fair_value(&self) -> Option<f64> {
        self.consensus_fair_value_inner(None)
    }

    /// Compute conservative consensus fair value for a given trade direction.
    /// `buying_kalshi_yes`: true if we'd buy YES on Kalshi, false for NO.
    /// Uses the adverse Polymarket side to ensure edge is real.
    pub fn consensus_fair_value_conservative(&self, buying_kalshi_yes: bool) -> Option<f64> {
        self.consensus_fair_value_inner(Some(buying_kalshi_yes))
    }

    fn consensus_fair_value_inner(&self, buying_kalshi_yes: Option<bool>) -> Option<f64> {
        let mut sum = 0.0;
        let mut count = 0;

        if let Some(p) = self.espn_home_win_prob {
            sum += p;
            count += 1;
        }
        if let Some(p) = self.dk_home_implied_prob {
            sum += p;
            count += 1;
        }

        // Polymarket: use conservative side based on trade direction, skip wide markets
        if let (Some(bid), Some(ask)) = (self.polymarket_home_bid, self.polymarket_home_ask) {
            let spread = ask - bid;
            if spread <= Self::POLY_MAX_SPREAD && spread >= 0.0 {
                let poly_val = match buying_kalshi_yes {
                    Some(true) => {
                        // Buying YES on Kalshi → want LOWER fair value (conservative)
                        // If kalshi=home, lower home prob = poly home bid
                        // If kalshi=away, higher home prob → lower away prob = poly home ask
                        if self.kalshi_is_home { bid } else { ask }
                    }
                    Some(false) => {
                        // Buying NO on Kalshi → want HIGHER fair value (conservative)
                        // If kalshi=home, higher home prob = poly home ask
                        // If kalshi=away, lower home prob → higher away prob = poly home bid
                        if self.kalshi_is_home { ask } else { bid }
                    }
                    None => {
                        // No direction specified, use mid
                        (bid + ask) / 2.0
                    }
                };
                sum += poly_val;
                count += 1;
            }
        }

        if count >= 2 {
            Some(sum / count as f64)
        } else {
            None
        }
    }

    /// Get the fair value aligned with the Kalshi ticker's team (mid-based, for direction check).
    pub fn kalshi_aligned_fair_value(&self) -> Option<f64> {
        let home_fair = self.consensus_fair_value()?;
        if self.kalshi_is_home {
            Some(home_fair)
        } else {
            Some(1.0 - home_fair)
        }
    }

    /// Get conservative fair value aligned with Kalshi ticker's team.
    /// Uses the adverse Polymarket side to confirm edge is real.
    pub fn kalshi_aligned_fair_value_conservative(&self, buying_kalshi_yes: bool) -> Option<f64> {
        let home_fair = self.consensus_fair_value_conservative(buying_kalshi_yes)?;
        if self.kalshi_is_home {
            Some(home_fair)
        } else {
            Some(1.0 - home_fair)
        }
    }

    /// Compute edge: fair_value - kalshi_price. Positive means Kalshi is cheap (buy YES).
    pub fn edge_vs_kalshi(&self) -> Option<f64> {
        let fair = self.kalshi_aligned_fair_value()?;
        let kalshi = self.kalshi_yes_mid?;
        Some(fair - kalshi / 100.0) // kalshi is in cents, fair is 0-1
    }
}

/// Manages all active game states.
pub struct GameStateManager {
    pub games: HashMap<String, GameState>, // keyed by ESPN event ID
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
