use anyhow::{Context, Result};
use rusqlite::Connection;

/// Persistent trade and order logger backed by SQLite.
pub struct TradeLogger {
    conn: Connection,
}

impl TradeLogger {
    pub fn new(db_path: &str) -> Result<Self> {
        let conn = Connection::open(db_path).context("Failed to open trade log database")?;

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

            CREATE TABLE IF NOT EXISTS pnl_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                strategy TEXT,
                unrealized_pnl REAL,
                realized_pnl REAL,
                total_exposure REAL,
                snapshot_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            ",
        )
        .context("Failed to create trade log tables")?;

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
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO orders (order_id, ticker, strategy, action, side, price_cents, count, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![order_id, ticker, strategy, action, side, price_cents, count, status],
        )?;
        Ok(())
    }

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
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO fills (trade_id, order_id, ticker, side, action, price_cents, count, fee_cents)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![trade_id, order_id, ticker, side, action, price_cents, count, fee_cents],
        )?;
        Ok(())
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
