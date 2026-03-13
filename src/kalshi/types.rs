use serde::{Deserialize, Serialize};

// --- Kalshi API v2 Format (2026-03) ---
// Kalshi migrated from integer cents to decimal dollar strings:
//   price (i64 cents)    → price_dollars (string "0.2700")
//   delta/count (i64)    → delta_fp/count_fp (string "-52.00")
//   yes_price/no_price   → yes_price_dollars/no_price_dollars (string)
//   remaining_count      → remaining_count_fp (string)
// Internal representation stays as i64 cents/contracts.

// --- Custom Deserializers ---

/// Parse a dollar string like "0.2700" → i64 cents (27).
/// Also handles plain integers for backward compatibility.
fn deserialize_dollars_to_cents<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct DollarVisitor;
    impl<'de> de::Visitor<'de> for DollarVisitor {
        type Value = i64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a dollar string like \"0.2700\" or an integer in cents")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let dollars: f64 = v.parse().map_err(de::Error::custom)?;
            Ok((dollars * 100.0).round() as i64)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v as i64)
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok((v * 100.0).round() as i64)
        }
    }

    deserializer.deserialize_any(DollarVisitor)
}

/// Parse an optional dollar string like "0.2700" → Option<i64> cents.
fn deserialize_opt_dollars_to_cents<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct OptDollarVisitor;
    impl<'de> de::Visitor<'de> for OptDollarVisitor {
        type Value = Option<i64>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a dollar string, integer, or null")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let dollars: f64 = v.parse().map_err(de::Error::custom)?;
            Ok(Some((dollars * 100.0).round() as i64))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v as i64))
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok(Some((v * 100.0).round() as i64))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(OptDollarVisitor)
        }
    }

    deserializer.deserialize_any(OptDollarVisitor)
}

/// Parse an FP string like "-52.00" → i64 (-52).
/// Also handles plain integers for backward compatibility.
fn deserialize_fp_to_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct FpVisitor;
    impl<'de> de::Visitor<'de> for FpVisitor {
        type Value = i64;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an FP string like \"-52.00\" or an integer")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let val: f64 = v.parse().map_err(de::Error::custom)?;
            Ok(val.round() as i64)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v as i64)
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok(v.round() as i64)
        }
    }

    deserializer.deserialize_any(FpVisitor)
}

/// Deserialize an Option<f64> that may come as a JSON number or a JSON string.
fn deserialize_optional_f64_from_any<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct F64Visitor;
    impl<'de> de::Visitor<'de> for F64Visitor {
        type Value = Option<f64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a number or numeric string")
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v as f64))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v as f64))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            v.parse::<f64>().map(Some).map_err(de::Error::custom)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: serde::Deserializer<'de>>(self, deserializer: D2) -> Result<Self::Value, D2::Error> {
            deserializer.deserialize_any(F64Visitor)
        }
    }

    deserializer.deserialize_any(F64Visitor)
}

// --- Order Book ---

/// OrderBookSnapshot handles both old (integer) and new (dollar-string) formats:
/// - Old: `{"yes": [{"price": 45, "quantity": 100}], "no": [...]}`
/// - WS new: `{"yes_dollars_fp": [["0.4500", "100.00"]], "no_dollars_fp": [...]}`
/// - REST new: `{"yes_dollars": [["0.4500", "100.00"]], "no_dollars": [...]}`
#[derive(Debug, Clone, Serialize)]
pub struct OrderBookSnapshot {
    pub yes: Vec<PriceLevel>,
    pub no: Vec<PriceLevel>,
}

