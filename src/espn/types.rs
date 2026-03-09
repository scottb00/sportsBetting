use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreboardResponse {
    pub events: Vec<EspnEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EspnEvent {
    pub id: String,
    pub name: String,
    #[serde(rename = "shortName")]
    pub short_name: String,
    pub competitions: Vec<Competition>,
    pub status: EventStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Competition {
    pub id: String,
    pub competitors: Vec<Competitor>,
    #[serde(default)]
    pub odds: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Competitor {
    pub id: String,
    pub team: Team,
    #[serde(rename = "homeAway")]
    pub home_away: String,
    pub score: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub abbreviation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventStatus {
    #[serde(rename = "type")]
    pub status_type: StatusType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusType {
    pub id: String,
    pub name: String,
    pub state: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
}

// --- Summary endpoint types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryResponse {
    #[serde(default)]
    pub winprobability: Vec<WinProbability>,
    #[serde(default)]
    pub pickcenter: Option<Vec<PickcenterEntry>>,
    #[serde(default)]
    pub predictor: Option<Predictor>,
    #[serde(default)]
    pub header: Option<serde_json::Value>,
    #[serde(default)]
    pub boxscore: Option<serde_json::Value>,
    #[serde(default)]
    pub plays: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Predictor {
    #[serde(rename = "homeTeam")]
    pub home_team: Option<PredictorTeam>,
    #[serde(rename = "awayTeam")]
    pub away_team: Option<PredictorTeam>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictorTeam {
    pub id: String,
    #[serde(rename = "gameProjection", deserialize_with = "string_to_f64")]
    pub game_projection: f64,
}

fn string_to_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    s.parse().map_err(serde::de::Error::custom)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WinProbability {
    #[serde(rename = "homeWinPercentage")]
    pub home_win_percentage: f64,
    #[serde(rename = "tiePercentage")]
    pub tie_percentage: f64,
    #[serde(rename = "playId")]
    pub play_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickcenterEntry {
    pub provider: Option<Provider>,
    pub spread: Option<f64>,
    #[serde(rename = "overUnder")]
    pub over_under: Option<f64>,
    // Moneyline: home/away with close/open/live sub-objects
    pub moneyline: Option<serde_json::Value>,
    // homeTeamOdds/awayTeamOdds have moneyLine as integer
    #[serde(rename = "homeTeamOdds")]
    pub home_team_odds: Option<TeamOdds>,
    #[serde(rename = "awayTeamOdds")]
    pub away_team_odds: Option<TeamOdds>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamOdds {
    pub favorite: Option<bool>,
    pub underdog: Option<bool>,
    #[serde(rename = "moneyLine")]
    pub money_line: Option<f64>,
    #[serde(rename = "spreadOdds")]
    pub spread_odds: Option<f64>,
}

/// Parsed game state from ESPN data.
#[derive(Debug, Clone)]
pub struct GameInfo {
    pub event_id: String,
    pub home_team: String,
    pub away_team: String,
    pub home_abbreviation: String,
    pub away_abbreviation: String,
    pub home_score: Option<i32>,
    pub away_score: Option<i32>,
    pub game_phase: GamePhase,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GamePhase {
    PreGame,
    Live,
    Halftime,
    Break,
    Final,
    Unknown,
}

impl std::fmt::Display for GamePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GamePhase::PreGame => write!(f, "PreGame"),
            GamePhase::Live => write!(f, "Live"),
            GamePhase::Halftime => write!(f, "Halftime"),
            GamePhase::Break => write!(f, "Break"),
            GamePhase::Final => write!(f, "Final"),
            GamePhase::Unknown => write!(f, "Unknown"),
        }
    }
}

impl GamePhase {
    pub fn from_espn_status(name: &str, state: &str) -> Self {
        match (state, name) {
            ("pre", _) => GamePhase::PreGame,
            ("post", _) => GamePhase::Final,
            (_, "STATUS_HALFTIME") => GamePhase::Halftime,
            ("in", _) => GamePhase::Live,
            _ => GamePhase::Unknown,
        }
    }

    pub fn is_break(&self) -> bool {
        matches!(self, GamePhase::Halftime | GamePhase::Break)
    }
}
