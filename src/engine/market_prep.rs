use std::collections::HashMap;

use crate::espn::types::GameInfo;
use crate::kalshi::orderbook::LocalOrderBook;
use crate::kalshi::rest::KalshiRestClient;
use crate::kalshi::types::{Event, GetEventsResponse};

/// CBB series prefix — all men's college basketball series start with this.
const CBB_SERIES_PREFIX: &str = "KXNCAAMB";

/// Series suffixes to EXCLUDE — these are not game-winner markets.
/// Spreads, totals, first-half props, awards, season props, championship futures.
const EXCLUDED_SERIES_SUFFIXES: &[&str] = &[
    "SPREAD", "TOTAL", "1HSPREAD", "1HTOTAL", "1HWINNER",
    "APRANK", "NAISMITH", "COTY", "MOP",
    "UNDEFEATED", "FIRST10", "ACHAMP",
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

/// Get today's date and tomorrow's date as Kalshi date tags, plus today's YYYY-MM-DD string.
pub fn today_and_tomorrow_tags() -> (String, String, Vec<String>) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tomorrow = (chrono::Local::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let tags = vec![kalshi_date_tag(&today), kalshi_date_tag(&tomorrow)];
    (today, tomorrow, tags)
}

/// Discover all KXNCAAMB* series dynamically via the Kalshi /series endpoint,
/// then fetch open events for each series concurrently.
pub async fn fetch_all_kalshi_cbb_events(
    kalshi_rest: &KalshiRestClient,
) -> GetEventsResponse {
    // Step 1: Discover all CBB series dynamically
    let series_list = discover_cbb_series(kalshi_rest).await;

    // Step 2: Fetch events for each series concurrently
    let futs: Vec<_> = series_list
        .iter()
        .map(|series| {
            let series = series.clone();
            async move {
                match kalshi_rest
                    .get_events_with_series(None, Some(&series), Some("open"), None, Some(200))
                    .await
                {
                    Ok(resp) => {
                        if !resp.events.is_empty() {
                            tracing::debug!("Fetched {} events from series {}", resp.events.len(), series);
                        }
                        resp.events
                    }
                    Err(e) => {
                        tracing::debug!("Failed to fetch series {}: {:?}", series, e);
                        vec![]
                    }
                }
            }
        })
        .collect();

    let results = futures_util::future::join_all(futs).await;
    let all_events = results.into_iter().flatten().collect();

    GetEventsResponse {
        events: all_events,
        cursor: None,
    }
}

/// Discover all KXNCAAMB* series from the Kalshi /series API.
/// Falls back to just KXNCAAMBGAME if the API call fails.
async fn discover_cbb_series(kalshi_rest: &KalshiRestClient) -> Vec<String> {
    match kalshi_rest.get_series_list(None).await {
        Ok(resp) => {
            let cbb: Vec<String> = resp.series.iter()
                .filter(|s| {
                    s.ticker.starts_with(CBB_SERIES_PREFIX)
                        && !EXCLUDED_SERIES_SUFFIXES.iter().any(|suffix| {
                            s.ticker.ends_with(suffix)
                        })
                })
                .map(|s| s.ticker.clone())
                .collect();
            if cbb.is_empty() {
                tracing::warn!("No {} series found in API, using fallback", CBB_SERIES_PREFIX);
                vec!["KXNCAAMBGAME".to_string()]
            } else {
                tracing::info!("Discovered {} CBB series: {:?}", cbb.len(), cbb);
                cbb
            }
        }
        Err(e) => {
            tracing::warn!("Series discovery failed ({:?}), using fallback", e);
            vec!["KXNCAAMBGAME".to_string()]
        }
    }
}

/// Filter Kalshi events to matching date tags (KXNCAAMBGAME) plus all conference tournaments.
pub fn filter_events_for_dates(events: &mut GetEventsResponse, date_tags: &[String]) {
    events.events.retain(|e| {
        date_tags.iter().any(|tag| e.event_ticker.contains(tag))
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
    events
        .events
        .iter()
        .flat_map(|e| e.markets.as_ref().map_or(&[][..], |v| v.as_slice()))
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

/// Look up bid/ask/mid for a ticker from the order books map.
/// Returns empty BookPrices (all None) if the ticker is not found.
pub fn book_prices(order_books: &HashMap<String, LocalOrderBook>, ticker: &str) -> BookPrices {
    match order_books.get(ticker) {
        Some(book) => extract_book_prices(book),
        None => BookPrices { bid: None, ask: None, mid: None },
    }
}
