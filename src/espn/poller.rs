use anyhow::{Context, Result};
use reqwest::Client;
use std::collections::HashMap;

use super::types::*;

#[cfg(test)]
use std::path::Path;

const SCOREBOARD_URL: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/mens-college-basketball/scoreboard";
const SUMMARY_URL: &str =
    "https://site.api.espn.com/apis/site/v2/sports/basketball/mens-college-basketball/summary";

pub struct EspnPoller {
    client: Client,
}

impl Default for EspnPoller {
    fn default() -> Self {
        Self::new()
    }
}

impl EspnPoller {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    /// Fetch scoreboard — returns all D1 games for today (and optionally tomorrow).
    /// Uses groups=50 to get all Division I games, not just featured.
    pub async fn fetch_scoreboard(&self) -> Result<Vec<GameInfo>> {
        let mut all_games = Vec::new();

        // ESPN's default (no date param) returns the "current window" which may only
        // include in-progress/recently-finished games, missing upcoming scheduled games.
        // Fetch it for live games, then also explicitly fetch today + tomorrow by date.
        let today = chrono::Local::now().format("%Y%m%d").to_string();
        let tomorrow = (chrono::Local::now() + chrono::Duration::days(1))
            .format("%Y%m%d")
            .to_string();

        let (current_result, today_result, tomorrow_result) = tokio::join!(
            self.fetch_scoreboard_for_date(None),
            self.fetch_scoreboard_for_date(Some(&today)),
            self.fetch_scoreboard_for_date(Some(&tomorrow)),
        );

        all_games.extend(current_result?);
        all_games.extend(today_result?);
        all_games.extend(tomorrow_result?);

        // Dedup by event_id
        let mut seen = std::collections::HashSet::new();
        all_games.retain(|g| seen.insert(g.event_id.clone()));

        Ok(all_games)
    }

    /// Fetch scoreboard for a specific date (YYYYMMDD) or today if None.
    async fn fetch_scoreboard_for_date(&self, date: Option<&str>) -> Result<Vec<GameInfo>> {
        let url = match date {
            Some(d) => format!("{}?groups=50&dates={}", SCOREBOARD_URL, d),
            None => format!("{}?groups=50", SCOREBOARD_URL),
        };

        let resp: ScoreboardResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch ESPN scoreboard")?
            .json()
            .await
            .context("Failed to parse ESPN scoreboard")?;

        let mut games = Vec::new();
        for event in &resp.events {
            if let Some(game) = Self::parse_event(event) {
                games.push(game);
            }
        }

        Ok(games)
    }

    /// Fetch detailed summary for a specific game — includes win probability and DK odds.
    /// Only call this when a break is detected.
    pub async fn fetch_summary(&self, event_id: &str) -> Result<SummaryResponse> {
        let url = format!("{}?event={}", SUMMARY_URL, event_id);
        self.client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to fetch ESPN summary for event {}", event_id))?
            .json()
            .await
            .with_context(|| format!("Failed to parse ESPN summary for event {}", event_id))
    }

    /// Get the latest win probability from a summary response.
    /// During live games, uses the real-time winprobability array.
    /// Pre-game, falls back to ESPN's BPI predictor (gameProjection).
    pub fn latest_win_prob(summary: &SummaryResponse) -> Option<f64> {
        // Prefer live win probability if available
        if let Some(wp) = summary.winprobability.last() {
            return Some(wp.home_win_percentage);
        }

        // Fall back to ESPN predictor (BPI) for pre-game
        summary
            .predictor
            .as_ref()
            .and_then(|p| p.home_team.as_ref())
            .map(|t| t.game_projection / 100.0)
    }

    /// Extract DK moneyline from pickcenter data.
    /// Returns (home_moneyline, away_moneyline) as American odds.
    /// Uses homeTeamOdds/awayTeamOdds which have moneyLine as a number,
    /// falling back to parsing the moneyline.home/away.live.odds string.
    pub fn extract_dk_moneyline(summary: &SummaryResponse) -> Option<(f64, f64)> {
        let pickcenter = summary.pickcenter.as_ref()?;
        for entry in pickcenter {
            let is_dk = entry
                .provider
                .as_ref()
                .map(|p| p.name.contains("Draft Kings") || p.name.contains("DraftKings"))
                .unwrap_or(false);

            if is_dk || pickcenter.len() == 1 {
                // Try homeTeamOdds/awayTeamOdds first (clean numeric values)
                if let (Some(home_odds), Some(away_odds)) =
                    (&entry.home_team_odds, &entry.away_team_odds)
                    && let (Some(h), Some(a)) = (home_odds.money_line, away_odds.money_line)
                {
                    return Some((h, a));
                }

                // Fall back to moneyline JSON (home/away with live/close/open string odds)
                if let Some(ml) = &entry.moneyline {
                    let home_ml = Self::extract_odds_from_ml_json(ml, "home");
                    let away_ml = Self::extract_odds_from_ml_json(ml, "away");
                    if let (Some(h), Some(a)) = (home_ml, away_ml) {
                        return Some((h, a));
                    }
                }
            }
        }
        None
    }

