use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::auth::KalshiAuth;
use super::types::*;

const WS_URL: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";
const DEMO_WS_URL: &str = "wss://demo-api.kalshi.co/trade-api/ws/v2";

#[derive(Debug, Clone)]
pub enum KalshiWsEvent {
    OrderBookSnapshot {
        market_ticker: String,
        snapshot: OrderBookSnapshot,
    },
    OrderBookDelta(OrderBookDelta),
    Trade(Trade),
    Fill(Fill),
    Connected,
    Disconnected,
    Error(String),
}

pub struct KalshiWsClient {
    auth: KalshiAuth,
    demo: bool,
}

impl KalshiWsClient {
    pub fn new(auth: KalshiAuth, demo: bool) -> Self {
        Self { auth, demo }
    }

    /// Start the WebSocket connection and return a receiver for events.
    /// Subscribes to orderbook_delta, trade, fill, and user_orders channels
    /// for the given market tickers.
    pub async fn connect(
        &self,
        market_tickers: Vec<String>,
    ) -> Result<mpsc::UnboundedReceiver<KalshiWsEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let auth = self.auth.clone();
        let demo = self.demo;
        let tickers = market_tickers.clone();

        tokio::spawn(async move {
            loop {
                match Self::run_connection(&auth, demo, &tickers, &tx).await {
                    Ok(()) => {
                        tracing::info!("Kalshi WS connection closed normally, reconnecting...");
                    }
                    Err(e) => {
                        tracing::error!("Kalshi WS error: {:?}, reconnecting in 5s...", e);
                        let _ = tx.send(KalshiWsEvent::Error(e.to_string()));
                    }
                }
                let _ = tx.send(KalshiWsEvent::Disconnected);
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        });

        Ok(rx)
    }

    async fn run_connection(
        auth: &KalshiAuth,
        demo: bool,
        market_tickers: &[String],
        tx: &mpsc::UnboundedSender<KalshiWsEvent>,
    ) -> Result<()> {
        let base_url = if demo { DEMO_WS_URL } else { WS_URL };
        let headers = auth.sign_websocket()?;

        let url = url::Url::parse(base_url)?;
        let request = tokio_tungstenite::tungstenite::http::Request::builder()
            .uri(base_url)
            .header("Host", url.host_str().unwrap_or(""))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .header("KALSHI-ACCESS-KEY", &headers.key_id)
            .header("KALSHI-ACCESS-TIMESTAMP", &headers.timestamp)
            .header("KALSHI-ACCESS-SIGNATURE", &headers.signature)
            .body(())?;

        let (ws_stream, _) = connect_async(request)
            .await
            .context("Failed to connect to Kalshi WebSocket")?;

        tracing::info!("Connected to Kalshi WebSocket");
        let _ = tx.send(KalshiWsEvent::Connected);

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to channels
        let subscribe_cmd = WsCommand {
            id: 1,
            cmd: "subscribe".to_string(),
            params: WsParams {
                channels: vec![
                    "orderbook_delta".to_string(),
                    "trade".to_string(),
                    "fill".to_string(),
                ],
                market_tickers: if market_tickers.is_empty() {
                    None
                } else {
                    Some(market_tickers.to_vec())
                },
            },
        };

        let cmd_json = serde_json::to_string(&subscribe_cmd)?;
        write.send(Message::Text(cmd_json.into())).await?;
        tracing::info!(
            "Subscribed to Kalshi channels for {} markets",
            market_tickers.len()
        );

        // Process incoming messages
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Err(e) = Self::handle_message(&text, tx) {
                        tracing::warn!("Failed to handle WS message: {:?}", e);
                    }
                }
                Ok(Message::Ping(data)) => {
                    write.send(Message::Pong(data)).await?;
                }
                Ok(Message::Close(_)) => {
                    tracing::info!("Kalshi WS received close frame");
                    break;
                }
                Err(e) => {
                    return Err(e.into());
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_message(
        text: &str,
        tx: &mpsc::UnboundedSender<KalshiWsEvent>,
    ) -> Result<()> {
        let msg: WsMessage = serde_json::from_str(text)?;

        let msg_type = msg.msg_type.as_deref().unwrap_or("");
        let data = match msg.msg {
            Some(d) => d,
            None => return Ok(()),
        };

        match msg_type {
            "orderbook_snapshot" => {
                if let (Some(ticker), Some(snapshot)) = (
                    data.get("market_ticker").and_then(|v| v.as_str()),
                    serde_json::from_value::<OrderBookSnapshot>(data.clone()).ok(),
                ) {
                    let _ = tx.send(KalshiWsEvent::OrderBookSnapshot {
                        market_ticker: ticker.to_string(),
                        snapshot,
                    });
                }
            }
            "orderbook_delta" => {
                if let Ok(delta) = serde_json::from_value::<OrderBookDelta>(data) {
                    let _ = tx.send(KalshiWsEvent::OrderBookDelta(delta));
                }
            }
            "trade" => {
                if let Ok(trade) = serde_json::from_value::<Trade>(data) {
                    let _ = tx.send(KalshiWsEvent::Trade(trade));
                }
            }
            "fill" => {
                if let Ok(fill) = serde_json::from_value::<Fill>(data) {
                    let _ = tx.send(KalshiWsEvent::Fill(fill));
                }
            }
            _ => {
                tracing::trace!("Unhandled WS message type: {}", msg_type);
            }
        }

        Ok(())
    }

    /// Update subscriptions to add/remove market tickers.
    /// Returns a command that should be sent over the WebSocket.
    pub fn subscribe_markets(tickers: &[String]) -> String {
        let cmd = WsCommand {
            id: 2,
            cmd: "subscribe".to_string(),
            params: WsParams {
                channels: vec!["orderbook_delta".to_string(), "trade".to_string()],
                market_tickers: Some(tickers.to_vec()),
            },
        };
        serde_json::to_string(&cmd).unwrap_or_default()
    }
}
