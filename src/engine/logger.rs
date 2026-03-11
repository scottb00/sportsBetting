use anyhow::{Context, Result};
use rusqlite::Connection;

/// Game enrichment data to persist alongside orders/fills.
#[derive(Clone, Debug)]
pub struct GameInfo {
    pub game_name: String,
    pub home_team: String,
    pub away_team: String,
    pub is_home: bool,
    /// Edge in basis points at the time of order creation (positive = favorable)
    pub edge_bps: Option<f64>,
    /// ESPN fair value (probability, 0-1) for this ticker's YES side
    pub espn_fair: Option<f64>,
}

impl GameInfo {
    /// Compute signed edge in basis points from this game's fair value.
    /// Returns positive for favorable edge (buying below fair / selling above fair).
    pub fn compute_edge_bps(&self, price_cents: i64, side: &str, action: &str) -> Option<f64> {
        let fv = self.espn_fair?;
        let order_prob = price_cents as f64 / 100.0;
        let is_yes = side.eq_ignore_ascii_case("yes");
        let fair_for_side = if is_yes { fv } else { 1.0 - fv };
        let is_buy = action.eq_ignore_ascii_case("buy");
        let raw_edge = fair_for_side - order_prob;
        Some((if is_buy { raw_edge } else { -raw_edge }) * 10000.0)
    }

    /// Look up game info from the live game state for a given Kalshi ticker.
    pub fn from_game_state(
        game_state: &crate::engine::game_state::GameStateManager,
        ticker: &str,
    ) -> Option<Self> {
        let game = game_state.get_by_kalshi_ticker(ticker)?;
        let market = game.kalshi_markets.iter().find(|m| m.ticker == ticker)?;
        let fair_value = game.fair_value_for_market(market);
        Some(GameInfo {
            game_name: format!("{} vs {}", game.away_team, game.home_team),
            home_team: game.home_team.clone(),
            away_team: game.away_team.clone(),
            is_home: market.is_home,
            edge_bps: None,  // Caller computes if needed
            espn_fair: fair_value,
        })
    }
}

/// Persistent trade and order logger backed by SQLite.
pub struct TradeLogger {
    conn: Connection,
}

impl TradeLogger {
    pub fn new(db_path: &str) -> Result<Self> {
        let conn = Connection::open(db_path).context("Failed to open trade log database")?;

        // Enable WAL mode for concurrent read/write access (dashboard reads while bot writes)
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS orders (
                order_id TEXT PRIMARY KEY,
                ticker TEXT NOT NULL,
                strategy TEXT,
                action TEXT NOT NULL,
                side TEXT NOT NULL,
                price_cents INTEGER NOT NULL,
                count INTEGER NOT NULL,
                status TEXT NOT NULL,
                edge_bps REAL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS fills (
                fill_id INTEGER PRIMARY KEY AUTOINCREMENT,
                trade_id TEXT NOT NULL,
                order_id TEXT NOT NULL,
                ticker TEXT NOT NULL,
                side TEXT NOT NULL,
                action TEXT NOT NULL,
                price_cents INTEGER NOT NULL,
                count INTEGER NOT NULL,
                fee_cents REAL,
                filled_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_fills_trade_id ON fills(trade_id);

            CREATE TABLE IF NOT EXISTS clv_checks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                order_id TEXT NOT NULL,
                ticker TEXT NOT NULL,
                side TEXT NOT NULL,
                order_price_cents INTEGER NOT NULL,
                closing_mid_cents INTEGER NOT NULL,
                clv_cents INTEGER NOT NULL,
                captured INTEGER NOT NULL DEFAULT 0,
                checked_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            ",
        )
        .context("Failed to create trade log tables")?;

        // Migrate: add columns if they don't exist (for existing DBs)
        let _ = conn.execute_batch("ALTER TABLE orders ADD COLUMN edge_bps REAL");
        let _ = conn.execute_batch("ALTER TABLE orders ADD COLUMN game_name TEXT");
        let _ = conn.execute_batch("ALTER TABLE orders ADD COLUMN home_team TEXT");
        let _ = conn.execute_batch("ALTER TABLE orders ADD COLUMN away_team TEXT");
        let _ = conn.execute_batch("ALTER TABLE orders ADD COLUMN is_home INTEGER");
        let _ = conn.execute_batch("ALTER TABLE fills ADD COLUMN settlement_cents INTEGER");
        let _ = conn.execute_batch("ALTER TABLE orders ADD COLUMN espn_fair REAL");

        // One-time backfill: set empty strategies to "clv_hunter" (only strategy that has placed orders)
        let _ = conn.execute(
            "UPDATE orders SET strategy = 'clv_hunter' WHERE strategy IS NULL OR strategy = ''",
            [],
        );

        Ok(Self { conn })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log_order(
        &self,
        order_id: &str,
        ticker: &str,
        strategy: &str,
        action: &str,
        side: &str,
        price_cents: i64,
        count: i64,
        status: &str,
        edge_bps: Option<f64>,
        game_info: Option<&GameInfo>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO orders (order_id, ticker, strategy, action, side, price_cents, count, status, edge_bps, game_name, home_team, away_team, is_home, espn_fair)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                order_id, ticker, strategy, action, side, price_cents, count, status, edge_bps,
                game_info.map(|g| g.game_name.as_str()),
                game_info.map(|g| g.home_team.as_str()),
                game_info.map(|g| g.away_team.as_str()),
                game_info.map(|g| g.is_home as i32),
                game_info.and_then(|g| g.espn_fair),
            ],
        )?;
        Ok(())
    }

