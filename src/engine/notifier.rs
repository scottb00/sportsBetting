use crate::config::NotifyConfig;

/// Sends push notifications via ntfy.sh.
/// Subscribe on your phone: install ntfy app, add your topic.
/// No signup required.
#[derive(Clone)]
pub struct Notifier {
    client: reqwest::Client,
    topic_url: String,
}

impl Notifier {
    pub fn new(config: &NotifyConfig) -> Self {
        let topic_url = format!("https://ntfy.sh/{}", config.ntfy_topic);
        Self {
            client: reqwest::Client::new(),
            topic_url,
        }
    }

    /// Send a notification. Fire-and-forget — errors are logged, not propagated.
    pub async fn send(&self, title: &str, body: &str, priority: Priority) {
        let priority_str = match priority {
            Priority::High => "high",
            Priority::Default => "default",
            Priority::Low => "low",
        };

        let result = self.client
            .post(&self.topic_url)
            .header("Title", title)
            .header("Priority", priority_str)
            .header("Tags", "chart_with_upwards_trend")
            .body(body.to_string())
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!("Notification sent: {}", title);
            }
            Ok(resp) => {
                tracing::warn!("Notification failed ({}): {}", resp.status(), title);
            }
            Err(e) => {
                tracing::warn!("Notification error: {:?}", e);
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
