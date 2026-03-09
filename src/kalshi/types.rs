use serde::{Deserialize, Serialize};

// --- Order Book ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBookSnapshot {
    pub yes: Vec<PriceLevel>,
    pub no: Vec<PriceLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: i64, // cents (1-99)
    pub quantity: i64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub order_id: String,
    pub ticker: String,
    pub action: OrderAction,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: String,
    pub status: String,
    pub yes_price: Option<i64>,
    pub no_price: Option<i64>,
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
    pub balance: i64, // cents
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub ticker: String,
    #[serde(default)]
    pub market_exposure: i64,
    #[serde(default)]
    pub realized_pnl: i64,
    #[serde(default)]
    pub resting_orders_count: i64,
    #[serde(default)]
    pub total_traded: i64,
    #[serde(default)]
    pub yes_amount: i64,
    #[serde(default)]
    pub yes_avg_price: i64,
    #[serde(default)]
    pub no_amount: i64,
    #[serde(default)]
    pub no_avg_price: i64,
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
    pub yes_bid: Option<i64>,
    pub yes_ask: Option<i64>,
    pub no_bid: Option<i64>,
    pub no_ask: Option<i64>,
    pub volume: Option<i64>,
    pub open_interest: Option<i64>,
    #[serde(default)]
    pub yes_sub_title: Option<String>,
    #[serde(default)]
    pub no_sub_title: Option<String>,
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
    pub price: i64,
    pub delta: i64,
    pub side: String, // "yes" or "no"
    /// Kalshi sends "ts" as ISO-8601 string, not "timestamp" as integer.
    #[serde(default)]
    pub ts: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub trade_id: String,
    pub market_ticker: String,
    pub yes_price: i64,
    pub no_price: i64,
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
    pub yes_price: i64,
    pub no_price: i64,
    pub count: i64,
}
