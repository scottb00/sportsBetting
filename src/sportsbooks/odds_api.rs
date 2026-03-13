use anyhow::{Context, Result};
use chrono::DateTime;
use reqwest::Client;
use serde::Deserialize;

use super::types::BookOdds;

const ODDS_API_BASE: &str = "https://api.the-odds-api.com/v4/sports/basketball_ncaab/odds/";

/// Client for The Odds API (the-odds-api.com).
/// Returns moneyline odds from multiple bookmakers (Pinnacle, DraftKings, FanDuel, etc.)
/// in a single API call.
pub struct OddsApiClient {
    client: Client,
    api_key: String,
}

impl OddsApiClient {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            api_key,
        }
    }

    /// Fetch NCAAB moneyline odds from all US bookmakers.
    /// Returns one entry per game with per-bookmaker raw odds.
    /// Costs 1 API credit per call (h2h market, us region).
    pub async fn fetch_ncaab_odds(&self) -> Result<Vec<OddsApiGame>> {
        let resp = self
            .client
            .get(ODDS_API_BASE)
            .query(&[
                ("apiKey", self.api_key.as_str()),
                ("regions", "eu,us"),
                ("markets", "h2h"),
                ("oddsFormat", "american"),
            ])
            .send()
            .await
            .context("Odds API request failed")?;

        // Log remaining credits from response headers
        if let Some(remaining) = resp.headers().get("x-requests-remaining")
            && let Ok(r) = remaining.to_str()
        {
            tracing::info!("Odds API credits remaining: {}", r);
        }

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Odds API returned {}: {}", status, body);
        }

        let events: Vec<RawOddsEvent> = resp
            .json()
            .await
            .context("Odds API JSON parse failed")?;

        let games: Vec<OddsApiGame> = events
            .into_iter()
            .filter_map(Self::convert_event)
            .collect();

        Ok(games)
    }

    fn convert_event(event: RawOddsEvent) -> Option<OddsApiGame> {
        let mut books = Vec::new();

        for bm in &event.bookmakers {
            let h2h = bm.markets.iter().find(|m| m.key == "h2h")?;
            if h2h.outcomes.len() != 2 {
                continue;
            }

            // Match outcome names to home/away using the event's home_team/away_team
            let home_outcome = h2h.outcomes.iter().find(|o| o.name == event.home_team);
            let away_outcome = h2h.outcomes.iter().find(|o| o.name == event.away_team);

            if let (Some(home), Some(away)) = (home_outcome, away_outcome) {
                let ts = bm.last_update.as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.timestamp());
                books.push(BookOdds::from_moneylines_with_ts(
                    bm.key.clone(),
                    home.price,
                    away.price,
                    ts,
                ));
            }
        }

        if books.is_empty() {
            return None;
        }

        Some(OddsApiGame {
            home_team: event.home_team,
            away_team: event.away_team,
            commence_time: event.commence_time,
            books,
        })
    }
}

/// Processed game with per-bookmaker raw odds.
#[derive(Debug, Clone)]
pub struct OddsApiGame {
    pub home_team: String,
    pub away_team: String,
    pub commence_time: String,
    pub books: Vec<BookOdds>,
}

// --- Raw JSON response types ---

#[derive(Deserialize)]
struct RawOddsEvent {
    #[allow(dead_code)]
    id: String,
    home_team: String,
    away_team: String,
    commence_time: String,
    #[serde(default)]
    bookmakers: Vec<RawBookmaker>,
}

#[derive(Deserialize)]
struct RawBookmaker {
    key: String,
    #[allow(dead_code)]
    title: String,
    #[serde(default)]
    last_update: Option<String>,
    markets: Vec<RawMarket>,
}

#[derive(Deserialize)]
struct RawMarket {
    key: String,
    outcomes: Vec<RawOutcome>,
}

#[derive(Deserialize)]
struct RawOutcome {
    name: String,
    price: f64,
}
