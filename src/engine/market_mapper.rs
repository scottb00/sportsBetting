use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Info about a single Kalshi market within a game event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KalshiMarketInfo {
    pub ticker: String,
    pub yes_sub_title: String, // team name that YES resolves to
}

/// A single mapping between ESPN, Kalshi, and Polymarket for one game.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketMapping {
    pub espn_event_id: String,
    pub espn_name: String, // e.g. "Illinois Fighting Illini @ Maryland Terrapins"
    pub kalshi_event_ticker: Option<String>,
    pub kalshi_title: Option<String>,
    /// All market tickers within this Kalshi event (typically 2: one per team)
    #[serde(default)]
    pub kalshi_markets: Vec<KalshiMarketInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polymarket_token_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polymarket_title: Option<String>,
}

/// Persisted mapping file format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MappingCache {
    pub date: String, // YYYY-MM-DD
    pub mappings: Vec<MarketMapping>,
}

/// Maps between Kalshi market tickers, Polymarket token IDs, and ESPN event IDs.
pub struct MarketMapper {
    /// ESPN event ID -> mapping
    by_espn: HashMap<String, MarketMapping>,
    /// Kalshi market ticker -> ESPN event ID
    kalshi_to_espn: HashMap<String, String>,
    cache_path: String,
}

impl MarketMapper {
    pub fn new(cache_path: &str) -> Self {
        Self {
            by_espn: HashMap::new(),
            kalshi_to_espn: HashMap::new(),
            cache_path: cache_path.to_string(),
        }
    }

    /// Try to load today's cached mappings from disk.
    pub fn load_cache(&mut self, today: &str) -> bool {
        let path = Path::new(&self.cache_path);
        if !path.exists() {
            return false;
        }

        let Ok(contents) = std::fs::read_to_string(path) else {
            return false;
        };

        let Ok(cache) = serde_json::from_str::<MappingCache>(&contents) else {
            return false;
        };

        if cache.date != today {
            tracing::info!("Cache is from {}, today is {} — will refresh", cache.date, today);
            return false;
        }

        for mapping in &cache.mappings {
            self.insert_mapping(mapping.clone());
        }

        tracing::info!("Loaded {} cached mappings for {}", cache.mappings.len(), today);
        true
    }

    /// Save current mappings to disk.
    pub fn save_cache(&self, today: &str) -> Result<()> {
        let cache = MappingCache {
            date: today.to_string(),
            mappings: self.by_espn.values().cloned().collect(),
        };

        let json = serde_json::to_string_pretty(&cache)?;
        std::fs::write(&self.cache_path, json)
            .with_context(|| format!("Failed to write mapping cache to {}", self.cache_path))?;

        tracing::info!("Saved {} mappings to cache", cache.mappings.len());
        Ok(())
    }

    fn insert_mapping(&mut self, mapping: MarketMapping) {
        for km in &mapping.kalshi_markets {
            self.kalshi_to_espn
                .insert(km.ticker.clone(), mapping.espn_event_id.clone());
        }
        self.by_espn
            .insert(mapping.espn_event_id.clone(), mapping);
    }

    /// Get all Kalshi market info for a game.
    pub fn kalshi_markets_for_game(&self, espn_event_id: &str) -> &[KalshiMarketInfo] {
        self.by_espn
            .get(espn_event_id)
            .map(|m| m.kalshi_markets.as_slice())
            .unwrap_or(&[])
    }

    /// Get the Kalshi event title for a game (e.g., "Away at Home Winner?").
    pub fn kalshi_title(&self, espn_event_id: &str) -> Option<&str> {
        self.by_espn
            .get(espn_event_id)
            .and_then(|m| m.kalshi_title.as_deref())
    }

