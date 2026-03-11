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

    fn api_path(path: &str) -> String {
        format!("/trade-api/v2{}", path)
    }

    /// Send an authenticated request, check status, and return the raw response.
    /// Retries up to 3 times with exponential backoff on HTTP 429 (Too Many Requests).
    async fn request(&self, method: &str, path: &str, builder: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let api_path = Self::api_path(path);
        let headers = self.auth.sign_request(method, &api_path)?;

        let request = headers
            .apply(builder)
            .build()
            .with_context(|| format!("Failed to build {} {} request", method, path))?;

        let max_retries = 3u32;
        let mut last_body = String::new();
        let mut last_status = reqwest::StatusCode::OK;

        for attempt in 0..=max_retries {
            let req = request
                .try_clone()
                .with_context(|| format!("Failed to clone {} {} request", method, path))?;

            let resp = self
                .client
                .execute(req)
                .await
                .with_context(|| format!("{} {} failed", method, path))?;

            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }

            let body = resp.text().await.unwrap_or_default();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < max_retries {
                let delay = 1u64 << attempt; // 1s, 2s, 4s
                tracing::warn!(
                    "{} {} returned 429, retrying in {}s (attempt {}/{})",
                    method,
                    path,
                    delay,
                    attempt + 1,
                    max_retries
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                last_body = body;
                last_status = status;
                continue;
            }

            anyhow::bail!("{} {} returned {}: {}", method, path, status, body);
        }

        anyhow::bail!(
            "{} {} returned {} after {} retries: {}",
            method,
            path,
            last_status,
            max_retries,
            last_body
        );
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path);
        let resp = self.request("GET", path, self.client.get(&url)).await?;
        let body = resp.text().await
            .with_context(|| format!("Failed to read response body from GET {}", path))?;
        serde_json::from_str::<T>(&body)
            .with_context(|| format!("Failed to parse response from GET {}:\n{}", path, &body[..body.len().min(500)]))
    }

    async fn post<T: serde::de::DeserializeOwned, B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let url = self.url(path);
        self.request("POST", path, self.client.post(&url).json(body))
            .await?
            .json::<T>()
            .await
            .with_context(|| format!("Failed to parse response from POST {}", path))
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let url = self.url(path);
        self.request("DELETE", path, self.client.delete(&url)).await?;
        Ok(())
    }

    /// Build a query string from key-value pairs, filtering out None values.
    fn build_query(params: &[(&str, Option<String>)]) -> String {
        let parts: Vec<String> = params.iter()
            .filter_map(|(k, v)| v.as_ref().map(|val| format!("{}={}", k, val)))
            .collect();
        if parts.is_empty() { String::new() } else { format!("?{}", parts.join("&")) }
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
        let query = Self::build_query(&[
            ("ticker", ticker.map(|s| s.to_string())),
            ("min_ts", min_ts.map(|s| s.to_string())),
            ("limit", limit.map(|l| l.to_string())),
        ]);
        self.get(&format!("/portfolio/fills{}", query)).await
    }

    pub async fn get_orders(&self, ticker: Option<&str>) -> Result<GetOrdersResponse> {
        let path = match ticker {
            Some(t) => format!("/portfolio/orders?ticker={}&status=resting", t),
            None => "/portfolio/orders?status=resting".to_string(),
        };
        self.get(&path).await
    }

    // --- Order Book ---

    /// Fetch the current orderbook for a market ticker.
    pub async fn get_orderbook(&self, ticker: &str) -> Result<OrderBookSnapshot> {
        let resp: GetOrderbookResponse = self.get(&format!("/markets/{}/orderbook", ticker)).await?;
        Ok(resp.orderbook)
    }

    // --- Market Data ---

    pub async fn get_events_with_series(
        &self,
        category: Option<&str>,
        series_ticker: Option<&str>,
        status: Option<&str>,
        cursor: Option<&str>,
        limit: Option<i32>,
    ) -> Result<GetEventsResponse> {
        let query = Self::build_query(&[
            ("category", category.map(|s| s.to_string())),
            ("series_ticker", series_ticker.map(|s| s.to_string())),
            ("status", status.map(|s| s.to_string())),
            ("cursor", cursor.map(|s| s.to_string())),
            ("limit", limit.map(|l| l.to_string())),
            ("with_nested_markets", Some("true".to_string())),
        ]);
        self.get(&format!("/events{}", query)).await
    }

    /// Fetch a single market by ticker (includes settlement result for settled markets).
    pub async fn get_market(&self, ticker: &str) -> Result<Market> {
        let resp: serde_json::Value = self.get(&format!("/markets/{}", ticker)).await?;
        let market = resp.get("market").cloned().unwrap_or(resp);
        serde_json::from_value(market).context("Failed to parse market response")
    }

    pub async fn search_markets(&self, query: &str) -> Result<Vec<Market>> {
        let resp: serde_json::Value =
            self.get(&format!("/markets?search={}", urlencoding::encode(query)))
                .await?;
        let markets = resp.get("markets").cloned().unwrap_or(serde_json::Value::Array(vec![]));
        serde_json::from_value(markets).context("Failed to parse markets search result")
    }
}
