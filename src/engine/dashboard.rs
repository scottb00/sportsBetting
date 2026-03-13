use axum::{
    Json, Router,
    extract::{Query, State},
    response::{Html, IntoResponse},
    routing::get,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::engine::bot::{SharedState, SharedOrderBooks, SharedLogger, SharedBreakLog};
use crate::engine::market_prep::book_prices;
use crate::strategies::common::compute_edge_and_alo;
use crate::sportsbooks::types::SportsbookSpread;

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
    display_clock: Option<String>,
    period: Option<i32>,
    espn_home_win_prob: Option<f64>,
    /// Composite sportsbook spread: best bid on home (highest across books).
    spread_bid_home: Option<f64>,
    /// Composite sportsbook spread: best offer on home (lowest across books).
    spread_offer_home: Option<f64>,
    /// Number of bookmakers contributing to the spread.
    spread_books_count: usize,
    /// How many seconds since ESPN last updated this game's data.
    espn_age_secs: u64,
    /// True if ESPN data is considered stale (>90s for in-game, never for pregame).
    espn_stale: bool,
    /// Game start time as unix seconds (from ESPN).
    start_time_ts: Option<i64>,
    /// Net game risk: positive = long home team winning, negative = long away team winning.
    /// Aggregates across all markets (YES-DUKE and NO-UNC count as the same exposure).
    net_game_contracts: i64,
    /// Total resting contracts across all markets in this game.
    total_resting_contracts: i64,
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
    /// Live conviction score from sportsbook consensus (if spread data available).
    conviction_score: Option<f64>,
    /// Per-book conviction breakdown (JSON string).
    conviction_details: Option<String>,
}

#[derive(Serialize)]
struct RiskView {
    daily_pnl: f64,
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
    /// Edge at placement time in basis points (from DB). Preferred over live-computed edge.
    edge_bps: Option<f64>,
    /// Fair value in cents (0–100) at placement time.
    fair_value_cents: Option<i64>,
    /// Whether this is a closing order (unwinding existing position).
    is_close: Option<bool>,
    /// Perceived edge fraction for display when edge_bps is unavailable (legacy fallback only).
    edge: Option<f64>,
    /// Conviction score at order placement time (from DB).
    conviction_score: Option<f64>,
    /// Per-book conviction breakdown at order placement time (JSON string from DB).
    conviction_details: Option<String>,
    /// Live sportsbook spread: best bid on home.
    spread_bid_home: Option<f64>,
    /// Live sportsbook spread: best offer on home.
    spread_offer_home: Option<f64>,
    /// Number of bookmakers contributing to the spread.
    spread_books_count: usize,
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
    fair_value_cents: Option<i64>,
    /// Whether this fill's parent order was a closing order.
    is_close: Option<bool>,
    /// Settlement PnL from DB (used to back-derive mark for settled fills).
    fill_pnl: Option<f64>,
    /// Mark price in cents (0–100). Settled fills: 0 or 100. Open fills: live fair value.
    mark_cents: Option<f64>,
    /// Conviction score at order placement time (from joined orders table).
    conviction_score: Option<f64>,
    /// Per-book conviction breakdown at order placement time (JSON string from joined orders table).
    conviction_details: Option<String>,
    /// Live sportsbook spread: best bid on home.
    spread_bid_home: Option<f64>,
    /// Live sportsbook spread: best offer on home.
    spread_offer_home: Option<f64>,
    /// Number of bookmakers contributing to the spread.
    spread_books_count: usize,
}

#[derive(Serialize)]
struct EdgeSummary {
    total_edge_dollars: f64,
    total_fills: i64,
    avg_edge_bps: f64,
    today_edge_dollars: f64,
    today_fills: i64,
}

#[derive(Serialize)]
struct HealthView {
    status: &'static str,
    last_espn_update_secs_ago: u64,
    last_kalshi_sync_secs_ago: u64,
    position_corrections_today: u32,
    dry_run: bool,
}

// --- Handlers ---

