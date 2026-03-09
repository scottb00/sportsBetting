use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A single mapping between ESPN, Kalshi, and Polymarket for one game.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketMapping {
    pub espn_event_id: String,
    pub espn_name: String, // e.g. "Illinois Fighting Illini at Maryland Terrapins"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kalshi_ticker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kalshi_title: Option<String>,
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
/// Uses Claude Haiku to match markets across platforms, then caches results locally.
pub struct MarketMapper {
    /// ESPN event ID -> mapping
    by_espn: HashMap<String, MarketMapping>,
    /// Kalshi ticker -> ESPN event ID
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
    /// Returns true if cache was loaded successfully.
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
        if let Some(kt) = &mapping.kalshi_ticker {
            self.kalshi_to_espn
                .insert(kt.clone(), mapping.espn_event_id.clone());
        }
        self.by_espn
            .insert(mapping.espn_event_id.clone(), mapping);
    }

    pub fn kalshi_ticker(&self, espn_event_id: &str) -> Option<&str> {
        self.by_espn
            .get(espn_event_id)
            .and_then(|m| m.kalshi_ticker.as_deref())
    }

    /// Determine if the Kalshi ticker's YES side is for the home team.
    /// Kalshi ticker format: KXNCAAMBGAME-{date}{AWAY}{HOME}-{SUFFIX}
    /// The suffix is the team code for the YES side.
    /// We compare the suffix against both team names in the title to determine direction.
    /// Title format: "Away at Home Winner?" — if suffix matches home team, YES = home.
    pub fn kalshi_is_home_team(&self, espn_event_id: &str) -> bool {
        let Some(mapping) = self.by_espn.get(espn_event_id) else {
            return true;
        };
        let Some(ticker) = &mapping.kalshi_ticker else {
            return true;
        };
        let Some(kalshi_title) = &mapping.kalshi_title else {
            return true;
        };

        // Extract suffix from ticker: last part after final '-'
        let suffix = ticker.rsplit('-').next().unwrap_or("").to_lowercase();
        if suffix.is_empty() {
            return true;
        }

        // Title: "Away at Home Winner?" — split to get away and home team names
        let title_clean = kalshi_title.trim_end_matches(" Winner?").trim_end_matches('?');
        let parts: Vec<&str> = title_clean.split(" at ").collect();
        if parts.len() != 2 {
            return true;
        }

        let away_name = Self::normalize_team_name(&parts[0].trim().to_lowercase());
        let home_name = Self::normalize_team_name(&parts[1].trim().to_lowercase());

        // Check which team the suffix abbreviation matches
        // Generate plausible abbreviations from team names
        let home_matches = Self::suffix_matches_team(&suffix, &home_name);
        let away_matches = Self::suffix_matches_team(&suffix, &away_name);

        tracing::debug!(
            "kalshi_is_home: suffix='{}' away='{}' home='{}' away_matches={} home_matches={}",
            suffix, away_name, home_name, away_matches, home_matches
        );

        if home_matches && !away_matches {
            true // YES = home team
        } else if away_matches && !home_matches {
            false // YES = away team
        } else {
            // Ambiguous — fall back to checking ESPN home
            let espn_home = mapping.espn_name.split(" @ ").nth(1).unwrap_or("").to_lowercase();
            // Default: check if suffix resembles any word in ESPN home team
            espn_home.split_whitespace().any(|w| {
                let w = w.trim_end_matches('.').to_lowercase();
                suffix.starts_with(&w[..w.len().min(suffix.len())]) || w.starts_with(&suffix)
            })
        }
    }

    /// Normalize abbreviations in team names for matching.
    fn normalize_team_name(name: &str) -> String {
        name.replace("st.", "state")
            .replace("miss.", "mississippi")
            .replace("n.c.", "north carolina")
            .replace("s.c.", "south carolina")
            .replace(" state state", " state") // fix double from "State St."
    }

    /// Check if a ticker suffix plausibly abbreviates a team name.
    /// E.g. "mrsh" matches "marshall", "gaso" matches "georgia southern", "ewu" matches "eastern washington"
    fn suffix_matches_team(suffix: &str, team_name: &str) -> bool {
        let normalized = Self::normalize_team_name(team_name);
        let words: Vec<&str> = normalized.split_whitespace().collect();
        if words.is_empty() {
            return false;
        }

        // Exact match on first word or prefix match
        if words[0].starts_with(suffix) || suffix.starts_with(words[0]) {
            return true;
        }

        // Multi-word concatenation: try all split points of suffix across words
        // "gaso" = "ga" + "so" (georgia + southern)
        // "mtst" = "mt" + "st" (montana + state)
        // "amcc" = "a" + "m" + "cc" or "am" + "cc" (a&m + corpus christi)
        if words.len() >= 2 {
            for split in 1..suffix.len() {
                let (a, b) = suffix.split_at(split);
                if words[0].starts_with(a) && words.iter().skip(1).any(|w| w.starts_with(b)) {
                    return true;
                }
            }
        }

        // Three-way split for 3+ word names: "utrgv" = "ut" + "r" + "gv" or similar
        if words.len() >= 3 {
            for s1 in 1..suffix.len().saturating_sub(1) {
                for s2 in (s1 + 1)..suffix.len() {
                    let a = &suffix[..s1];
                    let b = &suffix[s1..s2];
                    let c = &suffix[s2..];
                    if words[0].starts_with(a) && words[1].starts_with(b)
                        && words.iter().skip(2).any(|w| w.starts_with(c))
                    {
                        return true;
                    }
                }
            }
        }

        // University suffix: "ewu" = "e" + "w" + "u" (eastern washington university)
        // Check if suffix ends with 'u' and rest matches initials
        if suffix.ends_with('u') && words.len() >= 2 {
            let prefix = &suffix[..suffix.len() - 1];
            for split in 1..prefix.len() {
                let (a, b) = prefix.split_at(split);
                if words[0].starts_with(a) && words.iter().skip(1).any(|w| w.starts_with(b)) {
                    return true;
                }
            }
        }

        // Consonant skeleton: "mrsh" = "marshall" without vowels
        let consonants: String = words[0].chars().filter(|c| !"aeiou".contains(*c)).collect();
        if !consonants.is_empty()
            && (consonants == suffix
                || consonants.starts_with(suffix)
                || suffix.starts_with(&consonants))
        {
            return true;
        }

        // Prefix match with tolerance (3+ chars matching)
        let common_prefix = suffix
            .chars()
            .zip(words[0].chars())
            .take_while(|(a, b)| a == b)
            .count();
        if common_prefix >= 3 {
            return true;
        }

        false
    }

    pub fn polymarket_token(&self, espn_event_id: &str) -> Option<&str> {
        self.by_espn
            .get(espn_event_id)
            .and_then(|m| m.polymarket_token_id.as_deref())
    }

    /// Determine if the Polymarket YES token is for the home team.
    /// Polymarket titles: "TeamA vs. TeamB" — TeamA is the YES side.
    /// ESPN names: "Away @ Home"
    /// If TeamA matches the ESPN home team, YES = home; otherwise YES = away.
    pub fn polymarket_is_home_team(&self, espn_event_id: &str) -> bool {
        let Some(mapping) = self.by_espn.get(espn_event_id) else {
            return false; // default: assume away (safer)
        };
        let Some(poly_title) = &mapping.polymarket_title else {
            return false;
        };

        // Poly title: "TeamA Mascot vs. TeamB Mascot" — TeamA is the YES outcome
        let poly_first = poly_title
            .split(" vs.")
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();

        // ESPN name: "Away Team @ Home Team"
        let espn_parts: Vec<&str> = mapping.espn_name.split(" @ ").collect();
        if espn_parts.len() != 2 || poly_first.is_empty() {
            return false;
        }

        let espn_home = espn_parts[1].trim().to_lowercase();
        let espn_away = espn_parts[0].trim().to_lowercase();

        // Use proper team name matching, not substring contains
        let matches_home = Self::team_name_matches(&espn_home, &poly_first)
            || Self::team_name_matches(&poly_first, &espn_home);
        let matches_away = Self::team_name_matches(&espn_away, &poly_first)
            || Self::team_name_matches(&poly_first, &espn_away);

        tracing::debug!(
            "poly_is_home: poly_first='{}' espn_home='{}' espn_away='{}' matches_home={} matches_away={}",
            poly_first, espn_home, espn_away, matches_home, matches_away
        );

        if matches_home && !matches_away {
            true
        } else if matches_away && !matches_home {
            false
        } else {
            // Ambiguous or no match — default to false (YES = away)
            false
        }
    }

    pub fn all_mapped_kalshi_tickers(&self) -> Vec<String> {
        self.kalshi_to_espn.keys().cloned().collect()
    }

    /// Match markets to ESPN games using deterministic fuzzy matching.
    pub fn resolve_deterministic(
        &mut self,
        espn_games: &[(String, String)],        // (event_id, "Away @ Home")
        kalshi_markets: &[(String, String)],     // (ticker, title)
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
            let mut kalshi_match = None;
            let mut poly_match = None;

            // Try matching Kalshi markets
            for (ticker, title) in kalshi_markets {
                if matched_kalshi.contains(ticker) { continue; }
                if Self::fuzzy_game_match(espn_name, title, " at ", " Winner?") {
                    kalshi_match = Some((ticker.clone(), title.clone()));
                    matched_kalshi.insert(ticker.clone());
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
                let mapping = MarketMapping {
                    espn_event_id: espn_id.clone(),
                    espn_name: espn_name.clone(),
                    kalshi_ticker: kalshi_match.as_ref().map(|(t, _)| t.clone()),
                    kalshi_title: kalshi_match.as_ref().map(|(_, t)| t.clone()),
                    polymarket_token_id: poly_match.as_ref().map(|(t, _)| t.clone()),
                    polymarket_title: poly_match.as_ref().map(|(_, t)| t.clone()),
                };
                self.insert_mapping(mapping);
                matched_count += 1;
            }
        }

        let unmatched_kalshi = kalshi_markets.len() - matched_kalshi.len();
        let unmatched_poly = polymarket_markets.len() - matched_poly.len();
        tracing::info!(
            "Deterministic matching: {} ESPN games matched ({} Kalshi, {} Poly). Unmatched: {} Kalshi, {} Poly",
            matched_count, matched_kalshi.len(), matched_poly.len(), unmatched_kalshi, unmatched_poly
        );

        self.save_cache(today)?;
        Ok(())
    }

    /// Fuzzy match an ESPN game name against a market title.
    /// ESPN format: "Away Team @ Home Team"
    /// Market format: "TeamA {sep} TeamB{suffix}" (e.g., " at " for Kalshi, " vs. " for Poly)
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

        // Check if market teams match ESPN teams (in either order)
        let fwd = Self::team_name_matches(&espn_away, &market_a) && Self::team_name_matches(&espn_home, &market_b);
        let rev = Self::team_name_matches(&espn_away, &market_b) && Self::team_name_matches(&espn_home, &market_a);

        fwd || rev
    }

    /// Check if two team name strings refer to the same team.
    /// Handles abbreviations like "St." for "State", "Miss." for "Mississippi", etc.
    fn team_name_matches(full: &str, short: &str) -> bool {
        if full == short { return true; }

        let full_norm = Self::normalize_team_name(full);
        let short_norm = Self::normalize_team_name(short);

        // One contains the other
        if full_norm.contains(&short_norm) || short_norm.contains(&full_norm) {
            return true;
        }

        // Extract distinctive words (skip mascots and common words)
        let skip = ["state", "university", "the", "of", "college",
            // Common mascots that appear across many teams
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

        // At least the first word of each must match (school name, not mascot)
        let match_count = full_words.iter()
            .filter(|w| short_words.iter().any(|sw| sw == *w || sw.starts_with(*w) || w.starts_with(sw)))
            .count();

        // Need at least 2 word matches, OR 1 match when both have only 1 distinctive word
        let min_words = full_words.len().min(short_words.len());
        if min_words <= 1 {
            match_count >= 1
        } else {
            match_count >= 2
        }
    }

}