    /// Log a fill. Returns true if a new row was inserted (false if duplicate trade_id).
    /// `filled_at` should be the actual fill timestamp from Kalshi (ISO 8601).
    /// If None, falls back to current time.
    /// `fee_dollars` is the fee in dollars (Kalshi API `fee_cost` field). Stored in DB column `fee_cents` (legacy name).
    #[allow(clippy::too_many_arguments)]
    pub fn log_fill(
        &self,
        trade_id: &str,
        order_id: &str,
        ticker: &str,
        side: &str,
        action: &str,
        price_cents: i64,
        count: i64,
        fee_dollars: f64,
        filled_at: Option<&str>,
    ) -> Result<bool> {
        let rows = self.conn.execute(
            "INSERT INTO fills (trade_id, order_id, ticker, side, action, price_cents, count, fee_cents, filled_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, datetime('now')))
             ON CONFLICT(trade_id) DO UPDATE SET
               fee_cents = CASE WHEN excluded.fee_cents > 0 THEN excluded.fee_cents ELSE fee_cents END",
            rusqlite::params![trade_id, order_id, ticker, side, action, price_cents, count, fee_dollars, filled_at],
        )?;
        Ok(rows > 0)
    }

    /// Log a CLV (closing line value) check for a pre-game order.
    /// `clv_cents` is positive when the order captured value (bought below / sold above closing mid).
    #[allow(clippy::too_many_arguments)]
    pub fn log_clv_check(
        &self,
        order_id: &str,
        ticker: &str,
        side: &str,
        order_price_cents: i64,
        closing_mid_cents: i64,
        clv_cents: i64,
    ) -> Result<()> {
        let captured = if clv_cents > 0 { 1 } else { 0 };
        self.conn.execute(
            "INSERT INTO clv_checks (order_id, ticker, side, order_price_cents, closing_mid_cents, clv_cents, captured)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![order_id, ticker, side, order_price_cents, closing_mid_cents, clv_cents, captured],
        )?;
        Ok(())
    }

