use axum::{
    Json, Router,
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
};
use rusqlite::Connection;
use serde::Serialize;

use crate::engine::bot::{SharedState, SharedOrderBooks, SharedLogger, SharedBreakLog};
use crate::engine::market_prep::book_prices;

/// Shared state for the dashboard server.
#[derive(Clone)]
struct DashboardState {
    bot: SharedState,
    order_books: SharedOrderBooks,
    break_log: SharedBreakLog,
    db_path: String,
    dry_run: bool,
}

// --- JSON response types ---

#[derive(Serialize)]
struct GameView {
    espn_event_id: String,
    home_team: String,
    away_team: String,
    home_score: Option<i32>,
    away_score: Option<i32>,
    phase: String,
    status_detail: String,
    espn_home_win_prob: Option<f64>,
    /// Game start time as unix seconds (from ESPN).
    start_time_ts: Option<i64>,
    markets: Vec<MarketView>,
}

#[derive(Serialize)]
struct MarketView {
    ticker: String,
    is_home: bool,
    yes_bid: Option<f64>,
    yes_ask: Option<f64>,
    yes_mid: Option<f64>,
    fair_value: Option<f64>,
    /// Best tradeable edge (always positive if opportunity exists). Sign indicates YES (>0 raw) or NO (<0 raw) side.
    edge: Option<f64>,
    /// Which side has the edge: "YES" or "NO"
    edge_side: Option<String>,
    volume: Option<i64>,
    has_resting_order: bool,
    exposure: f64,
    /// Net position: positive = holding YES contracts, negative = holding NO contracts
    position: i64,
}

#[derive(Serialize)]
struct RiskView {
    daily_pnl: f64,
    current_total_exposure: f64,
    max_total_exposure: f64,
    max_position_per_game: f64,
    daily_loss_limit: f64,
    halted: bool,
    dry_run: bool,
    open_orders: usize,
    in_flight: usize,
    started_at: String,
}

#[derive(Serialize)]
struct OrderRow {
    order_id: String,
    ticker: String,
    game_name: Option<String>,
    /// Whether YES on this ticker = home team wins
    is_home: Option<bool>,
    home_team: Option<String>,
    away_team: Option<String>,
    strategy: String,
    action: String,
    side: String,
    price_cents: i64,
    count: i64,
    status: String,
    created_at: String,
    /// Perceived edge: fair_value - order_price (positive = buying below fair value)
    edge: Option<f64>,
}

#[derive(Serialize)]
struct FillRow {
    trade_id: String,
    order_id: String,
    ticker: String,
    game_name: Option<String>,
    is_home: Option<bool>,
    home_team: Option<String>,
    away_team: Option<String>,
    strategy: String,
    side: String,
    action: String,
    price_cents: i64,
    count: i64,
    fee_dollars: f64,
    filled_at: String,
    edge_bps: Option<f64>,
}

#[derive(Serialize)]
struct EdgeSummary {
    total_edge_dollars: f64,
    total_fills: i64,
    avg_edge_bps: f64,
    today_edge_dollars: f64,
    today_fills: i64,
}

// --- Handlers ---

async fn index() -> impl IntoResponse {
    let html = include_str!("../../static/dashboard.html");
    Html(html)
}

// Snapshot structs for lock-free view building in api_games.
struct MarketSnapshot {
    ticker: String,
    is_home: bool,
    fair_value: Option<f64>,
    bid: Option<f64>,
    ask: Option<f64>,
    mid: Option<f64>,
    volume: Option<i64>,
    has_resting: bool,
    committed: i64,
    position: i64,
}

struct GameSnapshot {
    espn_event_id: String,
    home_team: String,
    away_team: String,
    home_score: Option<i32>,
    away_score: Option<i32>,
    phase: String,
    status_detail: String,
    espn_home_win_prob: Option<f64>,
    start_time_ts: Option<i64>,
    markets: Vec<MarketSnapshot>,
}