impl<'de> serde::Deserialize<'de> for OrderBookSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;

        fn parse_levels(val: &serde_json::Value) -> Vec<PriceLevel> {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return vec![],
            };
            arr.iter()
                .filter_map(|item| {
                    // New format: [price_dollars_str, count_fp_str]
                    if let Some(pair) = item.as_array() {
                        if pair.len() == 2 {
                            let price_dollars: f64 = pair[0].as_str()?.parse().ok()?;
                            let qty: f64 = pair[1].as_str()?.parse().ok()?;
                            return Some(PriceLevel {
                                price: (price_dollars * 100.0).round() as i64,
                                quantity: qty.round() as i64,
                            });
                        }
                    }
                    // Old format: {price: i64, quantity: i64}
                    if let Some(obj) = item.as_object() {
                        let price = obj.get("price")?.as_i64()?;
                        let quantity = obj.get("quantity")?.as_i64()?;
                        return Some(PriceLevel { price, quantity });
                    }
                    None
                })
                .collect()
        }

        let yes = v
            .get("yes_dollars_fp")
            .or_else(|| v.get("yes_dollars"))
            .or_else(|| v.get("yes"))
            .map(|v| parse_levels(v))
            .unwrap_or_default();

        let no = v
            .get("no_dollars_fp")
            .or_else(|| v.get("no_dollars"))
            .or_else(|| v.get("no"))
            .map(|v| parse_levels(v))
            .unwrap_or_default();

        Ok(OrderBookSnapshot { yes, no })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    #[serde(deserialize_with = "deserialize_dollars_to_cents")]
    pub price: i64, // cents (1-99)
    #[serde(alias = "quantity_fp", deserialize_with = "deserialize_fp_to_i64")]
    pub quantity: i64,
}

/// REST API response for GET /markets/{ticker}/orderbook.
/// Kalshi wraps the book in `orderbook` (old) or `orderbook_fp` (new).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetOrderbookResponse {
    #[serde(alias = "orderbook_fp")]
    pub orderbook: OrderBookSnapshot,
}

