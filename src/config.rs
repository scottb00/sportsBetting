use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub kalshi: KalshiConfig,
    #[serde(default)]
    pub anthropic: Option<AnthropicConfig>,
    pub risk: RiskConfig,
    pub strategy: StrategyConfig,
    pub polling: PollingConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub intervals: IntervalConfig,
    #[serde(default)]
    pub notify: Option<NotifyConfig>,
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

#[derive(Debug, Deserialize)]
pub struct RiskConfig {
    pub max_position_per_game: f64,
    pub max_total_exposure: f64,
    pub daily_loss_limit: f64,
    pub kelly_fraction: f64,
    pub min_edge_threshold: f64,
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
    vec!["clv_hunter".to_string()]
}
fn default_min_volume() -> i64 { 20_000 }
fn default_min_price_cents() -> f64 { 10.0 }
fn default_max_price_cents() -> f64 { 90.0 }
fn default_order_ttl_secs() -> u64 { 120 }
fn default_max_contracts_per_game() -> i64 { 20 }

#[derive(Debug, Deserialize)]
pub struct StrategyConfig {
    pub break_ev_min_edge: f64,
    pub clv_hunter_min_edge: f64,
    /// Which strategies are allowed to place real orders (when dry_run = false).
    /// Others will log as DRY RUN. Default: ["clv_hunter"]
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
}

#[derive(Debug, Deserialize)]
pub struct PollingConfig {
    pub scoreboard_interval_secs: u64,
    pub summary_on_break_only: bool,
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

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;
        toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path))
    }
}