    /// Parse odds from the moneyline JSON object.
    /// Structure: moneyline.{side}.live.odds (string) or .close.odds (string)
    fn extract_odds_from_ml_json(ml: &serde_json::Value, side: &str) -> Option<f64> {
        let side_obj = ml.get(side)?;
        // Prefer live, then close, then open
        for key in &["live", "close", "open"] {
            if let Some(odds_str) = side_obj
                .get(key)
                .and_then(|o| o.get("odds"))
                .and_then(|v| v.as_str())
            {
                // Parse "+950" or "-1650" as f64
                return odds_str.parse::<f64>().ok();
            }
        }
        None
    }

    fn parse_event(event: &EspnEvent) -> Option<GameInfo> {
        let comp = event.competitions.first()?;
        let home = comp.competitors.iter().find(|c| c.home_away == "home")?;
        let away = comp.competitors.iter().find(|c| c.home_away == "away")?;

        let mut phase = GamePhase::from_espn_status(
            &event.status.status_type.name,
            &event.status.status_type.state,
        );

        // Extract lastPlay from situation
        let last_play = comp.situation.as_ref().and_then(|s| s.last_play.as_ref());
        let last_play_text = last_play.and_then(|lp| lp.text.clone());
        let last_play_type = last_play
            .and_then(|lp| lp.play_type.as_ref())
            .map(|pt| pt.text.clone());
        let last_play_home_win_prob = last_play
            .and_then(|lp| lp.probability.as_ref())
            .map(|p| p.home_win_percentage);

        // Detect timeouts: upgrade Live to Break when lastPlay is a timeout
        if phase == GamePhase::Live {
            let is_timeout = last_play_type.as_deref()
                .map(|t| t.to_lowercase().contains("timeout"))
                .unwrap_or(false)
                || last_play_text.as_deref()
                    .map(|t| t.to_lowercase().contains("timeout"))
                    .unwrap_or(false);
            if is_timeout {
                phase = GamePhase::Break;
            }
        }

        // Parse ISO-8601 date to unix timestamp (seconds)
        // ESPN uses truncated format like "2026-03-08T19:00Z" (no seconds),
        // which isn't valid RFC 3339. Normalize by adding ":00" before "Z" if needed.
        let start_time_ts = event.date.as_deref().and_then(|d| {
            let normalized = if d.ends_with('Z') && d.matches(':').count() == 1 {
                // "2026-03-08T19:00Z" -> "2026-03-08T19:00:00Z"
                format!("{}:00Z", &d[..d.len()-1])
            } else {
                d.to_string()
            };
            chrono::DateTime::parse_from_rfc3339(&normalized)
                .ok()
                .map(|dt| dt.timestamp())
        });

        Some(GameInfo {
            event_id: event.id.clone(),
            home_team: home.team.display_name.clone(),
            away_team: away.team.display_name.clone(),
            home_abbreviation: home.team.abbreviation.clone(),
            away_abbreviation: away.team.abbreviation.clone(),
            home_score: home.score.as_ref().and_then(|s| s.parse().ok()),
            away_score: away.score.as_ref().and_then(|s| s.parse().ok()),
            game_phase: phase,
            start_time_ts,
            status_detail: event.status.status_type.description.clone(),
            last_play: last_play_text,
            last_play_type,
            last_play_home_win_prob,
        })
    }
}

/// Tracks game states and detects phase transitions (e.g., Live -> Halftime).
pub struct GameTracker {
    previous_phases: HashMap<String, GamePhase>,
}

impl Default for GameTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl GameTracker {
    pub fn new() -> Self {
        Self {
            previous_phases: HashMap::new(),
        }
    }

    /// Update game states and return event IDs where a break just started.
    pub fn update(&mut self, games: &[GameInfo]) -> Vec<String> {
        let mut new_breaks = Vec::new();

        for game in games {
            let prev = self.previous_phases.get(&game.event_id);
            let is_new_break = game.game_phase.is_break()
                && prev.map(|p| !p.is_break()).unwrap_or(true);

            if is_new_break {
                tracing::info!(
                    "Break detected: {} vs {} (event {})",
                    game.away_team,
                    game.home_team,
                    game.event_id
                );
                new_breaks.push(game.event_id.clone());
            }

            self.previous_phases
                .insert(game.event_id.clone(), game.game_phase.clone());
        }

        new_breaks
    }