async fn api_health(State(state): State<DashboardState>) -> impl IntoResponse {
    let s = state.bot.lock().await;
    let espn_secs = s.last_espn_update.elapsed().as_secs();
    let kalshi_secs = s.last_kalshi_sync.elapsed().as_secs();
    let status = if espn_secs > 120 { "stale_espn" }
                 else if kalshi_secs > 300 { "stale_kalshi" }
                 else { "ok" };
    Json(HealthView {
        status,
        last_espn_update_secs_ago: espn_secs,
        last_kalshi_sync_secs_ago: kalshi_secs,
        position_corrections_today: s.position_corrections,
        dry_run: state.dry_run,
    })
}

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
    resting: i64,
    position: i64,
    conviction_score: Option<f64>,
    conviction_details: Option<String>,
}

struct GameSnapshot {
    espn_event_id: String,
    home_team: String,
    away_team: String,
    home_score: Option<i32>,
    away_score: Option<i32>,
    phase: String,
    status_detail: String,
    display_clock: Option<String>,
    period: Option<i32>,
    espn_home_win_prob: Option<f64>,
    spread_bid_home: Option<f64>,
    spread_offer_home: Option<f64>,
    spread_books_count: usize,
    espn_age_secs: u64,
    start_time_ts: Option<i64>,
    net_game_contracts: i64,
    markets: Vec<MarketSnapshot>,
}

