use std::time::Instant;

/// Raw moneyline odds for a single game from a sportsbook.
#[derive(Debug, Clone)]
pub struct MoneylineOdds {
    pub home_team: String,
    pub away_team: String,
    pub home_moneyline: f64,
    pub away_moneyline: f64,
}

impl MoneylineOdds {
    /// Convert American moneyline odds to implied probability (0.0–1.0).
    pub fn implied_prob(ml: f64) -> f64 {
        if ml < 0.0 {
            -ml / (-ml + 100.0)
        } else if ml > 0.0 {
            100.0 / (ml + 100.0)
        } else {
            0.5
        }
    }

    /// Devigged home win probability: normalize both sides to sum to 1.0.
    pub fn home_prob(&self) -> Option<f64> {
        let home_raw = Self::implied_prob(self.home_moneyline);
        let away_raw = Self::implied_prob(self.away_moneyline);
        let total = home_raw + away_raw;
        if total > 0.0 {
            Some(home_raw / total)
        } else {
            None
        }
    }
}

/// Raw implied probabilities for one bookmaker on one game (NOT devigged).
/// All values are HOME-aligned.
#[derive(Debug, Clone)]
pub struct BookOdds {
    pub bookmaker: String,
    pub home_implied_raw: f64,
    pub away_implied_raw: f64,
    /// When this bookmaker last updated its odds (unix seconds).
    pub last_update_ts: Option<i64>,
}

impl BookOdds {
    /// Create from American moneylines.
    pub fn from_moneylines(bookmaker: String, home_ml: f64, away_ml: f64) -> Self {
        Self {
            bookmaker,
            home_implied_raw: MoneylineOdds::implied_prob(home_ml),
            away_implied_raw: MoneylineOdds::implied_prob(away_ml),
            last_update_ts: None,
        }
    }

    /// Create from American moneylines with a last_update timestamp.
    pub fn from_moneylines_with_ts(bookmaker: String, home_ml: f64, away_ml: f64, last_update_ts: Option<i64>) -> Self {
        Self {
            bookmaker,
            home_implied_raw: MoneylineOdds::implied_prob(home_ml),
            away_implied_raw: MoneylineOdds::implied_prob(away_ml),
            last_update_ts,
        }
    }

    /// Age in seconds since last update, or None if unknown.
    pub fn age_secs(&self) -> Option<i64> {
        self.last_update_ts.map(|ts| chrono::Utc::now().timestamp() - ts)
    }

    /// Sportsbook "bid" on home = what you could sell home at = 1 - away_raw.
    pub fn home_bid(&self) -> f64 {
        1.0 - self.away_implied_raw
    }

    /// Sportsbook "offer" on home = what you'd pay to buy home = home_raw.
    pub fn home_offer(&self) -> f64 {
        self.home_implied_raw
    }

    /// Devigged midpoint (normalize so both sides sum to 1.0).
    pub fn home_devigged(&self) -> f64 {
        let total = self.home_implied_raw + self.away_implied_raw;
        if total > 0.0 {
            self.home_implied_raw / total
        } else {
            0.5
        }
    }
}

/// Maximum age (seconds) before a bookmaker's odds are considered stale and excluded from spread.
const MAX_BOOK_AGE_SECS: i64 = 120;

/// Composite sportsbook spread for a game, all HOME-aligned.
#[derive(Debug, Clone)]
pub struct SportsbookSpread {
    /// All books returned by the API (including stale ones).
    pub books: Vec<BookOdds>,
    /// Number of books that passed the freshness filter.
    pub fresh_count: usize,
    /// Best (highest) bid across fresh books — tightest bid on home.
    pub best_bid_home: Option<f64>,
    /// Best (lowest) offer across fresh books — tightest offer on home.
    pub best_offer_home: Option<f64>,
    /// When this spread was last updated.
    pub updated_at: Instant,
}

impl Default for SportsbookSpread {
    fn default() -> Self {
        Self {
            books: Vec::new(),
            fresh_count: 0,
            best_bid_home: None,
            best_offer_home: None,
            updated_at: Instant::now(),
        }
    }
}

