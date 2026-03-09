use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::types::*;

const GAMMA_URL: &str = "https://gamma-api.polymarket.com";
const CLOB_URL: &str = "https://clob.polymarket.com";
const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

#[derive(Debug, Clone)]
pub enum PolymarketEvent {
    PriceUpdate {
        asset_id: String,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
    },
    TradeUpdate {
        asset_id: String,
        price: f64,
        size: f64,
    },
    Connected,
    Disconnected,
}

pub struct PolymarketClient {
    client: Client,
}

impl Default for PolymarketClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PolymarketClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    // --- Gamma API (market discovery) ---

    /// Fetch active CBB game-level events (moneylines, spreads, totals) from Polymarket.
    ///
    /// Uses tag_id=102114 ("NCAA Basketball") with descending startDate ordering,
    /// which returns individual game events rather than futures markets.
    /// Paginates through all results and filters to only include events whose slug
    /// starts with "cbb-" (excluding women's basketball "cwbb-" and futures).
    pub async fn fetch_cbb_events(&self) -> Result<Vec<GammaEvent>> {
        let mut all_events = Vec::new();
        let page_size = 100;
        let mut offset = 0;

        loop {
            // tag_id=102114 is "NCAA Basketball" — with order=startDate&ascending=false
            // this returns individual game events (not futures/tournament winner markets).
            let url = format!(
                "{}/events?tag_id=102114&active=true&closed=false&limit={}&offset={}&order=startDate&ascending=false",
                GAMMA_URL, page_size, offset
            );
            let events: Vec<GammaEvent> = self
                .client
                .get(&url)
                .send()
                .await
                .context("Failed to fetch Polymarket CBB game events")?
                .json()
                .await
                .context("Failed to parse Polymarket CBB game events")?;

            let count = events.len();
            all_events.extend(events);

            if count < page_size {
                break;
            }
            offset += page_size;
        }

        // Filter to only CBB game events (slug starts with "cbb-"), excluding
        // women's basketball ("cwbb-") and any futures markets that lack the prefix.
        all_events.retain(|e| {
            e.slug
                .as_deref()
                .map(|s| s.starts_with("cbb-"))
                .unwrap_or(false)
        });

        // Deduplicate by event ID
        all_events.sort_by(|a, b| a.id.cmp(&b.id));
        all_events.dedup_by(|a, b| a.id == b.id);

        Ok(all_events)
    }

    /// Fetch active CBB futures events (tournament winner, seeds, etc.) from Polymarket.
    ///
    /// Uses the original tag_id=101178 (College Basketball) and tag_id=100149 (March Madness)
    /// which return futures/season-level markets.
    pub async fn fetch_cbb_futures(&self) -> Result<Vec<GammaEvent>> {
        let mut all_events = Vec::new();

        for tag_id in &["101178", "100149"] {
            let url = format!(
                "{}/events?tag_id={}&active=true&closed=false&limit=100",
                GAMMA_URL, tag_id
            );
            let events: Vec<GammaEvent> = self
                .client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("Failed to fetch Polymarket futures for tag {}", tag_id))?
                .json()
                .await
                .with_context(|| format!("Failed to parse Polymarket futures for tag {}", tag_id))?;

            all_events.extend(events);
        }

        // Deduplicate by event ID
        all_events.sort_by(|a, b| a.id.cmp(&b.id));
        all_events.dedup_by(|a, b| a.id == b.id);

        Ok(all_events)
    }

    // --- CLOB API (pricing) ---

    /// Fetch the order book for a specific token.
    pub async fn fetch_orderbook(&self, token_id: &str) -> Result<ClobOrderBook> {
        let url = format!("{}/order-book?token_id={}", CLOB_URL, token_id);
        self.client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch Polymarket order book")?
            .json()
            .await
            .context("Failed to parse Polymarket order book")
    }

    /// Fetch midpoint price for a token.
    pub async fn fetch_midpoint(&self, token_id: &str) -> Result<f64> {
        let url = format!("{}/midpoint?token_id={}", CLOB_URL, token_id);
        let resp: serde_json::Value = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch Polymarket midpoint")?
            .json()
            .await
            .context("Failed to parse Polymarket midpoint")?;

        resp.get("mid")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .or_else(|| resp.get("mid").and_then(|v| v.as_f64()))
            .context("Missing 'mid' field in midpoint response")
    }

    // --- WebSocket ---

    /// Connect to Polymarket WebSocket and stream price updates for given token IDs.
    pub async fn connect_ws(
        token_ids: Vec<String>,
    ) -> Result<mpsc::UnboundedReceiver<PolymarketEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            loop {
                match Self::run_ws_connection(&token_ids, &tx).await {
                    Ok(()) => {
                        tracing::info!("Polymarket WS closed normally, reconnecting...");
                    }
                    Err(e) => {
                        tracing::error!("Polymarket WS error: {:?}, reconnecting in 5s...", e);
                    }
                }
                let _ = tx.send(PolymarketEvent::Disconnected);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        });

        Ok(rx)
    }

    async fn run_ws_connection(
        token_ids: &[String],
        tx: &mpsc::UnboundedSender<PolymarketEvent>,
    ) -> Result<()> {
        let (ws_stream, _) = connect_async(WS_URL)
            .await
            .context("Failed to connect to Polymarket WebSocket")?;

        tracing::info!("Connected to Polymarket WebSocket");
        let _ = tx.send(PolymarketEvent::Connected);

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to market channel
        let sub = WsSubscription {
            assets_ids: token_ids.to_vec(),
            sub_type: "market".to_string(),
            custom_feature_enabled: Some(true),
        };

        let sub_json = serde_json::to_string(&sub)?;
        write.send(Message::Text(sub_json)).await?;
        tracing::info!(
            "Subscribed to Polymarket updates for {} tokens",
            token_ids.len()
        );

        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    Self::handle_message(&text, tx);
                }
                Ok(Message::Ping(data)) => {
                    write.send(Message::Pong(data)).await?;
                }
                Ok(Message::Close(_)) => break,
                Err(e) => return Err(e.into()),
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_message(text: &str, tx: &mpsc::UnboundedSender<PolymarketEvent>) {
        // Polymarket sends various event types
        let Ok(events) = serde_json::from_str::<Vec<serde_json::Value>>(text)
            .or_else(|_| serde_json::from_str::<serde_json::Value>(text).map(|v| vec![v]))
        else {
            return;
        };

        for event in events {
            let event_type = event
                .get("event_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match event_type {
                "price_change" | "best_bid_ask" => {
                    if let Some(asset_id) = event.get("asset_id").and_then(|v| v.as_str()) {
                        let best_bid = event
                            .get("best_bid")
                            .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_f64()));
                        let best_ask = event
                            .get("best_ask")
                            .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_f64()));
                        let _ = tx.send(PolymarketEvent::PriceUpdate {
                            asset_id: asset_id.to_string(),
                            best_bid,
                            best_ask,
                        });
                    }
                }
                "last_trade_price" => {
                    if let Some(asset_id) = event.get("asset_id").and_then(|v| v.as_str()) {
                        let price = event
                            .get("price")
                            .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_f64()))
                            .unwrap_or(0.0);
                        let size = event
                            .get("size")
                            .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or(v.as_f64()))
                            .unwrap_or(0.0);
                        let _ = tx.send(PolymarketEvent::TradeUpdate {
                            asset_id: asset_id.to_string(),
                            price,
                            size,
                        });
                    }
                }
                _ => {}
            }
        }
    }
}
