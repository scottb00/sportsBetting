use anyhow::{Context, Result};
use rusqlite::Connection;

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

            CREATE TABLE IF NOT EXISTS pnl_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                strategy TEXT,
                unrealized_pnl REAL,
                realized_pnl REAL,
                total_exposure REAL,
                snapshot_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

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

        // Migrate: add edge_bps column if it doesn't exist (for existing DBs)
        let _ = conn.execute_batch("ALTER TABLE orders ADD COLUMN edge_bps REAL");

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
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO orders (order_id, ticker, strategy, action, side, price_cents, count, status, edge_bps)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![order_id, ticker, strategy, action, side, price_cents, count, status, edge_bps],
        )?;
        Ok(())
    }

    /// Log a fill. Returns true if a new row was inserted (false if duplicate trade_id).
    /// `filled_at` should be the actual fill timestamp from Kalshi (ISO 8601).
    /// If None, falls back to current time.
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
        fee_cents: f64,
        filled_at: Option<&str>,
    ) -> Result<bool> {
        let rows = self.conn.execute(
            "INSERT OR IGNORE INTO fills (trade_id, order_id, ticker, side, action, price_cents, count, fee_cents, filled_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, datetime('now')))",
            rusqlite::params![trade_id, order_id, ticker, side, action, price_cents, count, fee_cents, filled_at],
        )?;
        Ok(rows > 0)
    }

    pub fn log_pnl_snapshot(
        &self,
        strategy: &str,
        unrealized: f64,
        realized: f64,
        exposure: f64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pnl_snapshots (strategy, unrealized_pnl, realized_pnl, total_exposure)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![strategy, unrealized, realized, exposure],
        )?;
        Ok(())
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

    /// Insert an order row only if it doesn't already exist (backfill from sync).
    /// Uses INSERT OR IGNORE so it won't overwrite orders with real strategy/edge data.
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
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO orders (order_id, ticker, strategy, action, side, price_cents, count, status, created_at)
             VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, COALESCE(?8, datetime('now')))",
            rusqlite::params![order_id, ticker, action, side, price_cents, count, status, created_at],
        )?;
        Ok(())
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

    /// Get total realized P&L for today.
    pub fn daily_realized_pnl(&self) -> Result<f64> {
        let pnl: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(
                CASE WHEN action = 'buy' THEN -price_cents * count / 100.0
                     ELSE price_cents * count / 100.0
                END - COALESCE(fee_cents, 0) / 100.0
            ), 0.0)
            FROM fills WHERE date(filled_at) = date('now')",
            [],
            |row| row.get(0),
        )?;
        Ok(pnl)
    }
}