    /// Determine if a market's YES side is for the home team,
    /// using the yes_sub_title and the Kalshi title ("Away at Home Winner?").
    pub fn market_is_home_team(kalshi_title: &str, yes_sub_title: &str) -> bool {
        // Title: "Away at Home Winner?" — extract home team name
        let title_clean = kalshi_title.trim_end_matches(" Winner?").trim_end_matches('?');
        let parts: Vec<&str> = title_clean.split(" at ").collect();
        if parts.len() != 2 {
            return true; // default
        }

        let home_name = parts[1].trim().to_lowercase();
        let yes_name = yes_sub_title.to_lowercase();

        // Check if yes_sub_title matches the home team
        Self::team_name_matches(&home_name, &yes_name)
            || Self::team_name_matches(&yes_name, &home_name)
    }

    pub fn polymarket_token(&self, espn_event_id: &str) -> Option<&str> {
        self.by_espn
            .get(espn_event_id)
            .and_then(|m| m.polymarket_token_id.as_deref())
    }

    /// Determine if the Polymarket YES token is for the home team.
    pub fn polymarket_is_home_team(&self, espn_event_id: &str) -> bool {
        let Some(mapping) = self.by_espn.get(espn_event_id) else {
            return false;
        };
        let Some(poly_title) = &mapping.polymarket_title else {
            return false;
        };

        let poly_first = poly_title
            .split(" vs.")
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();

        let espn_parts: Vec<&str> = mapping.espn_name.split(" @ ").collect();
        if espn_parts.len() != 2 || poly_first.is_empty() {
            return false;
        }

        let espn_home = espn_parts[1].trim().to_lowercase();
        let espn_away = espn_parts[0].trim().to_lowercase();

        let matches_home = Self::team_name_matches(&espn_home, &poly_first)
            || Self::team_name_matches(&poly_first, &espn_home);
        let matches_away = Self::team_name_matches(&espn_away, &poly_first)
            || Self::team_name_matches(&poly_first, &espn_away);

        matches_home && !matches_away
    }

    pub fn all_mapped_kalshi_tickers(&self) -> Vec<String> {
        self.kalshi_to_espn.keys().cloned().collect()
    }

    /// Match markets to ESPN games using deterministic fuzzy matching.
    #[allow(clippy::type_complexity)]
    pub fn resolve_deterministic(
        &mut self,
        espn_games: &[(String, String)],        // (event_id, "Away @ Home")
        kalshi_events: &[(String, String, Vec<(String, String)>)], // (event_ticker, title, markets)
        polymarket_markets: &[(String, String)], // (token_id, question)
        today: &str,
    ) -> Result<()> {
        if espn_games.is_empty() {
            return Ok(());
        }

        let mut matched_kalshi: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut matched_poly: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut matched_count = 0;

        for (espn_id, espn_name) in espn_games {
            let mut kalshi_match: Option<(String, String, Vec<(String, String)>)> = None;
            let mut poly_match = None;

            // Try matching Kalshi events by title
            for (event_ticker, title, markets) in kalshi_events {
                if matched_kalshi.contains(event_ticker) { continue; }
                if Self::fuzzy_game_match(espn_name, title, " at ", " Winner?")
                    || Self::fuzzy_game_match(espn_name, title, " at ", "")
                {
                    kalshi_match = Some((event_ticker.clone(), title.clone(), markets.clone()));
                    matched_kalshi.insert(event_ticker.clone());
                    break;
                }
            }

            // Try matching Polymarket markets
            for (token_id, question) in polymarket_markets {
                if matched_poly.contains(token_id) { continue; }
                if Self::fuzzy_game_match(espn_name, question, " vs. ", "") {
                    poly_match = Some((token_id.clone(), question.clone()));
                    matched_poly.insert(token_id.clone());
                    break;
                }
            }

            if kalshi_match.is_some() || poly_match.is_some() {
                let espn_home = espn_name.split(" @ ").nth(1).unwrap_or("");
                let kalshi_markets = if let Some((_, ref _title, ref markets)) = kalshi_match {
                    markets.iter().map(|(ticker, yes_sub)| {
                        let is_home = Self::yes_is_home_team(espn_home, yes_sub);
                        tracing::info!(
                            "  Kalshi market {} | YES={} | is_home={}",
                            ticker, yes_sub, is_home
                        );
                        KalshiMarketInfo {
                            ticker: ticker.clone(),
                            yes_sub_title: yes_sub.clone(),
                        }
                    }).collect()
                } else {
                    vec![]
                };

                let mapping = MarketMapping {
                    espn_event_id: espn_id.clone(),
                    espn_name: espn_name.clone(),
                    kalshi_event_ticker: kalshi_match.as_ref().map(|(t, _, _)| t.clone()),
                    kalshi_title: kalshi_match.as_ref().map(|(_, t, _)| t.clone()),
                    kalshi_markets,
                    polymarket_token_id: poly_match.as_ref().map(|(t, _)| t.clone()),
                    polymarket_title: poly_match.as_ref().map(|(_, t)| t.clone()),
                };
                self.insert_mapping(mapping);
                matched_count += 1;
            }
        }

        let unmatched_kalshi = kalshi_events.len() - matched_kalshi.len();
        let unmatched_poly = polymarket_markets.len() - matched_poly.len();
        tracing::info!(
            "Deterministic matching: {} ESPN games matched ({} Kalshi, {} Poly). Unmatched: {} Kalshi, {} Poly",
            matched_count, matched_kalshi.len(), matched_poly.len(), unmatched_kalshi, unmatched_poly
        );

        self.save_cache(today)?;
        Ok(())
    }