async fn api_games(State(state): State<DashboardState>) -> impl IntoResponse {
    // Phase 1: Snapshot all data under locks (O(1) lookups only, no string formatting).
    let snapshots: Vec<GameSnapshot> = {
        let books = state.order_books.read().await;
        let s = state.bot.lock().await;
        s.game_state.games.values()
            .filter(|g| {
                !g.home_team.eq_ignore_ascii_case("TBD") && !g.away_team.eq_ignore_ascii_case("TBD")
            })
            .map(|g| {
                let markets = g.kalshi_markets.iter().map(|m| {
                    let prices = book_prices(&books, &m.ticker);
                    MarketSnapshot {
                        ticker: m.ticker.clone(),
                        is_home: m.is_home,
                        fair_value: g.fair_value_for_market(m),
                        bid: prices.bid,
                        ask: prices.ask,
                        mid: prices.mid,
                        volume: m.volume,
                        has_resting: s.order_manager.has_resting_order(&m.ticker),
                        committed: s.order_manager.committed_contracts(&m.ticker),
                        position: s.risk.net_position(&m.ticker),
                    }
                }).collect();
                GameSnapshot {
                    espn_event_id: g.espn_event_id.clone(),
                    home_team: g.home_team.clone(),
                    away_team: g.away_team.clone(),
                    home_score: g.home_score,
                    away_score: g.away_score,
                    phase: format!("{:?}", g.phase),
                    status_detail: g.status_detail.clone(),
                    espn_home_win_prob: g.espn_home_win_prob,
                    start_time_ts: g.start_time_ts,
                    markets,
                }
            })
            .collect()
        // books and s (both locks) drop here
    };

    // Phase 2: Build view structs and sort — no locks held.
    let mut games: Vec<GameView> = snapshots.into_iter().map(|g| {
        let markets = g.markets.into_iter().map(|m| {
            let (edge, edge_side) = match (m.fair_value, m.mid) {
                (Some(fv), Some(mid)) => {
                    let mid_prob = mid / 100.0;
                    let yes_edge = fv - mid_prob;
                    if yes_edge >= 0.0 {
                        (Some(yes_edge), Some("YES".to_string()))
                    } else {
                        (Some(-yes_edge), Some("NO".to_string()))
                    }
                }
                _ => (None, None),
            };
            MarketView {
                ticker: m.ticker,
                is_home: m.is_home,
                yes_bid: m.bid,
                yes_ask: m.ask,
                yes_mid: m.mid,
                fair_value: m.fair_value,
                edge,
                edge_side,
                volume: m.volume,
                has_resting_order: m.has_resting,
                exposure: m.committed as f64,
                position: m.position,
            }
        }).collect();
        GameView {
            espn_event_id: g.espn_event_id,
            home_team: g.home_team,
            away_team: g.away_team,
            home_score: g.home_score,
            away_score: g.away_score,
            phase: g.phase,
            status_detail: g.status_detail,
            espn_home_win_prob: g.espn_home_win_prob,
            start_time_ts: g.start_time_ts,
            markets,
        }
    }).collect();

    // Sort: Live/Break first, then by start time (soonest first), Final last
    games.sort_by(|a, b| {
        let phase_ord = |p: &str| -> u8 {
            match p {
                "Live" => 0,
                "Break" | "Halftime" => 1,
                "PreGame" => 2,
                "Final" => 4,
                _ => 3,
            }
        };
        let pa = phase_ord(&a.phase);
        let pb = phase_ord(&b.phase);
        pa.cmp(&pb).then_with(|| {
            let ta = a.start_time_ts.unwrap_or(i64::MAX);
            let tb = b.start_time_ts.unwrap_or(i64::MAX);
            ta.cmp(&tb)
        })
    });
    Json(games)
}

async fn api_risk(State(state): State<DashboardState>) -> impl IntoResponse {
    let s = state.bot.lock().await;
    Json(RiskView {
        daily_pnl: s.risk.daily_pnl,
        current_total_exposure: s.risk.current_total_exposure,
        max_total_exposure: s.risk.max_total_exposure,
        max_position_per_game: s.risk.max_position_per_game,
        daily_loss_limit: s.risk.daily_loss_limit,
        halted: s.risk.is_halted(),
        dry_run: state.dry_run,
        open_orders: s.order_manager.open_order_count(),
        in_flight: s.order_manager.in_flight_count(),
        started_at: s.started_at.to_rfc3339(),
    })
}

/// Per-ticker enrichment data snapshotted from live game state.
struct TickerEnrich {
    game_name: String,
    home_team: String,
    away_team: String,
    is_home: bool,
    fair_value: Option<f64>,
}

