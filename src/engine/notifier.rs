use crate::config::NotifyConfig;
use crate::engine::executor::PlacedOrder;

/// Summary of a fill for notification purposes.
pub struct FillInfo {
    pub ticker: String,
    pub side: String,
    pub action: String,
    pub price_cents: i64,
    pub count: i64,
    pub fee_dollars: f64,
    pub game_name: Option<String>,
}

/// Escape special characters for Telegram MarkdownV2.
fn escape_markdown(s: &str) -> String {
    const SPECIAL: &[char] = &['_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!'];
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if SPECIAL.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

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
    async fn send(&self, title: &str, body: &str) {
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

    /// Format a single order as a multi-line body string.
    fn format_order(o: &PlacedOrder) -> String {
        // Game line: "Duke at UNC | 31-34 | Halftime" (score only if available)
        let game_line = if !o.score.is_empty() && !o.clock.is_empty() {
            format!("{} | {} (Away-Home) | {}", o.game_label, o.score, o.clock)
        } else if !o.clock.is_empty() {
            format!("{} | {}", o.game_label, o.clock)
        } else {
            o.game_label.clone()
        };

        // What we're buying: "YES on Duke" or "NO on Duke (= YES on UNC)"
        let side_label = if !o.yes_team.is_empty() {
            if o.side == "Yes" {
                format!("YES on {}", o.yes_team)
            } else {
                format!("NO on {}", o.yes_team)
            }
        } else {
            o.side.clone()
        };

        // Position change: "0 → +5 YES" or "+3 → 0 (closed)"
        fn pos_str(p: i64) -> String {
            if p > 0 { format!("+{} YES", p) }
            else if p < 0 { format!("{} YES (= {} NO)", p, p.unsigned_abs()) }
            else { "flat".to_string() }
        }
        let pos_line = if o.target_pos == 0 {
            format!("Position: {} -> flat (closed)", pos_str(o.current_pos))
        } else {
            format!("Position: {} -> {}", pos_str(o.current_pos), pos_str(o.target_pos))
        };

        // Risk label
        let risk_tag = if o.reduce_only { "[RISK REDUCING] " } else { "" };

        let edge_line = if o.edge_after_fees > 0.0 {
            format!("{}{} {}ct @ {}c | ${:.2} | Edge {:.1}% | {}", risk_tag, side_label, o.count, o.price_cents, o.size_dollars, o.edge_after_fees * 100.0, o.strategy)
        } else {
            format!("{}{} {}ct @ {}c | ${:.2} | {} (close)", risk_tag, side_label, o.count, o.price_cents, o.size_dollars, o.strategy)
        };

        format!("{}\n{}\n{}", game_line, edge_line, pos_line)
    }

    /// Send an alert notification (for anomalies, not trades).
    pub async fn send_alert(&self, title: &str, body: &str) {
        self.send(title, body).await;
    }

    /// Send a batched notification for fills received.
    pub async fn notify_fills(&self, fills: &[FillInfo], daily_pnl: f64) {
        if fills.is_empty() {
            return;
        }

        if fills.len() == 1 {
            let f = &fills[0];
            let game = f.game_name.as_deref().unwrap_or(&f.ticker);
            let side_upper = f.side.to_uppercase();
            let title = format!("Fill: {} {} {}ct @ {}c", game, side_upper, f.count, f.price_cents);
            let cost = f.price_cents as f64 * f.count as f64 / 100.0;
            let body = format!(
                "{}\n{} {} {}ct @ {}c | ${:.2} | fee ${:.4}\nDaily P&L: ${:.2}",
                game, f.action, side_upper, f.count, f.price_cents, cost, f.fee_dollars, daily_pnl,
            );
            self.send(&title, &body).await;
            return;
        }

        let total_contracts: i64 = fills.iter().map(|f| f.count).sum();
        let title = format!("{} Fills | {}ct total", fills.len(), total_contracts);
        let mut lines: Vec<String> = fills.iter().map(|f| {
            let game = f.game_name.as_deref().unwrap_or(&f.ticker);
            let cost = f.price_cents as f64 * f.count as f64 / 100.0;
            format!(
                "---\n{}\n{} {} {}ct @ {}c | ${:.2} | fee ${:.4}",
                game, f.action, f.side.to_uppercase(), f.count, f.price_cents, cost, f.fee_dollars,
            )
        }).collect();
        lines.push(format!("---\nDaily P&L: ${:.2}", daily_pnl));

        self.send(&title, &lines.join("\n")).await;
    }

    /// Send a single batched notification for multiple placed orders.
    pub async fn notify_orders_batch(&self, orders: &[PlacedOrder]) {
        if orders.is_empty() {
            return;
        }

        if orders.len() == 1 {
            let o = &orders[0];
            let risk_tag = if o.reduce_only { "RISK REDUCING | " } else { "" };
            let title = format!("{}Order: {} {} {}ct", risk_tag, o.game_label, o.side, o.count);
            let body = Self::format_order(o);
            self.send(&title, &body).await;
            return;
        }

        let total_size: f64 = orders.iter().map(|o| o.size_dollars).sum();
        let title = format!("{} Orders | ${:.2} total", orders.len(), total_size);
        let mut lines: Vec<String> = orders.iter().map(|o| {
            format!("---\n{}", Self::format_order(o))
        }).collect();
        lines.push(format!("---\nTotal: ${:.2}", total_size));

        self.send(&title, &lines.join("\n")).await;
    }
}