    /// Return event IDs for games that just transitioned OUT of a break
    /// (i.e., were on break last tick but are now Live or another non-break phase).
    pub fn breaks_ended(&self, games: &[GameInfo]) -> Vec<String> {
        let mut ended = Vec::new();

        for game in games {
            if let Some(prev) = self.previous_phases.get(&game.event_id)
                && prev.is_break() && !game.game_phase.is_break()
            {
                tracing::info!(
                    "Break ended: {} vs {} (event {}) — {:?} -> {:?}",
                    game.away_team,
                    game.home_team,
                    game.event_id,
                    prev,
                    game.game_phase,
                );
                ended.push(game.event_id.clone());
            }
        }

        ended
    }

    /// Return event IDs for games that just transitioned from PreGame to a live phase
    /// (Live, Halftime, Break). Must be called BEFORE `update()` so previous phases are intact.
    pub fn pregame_to_live(&self, games: &[GameInfo]) -> Vec<String> {
        let mut transitioned = Vec::new();

        for game in games {
            if let Some(prev) = self.previous_phases.get(&game.event_id)
                && *prev == GamePhase::PreGame
                && game.game_phase != GamePhase::PreGame
                && game.game_phase != GamePhase::Final
            {
                tracing::info!(
                    "PreGame -> Live: {} vs {} (event {}) — {:?} -> {:?}",
                    game.away_team,
                    game.home_team,
                    game.event_id,
                    prev,
                    game.game_phase,
                );
                transitioned.push(game.event_id.clone());
            }
        }

        transitioned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to load {}: {}", name, e))
    }

    #[test]
    fn test_parse_espn_date_truncated() {
        // ESPN sends "2026-03-08T19:00Z" (no seconds) — must parse correctly
        let event = EspnEvent {
            id: "1".to_string(),
            name: "A at B".to_string(),
            short_name: "A @ B".to_string(),
            date: Some("2026-03-08T19:00Z".to_string()),
            competitions: vec![Competition {
                id: "1".to_string(),
                competitors: vec![
                    Competitor {
                        id: "1".to_string(),
                        team: Team { id: "1".to_string(), display_name: "Home".to_string(), abbreviation: "HM".to_string() },
                        home_away: "home".to_string(),
                        score: None,
                    },
                    Competitor {
                        id: "2".to_string(),
                        team: Team { id: "2".to_string(), display_name: "Away".to_string(), abbreviation: "AW".to_string() },
                        home_away: "away".to_string(),
                        score: None,
                    },
                ],
                odds: None,
                situation: None,
            }],
            status: EventStatus {
                status_type: StatusType {
                    id: "1".to_string(),
                    name: "STATUS_SCHEDULED".to_string(),
                    state: "pre".to_string(),
                    description: "Scheduled".to_string(),
                },
                display_clock: None,
                period: None,
            },
        };
        let game = EspnPoller::parse_event(&event).unwrap();
        assert!(game.start_time_ts.is_some(), "Should parse truncated ESPN date");
        assert_eq!(game.start_time_ts.unwrap(), 1772996400);
    }

    #[test]
    fn test_parse_scoreboard_fixture() {
        let json = load_fixture("espn_scoreboard.json");
        let resp: ScoreboardResponse = serde_json::from_str(&json).expect("Failed to parse scoreboard");

        assert_eq!(resp.events.len(), 3);

        // First game: ILL @ MD (in progress)
        let event = &resp.events[0];
        assert_eq!(event.id, "401825567");
        assert!(event.name.contains("Illinois"));
        assert!(event.name.contains("Maryland"));
        assert_eq!(event.status.status_type.state, "in");

        let game = EspnPoller::parse_event(event).expect("Should parse first event");
        assert_eq!(game.event_id, "401825567");
        assert_eq!(game.home_team, "Maryland Terrapins");
        assert_eq!(game.away_team, "Illinois Fighting Illini");
        assert_eq!(game.home_abbreviation, "MD");
        assert_eq!(game.away_abbreviation, "ILL");
        assert_eq!(game.home_score, Some(59));
        assert_eq!(game.away_score, Some(61));
        assert_eq!(game.game_phase, GamePhase::Live);
    }

    #[test]
    fn test_parse_scoreboard_pregame() {
        let json = load_fixture("espn_scoreboard.json");
        let resp: ScoreboardResponse = serde_json::from_str(&json).unwrap();

        // Third game: Iowa @ Nebraska (scheduled/pre)
        let event = &resp.events[2];
        let game = EspnPoller::parse_event(event).expect("Should parse pregame event");
        assert_eq!(game.game_phase, GamePhase::PreGame);
        assert_eq!(game.home_team, "Nebraska Cornhuskers");
        assert_eq!(game.away_team, "Iowa Hawkeyes");
    }