    /// Insert an order row if it doesn't exist, or update stub orders (strategy='')
    /// with corrected data from sync. Won't overwrite bot-placed orders that have strategy/edge.
    /// `created_at` should be the actual order creation time from Kalshi (ISO 8601).
    #[allow(clippy::too_many_arguments)]
    pub fn log_order_if_missing(
        &self,
        order_id: &str,
        ticker: &str,
        action: &str,
        side: &str,
        price_cents: i64,
        count: i64,
        status: &str,
        created_at: Option<&str>,
        game_info: Option<&GameInfo>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO orders (order_id, ticker, strategy, action, side, price_cents, count, status, created_at, game_name, home_team, away_team, is_home, edge_bps, espn_fair)
             VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, COALESCE(?8, datetime('now')), ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(order_id) DO UPDATE SET
               price_cents = excluded.price_cents,
               count = excluded.count,
               status = excluded.status,
               game_name = COALESCE(excluded.game_name, game_name),
               home_team = COALESCE(excluded.home_team, home_team),
               away_team = COALESCE(excluded.away_team, away_team),
               is_home = COALESCE(excluded.is_home, is_home),
               edge_bps = COALESCE(excluded.edge_bps, edge_bps),
               espn_fair = COALESCE(excluded.espn_fair, espn_fair)
             WHERE strategy = ''",
            rusqlite::params![
                order_id, ticker, action, side, price_cents, count, status, created_at,
                game_info.map(|g| g.game_name.as_str()),
                game_info.map(|g| g.home_team.as_str()),
                game_info.map(|g| g.away_team.as_str()),
                game_info.map(|g| g.is_home as i32),
                game_info.and_then(|g| g.edge_bps),
                game_info.and_then(|g| g.espn_fair),
            ],
        )?;
        Ok(())
    }

    /// Update order strategy if currently empty (backfill from in-memory state).
    pub fn update_order_strategy(&self, order_id: &str, strategy: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE orders SET strategy = ?1 WHERE order_id = ?2 AND (strategy IS NULL OR strategy = '')",
            rusqlite::params![strategy, order_id],
        )?;
        Ok(())
    }

    /// Get strategy for a single order from the DB (fallback when in-memory state is lost).
    pub fn get_order_strategy(&self, order_id: &str) -> Result<Option<String>> {
        let result: Option<String> = self.conn.query_row(
            "SELECT strategy FROM orders WHERE order_id = ?1",
            rusqlite::params![order_id],
            |row| row.get(0),
        ).ok();
        // Return None for empty strings (stub orders without strategy)
        Ok(result.filter(|s| !s.is_empty()))
    }

    /// Batch-read strategies for multiple orders from the DB (for startup recovery).
    pub fn get_order_strategies(&self, order_ids: &[String]) -> std::collections::HashMap<String, String> {
        let mut result = std::collections::HashMap::new();
        for order_id in order_ids {
            if let Ok(Some(strategy)) = self.get_order_strategy(order_id) {
                result.insert(order_id.clone(), strategy);
            }
        }
        result
    }

    /// Backfill game_name/home_team/away_team/is_home for orders that have a ticker but missing game data.
    /// Called at startup with live game state so historical orders get enriched.
    pub fn backfill_game_info(&self, game_info_by_ticker: &std::collections::HashMap<String, GameInfo>) -> Result<usize> {
        let mut count = 0;
        for (ticker, info) in game_info_by_ticker {
            let rows = self.conn.execute(
                "UPDATE orders SET game_name = ?1, home_team = ?2, away_team = ?3, is_home = ?4,
                 espn_fair = COALESCE(?5, espn_fair)
                 WHERE ticker = ?6 AND (game_name IS NULL OR game_name = '')",
                rusqlite::params![info.game_name, info.home_team, info.away_team, info.is_home as i32, info.espn_fair, ticker],
            )?;
            count += rows;
        }
        Ok(count)
    }

    /// Get distinct tickers from orders that have no game_name recorded.
    pub fn tickers_missing_game_info(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT ticker FROM orders WHERE game_name IS NULL OR game_name = ''"
        )?;
        let tickers = stmt.query_map([], |row| row.get(0))?
            .filter_map(Result::ok)
            .collect();
        Ok(tickers)
    }

    /// Get distinct tickers from fills that have no settlement recorded yet.
    pub fn unsettled_tickers(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT ticker FROM fills WHERE settlement_cents IS NULL"
        )?;
        let tickers = stmt.query_map([], |row| row.get(0))?
            .filter_map(Result::ok)
            .collect();
        Ok(tickers)
    }

    /// Record settlement price for all fills on a ticker.
    /// Called during cleanup when a game reaches Final phase.
    /// settlement_cents: 100 if YES side won, 0 if YES side lost.
    pub fn record_settlement(&self, ticker: &str, settlement_cents: i64) -> Result<usize> {
        let rows = self.conn.execute(
            "UPDATE fills SET settlement_cents = ?1 WHERE ticker = ?2 AND settlement_cents IS NULL",
            rusqlite::params![settlement_cents, ticker],
        )?;
        Ok(rows)
    }

    /// Update order status (e.g. to "filled" or "partial_fill").
    pub fn update_order_status(&self, order_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE orders SET status = ?1 WHERE order_id = ?2",
            rusqlite::params![status, order_id],
        )?;
        Ok(())
    }

    /// Get theoretical edge summary: total edge-weighted dollars from filled orders.
    /// Returns (total_edge_dollars, fill_count, avg_edge_bps) across all time.
    pub fn edge_summary(&self) -> Result<(f64, i64, f64)> {
        let (total_edge_dollars, fill_count): (f64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(o.edge_bps / 10000.0 * f.price_cents * f.count / 100.0), 0.0),
                    COUNT(*)
             FROM fills f
             JOIN orders o ON f.order_id = o.order_id
             WHERE o.edge_bps IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let avg_edge_bps = if fill_count > 0 {
            total_edge_dollars / fill_count as f64 * 10000.0
        } else {
            0.0
        };
        Ok((total_edge_dollars, fill_count, avg_edge_bps))
    }

    /// Get today's theoretical edge summary.
    pub fn edge_summary_today(&self) -> Result<(f64, i64)> {
        let (total_edge_dollars, fill_count): (f64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(o.edge_bps / 10000.0 * f.price_cents * f.count / 100.0), 0.0),
                    COUNT(*)
             FROM fills f
             JOIN orders o ON f.order_id = o.order_id
             WHERE o.edge_bps IS NOT NULL AND date(f.filled_at) = date('now')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((total_edge_dollars, fill_count))
    }

    /// Get realized P&L for today (settled fills only).
    ///
    /// Only includes fills where settlement is known. Open positions are excluded —
    /// their cost is already reflected in the Exposure metric.
    ///
    /// Note: `fee_cents` column stores fee in dollars despite the column name.
    pub fn daily_realized_pnl(&self) -> Result<f64> {
        let pnl: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(
                CASE
                  WHEN side = 'yes' AND action = 'buy'  THEN (settlement_cents - price_cents) * count / 100.0
                  WHEN side = 'yes' AND action = 'sell' THEN (price_cents - settlement_cents) * count / 100.0
                  WHEN side = 'no'  AND action = 'buy'  THEN ((100 - settlement_cents) - price_cents) * count / 100.0
                  WHEN side = 'no'  AND action = 'sell' THEN (price_cents - (100 - settlement_cents)) * count / 100.0
                END - COALESCE(fee_cents, 0)
            ), 0.0)
            FROM fills
            WHERE date(filled_at) = date('now') AND settlement_cents IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(pnl)
    }
}
