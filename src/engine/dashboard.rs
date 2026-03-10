use axum::{
    Json, Router,
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
};
use rusqlite::Connection;
use serde::Serialize;

use crate::engine::bot::{SharedState, SharedLogger};

/// Shared state for the dashboard server.
#[derive(Clone)]
struct DashboardState {
    bot: SharedState,
    logger: SharedLogger,
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
    fee_cents: f64,
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

async fn api_games(State(state): State<DashboardState>) -> impl IntoResponse {
    let s = state.bot.lock().await;
    let mut games: Vec<GameView> = s.game_state.games.values().filter(|g| {
        // Skip games with TBD teams (e.g. tournament bracket placeholders)
        !g.home_team.eq_ignore_ascii_case("TBD") && !g.away_team.eq_ignore_ascii_case("TBD")
    }).map(|g| {
        let markets: Vec<MarketView> = g.kalshi_markets.iter().map(|m| {
            let fair_value = g.fair_value_for_market(m);
            let prices = s.book_prices(&m.ticker);
            // Compute tradeable edge: pick the better side (YES or NO)
            let (edge, edge_side) = match (fair_value, prices.mid) {
                (Some(fv), Some(mid)) => {
                    let mid_prob = mid / 100.0;
                    let yes_edge = fv - mid_prob;       // positive = YES is cheap
                    if yes_edge >= 0.0 {
                        (Some(yes_edge), Some("YES".to_string()))
                    } else {
                        (Some(-yes_edge), Some("NO".to_string()))
                    }
                }
                _ => (None, None),
            };
            MarketView {
                ticker: m.ticker.clone(),
                is_home: m.is_home,
                yes_bid: prices.bid,
                yes_ask: prices.ask,
                yes_mid: prices.mid,
                fair_value,
                edge,
                edge_side,
                volume: m.volume,
                has_resting_order: s.order_manager.has_resting_order(&m.ticker),
                exposure: s.order_manager.committed_contracts(&m.ticker) as f64,
                position: s.risk.net_position(&m.ticker),
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
    })
}

async fn api_orders(State(state): State<DashboardState>) -> impl IntoResponse {
    let mut rows = match query_orders(&state.db_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Dashboard: orders query failed: {:?}", e);
            vec![]
        }
    };
    // Enrich with live game state (overrides DB values with fresher data, computes live edge)
    let s = state.bot.lock().await;
    for row in &mut rows {
        if let Some(game) = s.game_state.get_by_kalshi_ticker(&row.ticker) {
            // Live state overrides persisted game info (fresher team names/matchups)
            row.game_name = Some(format!("{} vs {}", game.away_team, game.home_team));
            row.home_team = Some(game.home_team.clone());
            row.away_team = Some(game.away_team.clone());
            // Compute perceived edge: fair_value vs order price
            if let Some(market) = game.kalshi_markets.iter().find(|m| m.ticker == row.ticker) {
                row.is_home = Some(market.is_home);
                if let Some(fv) = game.fair_value_for_market(market)
                {
                    let order_prob = row.price_cents as f64 / 100.0;
                    let is_yes = row.side.eq_ignore_ascii_case("yes");
                    let fair_for_side = if is_yes { fv } else { 1.0 - fv };
                    let raw_edge = fair_for_side - order_prob;
                    row.edge = Some(if row.action.eq_ignore_ascii_case("buy") { raw_edge } else { -raw_edge });
                }
            }
        }
        // DB values (game_name, home_team, away_team, is_home) remain as fallback from query
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
    // Enrich with live game state (overrides DB values with fresher data, computes edge fallback)
    let s = state.bot.lock().await;
    for row in &mut rows {
        if let Some(game) = s.game_state.get_by_kalshi_ticker(&row.ticker) {
            row.game_name = Some(format!("{} vs {}", game.away_team, game.home_team));
            row.home_team = Some(game.home_team.clone());
            row.away_team = Some(game.away_team.clone());
            if let Some(market) = game.kalshi_markets.iter().find(|m| m.ticker == row.ticker) {
                row.is_home = Some(market.is_home);
                // Compute edge from live state if not stored in DB (e.g. synced/historical orders)
                if row.edge_bps.is_none()
                    && let Some(fv) = game.fair_value_for_market(market)
                {
                    let order_prob = row.price_cents as f64 / 100.0;
                    let is_yes = row.side.eq_ignore_ascii_case("yes");
                    let fair_for_side = if is_yes { fv } else { 1.0 - fv };
                    let is_buy = row.action.eq_ignore_ascii_case("buy");
                    let raw_edge = fair_for_side - order_prob;
                    let signed_edge = if is_buy { raw_edge } else { -raw_edge };
                    row.edge_bps = Some(signed_edge * 10000.0);
                }
            }
        }
        // DB values (game_name, home_team, away_team, is_home) remain as fallback from query
    }
    Json(rows)
}

async fn api_edge(State(state): State<DashboardState>) -> impl IntoResponse {
    let log = state.logger.lock().unwrap();
    let (total_edge_dollars, total_fills, avg_edge_bps) =
        log.edge_summary().unwrap_or((0.0, 0, 0.0));
    let (today_edge_dollars, today_fills) =
        log.edge_summary_today().unwrap_or((0.0, 0));
    Json(EdgeSummary {
        total_edge_dollars,
        total_fills,
        avg_edge_bps,
        today_edge_dollars,
        today_fills,
    })
}

// --- SQLite queries (read-only connection) ---

fn query_orders(db_path: &str) -> anyhow::Result<Vec<OrderRow>> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
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
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
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
            fee_cents: row.get(7)?,
            filled_at: row.get(8)?,
            edge_bps: row.get(10)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

// --- Server ---

pub async fn serve(bot_state: SharedState, logger: SharedLogger, db_path: &str, port: u16, dry_run: bool) -> anyhow::Result<()> {
    let state = DashboardState {
        bot: bot_state,
        logger,
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
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Dashboard server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
