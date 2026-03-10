use crate::config::NotifyConfig;
use crate::engine::executor::PlacedOrder;

/// Sends push notifications via Telegram Bot API.
#[derive(Clone)]
pub struct Notifier {
    client: reqwest::Client,
    api_url: String,
    chat_id: i64,
}

impl Notifier {
    pub fn new(config: &NotifyConfig) -> Self {
        let api_url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            config.telegram_bot_token
        );
        Self {
            client: reqwest::Client::new(),
            api_url,
            chat_id: config.telegram_chat_id,
        }
    }

    /// Send a notification. Fire-and-forget — errors are logged, not propagated.
    pub async fn send(&self, title: &str, body: &str, _priority: Priority) {
        let text = format!("*{}*\n{}", escape_markdown(title), escape_markdown(body));

        let result = self.client
            .post(&self.api_url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "MarkdownV2",
            }))
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!("Telegram notification sent: {}", title);
            }
            Ok(resp) => {
                let status = resp.status();
                let body_text = resp.text().await.unwrap_or_default();
                tracing::warn!("Telegram notification failed ({}): {}", status, body_text);
            }
            Err(e) => {
                tracing::warn!("Telegram notification error: {:?}", e);
            }
        }
    }

    /// Notify that an order was placed.
    pub async fn notify_order_placed(
        &self,
        strategy: &str,
        ticker: &str,
        side: &str,
        action: &str,
        count: i64,
        price_cents: i64,
        size_dollars: f64,
    ) {
        let title = format!("Order Placed: {}", strategy);
        let body = format!(
            "{} {} {} {} contracts @ {}c\nSize: ${:.2}\nTicker: {}",
            action, side, ticker, count, price_cents, size_dollars, ticker,
        );
        self.send(&title, &body, Priority::High).await;
    }

    /// Notify that a fill was received.
    pub async fn notify_fill(
        &self,
        ticker: &str,
        action: &str,
        count: i64,
        yes_price: i64,
    ) {
        let title = "Fill Received".to_string();
        let body = format!(
            "{} {} contracts @ {}c yes_price\nTicker: {}",
            action, count, yes_price, ticker,
        );
        self.send(&title, &body, Priority::High).await;
    }

    /// Send a single batched notification for multiple placed orders.
    pub async fn notify_orders_batch(&self, orders: &[PlacedOrder]) {
        if orders.is_empty() {
            return;
        }

        if orders.len() == 1 {
            let o = &orders[0];
            let title = format!("Order: {} {}", o.action, o.ticker);
            let body = format!(
                "{} {} {} contracts @ {}c\nSize: ${:.2}\nEdge: {:.1}%\nStrategy: {}",
                o.action, o.side, o.count, o.price_cents,
                o.size_dollars, o.edge_after_fees * 100.0, o.strategy,
            );
            self.send(&title, &body, Priority::High).await;
            return;
        }

        let total_size: f64 = orders.iter().map(|o| o.size_dollars).sum();
        let title = format!("{} Orders Placed", orders.len());
        let mut lines = Vec::new();
        for o in orders {
            lines.push(format!(
                "{} {} {} {}ct @ {}c ${:.2} (edge {:.1}%)",
                o.action, o.side, o.ticker, o.count, o.price_cents,
                o.size_dollars, o.edge_after_fees * 100.0,
            ));
        }
        lines.push(format!("Total: ${:.2}", total_size));

        self.send(&title, &lines.join("\n"), Priority::High).await;
    }

    /// Notify a risk event (loss limit, halt).
    pub async fn notify_risk_event(&self, message: &str) {
        self.send("Risk Alert", message, Priority::High).await;
    }
}

pub enum Priority {
    High,
    Default,
    Low,
}

/// Escape special characters for Telegram MarkdownV2.
fn escape_markdown(s: &str) -> String {
    let special = ['_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!'];
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if special.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
