use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub kalshi: KalshiConfig,
    #[serde(default)]
    pub anthropic: Option<AnthropicConfig>,
    pub strategy: StrategyConfig,
    pub polling: PollingConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub intervals: IntervalConfig,
    #[serde(default)]
    pub notify: Option<NotifyConfig>,
    #[serde(default)]
    pub odds_api: Option<OddsApiConfig>,
}

#[derive(Debug, Deserialize)]
pub struct KalshiConfig {
    pub api_key_id: String,
    pub private_key_path: String,
    pub demo: bool,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicConfig {
    pub api_key: String,
}

fn default_maintenance_interval_secs() -> u64 { 30 }

#[derive(Debug, Deserialize)]
pub struct IntervalConfig {
    /// How often to run the combined maintenance tick (cleanup + discovery + order/fill sync).
    #[serde(default = "default_maintenance_interval_secs")]
    pub maintenance_interval_secs: u64,
}

impl Default for IntervalConfig {
    fn default() -> Self {
        Self {
            maintenance_interval_secs: default_maintenance_interval_secs(),
        }
    }
}

fn default_live_strategies() -> Vec<String> {
    vec!["pregame".to_string(), "break_ev".to_string()]
}
fn default_min_volume() -> i64 { 20_000 }
fn default_min_price_cents() -> f64 { 10.0 }
fn default_max_price_cents() -> f64 { 90.0 }
fn default_order_ttl_secs() -> u64 { 120 }
fn default_max_contracts_per_game() -> i64 { 20 }
fn default_min_trade_contracts() -> i64 { 5 }
fn default_max_close_contracts() -> i64 { 30 }
fn default_conviction_max_contracts() -> i64 { 100 }
// Long break tiers: (min_score, contracts)
fn default_conviction_long_tiers() -> Vec<(f64, i64)> {
    vec![(0.0, 5), (1.0, 15), (3.0, 40), (5.0, 100)]
}
// Short break tiers (reduced weights already applied to scores)
fn default_conviction_short_tiers() -> Vec<(f64, i64)> {
    vec![(0.0, 10), (1.0, 30), (2.0, 100)]
}

#[derive(Debug, Deserialize)]
pub struct StrategyConfig {
    pub break_ev_min_edge: f64,
    pub clv_hunter_min_edge: f64, // used as pregame_min_edge
    /// Which strategies are allowed to place real orders (when dry_run = false).
    /// Others will log as DRY RUN. Default: ["pregame", "break_ev"]
    #[serde(default = "default_live_strategies")]
    pub live_strategies: Vec<String>,
    #[serde(default = "default_min_volume")]
    pub min_volume: i64,
    #[serde(default = "default_min_price_cents")]
    pub min_price_cents: f64,
    #[serde(default = "default_max_price_cents")]
    pub max_price_cents: f64,
    /// TTL for resting orders in seconds. Orders older than this are cancelled.
    /// Defaults to 120 seconds (2 minutes).
    #[serde(default = "default_order_ttl_secs")]
    pub order_ttl_secs: u64,
    /// Hard cap on total contracts per game (across all tickers/orders). Default: 20.
    #[serde(default = "default_max_contracts_per_game")]
    pub max_contracts_per_game: i64,
    /// Minimum contract delta required to place an add order (anti-scalp guard).
    /// Prevents tiny top-up orders when position is already close to target. Default: 5.
    #[serde(default = "default_min_trade_contracts")]
    pub min_trade_contracts: i64,
    /// Maximum contracts per single close order. Limits visibility of large close orders
    /// on thin books. Default: 30.
    #[serde(default = "default_max_close_contracts")]
    pub max_close_contracts: i64,
    /// Maximum contracts for highest conviction tier. Default: 100.
    #[serde(default = "default_conviction_max_contracts")]
    pub conviction_max_contracts: i64,
    /// Long break conviction tiers: (min_score_threshold, contracts).
    /// Evaluated in reverse order (highest threshold first). Default: [(0,5),(1,15),(3,40),(5,100)].
    #[serde(default = "default_conviction_long_tiers")]
    pub conviction_long_tiers: Vec<(f64, i64)>,
    /// Short break conviction tiers (TV timeouts, with halved book weights).
    /// Default: [(0,10),(1,30),(2,100)].
    #[serde(default = "default_conviction_short_tiers")]
    pub conviction_short_tiers: Vec<(f64, i64)>,
}

#[derive(Debug, Deserialize)]
pub struct PollingConfig {
    pub scoreboard_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    pub db_path: String,
    pub cache_path: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NotifyConfig {
    /// Telegram bot token from @BotFather
    pub telegram_bot_token: String,
    /// Telegram chat ID to send notifications to
    pub telegram_chat_id: i64,
}

#[derive(Debug, Deserialize)]
pub struct OddsApiConfig {
    pub api_key: String,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;
        toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path))
    }
}