/// Compute live conviction score for a market given its spread, fair value, and book prices.
/// Returns (score, details_json) or (None, None) if insufficient data.
fn compute_market_conviction(
    spread: Option<&SportsbookSpread>,
    fair_value: Option<f64>,
    bid: Option<f64>,
    ask: Option<f64>,
    is_home: bool,
) -> (Option<f64>, Option<String>) {
    let spread = match spread {
        Some(s) if s.fresh_count > 0 => s,
        _ => return (None, None),
    };
    let fv = match fair_value {
        Some(v) => v,
        None => return (None, None),
    };
    let (b, a) = match (bid, ask) {
        (Some(b), Some(a)) => (b as i64, a as i64),
        _ => return (None, None),
    };
    let edge_result = match compute_edge_and_alo(b, a, fv, 1) {
        Some(r) => r,
        None => return (None, None),
    };
    let conv = spread.conviction_score(
        edge_result.alo_price,
        edge_result.buying_yes,
        is_home,
        false, // dashboard doesn't distinguish break type
        None,  // no break filter for dashboard display
    );
    let details = format!("{}", conv);
    (Some(conv.score), Some(details))
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
                    // Compute live conviction if we have spread data and can determine ALO/direction
                    let (conv_score, conv_details) = compute_market_conviction(
                        g.sportsbook_spread.as_ref(),
                        g.fair_value_for_market(m),
                        prices.bid,
                        prices.ask,
                        m.is_home,
                    );
                    MarketSnapshot {
                        ticker: m.ticker.clone(),
                        is_home: m.is_home,
                        fair_value: g.fair_value_for_market(m),
                        bid: prices.bid,
                        ask: prices.ask,
                        mid: prices.mid,
                        volume: m.volume,
                        has_resting: s.order_manager.has_resting_order(&m.ticker),
                        resting: s.order_manager.signed_resting_for_ticker(&m.ticker),
                        position: s.risk.net_position(&m.ticker),
                        conviction_score: conv_score,
                        conviction_details: conv_details,
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
                    display_clock: g.display_clock.clone(),
                    period: g.period,
                    espn_home_win_prob: g.espn_home_win_prob,
                    spread_bid_home: g.sportsbook_spread.as_ref().and_then(|s| s.best_bid_home),
                    spread_offer_home: g.sportsbook_spread.as_ref().and_then(|s| s.best_offer_home),
                    spread_books_count: g.sportsbook_spread.as_ref().map_or(0, |s| s.fresh_count),
                    espn_age_secs: g.last_updated.elapsed().as_secs(),
                    start_time_ts: g.start_time_ts,
                    net_game_contracts: s.risk.net_game_home_risk(&g.kalshi_markets),
                    markets,
                }
            })
            .collect()
        // books and s (both locks) drop here
    };

    // Phase 2: Build view structs and sort — no locks held.
    let mut games: Vec<GameView> = snapshots.into_iter().map(|g| {
        let mut total_resting: i64 = 0;

        let markets: Vec<MarketView> = g.markets.into_iter().map(|m| {
            total_resting += if m.is_home { m.resting } else { -m.resting };

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
                exposure: (m.position.unsigned_abs() as i64 + m.resting) as f64,
                position: m.position,
                conviction_score: m.conviction_score,
                conviction_details: m.conviction_details,
            }
        }).collect();
        let espn_stale = matches!(g.phase.as_str(), "Break" | "Halftime" | "Live") && g.espn_age_secs > 90;
        GameView {
            espn_event_id: g.espn_event_id,
            home_team: g.home_team,
            away_team: g.away_team,
            home_score: g.home_score,
            away_score: g.away_score,
            phase: g.phase,
            status_detail: g.status_detail,
            display_clock: g.display_clock,
            period: g.period,
            espn_home_win_prob: g.espn_home_win_prob,
            spread_bid_home: g.spread_bid_home,
            spread_offer_home: g.spread_offer_home,
            spread_books_count: g.spread_books_count,
            espn_age_secs: g.espn_age_secs,
            espn_stale,
            start_time_ts: g.start_time_ts,
            net_game_contracts: g.net_game_contracts,
            total_resting_contracts: total_resting,
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
    // Read daily PnL from DB (persisted, includes fees, survives restarts)
    let daily_pnl = query_daily_realized_pnl(&state.db_path);
    let s = state.bot.lock().await;
    Json(RiskView {
        daily_pnl,
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
    spread_bid_home: Option<f64>,
    spread_offer_home: Option<f64>,
    spread_books_count: usize,
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
                spread_bid_home: game.sportsbook_spread.as_ref().and_then(|s| s.best_bid_home),
                spread_offer_home: game.sportsbook_spread.as_ref().and_then(|s| s.best_offer_home),
                spread_books_count: game.sportsbook_spread.as_ref().map_or(0, |s| s.fresh_count),
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
            row.spread_bid_home = e.spread_bid_home;
            row.spread_offer_home = e.spread_offer_home;
            row.spread_books_count = e.spread_books_count;
        }
        // Use stored edge_bps from DB as the canonical edge; only fall back to live
        // fair value for old orders that predate edge_bps logging.
        if row.edge_bps.is_none() {
            if let Some(e) = enrich.get(&row.ticker) {
                if let Some(fv) = e.fair_value {
                    row.edge = Some(compute_order_edge(fv, row.price_cents, &row.side, &row.action));
                }
            }
        }
        // Enrich fair_value_cents from live state when missing in DB
        if row.fair_value_cents.is_none() {
            if let Some(e) = enrich.get(&row.ticker) {
                if let Some(fv) = e.fair_value {
                    row.fair_value_cents = Some((fv * 100.0).round() as i64);
                }
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
            row.spread_bid_home = e.spread_bid_home;
            row.spread_offer_home = e.spread_offer_home;
            row.spread_books_count = e.spread_books_count;
            if row.edge_bps.is_none()
                && let Some(fv) = e.fair_value
            {
                row.edge_bps = Some(compute_order_edge(fv, row.price_cents, &row.side, &row.action) * 10000.0);
            }
        }
        // Enrich fair_value_cents from live state when missing in DB
        if row.fair_value_cents.is_none() {
            if let Some(e) = enrich.get(&row.ticker) {
                if let Some(fv) = e.fair_value {
                    row.fair_value_cents = Some((fv * 100.0).round() as i64);
                }
            }
        }

        // Compute mark_cents
        if let Some(pnl) = row.fill_pnl {
            // Settled fill: back-derive mark (0 or 100) from fill_pnl.
            // settle_fills formula: Buy → (settlement - price) * count/100 - fee
            //                       Sell → (price - settlement) * count/100 - fee
            // where settlement = 100 if the fill's side won, 0 otherwise.
            let fee = row.fee_dollars;
            let gross = pnl + fee;
            let is_buy = row.action.eq_ignore_ascii_case("buy");
            let side_won = if is_buy { gross >= 0.0 } else { gross <= 0.0 };
            let is_yes = row.side.eq_ignore_ascii_case("yes");
            // mark_cents is the YES price: 100 if YES won, 0 if NO won
            let yes_won = if is_yes { side_won } else { !side_won };
            row.mark_cents = Some(if yes_won { 100.0 } else { 0.0 });
        } else if let Some(e) = enrich.get(&row.ticker) {
            // Open fill: use live fair value as mark-to-market (already YES-aligned)
            if let Some(fv) = e.fair_value {
                row.mark_cents = Some((fv * 100.0).round());
            }
        }
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
    /// Dollar edge for this fill: edge_bps/10000 * count (each contract pays $1).
    fill_edge: Option<f64>,
    fill_pnl: Option<f64>,
    count: i64,
    price_cents: i64,
    /// Raw edge_bps from the order, used by frontend for Bernoulli variance calc.
    edge_bps: Option<f64>,
}

#[derive(Serialize)]
struct DailyChart {
    points: Vec<DailyChartPoint>,
}

#[derive(serde::Deserialize)]
struct ChartParams {
    /// Lookback window in hours. 0 or absent = today only, -1 = all time.
    lookback_hours: Option<i64>,
}

async fn api_daily_chart(State(state): State<DashboardState>, Query(params): Query<ChartParams>) -> impl IntoResponse {
    let points = query_daily_chart(&state.db_path, params.lookback_hours).unwrap_or_default();
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
        "SELECT order_id, ticker, COALESCE(strategy,''), action, side, price_cents, count, status, created_at, game_name, home_team, away_team, is_home, edge_bps, fair_value_cents, is_close, conviction_score, conviction_details
         FROM orders ORDER BY datetime(created_at) DESC LIMIT 50"
    )?;
    let rows = stmt.query_map([], |row| {
        let is_home_int: Option<i32> = row.get(12)?;
        let is_close_int: Option<i32> = row.get(15)?;
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
            edge_bps: row.get(13)?,
            fair_value_cents: row.get(14)?,
            is_close: is_close_int.map(|v| v != 0),
            edge: None,
            conviction_score: row.get(16)?,
            conviction_details: row.get(17)?,
            spread_bid_home: None,
            spread_offer_home: None,
            spread_books_count: 0,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

fn query_fills(db_path: &str) -> anyhow::Result<Vec<FillRow>> {
    let conn = open_read_only(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT f.trade_id, f.order_id, f.ticker, f.side, f.action, f.price_cents, f.count, COALESCE(f.fee_cents,0), f.filled_at, COALESCE(o.strategy,''), o.edge_bps, o.game_name, o.home_team, o.away_team, o.is_home, o.fair_value_cents, o.is_close, f.fill_pnl, o.conviction_score, o.conviction_details
         FROM fills f LEFT JOIN orders o ON f.order_id = o.order_id
         WHERE COALESCE(f.excluded, 0) = 0
         ORDER BY f.filled_at DESC LIMIT 200"
    )?;
    let rows = stmt.query_map([], |row| {
        let is_home_int: Option<i32> = row.get(14)?;
        let is_close_int: Option<i32> = row.get(16)?;
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
            fair_value_cents: row.get(15)?,
            is_close: is_close_int.map(|v| v != 0),
            fill_pnl: row.get(17)?,
            mark_cents: None, // populated in api_fills after enrichment
            conviction_score: row.get(18)?,
            conviction_details: row.get(19)?,
            spread_bid_home: None,
            spread_offer_home: None,
            spread_books_count: 0,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(rows)
}

fn query_edge_summary(db_path: &str) -> anyhow::Result<(f64, i64, f64)> {
    let conn = open_read_only(db_path)?;
    let (total_edge_dollars, fill_count): (f64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(o.edge_bps / 10000.0 * f.count), 0.0),
                COUNT(*)
         FROM fills f
         JOIN orders o ON f.order_id = o.order_id
         WHERE o.edge_bps IS NOT NULL AND COALESCE(f.excluded, 0) = 0",
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
        "SELECT COALESCE(SUM(o.edge_bps / 10000.0 * f.count), 0.0),
                COUNT(*)
         FROM fills f
         JOIN orders o ON f.order_id = o.order_id
         WHERE o.edge_bps IS NOT NULL AND date(f.filled_at, '-8 hours') = date('now', '-8 hours') AND COALESCE(f.excluded, 0) = 0",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((total_edge_dollars, fill_count))
}

fn query_daily_realized_pnl(db_path: &str) -> f64 {
    let conn = match open_read_only(db_path) {
        Ok(c) => c,
        Err(_) => return 0.0,
    };
    conn.query_row(
        "SELECT COALESCE(SUM(fill_pnl), 0.0) FROM fills WHERE date(filled_at, '-8 hours') = date('now', '-8 hours') AND fill_pnl IS NOT NULL AND COALESCE(excluded, 0) = 0",
        [],
        |row| row.get(0),
    ).unwrap_or(0.0)
}

fn query_daily_chart(db_path: &str, lookback_hours: Option<i64>) -> anyhow::Result<Vec<DailyChartPoint>> {
    let conn = open_read_only(db_path)?;
    let base_select = "SELECT f.filled_at,
                CASE WHEN o.edge_bps IS NOT NULL THEN o.edge_bps / 10000.0 * f.count ELSE NULL END AS fill_edge,
                f.fill_pnl,
                f.count,
                f.price_cents,
                o.edge_bps
         FROM fills f
         LEFT JOIN orders o ON f.order_id = o.order_id";
    let excl = "COALESCE(f.excluded, 0) = 0";
    let sql = match lookback_hours {
        None | Some(0) => format!("{base_select} WHERE date(f.filled_at, '-8 hours') = date('now', '-8 hours') AND {excl} ORDER BY f.filled_at ASC"),
        Some(-1) => format!("{base_select} WHERE {excl} ORDER BY f.filled_at ASC"),
        Some(h) => format!("{base_select} WHERE f.filled_at >= datetime('now', '-{h} hours') AND {excl} ORDER BY f.filled_at ASC"),
    };
    let mut stmt = conn.prepare(&sql)?;
    let points = stmt.query_map([], |row| {
        Ok(DailyChartPoint {
            ts: row.get(0)?,
            fill_edge: row.get(1)?,
            fill_pnl: row.get(2)?,
            count: row.get(3)?,
            price_cents: row.get(4)?,
            edge_bps: row.get(5)?,
        })
    })?.filter_map(|r| r.ok()).collect();
    Ok(points)
}

// --- Variance Analysis ---

#[derive(Serialize)]
struct VarianceDayRow {
    date: String,
    fills: i64,
    contracts: i64,
    expected_pnl: f64,
    realized_pnl: f64,
    total_fees: f64,
    sigma_edge_real: f64,
    sigma_zero_edge: f64,
    z_edge_real: Option<f64>,
    z_zero_edge: Option<f64>,
    p_edge_real: Option<f64>,
    p_zero_edge: Option<f64>,
}

#[derive(Serialize)]
struct VarianceAnalysis {
    days: Vec<VarianceDayRow>,
    totals: VarianceDayRow,
}

/// Standard normal CDF approximation (Abramowitz & Stegun).
fn norm_cdf(x: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    0.5 * (1.0 + sign * y)
}

/// Per-fill data for variance analysis. Includes game context for correlation grouping.
#[derive(Clone)]
struct VarFill {
    count: i64,
    price_cents: i64,
    edge_bps: Option<f64>,
    fill_pnl: Option<f64>,
    fee: f64,
    game_name: Option<String>,
    side: String,
    action: String,
    is_home: Option<bool>,
}

impl VarFill {
    /// Signed home-aligned contracts: positive = long home wins, negative = long away wins.
    /// Formula: count * buy_sign * yes_sign * home_sign, where:
    ///   buy_sign = +1 for Buy, -1 for Sell (closes flip the sign)
    ///   yes_sign = +1 for YES side, -1 for NO side
    ///   home_sign = +1 if ticker is home-aligned, -1 if away-aligned
    fn signed_home_contracts(&self) -> f64 {
        let buy_sign = if self.action.eq_ignore_ascii_case("buy") { 1.0 } else { -1.0 };
        let yes_sign = if self.side.eq_ignore_ascii_case("yes") { 1.0 } else { -1.0 };
        let home_sign = if self.is_home.unwrap_or(true) { 1.0 } else { -1.0 };
        self.count as f64 * buy_sign * yes_sign * home_sign
    }

    /// Home-win probability implied by the fill price, converting from the fill's side/ticker.
    fn home_prob(&self) -> f64 {
        let is_yes = self.side.eq_ignore_ascii_case("yes");
        let is_home = self.is_home.unwrap_or(true);
        let p = self.price_cents as f64 / 100.0;
        // YES on home or NO on away → price directly implies home-win prob
        if is_yes == is_home { p } else { 1.0 - p }
    }
}

fn compute_day_row(date: &str, fills: &[VarFill]) -> VarianceDayRow {
    let total_fills = fills.len() as i64;
    let total_contracts: i64 = fills.iter().map(|f| f.count).sum();
    let total_fees: f64 = fills.iter().map(|f| f.fee).sum();
    let expected_pnl: f64 = fills.iter()
        .filter_map(|f| f.edge_bps.map(|e| e / 10000.0 * f.count as f64))
        .sum();
    let realized_pnl: f64 = fills.iter()
        .filter_map(|f| f.fill_pnl)
        .sum();

    // Group fills by game for correlated variance calculation.
    // Fills on the same game are perfectly correlated (same binary outcome),
    // so we net them and compute one variance term per game.
    let mut game_groups: std::collections::HashMap<String, Vec<&VarFill>> = std::collections::HashMap::new();
    let mut ungrouped: Vec<&VarFill> = Vec::new();
    for fill in fills {
        if let Some(ref gn) = fill.game_name {
            game_groups.entry(gn.clone()).or_default().push(fill);
        } else {
            ungrouped.push(fill);
        }
    }

    let mut var_edge = 0.0_f64;
    let mut var_zero = 0.0_f64;

    for (_game, group) in &game_groups {
        // Net home-aligned contracts across all fills in this game
        let net_home: f64 = group.iter().map(|f| f.signed_home_contracts()).sum();

        // Contract-weighted average home-win probability
        let total_abs: f64 = group.iter().map(|f| f.count as f64).sum();
        if total_abs == 0.0 { continue; }

        let avg_home_p0: f64 = group.iter()
            .map(|f| f.home_prob() * f.count as f64)
            .sum::<f64>() / total_abs;
        let p0 = avg_home_p0.clamp(0.01, 0.99);

        // Edge-real: adjust by contract-weighted average edge (in home direction)
        let avg_home_edge: f64 = group.iter()
            .map(|f| {
                let edge_frac = f.edge_bps.unwrap_or(0.0) / 10000.0;
                // Edge is always positive from trader perspective; convert to home direction
                let dir = if f.signed_home_contracts() >= 0.0 { 1.0 } else { -1.0 };
                edge_frac * dir * f.count as f64
            })
            .sum::<f64>() / total_abs;
        let q = (p0 + avg_home_edge).clamp(0.01, 0.99);

        // Var(game PnL) = net² * q*(1-q), because all contracts share the same outcome
        var_edge += net_home * net_home * q * (1.0 - q);
        var_zero += net_home * net_home * p0 * (1.0 - p0);
    }

    // Ungrouped fills (no game_name): treat as independent (fallback)
    for fill in &ungrouped {
        let p0 = (fill.price_cents as f64 / 100.0).clamp(0.01, 0.99);
        let q = if let Some(e) = fill.edge_bps {
            (p0 + e / 10000.0).clamp(0.01, 0.99)
        } else {
            p0
        };
        let n = fill.count as f64;
        var_edge += n * n * q * (1.0 - q);
        var_zero += n * n * p0 * (1.0 - p0);
    }

    let sigma_edge = var_edge.sqrt();
    let sigma_zero = var_zero.sqrt();

    let z_edge = if sigma_edge > 0.0 { Some((realized_pnl - expected_pnl) / sigma_edge) } else { None };
    let z_zero = if sigma_zero > 0.0 { Some((realized_pnl + total_fees) / sigma_zero) } else { None };

    VarianceDayRow {
        date: date.to_string(),
        fills: total_fills,
        contracts: total_contracts,
        expected_pnl,
        realized_pnl,
        total_fees,
        sigma_edge_real: sigma_edge,
        sigma_zero_edge: sigma_zero,
        z_edge_real: z_edge,
        z_zero_edge: z_zero,
        p_edge_real: z_edge.map(norm_cdf),
        p_zero_edge: z_zero.map(norm_cdf),
    }
}

fn query_variance_analysis(db_path: &str) -> anyhow::Result<VarianceAnalysis> {
    let conn = open_read_only(db_path)?;
    let mut stmt = conn.prepare(
        "SELECT date(f.filled_at, '-8 hours') as day, f.count, f.price_cents, o.edge_bps, f.fill_pnl, COALESCE(f.fee_cents, 0),
                o.game_name, LOWER(f.side), LOWER(f.action), o.is_home
         FROM fills f
         LEFT JOIN orders o ON f.order_id = o.order_id
         WHERE f.fill_pnl IS NOT NULL AND COALESCE(f.excluded, 0) = 0
         ORDER BY f.filled_at ASC"
    )?;

    let mut by_day: std::collections::BTreeMap<String, Vec<VarFill>> = std::collections::BTreeMap::new();
    let mut all_fills: Vec<VarFill> = Vec::new();

    let rows = stmt.query_map([], |row| {
        let day: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        let price_cents: i64 = row.get(2)?;
        let edge_bps: Option<f64> = row.get(3)?;
        let fill_pnl: Option<f64> = row.get(4)?;
        let fee: f64 = row.get(5)?;
        let game_name: Option<String> = row.get(6)?;
        let side: String = row.get::<_, Option<String>>(7)?.unwrap_or_default();
        let action: String = row.get::<_, Option<String>>(8)?.unwrap_or_else(|| "buy".to_string());
        let is_home: Option<bool> = row.get(9)?;
        Ok((day, VarFill { count, price_cents, edge_bps, fill_pnl, fee, game_name, side, action, is_home }))
    })?;

    for row in rows {
        let (day, fill) = row?;
        all_fills.push(fill.clone());
        by_day.entry(day).or_default().push(fill);
    }

    let days: Vec<VarianceDayRow> = by_day.iter()
        .map(|(date, fills)| compute_day_row(date, fills))
        .collect();

    let totals = compute_day_row("All Time", &all_fills);

    Ok(VarianceAnalysis { days, totals })
}

async fn api_variance_analysis(State(state): State<DashboardState>) -> impl IntoResponse {
    match query_variance_analysis(&state.db_path) {
        Ok(analysis) => Json(analysis),
        Err(e) => {
            tracing::warn!("Dashboard: variance analysis query failed: {:?}", e);
            Json(VarianceAnalysis { days: vec![], totals: compute_day_row("All Time", &[]) })
        }
    }
}

// --- Exclude fills ---

#[derive(Deserialize)]
struct ExcludeQuery {
    ticker: String,
}

async fn api_exclude_fills(State(state): State<DashboardState>, Query(q): Query<ExcludeQuery>) -> impl IntoResponse {
    let conn = match open_read_only(&state.db_path) {
        Ok(_) => {
            // Need a writable connection for UPDATE
            match rusqlite::Connection::open(&state.db_path) {
                Ok(c) => c,
                Err(e) => return Json(serde_json::json!({"error": format!("{e}")})),
            }
        }
        Err(e) => return Json(serde_json::json!({"error": format!("{e}")})),
    };
    match conn.execute(
        "UPDATE fills SET excluded = 1 WHERE ticker = ?1",
        rusqlite::params![q.ticker],
    ) {
        Ok(n) => Json(serde_json::json!({"excluded": n, "ticker": q.ticker})),
        Err(e) => Json(serde_json::json!({"error": format!("{e}")})),
    }
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
        .route("/api/variance_analysis", get(api_variance_analysis))
        .route("/api/health", get(api_health))
        .route("/api/exclude_fills", axum::routing::post(api_exclude_fills))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Dashboard server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
