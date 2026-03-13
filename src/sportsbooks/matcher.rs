use std::collections::HashMap;

use crate::engine::game_state::GameState;
use crate::engine::market_mapper::MarketMapper;

use super::odds_api::OddsApiGame;
use super::types::{MoneylineOdds, SportsbookSpread};

/// Match sportsbook moneyline odds to existing GameState entries.
/// Returns a map of ESPN event_id → devigged home win probability.
///
/// Both home and away team names must match to avoid false positives.
pub fn match_odds_to_games(
    odds: &[MoneylineOdds],
    games: &HashMap<String, GameState>,
) -> HashMap<String, f64> {
    let mut result = HashMap::new();

    for ml in odds {
        for (event_id, gs) in games {
            if result.contains_key(event_id) {
                continue; // already matched
            }

            let home_matches = MarketMapper::team_name_matches(&gs.home_team, &ml.home_team)
                || MarketMapper::team_name_matches(&ml.home_team, &gs.home_team);
            let away_matches = MarketMapper::team_name_matches(&gs.away_team, &ml.away_team)
                || MarketMapper::team_name_matches(&ml.away_team, &gs.away_team);

            if home_matches && away_matches
                && let Some(prob) = ml.home_prob()
            {
                result.insert(event_id.clone(), prob);
            }
        }
    }

    result
}

/// Match Odds API games to existing GameState entries.
/// Returns a map of ESPN event_id → SportsbookSpread (composite bid/ask from all bookmakers).
///
/// Both home and away team names must match to avoid false positives.
pub fn match_odds_api_to_games(
    api_games: &[OddsApiGame],
    games: &HashMap<String, GameState>,
) -> HashMap<String, SportsbookSpread> {
    let mut result = HashMap::new();

    for api_game in api_games {
        for (event_id, gs) in games {
            if result.contains_key(event_id) {
                continue;
            }

            let home_matches = MarketMapper::team_name_matches(&gs.home_team, &api_game.home_team)
                || MarketMapper::team_name_matches(&api_game.home_team, &gs.home_team);
            let away_matches = MarketMapper::team_name_matches(&gs.away_team, &api_game.away_team)
                || MarketMapper::team_name_matches(&api_game.away_team, &gs.away_team);

            if home_matches && away_matches {
                result.insert(
                    event_id.clone(),
                    SportsbookSpread::from_books(api_game.books.clone()),
                );
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_exact_names() {
        let mut games = HashMap::new();
        games.insert(
            "401234".to_string(),
            GameState::new("401234".to_string(), "Florida Gators".to_string(), "Kentucky Wildcats".to_string()),
        );

        let odds = vec![MoneylineOdds {
            home_team: "Florida".to_string(),
            away_team: "Kentucky".to_string(),
            home_moneyline: -200.0,
            away_moneyline: 170.0,
        }];

        let result = match_odds_to_games(&odds, &games);
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("401234"));
    }

    #[test]
    fn test_no_false_positive_partial() {
        let mut games = HashMap::new();
        games.insert(
            "401234".to_string(),
            GameState::new("401234".to_string(), "Florida Gators".to_string(), "Kentucky Wildcats".to_string()),
        );

        // Only home matches, away doesn't — should NOT match
        let odds = vec![MoneylineOdds {
            home_team: "Florida".to_string(),
            away_team: "Duke".to_string(),
            home_moneyline: -200.0,
            away_moneyline: 170.0,
        }];

        let result = match_odds_to_games(&odds, &games);
        assert!(result.is_empty());
    }
}