    /// Fuzzy match an ESPN game name against a market title.
    fn fuzzy_game_match(espn_name: &str, market_title: &str, sep: &str, suffix: &str) -> bool {
        let espn_parts: Vec<&str> = espn_name.split(" @ ").collect();
        if espn_parts.len() != 2 { return false; }

        let espn_away = espn_parts[0].trim().to_lowercase();
        let espn_home = espn_parts[1].trim().to_lowercase();

        let title_clean = if !suffix.is_empty() {
            market_title.trim_end_matches(suffix).to_string()
        } else {
            market_title.to_string()
        };

        let market_parts: Vec<&str> = title_clean.split(sep).collect();
        if market_parts.len() != 2 { return false; }

        let market_a = market_parts[0].trim().to_lowercase();
        let market_b = market_parts[1].trim().to_lowercase();

        let fwd = Self::team_name_matches(&espn_away, &market_a) && Self::team_name_matches(&espn_home, &market_b);
        let rev = Self::team_name_matches(&espn_away, &market_b) && Self::team_name_matches(&espn_home, &market_a);

        fwd || rev
    }

    /// Normalize abbreviations in team names for matching.
    fn normalize_team_name(name: &str) -> String {
        name.replace("st.", "state")
            .replace("miss.", "mississippi")
            .replace("n.c.", "north carolina")
            .replace("s.c.", "south carolina")
            .replace(" state state", " state")
    }

    /// Check if a market's YES side is for the home team by comparing
    /// the yes_sub_title against the ESPN home team name directly.
    /// More reliable than title parsing for conference tournament markets.
    pub fn yes_is_home_team(espn_home_team: &str, yes_sub_title: &str) -> bool {
        let home = espn_home_team.to_lowercase();
        let yes = yes_sub_title.to_lowercase();
        Self::team_name_matches(&home, &yes)
            || Self::team_name_matches(&yes, &home)
    }