/// Build a ticker → enrichment map with a single pass over game state, then release the lock.
async fn snapshot_ticker_enrich(state: &SharedState) -> std::collections::HashMap<String, TickerEnrich> {
    let s = state.lock().await;
    let mut map = std::collections::HashMap::new();
    for game in s.game_state.games.values() {
        let game_name = format!("{} vs {}", game.away_team, game.home_team);
        for market in &game.kalshi_markets {
            map.insert(market.ticker.clone(), TickerEnrich {
                game_name: game_name.clone(),
                home_team: game.home_team.clone(),
                away_team: game.away_team.clone(),
                is_home: market.is_home,
                fair_value: game.fair_value_for_market(market),
            });
        }
    }
    map // lock dropped here
}

async fn api_orders(State(state): State<DashboardState>) -> impl IntoResponse {
    let mut rows = match query_orders(&state.db_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Dashboard: orders query failed: {:?}", e);
            vec![]
        }
    };
    // Snapshot enrichment data under one pass, then release state lock before iterating rows.
    let enrich = snapshot_ticker_enrich(&state.bot).await;
    for row in &mut rows {
        if let Some(e) = enrich.get(&row.ticker) {
            row.game_name = Some(e.game_name.clone());
            row.home_team = Some(e.home_team.clone());
            row.away_team = Some(e.away_team.clone());
            row.is_home = Some(e.is_home);
            if let Some(fv) = e.fair_value {
                row.edge = Some(compute_order_edge(fv, row.price_cents, &row.side, &row.action));
            }
        }
        // DB values remain as fallback for tickers no longer in live game state
    }
    Json(rows)
}

async fn api_fills(State(state): State<DashboardState>) -> impl IntoResponse {
    let mut rows = match query_fills(&state.db_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Dashboard: fills query failed: {:?}", e);
            vec![]
        }
    };
    // Snapshot enrichment data under one pass, then release state lock before iterating rows.
    let enrich = snapshot_ticker_enrich(&state.bot).await;
    for row in &mut rows {
        if let Some(e) = enrich.get(&row.ticker) {
            row.game_name = Some(e.game_name.clone());
            row.home_team = Some(e.home_team.clone());
            row.away_team = Some(e.away_team.clone());
            row.is_home = Some(e.is_home);
            if row.edge_bps.is_none()
                && let Some(fv) = e.fair_value
            {
                row.edge_bps = Some(compute_order_edge(fv, row.price_cents, &row.side, &row.action) * 10000.0);
            }
        }
        // DB values remain as fallback for tickers no longer in live game state
    }
    Json(rows)
}

async fn api_edge(State(state): State<DashboardState>) -> impl IntoResponse {
    // Use read-only connections — no logger lock needed.
    let (total_edge_dollars, total_fills, avg_edge_bps) =
        query_edge_summary(&state.db_path).unwrap_or((0.0, 0, 0.0));
    let (today_edge_dollars, today_fills) =
        query_edge_summary_today(&state.db_path).unwrap_or((0.0, 0));
    Json(EdgeSummary {
        total_edge_dollars,
        total_fills,
        avg_edge_bps,
        today_edge_dollars,
        today_fills,
    })
}

async fn api_break_evals(State(state): State<DashboardState>) -> impl IntoResponse {
    let log = state.break_log.lock().unwrap();
    let evals: Vec<_> = log.iter().cloned().collect();
    Json(evals)
}

#[derive(Serialize)]
struct DailyChartPoint {
    ts: String,
    fill_edge: Option<f64>,
    fill_pnl: Option<f64>,
}

#[derive(Serialize)]
struct DailyChart {
    points: Vec<DailyChartPoint>,
}

async fn api_daily_chart(State(state): State<DashboardState>) -> impl IntoResponse {
    let points = query_daily_chart(&state.db_path).unwrap_or_default();
    Json(DailyChart { points })
}

// --- SQLite queries (read-only connection) ---

