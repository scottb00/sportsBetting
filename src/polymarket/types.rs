use serde::{Deserialize, Deserializer, Serialize};

/// Deserialize a value that may be a number or a string containing a number.
fn string_or_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let val: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match val {
        Some(serde_json::Value::Number(n)) => Ok(n.as_f64()),
        Some(serde_json::Value::String(s)) => Ok(s.parse::<f64>().ok()),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GammaEvent {
    pub id: String,
    pub title: String,
    pub slug: Option<String>,
    pub active: bool,
    pub closed: bool,
    pub markets: Vec<GammaMarket>,
    /// Series slug identifying the sport/league, e.g. "ncaa-cbb".
    #[serde(rename = "seriesSlug", default)]
    pub series_slug: Option<String>,
    /// Date of the event in YYYY-MM-DD format.
    #[serde(rename = "eventDate", default)]
    pub event_date: Option<String>,
    /// Live game score, e.g. "68-75".
    #[serde(default)]
    pub score: Option<String>,
    /// Game period, e.g. "H1", "H2", "HT", "FT".
    #[serde(default)]
    pub period: Option<String>,
    /// Whether the game is currently live.
    #[serde(default)]
    pub live: Option<bool>,
    /// Whether the game has ended.
    #[serde(default)]
    pub ended: Option<bool>,
    /// Polymarket internal game ID.
    #[serde(rename = "gameId", default)]
    pub game_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GammaMarket {
    pub id: String,
    pub question: String,
    #[serde(rename = "outcomePrices")]
    pub outcome_prices: Option<String>, // JSON string like "[\"0.45\",\"0.55\"]"
    #[serde(rename = "bestBid", default, deserialize_with = "string_or_f64")]
    pub best_bid: Option<f64>,
    #[serde(rename = "bestAsk", default, deserialize_with = "string_or_f64")]
    pub best_ask: Option<f64>,
    #[serde(default, deserialize_with = "string_or_f64")]
    pub spread: Option<f64>,
    #[serde(rename = "lastTradePrice", default, deserialize_with = "string_or_f64")]
    pub last_trade_price: Option<f64>,
    #[serde(default, deserialize_with = "string_or_f64")]
    pub volume: Option<f64>,
    #[serde(rename = "volume24hr", default, deserialize_with = "string_or_f64")]
    pub volume_24hr: Option<f64>,
    #[serde(default, deserialize_with = "string_or_f64")]
    pub liquidity: Option<f64>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>, // JSON string like "[\"token1\",\"token2\"]"
    pub active: bool,
    pub closed: bool,
    /// Sports market type: "moneyline", "spreads", or "totals".
    #[serde(rename = "sportsMarketType", default)]
    pub sports_market_type: Option<String>,
    /// Line value for spreads/totals, e.g. -6.5 or 141.5.
    #[serde(default)]
    pub line: Option<f64>,
    /// Game start time, e.g. "2026-03-08 23:30:00+00".
    #[serde(rename = "gameStartTime", default)]
    pub game_start_time: Option<String>,
    /// Short title for grouped market display, e.g. "Spread -6.5", "O/U 141.5".
    #[serde(rename = "groupItemTitle", default)]
    pub group_item_title: Option<String>,
    /// Outcome labels as JSON string, e.g. "[\"Team A\", \"Team B\"]".
    #[serde(default)]
    pub outcomes: Option<String>,
}

impl GammaMarket {
    /// Parse outcome prices from the JSON string.
    /// Returns (yes_price, no_price).
    pub fn parsed_outcome_prices(&self) -> Option<(f64, f64)> {
        let prices_str = self.outcome_prices.as_ref()?;
        let prices: Vec<String> = serde_json::from_str(prices_str).ok()?;
        if prices.len() >= 2 {
            let yes = prices[0].parse::<f64>().ok()?;
            let no = prices[1].parse::<f64>().ok()?;
            Some((yes, no))
        } else {
            None
        }
    }

    /// Parse CLOB token IDs from JSON string.
    /// Returns (yes_token_id, no_token_id).
    pub fn parsed_token_ids(&self) -> Option<(String, String)> {
        let ids_str = self.clob_token_ids.as_ref()?;
        let ids: Vec<String> = serde_json::from_str(ids_str).ok()?;
        if ids.len() >= 2 {
            Some((ids[0].clone(), ids[1].clone()))
        } else {
            None
        }
    }

    /// Get the YES implied probability (midpoint or last trade).
    pub fn yes_probability(&self) -> Option<f64> {
        // Prefer midpoint from bid/ask
        if let (Some(bid), Some(ask)) = (self.best_bid, self.best_ask) {
            return Some((bid + ask) / 2.0);
        }
        // Fall back to outcome prices
        if let Some((yes, _)) = self.parsed_outcome_prices() {
            return Some(yes);
        }
        self.last_trade_price
    }
}

// --- WebSocket types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsSubscription {
    pub assets_ids: Vec<String>,
    #[serde(rename = "type")]
    pub sub_type: String, // "market"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_feature_enabled: Option<bool>,
}
