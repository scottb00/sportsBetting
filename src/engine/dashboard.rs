use axum::{
    Json, Router,
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
};
use rusqlite::Connection;
use serde::Serialize;

use crate::engine::bot::SharedState;

/// Shared state for the dashboard server.
#[derive(Clone)]
struct DashboardState {
    bot: SharedState,
    db_path: String,
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
    edge: Option<f64>,
    volume: Option<i64>,
    has_resting_order: bool,
    exposure: f64,
}

#[derive(Serialize)]
struct RiskView {
    daily_pnl: f64,
    current_total_exposure: f64,
    max_total_exposure: f64,
    max_position_per_game: f64,
    daily_loss_limit: f64,
    halted: bool,
    open_orders: usize,
    in_flight: usize,
}

#[derive(Serialize)]
struct OrderRow {
    order_id: String,
    ticker: String,
    strategy: String,
    action: String,
    side: String,
    price_cents: i64,
    count: i64,
    status: String,
    created_at: String,
}

#[derive(Serialize)]
struct FillRow {
    trade_id: String,
    order_id: String,
    ticker: String,
    side: String,
    action: String,
    price_cents: i64,
    count: i64,
    fee_cents: f64,
    filled_at: String,
}

// --- Handlers ---

async fn index() -> impl IntoResponse {
    let html = include_str!("../../static/dashboard.html");
    Html(html)
}

async fn api_games(State(state): State<DashboardState>) -> impl IntoResponse {
    let s = state.bot.lock().await;
    let mut games: Vec<GameView> = s.game_state.games.values().map(|g| {
        let markets: Vec<MarketView> = g.kalshi_markets.iter().map(|m| {
            let fair_value = g.fair_value_for_market(m);
            let edge = match (fair_value, m.yes_mid) {
                (Some(fv), Some(mid)) => Some(fv - mid / 100.0),
                _ => None,
            };
            MarketView {
                ticker: m.ticker.clone(),
                is_home: m.is_home,
                yes_bid: m.yes_bid,
                yes_ask: m.yes_ask,
                yes_mid: m.yes_mid,
                fair_value,
                edge,
                volume: m.volume,
                has_resting_order: s.order_manager.has_resting_order(&m.ticker),
                exposure: s.order_manager.committed_contracts(&m.ticker) as f64,
            }
        }).collect();

        GameView {
            espn_event_id: g.espn_event_id.clone(),
            home_team: g.home_team.clone(),
            away_team: g.away_team.clone(),
            home_score: g.home_score,
            away_score: g.away_score,
            phase: format!("{:?}", g.phase),
            status_detail: g.status_detail.clone(),
            espn_home_win_prob: g.espn_home_win_prob,
            markets,
        }
    }).collect();
    // Sort by phase (Live first, then Break, PreGame, etc.)
    games.sort_by_key(|g| match g.phase.as_str() {
        "Live" => 0,
        "Break" | "Halftime" => 1,
        "PreGame" => 2,
        _ => 3,
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
        open_orders: s.order_manager.open_order_count(),
        in_flight: s.order_manager.in_flight_count(),
    })
}

async fn api_orders(State(state): State<DashboardState>) -> impl IntoResponse {
    let rows = query_orders(&state.db_path).unwrap_or_default();
    Json(rows)
}

async fn api_fills(State(state): State<DashboardState>) -> impl IntoResponse {
    let rows = query_fills(&state.db_path).unwrap_or_default();
    Json(rows)
}

// --- SQLite queries (read-only connection) ---

fn query_orders(db_path: &str) -> anyhow::Result<Vec<OrderRow>> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut stmt = conn.prepare(
        "SELECT order_id, ticker, COALESCE(strategy,''), action, side, price_cents, count, status, created_at
         FROM orders ORDER BY created_at DESC LIMIT 50"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(OrderRow {
            order_id: row.get(0)?,
            ticker: row.get(1)?,
            strategy: row.get(2)?,
            action: row.get(3)?,
            side: row.get(4)?,
            price_cents: row.get(5)?,
            count: row.get(6)?,
            status: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

fn query_fills(db_path: &str) -> anyhow::Result<Vec<FillRow>> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut stmt = conn.prepare(
        "SELECT trade_id, order_id, ticker, side, action, price_cents, count, COALESCE(fee_cents,0), filled_at
         FROM fills ORDER BY filled_at DESC LIMIT 50"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(FillRow {
            trade_id: row.get(0)?,
            order_id: row.get(1)?,
            ticker: row.get(2)?,
            side: row.get(3)?,
            action: row.get(4)?,
            price_cents: row.get(5)?,
            count: row.get(6)?,
            fee_cents: row.get(7)?,
            filled_at: row.get(8)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

// --- Server ---

pub async fn serve(bot_state: SharedState, db_path: &str, port: u16) -> anyhow::Result<()> {
    let state = DashboardState {
        bot: bot_state,
        db_path: db_path.to_string(),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/games", get(api_games))
        .route("/api/risk", get(api_risk))
        .route("/api/orders", get(api_orders))
        .route("/api/fills", get(api_fills))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Dashboard server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