fn open_read_only(db_path: &str) -> anyhow::Result<Connection> {
    Ok(Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

/// Compute perceived edge for a given order side/action vs fair value.
/// Returns edge as a fraction (e.g. 0.05 = 5%).
fn compute_order_edge(fair_value: f64, price_cents: i64, side: &str, action: &str) -> f64 {
    let order_prob = price_cents as f64 / 100.0;
    let is_yes = side.eq_ignore_ascii_case("yes");
    let fair_for_side = if is_yes { fair_value } else { 1.0 - fair_value };
    let raw_edge = fair_for_side - order_prob;
    if action.eq_ignore_ascii_case("buy") { raw_edge } else { -raw_edge }
}

fn query_orders(db_path: &str) -> anyhow::Result<Vec<OrderRow>> {
    let conn = open_read_only(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT order_id, ticker, COALESCE(strategy,''), action, side, price_cents, count, status, created_at, game_name, home_team, away_team, is_home
         FROM orders ORDER BY created_at DESC LIMIT 50"
    )?;
    let rows = stmt.query_map([], |row| {
        let is_home_int: Option<i32> = row.get(12)?;
        Ok(OrderRow {
            order_id: row.get(0)?,
            ticker: row.get(1)?,
            game_name: row.get(9)?,
            is_home: is_home_int.map(|v| v != 0),
            home_team: row.get(10)?,
            away_team: row.get(11)?,
            strategy: row.get(2)?,
            action: row.get(3)?,
            side: row.get(4)?,
            price_cents: row.get(5)?,
            count: row.get(6)?,
            status: row.get(7)?,
            created_at: row.get(8)?,
            edge: None,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

fn query_fills(db_path: &str) -> anyhow::Result<Vec<FillRow>> {
    let conn = open_read_only(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT f.trade_id, f.order_id, f.ticker, f.side, f.action, f.price_cents, f.count, COALESCE(f.fee_cents,0), f.filled_at, COALESCE(o.strategy,''), o.edge_bps, o.game_name, o.home_team, o.away_team, o.is_home
         FROM fills f LEFT JOIN orders o ON f.order_id = o.order_id
         ORDER BY f.filled_at DESC LIMIT 200"
    )?;
    let rows = stmt.query_map([], |row| {
        let is_home_int: Option<i32> = row.get(14)?;
        Ok(FillRow {
            trade_id: row.get(0)?,
            order_id: row.get(1)?,
            ticker: row.get(2)?,
            game_name: row.get(11)?,
            is_home: is_home_int.map(|v| v != 0),
            home_team: row.get(12)?,
            away_team: row.get(13)?,
            strategy: row.get(9)?,
            side: row.get(3)?,
            action: row.get(4)?,
            price_cents: row.get(5)?,
            count: row.get(6)?,
            fee_dollars: row.get(7)?,
            filled_at: row.get(8)?,
            edge_bps: row.get(10)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

fn query_edge_summary(db_path: &str) -> anyhow::Result<(f64, i64, f64)> {
    let conn = open_read_only(db_path)?;
    let (total_edge_dollars, fill_count): (f64, i64) = conn.query_row(
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

fn query_edge_summary_today(db_path: &str) -> anyhow::Result<(f64, i64)> {
    let conn = open_read_only(db_path)?;
    let (total_edge_dollars, fill_count): (f64, i64) = conn.query_row(
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

fn query_daily_chart(db_path: &str) -> anyhow::Result<Vec<DailyChartPoint>> {
    let conn = open_read_only(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT f.filled_at,
                CASE WHEN o.edge_bps IS NOT NULL THEN o.edge_bps / 10000.0 * f.price_cents * f.count / 100.0 ELSE NULL END AS fill_edge,
                f.fill_pnl
         FROM fills f
         LEFT JOIN orders o ON f.order_id = o.order_id
         WHERE date(f.filled_at) = date('now')
         ORDER BY f.filled_at ASC"
    )?;
    let points = stmt.query_map([], |row| {
        Ok(DailyChartPoint {
            ts: row.get(0)?,
            fill_edge: row.get(1)?,
            fill_pnl: row.get(2)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(points)
}

// --- Server ---

pub async fn serve(bot_state: SharedState, order_books: SharedOrderBooks, _logger: SharedLogger, break_log: SharedBreakLog, db_path: &str, port: u16, dry_run: bool) -> anyhow::Result<()> {
    let state = DashboardState {
        bot: bot_state,
        order_books,
        break_log,
        db_path: db_path.to_string(),
        dry_run,
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/games", get(api_games))
        .route("/api/risk", get(api_risk))
        .route("/api/orders", get(api_orders))
        .route("/api/fills", get(api_fills))
        .route("/api/edge", get(api_edge))
        .route("/api/break_evals", get(api_break_evals))
        .route("/api/daily_chart", get(api_daily_chart))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Dashboard server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