// --- Orders ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Yes,
    No,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderAction {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    GoodTillCanceled,
    ImmediateOrCancel,
    FillOrKill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateOrderRequest {
    pub ticker: String,
    pub action: OrderAction,
    pub side: OrderSide,
    pub count: i64,
    #[serde(rename = "type")]
    pub order_type: String, // "limit" or "market"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yes_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<TimeInForce>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    /// Unix timestamp (seconds) when the order should auto-expire.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration_ts: Option<i64>,
}

/// Order from Kalshi REST API responses. Kalshi now sends dollar-string fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub order_id: String,
    pub ticker: String,
    pub action: OrderAction,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: String,
    pub status: String,
    #[serde(alias = "yes_price", alias = "yes_price_dollars", default, deserialize_with = "deserialize_opt_dollars_to_cents")]
    pub yes_price: Option<i64>,
    #[serde(alias = "no_price", alias = "no_price_dollars", default, deserialize_with = "deserialize_opt_dollars_to_cents")]
    pub no_price: Option<i64>,
    #[serde(alias = "remaining_count", alias = "remaining_count_fp", deserialize_with = "deserialize_fp_to_i64")]
    pub remaining_count: i64,
    pub created_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateOrderResponse {
    pub order: Order,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetOrdersResponse {
    pub orders: Vec<Order>,
    pub cursor: Option<String>,
}

// --- Portfolio ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    /// Balance is still integer cents per Kalshi docs, but deserialize_dollars_to_cents
    /// handles both integers (cents) and dollar strings just in case.
    #[serde(alias = "balance_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub balance: i64, // cents
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub ticker: String,
    /// Monetary field: old=integer cents, new=dollar string "1.50" → 150 cents
    #[serde(default, alias = "market_exposure_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub market_exposure: i64,
    #[serde(default, alias = "realized_pnl_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub realized_pnl: i64,
    #[serde(default, deserialize_with = "deserialize_fp_to_i64")]
    pub resting_orders_count: i64,
    #[serde(default, alias = "total_traded_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub total_traded: i64,
    /// Net position: positive = holding YES contracts, negative = holding NO.
    /// Old: integer, New: FP string "5.00" → 5
    #[serde(default, alias = "position_fp", deserialize_with = "deserialize_fp_to_i64")]
    pub position: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetPositionsResponse {
    pub market_positions: Vec<Position>,
    pub cursor: Option<String>,
}

// --- Market Data ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub status: String,
    #[serde(alias = "yes_bid", alias = "yes_bid_dollars", default, deserialize_with = "deserialize_opt_dollars_to_cents")]
    pub yes_bid: Option<i64>,
    #[serde(alias = "yes_ask", alias = "yes_ask_dollars", default, deserialize_with = "deserialize_opt_dollars_to_cents")]
    pub yes_ask: Option<i64>,
    #[serde(alias = "no_bid", alias = "no_bid_dollars", default, deserialize_with = "deserialize_opt_dollars_to_cents")]
    pub no_bid: Option<i64>,
    #[serde(alias = "no_ask", alias = "no_ask_dollars", default, deserialize_with = "deserialize_opt_dollars_to_cents")]
    pub no_ask: Option<i64>,
    #[serde(alias = "volume_fp", default, deserialize_with = "deserialize_opt_fp_to_i64")]
    pub volume: Option<i64>,
    #[serde(alias = "open_interest_fp", default, deserialize_with = "deserialize_opt_fp_to_i64")]
    pub open_interest: Option<i64>,
    #[serde(default)]
    pub yes_sub_title: Option<String>,
    #[serde(default)]
    pub no_sub_title: Option<String>,
    /// Settlement result: "yes" or "no" for settled markets, absent for active.
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event_ticker: String,
    pub title: String,
    pub category: String,
    pub markets: Option<Vec<Market>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetEventsResponse {
    pub events: Vec<Event>,
    pub cursor: Option<String>,
}

// --- Fills (REST) ---

/// Fill from Kalshi REST API. Note: Kalshi sends both `ticker` and `market_ticker`
/// fields with the same value — we keep `ticker` and ignore unknown fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestFill {
    pub trade_id: String,
    pub order_id: String,
    pub ticker: String,
    pub side: String,
    pub action: String,
    #[serde(alias = "count", alias = "count_fp", deserialize_with = "deserialize_fp_to_i64")]
    pub count: i64,
    #[serde(alias = "yes_price", alias = "yes_price_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub yes_price: i64,
    #[serde(alias = "no_price", alias = "no_price_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub no_price: i64,
    #[serde(default)]
    pub is_taker: bool,
    #[serde(default, deserialize_with = "deserialize_optional_f64_from_any")]
    pub fee_cost: Option<f64>,
    pub created_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetFillsResponse {
    pub fills: Vec<RestFill>,
    pub cursor: Option<String>,
}

// --- WebSocket ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsCommand {
    pub id: i64,
    pub cmd: String,
    pub params: WsParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsParams {
    pub channels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_tickers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsMessage {
    pub sid: Option<i64>,
    pub seq: Option<i64>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub msg: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookDelta {
    pub market_ticker: String,
    #[serde(alias = "price", alias = "price_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub price: i64,
    #[serde(alias = "delta", alias = "delta_fp", deserialize_with = "deserialize_fp_to_i64")]
    pub delta: i64,
    pub side: String, // "yes" or "no"
    /// Kalshi sends "ts" as ISO-8601 string, not "timestamp" as integer.
    #[serde(default)]
    pub ts: Option<String>,
    /// Sequence number from the WS envelope (set by WS handler after deserialization).
    #[serde(skip)]
    pub seq: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub trade_id: String,
    pub market_ticker: String,
    #[serde(alias = "yes_price", alias = "yes_price_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub yes_price: i64,
    #[serde(alias = "no_price", alias = "no_price_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub no_price: i64,
    #[serde(alias = "count", alias = "count_fp", deserialize_with = "deserialize_fp_to_i64")]
    pub count: i64,
    pub taker_side: String,
    pub created_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub trade_id: String,
    pub order_id: String,
    pub market_ticker: String,
    pub side: String,
    pub action: String,
    #[serde(alias = "yes_price", alias = "yes_price_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub yes_price: i64,
    #[serde(alias = "no_price", alias = "no_price_dollars", deserialize_with = "deserialize_dollars_to_cents")]
    pub no_price: i64,
    #[serde(alias = "count", alias = "count_fp", deserialize_with = "deserialize_fp_to_i64")]
    pub count: i64,
}

// --- Helper for Optional FP fields ---

fn deserialize_opt_fp_to_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct OptFpVisitor;
    impl<'de> de::Visitor<'de> for OptFpVisitor {
        type Value = Option<i64>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an FP string, integer, or null")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let val: f64 = v.parse().map_err(de::Error::custom)?;
            Ok(Some(val.round() as i64))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v as i64))
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok(Some(v.round() as i64))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(OptFpVisitor)
        }
    }

    deserializer.deserialize_any(OptFpVisitor)
}
