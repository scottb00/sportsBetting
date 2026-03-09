use std::collections::HashMap;

use crate::espn::types::GameInfo;
use crate::kalshi::orderbook::LocalOrderBook;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::types::{Event, GetEventsResponse};

/// All KXNCAAMB* series we want to fetch from Kalshi.
/// KXNCAAMBGAME = individual game markets, the rest are conference tournament series
/// where the championship game markets serve as game-winner markets.
const CBB_SERIES: &[&str] = &[
    "KXNCAAMBGAME",
    "KXNCAAMBSOCON",
    "KXNCAAMBSBELT",
    "KXNCAAMBACC",
    "KXNCAAMBSEC",
    "KXNCAAMBSWAC",
    "KXNCAAMBWCC",
    "KXNCAAMBCAA",
    "KXNCAAMBBSKY",
    "KXNCAAMBNEC",
    "KXNCAAMBAE",
    "KXNCAAMBWAC",
    "KXNCAAMBMEAC",
    "KXNCAAMBPAT",
    "KXNCAAMBB12",
    "KXNCAAMBBTEN",
    "KXNCAAMBBE",
    "KXNCAAMBHL",
    "KXNCAAMBMWC",
    "KXNCAAMBAAC",
    "KXNCAAMBSLAND",
    "KXNCAAMBOVC",
    "KXNCAAMBMAAC",
    "KXNCAAMBASUN",
    "KXNCAAMBCUSA",
    "KXNCAAMBMVC",
];

/// Extracted book prices with named fields (replaces raw tuple).
pub struct BookPrices {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub mid: Option<f64>,
}

/// Convert "2026-03-09" to Kalshi ticker date format "26MAR09".
pub fn kalshi_date_tag(date_str: &str) -> String {
    let dt = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .expect("invalid date format");
    dt.format("%y%b%d").to_string().to_uppercase()
}

/// Fetch all CBB events from Kalshi across multiple series.
pub async fn fetch_all_kalshi_cbb_events(
    kalshi_rest: &KalshiRestClient,
) -> GetEventsResponse {
    let mut all_events = Vec::new();

    for series in CBB_SERIES {
        match kalshi_rest
            .get_events_with_series(None, Some(series), Some("open"), None, Some(200))
            .await
        {
            Ok(resp) => {
                if !resp.events.is_empty() {
                    tracing::debug!("Fetched {} events from series {}", resp.events.len(), series);
                    all_events.extend(resp.events);
                }
            }
            Err(e) => {
                tracing::debug!("Failed to fetch series {}: {:?}", series, e);
            }
        }
    }

    GetEventsResponse {
        events: all_events,
        cursor: None,
    }
}

/// Filter Kalshi events to today's date tag (KXNCAAMBGAME) plus all conference tournaments.
pub fn filter_events_for_today(events: &mut GetEventsResponse, date_tag: &str) {
    events.events.retain(|e| {
        e.event_ticker.contains(date_tag)
            || !e.event_ticker.starts_with("KXNCAAMBGAME")
    });
}

/// Build ESPN matching data: (event_id, "Away @ Home").
pub fn build_espn_for_matching(games: &[GameInfo]) -> Vec<(String, String)> {
    games
        .iter()
        .map(|g| (g.event_id.clone(), format!("{} @ {}", g.away_team, g.home_team)))
        .collect()
}

/// Build Kalshi matching data: (event_ticker, title, markets).
/// Handles both regular game events and conference tournament events.
#[allow(clippy::type_complexity)]
pub fn build_kalshi_for_matching(
    events: &GetEventsResponse,
) -> Vec<(String, String, Vec<(String, String)>)> {
    events
        .events
        .iter()
        .filter_map(|e| {
            if !e.event_ticker.starts_with("KXNCAAMBGAME") {
                return normalize_conference_tournament_event(e);
            }
            let markets: Vec<(String, String)> = e.markets.as_ref()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|m| {
                    let yes_sub = m.yes_sub_title.as_ref()?;
                    Some((m.ticker.clone(), yes_sub.clone()))
                })
                .collect();
            Some((e.event_ticker.clone(), e.title.clone(), markets))
        })
        .collect()
}

/// Build Kalshi volume map: ticker -> volume.
pub fn build_kalshi_volume(events: &GetEventsResponse) -> HashMap<String, i64> {
    let empty_markets = vec![];
    events
        .events
        .iter()
        .flat_map(|e| e.markets.as_ref().unwrap_or(&empty_markets).iter())
        .filter_map(|m| m.volume.map(|v| (m.ticker.clone(), v)))
        .collect()
}

/// For conference tournament events, filter markets to only active finalists
/// and synthesize a game-style title ("TeamA vs TeamB") for matching.
/// Returns (event_ticker, synthetic_title, active_markets).
#[allow(clippy::type_complexity)]
pub fn normalize_conference_tournament_event(
    event: &Event,
) -> Option<(String, String, Vec<(String, String)>)> {
    let markets = event.markets.as_ref()?;

    // Find active markets with real bids (the finalists)
    let active: Vec<_> = markets
        .iter()
        .filter(|m| m.status == "active" && m.yes_bid.unwrap_or(0) > 0)
        .collect();

    if active.len() != 2 {
        return None; // Not a championship matchup (or already resolved)
    }

    // Extract team names from yes_sub_title or market title
    let team_a = active[0].yes_sub_title.as_ref()
        .cloned()
        .unwrap_or_else(|| extract_team_from_title(&active[0].title));
    let team_b = active[1].yes_sub_title.as_ref()
        .cloned()
        .unwrap_or_else(|| extract_team_from_title(&active[1].title));

    if team_a.is_empty() || team_b.is_empty() {
        return None;
    }

    // Synthesize a title that fuzzy_game_match can handle: "TeamA at TeamB"
    let synthetic_title = format!("{} at {}", team_a, team_b);

    let market_entries = vec![
        (active[0].ticker.clone(), team_a),
        (active[1].ticker.clone(), team_b),
    ];

    Some((event.event_ticker.clone(), synthetic_title, market_entries))
}

/// Extract team name from tournament market title like
/// "Will East Tennessee St. be the Southern Conference tournament champions?"
fn extract_team_from_title(title: &str) -> String {
    let s = title.strip_prefix("Will ").unwrap_or(title);
    if let Some(idx) = s.find(" be the ") {
        s[..idx].to_string()
    } else {
        String::new()
    }
}

/// Extract bid/ask/mid from a local order book.
pub fn extract_book_prices(book: &LocalOrderBook) -> BookPrices {
    BookPrices {
        bid: book.best_yes_bid().map(|l| l.price as f64),
        ask: book.best_yes_ask().map(|l| l.price as f64),
        mid: book.yes_mid(),
    }
}
