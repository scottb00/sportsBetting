use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex as StdMutex};
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

/// Handle for sending additional subscribe commands to an active WS connection.
#[derive(Clone)]
pub struct KalshiWsHandle {
    cmd_tx: mpsc::UnboundedSender<Vec<String>>,
    /// All tickers currently subscribed (for reconnect).
    all_tickers: Arc<StdMutex<Vec<String>>>,
}

impl KalshiWsHandle {
    /// Subscribe to additional market tickers on the live connection.
    /// Returns the number of new tickers added (skips already-subscribed ones).
    pub fn subscribe_additional(&self, new_tickers: Vec<String>) -> usize {
        let mut all = self.all_tickers.lock().unwrap();
        let truly_new: Vec<String> = new_tickers
            .into_iter()
            .filter(|t| !all.contains(t))
            .collect();
        let count = truly_new.len();
        if count > 0 {
            all.extend(truly_new.clone());
            let _ = self.cmd_tx.send(truly_new);
        }
        count
    }
}

pub struct KalshiWsClient {
    auth: KalshiAuth,
    demo: bool,
}

impl KalshiWsClient {
    pub fn new(auth: KalshiAuth, demo: bool) -> Self {
        Self { auth, demo }
    }

    /// Start the WebSocket connection and return a receiver for events
    /// plus a handle for subscribing to additional tickers mid-session.
    pub async fn connect(
        &self,
        market_tickers: Vec<String>,
    ) -> Result<(mpsc::UnboundedReceiver<KalshiWsEvent>, KalshiWsHandle)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let all_tickers = Arc::new(StdMutex::new(market_tickers.clone()));
        let auth = self.auth.clone();
        let demo = self.demo;
        let tickers_for_task = all_tickers.clone();

        tokio::spawn(async move {
            let mut cmd_rx = cmd_rx;
            loop {
                // Get full ticker list for this connection attempt
                let current_tickers = tickers_for_task.lock().unwrap().clone();
                match Self::run_connection(&auth, demo, &current_tickers, &tx, &mut cmd_rx).await {
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

        let handle = KalshiWsHandle { cmd_tx, all_tickers };
        Ok((rx, handle))
    }

    async fn run_connection(
        auth: &KalshiAuth,
        demo: bool,
        market_tickers: &[String],
        tx: &mpsc::UnboundedSender<KalshiWsEvent>,
        cmd_rx: &mut mpsc::UnboundedReceiver<Vec<String>>,
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
        let mut cmd_id: i64 = 1;
        if !market_tickers.is_empty() {
            let subscribe_cmd = WsCommand {
                id: cmd_id,
                cmd: "subscribe".to_string(),
                params: WsParams {
                    channels: vec![
                        "orderbook_delta".to_string(),
                        "trade".to_string(),
                        "fill".to_string(),
                    ],
                    market_tickers: Some(market_tickers.to_vec()),
                },
            };
            cmd_id += 1;

            let cmd_json = serde_json::to_string(&subscribe_cmd)?;
            write.send(Message::Text(cmd_json)).await?;
            tracing::info!(
                "Subscribed to Kalshi channels for {} markets",
                market_tickers.len()
            );
        }

        // Process incoming messages AND command channel
        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = Self::handle_message(&text, tx) {
                                tracing::warn!("Failed to handle WS message: {:?}", e);
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            write.send(Message::Pong(data)).await?;
                        }
                        Some(Ok(Message::Close(_))) => {
                            tracing::info!("Kalshi WS received close frame");
                            break;
                        }
                        Some(Err(e)) => {
                            return Err(e.into());
                        }
                        None => break,
                        _ => {}
                    }
                }
                Some(new_tickers) = cmd_rx.recv() => {
                    // Subscribe to additional tickers on the live connection
                    let subscribe_cmd = WsCommand {
                        id: cmd_id,
                        cmd: "subscribe".to_string(),
                        params: WsParams {
                            channels: vec![
                                "orderbook_delta".to_string(),
                                "trade".to_string(),
                            ],
                            market_tickers: Some(new_tickers.clone()),
                        },
                    };
                    cmd_id += 1;
                    let cmd_json = serde_json::to_string(&subscribe_cmd)?;
                    write.send(Message::Text(cmd_json)).await?;
                    tracing::info!(
                        "Subscribed to {} additional Kalshi markets mid-session",
                        new_tickers.len()
                    );
                }
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
}
