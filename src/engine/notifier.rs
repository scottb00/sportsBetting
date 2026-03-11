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
    pub async fn send(&self, title: &str, body: &str) {
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

    /// Send a single batched notification for multiple placed orders.
    pub async fn notify_orders_batch(&self, orders: &[PlacedOrder]) {
        if orders.is_empty() {
            return;
        }

        if orders.len() == 1 {
            let o = &orders[0];
            let order_type = order_type_label(o);
            let title = format!("{} Order: {} {}", order_type, o.action, o.ticker);
            let mut body = String::new();
            if !o.game_label.is_empty() {
                body.push_str(&o.game_label);
                if !o.score.is_empty() {
                    body.push_str(&format!(" | {}", o.score));
                }
                if !o.clock.is_empty() {
                    body.push_str(&format!(" | {}", o.clock));
                }
                body.push('\n');
            }
            body.push_str(&format!(
                "{} {} {} contracts @ {}c\nSize: ${:.2}\nEdge: {:.1}%\nStrategy: {}",
                o.action, o.side, o.count, o.price_cents,
                o.size_dollars, o.edge_after_fees * 100.0, o.strategy,
            ));
            self.send(&title, &body).await;
            return;
        }

        let total_size: f64 = orders.iter().map(|o| o.size_dollars).sum();
        let title = format!("{} Orders Placed", orders.len());
        let mut lines = Vec::new();
        for o in orders {
            let mut line = String::new();
            line.push_str(&format!("[{}] ", order_type_label(o)));
            if !o.game_label.is_empty() {
                line.push_str(&format!("[{}]", o.game_label));
                if !o.score.is_empty() {
                    line.push_str(&format!(" {}", o.score));
                }
                if !o.clock.is_empty() {
                    line.push_str(&format!(" {}", o.clock));
                }
                line.push_str(" - ");
            }
            line.push_str(&format!(
                "{} {} {}ct @ {}c ${:.2} (edge {:.1}%)",
                o.action, o.side, o.count, o.price_cents,
                o.size_dollars, o.edge_after_fees * 100.0,
            ));
            lines.push(line);
        }
        lines.push(format!("Total: ${:.2}", total_size));

        self.send(&title, &lines.join("\n")).await;
    }

}

/// Return a short label describing the order type for notification titles.
fn order_type_label(o: &PlacedOrder) -> &'static str {
    if o.reduce_only {
        "REDUCE"
    } else if o.strategy == "clv_hunter" {
        "CLV"
    } else {
        "Break"
    }
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