    #[test]
    fn test_parse_all_scoreboard_events() {
        let json = load_fixture("espn_scoreboard.json");
        let resp: ScoreboardResponse = serde_json::from_str(&json).unwrap();

        // All events should parse without errors
        for event in &resp.events {
            let game = EspnPoller::parse_event(event);
            assert!(game.is_some(), "Failed to parse event {}", event.id);
        }
    }

    #[test]
    fn test_parse_summary_fixture() {
        let json = load_fixture("espn_summary.json");
        let summary: SummaryResponse =
            serde_json::from_str(&json).expect("Failed to parse summary");

        // Win probability should have entries
        assert!(!summary.winprobability.is_empty());

        // Last win prob should be ~0.198 (MD home)
        let last_wp = EspnPoller::latest_win_prob(&summary);
        assert!(last_wp.is_some());
        let wp = last_wp.unwrap();
        assert!((wp - 0.198).abs() < 0.001, "Expected ~0.198, got {}", wp);
    }

    #[test]
    fn test_extract_dk_moneyline_from_summary() {
        let json = load_fixture("espn_summary.json");
        let summary: SummaryResponse = serde_json::from_str(&json).unwrap();

        let ml = EspnPoller::extract_dk_moneyline(&summary);
        assert!(ml.is_some(), "Should extract DK moneyline");

        let (home_ml, away_ml) = ml.unwrap();
        // From fixture: homeTeamOdds.moneyLine=950, awayTeamOdds.moneyLine=-1650
        assert_eq!(home_ml, 950.0);
        assert_eq!(away_ml, -1650.0);
    }

    #[test]
    fn test_extract_odds_from_ml_json() {
        // Simulate the real moneyline JSON structure
        let ml: serde_json::Value = serde_json::json!({
            "home": {
                "live": {"odds": "+290"},
                "close": {"odds": "+950"},
                "open": {"odds": "+800"}
            },
            "away": {
                "live": {"odds": "-410"},
                "close": {"odds": "-1650"},
                "open": {"odds": "-1200"}
            }
        });

        // Should prefer "live" odds
        let home = EspnPoller::extract_odds_from_ml_json(&ml, "home");
        assert_eq!(home, Some(290.0));

        let away = EspnPoller::extract_odds_from_ml_json(&ml, "away");
        assert_eq!(away, Some(-410.0));
    }

    #[test]
    fn test_extract_odds_fallback_to_close() {
        let ml: serde_json::Value = serde_json::json!({
            "home": {
                "close": {"odds": "+150"},
            }
        });

        let home = EspnPoller::extract_odds_from_ml_json(&ml, "home");
        assert_eq!(home, Some(150.0));
    }

    #[test]
    fn test_game_tracker_detects_breaks() {
        let mut tracker = GameTracker::new();

        let make_game = |phase: GamePhase| GameInfo {
            event_id: "123".to_string(),
            home_team: "Home".to_string(),
            away_team: "Away".to_string(),
            home_abbreviation: "HM".to_string(),
            away_abbreviation: "AW".to_string(),
            home_score: Some(50),
            away_score: Some(48),
            game_phase: phase,
            start_time_ts: None,
            status_detail: String::new(),
            last_play: None,
            last_play_type: None,
            last_play_home_win_prob: None,
        };

        // First update: Live game -> no break
        let breaks = tracker.update(&[make_game(GamePhase::Live)]);
        assert!(breaks.is_empty());

        // Live -> Halftime: break detected
        let breaks = tracker.update(&[make_game(GamePhase::Halftime)]);
        assert_eq!(breaks, vec!["123"]);

        // Halftime -> Halftime: no new break
        let breaks = tracker.update(&[make_game(GamePhase::Halftime)]);
        assert!(breaks.is_empty());

        // Halftime -> Live: no break
        let breaks = tracker.update(&[make_game(GamePhase::Live)]);
        assert!(breaks.is_empty());
    }

    #[test]
    fn test_game_phase_from_espn_status() {
        assert_eq!(GamePhase::from_espn_status("STATUS_SCHEDULED", "pre"), GamePhase::PreGame);
        assert_eq!(GamePhase::from_espn_status("STATUS_FINAL", "post"), GamePhase::Final);
        assert_eq!(GamePhase::from_espn_status("STATUS_HALFTIME", "in"), GamePhase::Halftime);
        assert_eq!(GamePhase::from_espn_status("STATUS_IN_PROGRESS", "in"), GamePhase::Live);
    }
}
