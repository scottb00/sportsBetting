use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use super::types::MoneylineOdds;

const FANDUEL_NCAAB_URL: &str =
    "https://sbapi.nj.sportsbook.fanduel.com/api/content-managed-page?page=CUSTOM&customPageId=ncaab&_ak=FhMFpcPWXMeyZxOx";

/// Men's basketball competition IDs on FanDuel.
const MENS_COMPETITION_IDS: &[i64] = &[10861937, 12274457];

pub struct FanDuelClient {
    client: Client,
}

impl Default for FanDuelClient {
    fn default() -> Self {
        Self::new()
    }
}

impl FanDuelClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Fetch current CBB men's moneyline odds from FanDuel.
    pub async fn fetch_cbb_moneylines(&self) -> Result<Vec<MoneylineOdds>> {
        let resp: FdResponse = self
            .client
            .get(FANDUEL_NCAAB_URL)
            .send()
            .await
            .context("FanDuel fetch failed")?
            .json()
            .await
            .context("FanDuel JSON parse failed")?;

        let attachments = resp.attachments;
        let events = attachments.events.unwrap_or_default();
        let markets = attachments.markets.unwrap_or_default();

        let mut results = Vec::new();

        for market in markets.values() {
            // Only moneyline markets for men's basketball
            if market.market_type != "MONEY_LINE" {
                continue;
            }
            if !MENS_COMPETITION_IDS.contains(&market.competition_id) {
                continue;
            }
            let runners = match &market.runners {
                Some(r) if r.len() == 2 => r,
                _ => continue,
            };

            let mut home_ml: Option<f64> = None;
            let mut away_ml: Option<f64> = None;
            let mut home_name: Option<String> = None;
            let mut away_name: Option<String> = None;

            for runner in runners {
                let odds = runner
                    .win_runner_odds
                    .as_ref()
                    .and_then(|o| o.american_display_odds.as_ref())
                    .map(|a| a.american_odds);

                let result_type = runner
                    .result
                    .as_ref()
                    .and_then(|r| r.result_type.as_deref());

                match result_type {
                    Some("HOME") => {
                        home_ml = odds;
                        home_name = Some(runner.runner_name.clone());
                    }
                    Some("AWAY") => {
                        away_ml = odds;
                        away_name = Some(runner.runner_name.clone());
                    }
                    _ => {}
                }
            }

            if let (Some(h_ml), Some(a_ml), Some(h_name), Some(a_name)) =
                (home_ml, away_ml, home_name, away_name)
            {
                // Strip "(W)" suffix from women's games that might slip through
                let h_name = h_name.trim_end_matches(" (W)").to_string();
                let a_name = a_name.trim_end_matches(" (W)").to_string();

                // Try to get team names from the event if available (cleaner names)
                let event_id_str = market.event_id.to_string();
                let _event = events.get(&event_id_str);

                results.push(MoneylineOdds {
                    home_team: h_name,
                    away_team: a_name,
                    home_moneyline: h_ml,
                    away_moneyline: a_ml,
                });
            }
        }

        Ok(results)
    }
}

// --- FanDuel JSON response types ---

#[derive(Deserialize)]
struct FdResponse {
    attachments: FdAttachments,
}

#[derive(Deserialize)]
struct FdAttachments {
    events: Option<std::collections::HashMap<String, FdEvent>>,
    markets: Option<std::collections::HashMap<String, FdMarket>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct FdEvent {
    name: Option<String>,
    open_date: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FdMarket {
    market_type: String,
    competition_id: i64,
    event_id: i64,
    runners: Option<Vec<FdRunner>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FdRunner {
    runner_name: String,
    result: Option<FdRunnerResult>,
    win_runner_odds: Option<FdWinRunnerOdds>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FdRunnerResult {
    #[serde(rename = "type")]
    result_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FdWinRunnerOdds {
    american_display_odds: Option<FdAmericanOdds>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FdAmericanOdds {
    american_odds: f64,
}