impl SportsbookSpread {
    /// Build a composite spread from per-bookmaker raw odds.
    /// Filters out books older than MAX_BOOK_AGE_SECS. If a book has no timestamp, it's included.
    /// If the resulting spread is inverted (bid > offer), clamps both to midpoint.
    pub fn from_books(books: Vec<BookOdds>) -> Self {
        let now_ts = chrono::Utc::now().timestamp();
        let fresh: Vec<&BookOdds> = books
            .iter()
            .filter(|b| match b.last_update_ts {
                Some(ts) => (now_ts - ts) <= MAX_BOOK_AGE_SECS,
                None => true, // no timestamp = assume fresh
            })
            .collect();
        let fresh_count = fresh.len();
        let best_bid = fresh.iter().map(|b| b.home_bid()).reduce(f64::max);
        let best_offer = fresh.iter().map(|b| b.home_offer()).reduce(f64::min);

        // Clamp inverted spreads (bid > offer) to midpoint — indicates cross-book lag
        let (best_bid, best_offer) = match (best_bid, best_offer) {
            (Some(b), Some(o)) if b > o => {
                let mid = (b + o) / 2.0;
                (Some(mid), Some(mid))
            }
            other => other,
        };

        Self {
            books,
            fresh_count,
            best_bid_home: best_bid,
            best_offer_home: best_offer,
            updated_at: Instant::now(),
        }
    }

    /// Recompute the spread using only books updated after `min_ts` (unix seconds).
    /// Returns (bid, offer, fresh_count) home-aligned. Used during breaks to filter
    /// out pre-break stale lines.
    pub fn post_break_spread(&self, min_ts: i64) -> (Option<f64>, Option<f64>, usize) {
        let fresh: Vec<&BookOdds> = self.books.iter()
            .filter(|b| b.last_update_ts.is_some_and(|ts| ts >= min_ts))
            .collect();
        let count = fresh.len();
        if count == 0 {
            return (None, None, 0);
        }
        let best_bid = fresh.iter().map(|b| b.home_bid()).reduce(f64::max);
        let best_offer = fresh.iter().map(|b| b.home_offer()).reduce(f64::min);
        // Clamp inverted
        let (best_bid, best_offer) = match (best_bid, best_offer) {
            (Some(b), Some(o)) if b > o => {
                let mid = (b + o) / 2.0;
                (Some(mid), Some(mid))
            }
            other => other,
        };
        (best_bid, best_offer, count)
    }

    /// Get the spread aligned to a market's YES side.
    /// If YES = home, return (bid, offer) directly.
    /// If YES = away, flip: bid_yes = 1-offer_home, offer_yes = 1-bid_home.
    pub fn aligned_spread(&self, is_home: bool) -> (Option<f64>, Option<f64>) {
        if is_home {
            (self.best_bid_home, self.best_offer_home)
        } else {
            (
                self.best_offer_home.map(|o| 1.0 - o),
                self.best_bid_home.map(|b| 1.0 - b),
            )
        }
    }

    /// Check if the spread data is older than the given duration.
    pub fn is_stale(&self, max_age: std::time::Duration) -> bool {
        self.updated_at.elapsed() > max_age
    }

