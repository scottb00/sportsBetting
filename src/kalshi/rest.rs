use anyhow::{Context, Result};
use reqwest::Client;

use super::auth::KalshiAuth;
use super::types::*;

const BASE_URL: &str = "https://api.elections.kalshi.com/trade-api/v2";
const DEMO_BASE_URL: &str = "https://demo-api.kalshi.co/trade-api/v2";

pub struct KalshiRestClient {
    client: Client,
    auth: KalshiAuth,
    base_url: String,
}

impl KalshiRestClient {
    pub fn new(auth: KalshiAuth, demo: bool) -> Self {
        Self {
            client: Client::new(),
            auth,
            base_url: if demo { DEMO_BASE_URL } else { BASE_URL }.to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn api_path(&self, path: &str) -> String {
        format!("/trade-api/v2{}", path)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let full_url = self.url(path);
        let api_path = self.api_path(path);
        let headers = self.auth.sign_request("GET", &api_path)?;

        let resp = headers
            .apply(self.client.get(&full_url))
            .send()
            .await
            .with_context(|| format!("GET {} failed", path))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GET {} returned {}: {}", path, status, body);
        }

        resp.json::<T>()
            .await
            .with_context(|| format!("Failed to parse response from GET {}", path))
    }

    async fn post<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let full_url = self.url(path);
        let api_path = self.api_path(path);
        let headers = self.auth.sign_request("POST", &api_path)?;

        let resp = headers
            .apply(self.client.post(&full_url).json(body))
            .send()
            .await
            .with_context(|| format!("POST {} failed", path))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            anyhow::bail!("POST {} returned {}: {}", path, status, body_text);
        }

        resp.json::<T>()
            .await
            .with_context(|| format!("Failed to parse response from POST {}", path))
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let full_url = self.url(path);
        let api_path = self.api_path(path);
        let headers = self.auth.sign_request("DELETE", &api_path)?;

        let resp = headers
            .apply(self.client.delete(&full_url))
            .send()
            .await
            .with_context(|| format!("DELETE {} failed", path))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("DELETE {} returned {}: {}", path, status, body);
        }

        Ok(())
    }

    // --- Portfolio ---

    pub async fn get_balance(&self) -> Result<Balance> {
        self.get("/portfolio/balance").await
    }

    pub async fn get_positions(&self) -> Result<GetPositionsResponse> {
        self.get("/portfolio/positions").await
    }

    // --- Orders ---

    pub async fn create_order(&self, order: &CreateOrderRequest) -> Result<CreateOrderResponse> {
        self.post("/portfolio/orders", order).await
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        self.delete(&format!("/portfolio/orders/{}", order_id)).await
    }

    /// Fetch fills from Kalshi. Optionally filter by ticker and/or min timestamp.
    pub async fn get_fills(&self, ticker: Option<&str>, min_ts: Option<&str>, limit: Option<i32>) -> Result<GetFillsResponse> {
        let mut params = Vec::new();
        if let Some(t) = ticker {
            params.push(format!("ticker={}", t));
        }
        if let Some(ts) = min_ts {
            params.push(format!("min_ts={}", ts));
        }
        if let Some(l) = limit {
            params.push(format!("limit={}", l));
        }
        let query = if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        };
        self.get(&format!("/portfolio/fills{}", query)).await
    }

    pub async fn get_orders(&self, ticker: Option<&str>) -> Result<GetOrdersResponse> {
        let path = match ticker {
            Some(t) => format!("/portfolio/orders?ticker={}&status=resting", t),
            None => "/portfolio/orders?status=resting".to_string(),
        };
        self.get(&path).await
    }

    // --- Market Data ---

    pub async fn get_orderbook(&self, ticker: &str, depth: Option<i32>) -> Result<OrderBookSnapshot> {
        let path = match depth {
            Some(d) => format!("/markets/{}/orderbook?depth={}", ticker, d),
            None => format!("/markets/{}/orderbook", ticker),
        };
        self.get::<serde_json::Value>(&path)
            .await
            .and_then(|v| {
                serde_json::from_value(v.get("orderbook").cloned().unwrap_or(v))
                    .context("Failed to parse orderbook")
            })
    }

    pub async fn get_events(
        &self,
        category: Option<&str>,
        status: Option<&str>,
        cursor: Option<&str>,
        limit: Option<i32>,
    ) -> Result<GetEventsResponse> {
        self.get_events_with_series(category, None, status, cursor, limit).await
    }

    pub async fn get_events_with_series(
        &self,
        category: Option<&str>,
        series_ticker: Option<&str>,
        status: Option<&str>,
        cursor: Option<&str>,
        limit: Option<i32>,
    ) -> Result<GetEventsResponse> {
        let mut params = Vec::new();
        if let Some(c) = category {
            params.push(format!("category={}", c));
        }
        if let Some(st) = series_ticker {
            params.push(format!("series_ticker={}", st));
        }
        if let Some(s) = status {
            params.push(format!("status={}", s));
        }
        if let Some(c) = cursor {
            params.push(format!("cursor={}", c));
        }
        if let Some(l) = limit {
            params.push(format!("limit={}", l));
        }
        params.push("with_nested_markets=true".to_string());
        let query = if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        };
        self.get(&format!("/events{}", query)).await
    }

    pub async fn get_market(&self, ticker: &str) -> Result<Market> {
        let resp: serde_json::Value = self.get(&format!("/markets/{}", ticker)).await?;
        serde_json::from_value(resp.get("market").cloned().unwrap_or(resp))
            .context("Failed to parse market")
    }

    pub async fn search_markets(&self, query: &str) -> Result<Vec<Market>> {
        let resp: serde_json::Value =
            self.get(&format!("/markets?search={}", urlencoding::encode(query)))
                .await?;
        let markets = resp.get("markets").cloned().unwrap_or(serde_json::Value::Array(vec![]));
        serde_json::from_value(markets).context("Failed to parse markets search result")
    }
}