    /// Check if two team name strings refer to the same team.
    fn team_name_matches(full: &str, short: &str) -> bool {
        if full == short { return true; }

        let full_norm = Self::normalize_team_name(full);
        let short_norm = Self::normalize_team_name(short);

        if full_norm.contains(&short_norm) || short_norm.contains(&full_norm) {
            return true;
        }

        let skip = ["state", "university", "the", "of", "college",
            "tigers", "eagles", "bulldogs", "wildcats", "bears", "panthers",
            "hawks", "knights", "cougars", "warriors", "raiders", "lions",
            "huskies", "broncos", "owls", "trojans", "rebels", "colonels",
            "saints", "vikings", "bobcats", "bison", "aggies", "demons",
        ];
        let full_words: Vec<&str> = full_norm.split_whitespace()
            .filter(|w| !skip.contains(&w.to_lowercase().as_str()))
            .collect();
        let short_words: Vec<&str> = short_norm.split_whitespace()
            .filter(|w| !skip.contains(&w.to_lowercase().as_str()))
            .collect();

        if full_words.is_empty() || short_words.is_empty() {
            return false;
        }

        let match_count = full_words.iter()
            .filter(|w| short_words.iter().any(|sw| sw == *w || sw.starts_with(*w) || w.starts_with(sw)))
            .count();

        let min_words = full_words.len().min(short_words.len());
        if min_words <= 1 {
            match_count >= 1
        } else {
            match_count >= 2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- fuzzy_game_match ---

    #[test]
    fn exact_match_kalshi_format() {
        assert!(MarketMapper::fuzzy_game_match(
            "Duke Blue Devils @ UNC Tar Heels",
            "Duke Blue Devils at UNC Tar Heels Winner?",
            " at ", " Winner?"
        ));
    }

    #[test]
    fn abbreviation_st_to_state() {
        assert!(MarketMapper::fuzzy_game_match(
            "Appalachian St. Mountaineers @ Georgia Bulldogs",
            "Appalachian State at Georgia Winner?",
            " at ", " Winner?"
        ));
    }

    #[test]
    fn mascot_filtering() {
        // "Tigers" should be filtered out, matching on "Auburn" alone
        assert!(MarketMapper::fuzzy_game_match(
            "Auburn Tigers @ Alabama Crimson Tide",
            "Auburn at Alabama Winner?",
            " at ", " Winner?"
        ));
    }

    #[test]
    fn wrong_game_does_not_match() {
        assert!(!MarketMapper::fuzzy_game_match(
            "Duke Blue Devils @ UNC Tar Heels",
            "Kentucky at Tennessee Winner?",
            " at ", " Winner?"
        ));
    }

    #[test]
    fn polymarket_format_vs_separator() {
        assert!(MarketMapper::fuzzy_game_match(
            "Duke Blue Devils @ UNC Tar Heels",
            "Duke vs. UNC",
            " vs. ", ""
        ));
    }

    #[test]
    fn reversed_order_still_matches() {
        // ESPN has Away @ Home, but title may have them reversed
        assert!(MarketMapper::fuzzy_game_match(
            "UNC Tar Heels @ Duke Blue Devils",
            "Duke Blue Devils at UNC Tar Heels Winner?",
            " at ", " Winner?"
        ));
    }

    #[test]
    fn conference_tournament_synthetic_title() {
        // Synthetic title from normalize_conference_tournament_event uses "at"
        assert!(MarketMapper::fuzzy_game_match(
            "East Tennessee St. Buccaneers @ Furman Paladins",
            "East Tennessee St. at Furman",
            " at ", ""
        ));
    }

    #[test]
    fn no_separator_returns_false() {
        assert!(!MarketMapper::fuzzy_game_match(
            "Duke @ UNC",
            "Duke UNC Winner?",
            " at ", " Winner?"
        ));
    }

    // --- team_name_matches ---

    #[test]
    fn team_name_exact() {
        assert!(MarketMapper::team_name_matches("duke", "duke"));
    }

    #[test]
    fn team_name_contains() {
        assert!(MarketMapper::team_name_matches("north carolina tar heels", "north carolina"));
    }

    #[test]
    fn team_name_abbreviation() {
        assert!(MarketMapper::team_name_matches(
            "appalachian state mountaineers",
            "appalachian st."
        ));
    }

    #[test]
    fn team_name_no_match() {
        assert!(!MarketMapper::team_name_matches("duke blue devils", "kentucky wildcats"));
    }

    // --- market_is_home_team ---

    #[test]
    fn market_is_home_team_yes_is_home() {
        // Title: "Away at Home Winner?", YES = Home
        assert!(MarketMapper::market_is_home_team(
            "Duke at UNC Winner?",
            "UNC"
        ));
    }

    #[test]
    fn market_is_home_team_yes_is_away() {
        assert!(!MarketMapper::market_is_home_team(
            "Duke at UNC Winner?",
            "Duke"
        ));
    }
}