    /// Serialize all non-stale books as a JSON string for order logging.
    /// Each entry: {"book":"pinnacle","bid":0.629,"offer":0.667,"devigged":0.643,"age_secs":45}
    /// If `break_started_at` is Some, only includes books updated after that timestamp.
    pub fn fresh_books_json(&self, break_started_at: Option<i64>) -> String {
        let now_ts = chrono::Utc::now().timestamp();
        let entries: Vec<String> = self.books.iter()
            .filter(|b| {
                // Must pass general staleness check
                let age_ok = match b.last_update_ts {
                    Some(ts) => (now_ts - ts) <= MAX_BOOK_AGE_SECS,
                    None => true,
                };
                // If in a break, must also be post-break
                let break_ok = match break_started_at {
                    Some(min_ts) => b.last_update_ts.is_some_and(|ts| ts >= min_ts),
                    None => true,
                };
                age_ok && break_ok
            })
            .map(|b| {
                let age = b.last_update_ts.map(|ts| now_ts - ts).unwrap_or(-1);
                format!(
                    r#"{{"book":"{}","bid":{:.4},"offer":{:.4},"devigged":{:.4},"age_secs":{}}}"#,
                    b.bookmaker, b.home_bid(), b.home_offer(), b.home_devigged(), age
                )
            })
            .collect();
        format!("[{}]", entries.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_devig_moneyline() {
        let odds = MoneylineOdds {
            home_team: "Florida".into(),
            away_team: "Kentucky".into(),
            home_moneyline: -720.0,
            away_moneyline: 500.0,
        };
        let prob = odds.home_prob().unwrap();
        // -720 raw = 720/820 = 0.878; +500 raw = 100/600 = 0.167; total = 1.045
        // devigged home = 0.878 / 1.045 ≈ 0.840
        assert!((prob - 0.840).abs() < 0.01, "Expected ~0.840, got {}", prob);
    }

    #[test]
    fn test_even_odds() {
        let odds = MoneylineOdds {
            home_team: "A".into(),
            away_team: "B".into(),
            home_moneyline: -110.0,
            away_moneyline: -110.0,
        };
        let prob = odds.home_prob().unwrap();
        assert!((prob - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_book_odds_bid_offer() {
        // -200 home, +170 away
        let book = BookOdds::from_moneylines("pinnacle".into(), -200.0, 170.0);
        // home_raw = 200/300 = 0.6667, away_raw = 100/270 = 0.3704
        // bid = 1 - 0.3704 = 0.6296, offer = 0.6667
        assert!((book.home_bid() - 0.6296).abs() < 0.01);
        assert!((book.home_offer() - 0.6667).abs() < 0.01);
        // Bid < offer (spread has vig)
        assert!(book.home_bid() < book.home_offer());
    }

    #[test]
    fn test_book_odds_devigged() {
        let book = BookOdds::from_moneylines("dk".into(), -200.0, 170.0);
        let devigged = book.home_devigged();
        // 0.6667 / (0.6667 + 0.3704) = 0.6667 / 1.0370 ≈ 0.6428
        assert!((devigged - 0.643).abs() < 0.01);
        // Devigged should be between bid and offer
        assert!(devigged > book.home_bid());
        assert!(devigged < book.home_offer());
    }

    #[test]
    fn test_sportsbook_spread_from_books() {
        let books = vec![
            BookOdds::from_moneylines("pinnacle".into(), -200.0, 170.0),
            BookOdds::from_moneylines("draftkings".into(), -220.0, 180.0),
        ];
        let spread = SportsbookSpread::from_books(books);
        // Best bid = max of all bids (tightest)
        // Best offer = min of all offers (tightest)
        assert!(spread.best_bid_home.is_some());
        assert!(spread.best_offer_home.is_some());
        let bid = spread.best_bid_home.unwrap();
        let offer = spread.best_offer_home.unwrap();
        assert!(bid < offer, "Bid {bid} should be < offer {offer}");
    }

    #[test]
    fn test_aligned_spread_home() {
        let books = vec![BookOdds::from_moneylines("pin".into(), -150.0, 130.0)];
        let spread = SportsbookSpread::from_books(books);
        let (bid, offer) = spread.aligned_spread(true);
        assert_eq!(bid, spread.best_bid_home);
        assert_eq!(offer, spread.best_offer_home);
    }

    #[test]
    fn test_aligned_spread_away() {
        let books = vec![BookOdds::from_moneylines("pin".into(), -150.0, 130.0)];
        let spread = SportsbookSpread::from_books(books);
        let (bid_away, offer_away) = spread.aligned_spread(false);
        // Away bid = 1 - home_offer, Away offer = 1 - home_bid
        let home_bid = spread.best_bid_home.unwrap();
        let home_offer = spread.best_offer_home.unwrap();
        assert!((bid_away.unwrap() - (1.0 - home_offer)).abs() < 1e-10);
        assert!((offer_away.unwrap() - (1.0 - home_bid)).abs() < 1e-10);
        // Away bid < away offer
        assert!(bid_away.unwrap() < offer_away.unwrap());
    }

    #[test]
    fn test_empty_spread() {
        let spread = SportsbookSpread::from_books(vec![]);
        assert!(spread.best_bid_home.is_none());
        assert!(spread.best_offer_home.is_none());
        let (bid, offer) = spread.aligned_spread(true);
        assert!(bid.is_none());
        assert!(offer.is_none());
    }
}
