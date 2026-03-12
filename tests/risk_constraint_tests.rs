//! Risk constraint tests: verify the bot never exceeds position limits, exposure caps,
//! per-game contract caps, or daily loss limits across realistic multi-game scenarios.
//!
//! These tests simulate sequences of strategy evaluations and fill recordings to ensure
//! the risk management system properly gates order flow.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use sports_betting::engine::bot::{BotState, StrategyRegistry};
use sports_betting::engine::executor::{build_eval_snapshot, evaluate_strategies};
use sports_betting::engine::game_state::{GameState, GameStateManager, KalshiMarketState};
use sports_betting::engine::order_manager::{OrderManager, OrderSignal};
use sports_betting::engine::risk::RiskManager;
use sports_betting::espn::types::GamePhase;
use sports_betting::kalshi::orderbook::LocalOrderBook;
use sports_betting::kalshi::types::{Order, OrderAction, OrderBookSnapshot, OrderSide, PriceLevel};
use sports_betting::strategies::break_ev::BreakEvQuoter;
use sports_betting::strategies::clv_hunter::ClvHunter;
use sports_betting::strategies::Strategy;

// ============================================================
// Helpers
// ============================================================

fn make_book(ticker: &str, yes_bid: i64, yes_ask: i64) -> LocalOrderBook {
    let mut book = LocalOrderBook::new(ticker.to_string());
    book.yes_levels.insert(yes_bid, 100);
    book.no_levels.insert(100 - yes_ask, 100);
    book
}

fn make_order(id: &str, ticker: &str, remaining: i64) -> Order {
    Order {
        order_id: id.to_string(),
        ticker: ticker.to_string(),
        action: OrderAction::Buy,
        side: OrderSide::Yes,
        order_type: "limit".to_string(),
        status: "resting".to_string(),
        yes_price: Some(50),
        no_price: None,
        remaining_count: remaining,
        created_time: "2026-03-10T12:00:00Z".to_string(),
    }
}

fn test_risk() -> RiskManager {
    RiskManager::new()
}

fn make_registry(max_contracts_per_game: i64) -> StrategyRegistry {
    StrategyRegistry {
        strategies: vec![
            Box::new(BreakEvQuoter::new(0.01, 20.0, 1)),
            Box::new(ClvHunter::new(0.01, 20.0, 1)),
        ],
        live_strategies: vec!["break_ev".into(), "clv_hunter".into()],
        min_volume: 1000,
        min_price_cents: 5.0,
        max_price_cents: 95.0,
        order_ttl: Duration::from_secs(120),
        max_contracts_per_game,
    }
}

/// Create a BotState with N games, each having home+away markets in a break phase.
fn make_bot_state_with_games(
    n_games: usize,
    risk: RiskManager,
) -> (BotState, HashMap<String, LocalOrderBook>) {
    let mut gsm = GameStateManager::new();
    let mut books = HashMap::new();

    for i in 0..n_games {
        let event_id = format!("evt_{}", i);
        let home_ticker = format!("T{}-HOME", i);
        let away_ticker = format!("T{}-AWAY", i);

        let gs = gsm.upsert(event_id.clone(), format!("Home{}", i), format!("Away{}", i));
        gs.espn_home_win_prob = Some(0.70); // clear edge vs 50c market
        gs.phase = GamePhase::Halftime;

        let mut home_mkt = KalshiMarketState::new(home_ticker.clone(), true);
        home_mkt.volume = Some(50000);
        gs.kalshi_markets.push(home_mkt);

        let mut away_mkt = KalshiMarketState::new(away_ticker.clone(), false);
        away_mkt.volume = Some(50000);
        gs.kalshi_markets.push(away_mkt);

        gsm.register_ticker(&home_ticker, &event_id);
        gsm.register_ticker(&away_ticker, &event_id);

        books.insert(home_ticker, make_book(&format!("T{}-HOME", i), 48, 52));
        books.insert(away_ticker, make_book(&format!("T{}-AWAY", i), 48, 52));
    }

    let state = BotState {
        started_at: chrono::Utc::now(),
        game_state: gsm,
        risk,
        order_manager: OrderManager::new(),
        last_espn_update: std::time::Instant::now(),
        last_kalshi_sync: std::time::Instant::now(),
        position_corrections: 0,
    };
    (state, books)
}

// ============================================================
// 3. Per-Game Contract Cap via evaluate_strategies
// ============================================================

#[test]
fn contract_cap_blocks_signals_when_at_limit() {
    let risk = test_risk();
    let (mut state, books) = make_bot_state_with_games(1, risk);
    let registry = make_registry(10);

    // Simulate 10 contracts already filled on T0-HOME
    state.risk.seed_positions("T0-HOME", "yes", 50, 10);

    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    assert!(
        signals.is_empty(),
        "Should produce no signals when game is at contract cap, got {} signals",
        signals.len()
    );
}

#[test]
fn contract_cap_limits_signal_max_contracts() {
    let risk = test_risk();
    let (mut state, books) = make_bot_state_with_games(1, risk);
    let registry = make_registry(20);

    // Simulate 15 contracts already filled
    state.risk.seed_positions("T0-HOME", "yes", 50, 15);

    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    for signal in &signals {
        assert!(
            signal.max_contracts.unwrap_or(i64::MAX) <= 5,
            "Signal max_contracts={:?} should be <= 5 (20 cap - 15 filled)",
            signal.max_contracts
        );
    }
}

#[test]
fn resting_orders_count_toward_contract_cap() {
    let risk = test_risk();
    let (mut state, books) = make_bot_state_with_games(1, risk);
    let registry = make_registry(10);

    // Place a resting order for 10 contracts
    state.order_manager.record_placed_order(
        make_order("resting-1", "T0-HOME", 10),
        10,
        "break_ev",
    );

    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    // The resting order should count: 10 resting = at cap
    assert!(
        signals.is_empty(),
        "Should produce no signals when resting orders fill the contract cap, got {} signals",
        signals.len()
    );
}

#[test]
fn position_plus_resting_combines_for_cap() {
    let risk = test_risk();
    let (mut state, books) = make_bot_state_with_games(1, risk);
    let registry = make_registry(20);

    // 8 filled + 8 resting = 16 of 20 cap
    state.risk.seed_positions("T0-HOME", "yes", 50, 8);
    state.order_manager.record_placed_order(
        make_order("resting-1", "T0-HOME", 8),
        8,
        "break_ev",
    );

    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    for signal in &signals {
        assert!(
            signal.max_contracts.unwrap_or(i64::MAX) <= 4,
            "Signal max_contracts={:?} should be <= 4 (20 - 8 filled - 8 resting)",
            signal.max_contracts
        );
    }
}

// ============================================================
// 4. Resting Order Dedup: No Double-Firing on Same Ticker
// ============================================================

#[test]
fn resting_order_prevents_new_signal_on_same_ticker() {
    let risk = test_risk();
    let (mut state, books) = make_bot_state_with_games(1, risk);
    let registry = make_registry(100); // high cap so it's not the blocker

    // Place a resting order on T0-HOME
    state.order_manager.record_placed_order(
        make_order("resting-1", "T0-HOME", 5),
        5,
        "break_ev",
    );

    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    // Should NOT fire a signal on the game that has a resting order
    // (evaluate_strategies skips games where any ticker has a resting order)
    assert!(
        signals.is_empty(),
        "Should not produce signals when a resting order exists on a game ticker, got {:?}",
        signals.iter().map(|s| &s.kalshi_ticker).collect::<Vec<_>>()
    );
}

#[test]
fn in_flight_prevents_new_signal() {
    let risk = test_risk();
    let (mut state, books) = make_bot_state_with_games(1, risk);
    let registry = make_registry(100);

    state.order_manager.mark_in_flight("T0-HOME");

    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    assert!(
        signals.is_empty(),
        "In-flight should block new signals, got {} signals",
        signals.len()
    );
}

// ============================================================
// 5. Multi-Game Isolation: One Game's Limits Don't Affect Others
// ============================================================

#[test]
fn contract_cap_is_per_game_not_global() {
    let risk = test_risk();
    let (mut state, books) = make_bot_state_with_games(3, risk);
    let registry = make_registry(10);

    // Max out game 0
    state.risk.seed_positions("T0-HOME", "yes", 50, 10);
    // Game 1 and 2 should still be able to trade

    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    // Game 0 should be blocked, games 1 and 2 should produce signals
    let blocked_game0 = signals.iter().all(|s| !s.kalshi_ticker.starts_with("T0"));
    assert!(blocked_game0, "Game 0 should produce no signals (at cap)");

    // At least one other game should produce a signal
    assert!(
        !signals.is_empty(),
        "Other games should still produce signals when game 0 is maxed"
    );
}

// ============================================================
// 7. Signal-to-Order Contract Count Respects Cap
// ============================================================

#[test]
fn signal_to_order_respects_max_contracts() {
    let signal = OrderSignal {
        strategy: "test".into(),
        kalshi_ticker: "TICKER".into(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 50,
        size_dollars: 100.0, // would be 200 contracts without cap
        post_only: true,
        expiration_ts: None,
        edge_after_fees: 0.05,
        fair_value_cents: None,
        is_close: false,
        max_contracts: Some(5),
    };

    let order = OrderManager::signal_to_order(&signal).unwrap();
    assert_eq!(order.count, 5, "Contract count should be capped at max_contracts");
}

#[test]
fn signal_to_order_uses_natural_count_when_below_cap() {
    let signal = OrderSignal {
        strategy: "test".into(),
        kalshi_ticker: "TICKER".into(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 50,
        size_dollars: 2.0, // = 4 contracts at 50c
        post_only: true,
        expiration_ts: None,
        edge_after_fees: 0.05,
        fair_value_cents: None,
        is_close: false,
        max_contracts: Some(100),
    };

    let order = OrderManager::signal_to_order(&signal).unwrap();
    assert_eq!(order.count, 4, "Should use natural count when below cap");
}

// ============================================================
// 10. Seed and Reseed Position Consistency
// ============================================================

#[test]
fn seed_positions_tracks_net_position() {
    let mut risk = test_risk();
    risk.seed_positions("T1", "yes", 60, 10);
    assert_eq!(risk.net_position("T1"), 10);
}

#[test]
fn reseed_replaces_old_position() {
    let mut risk = test_risk();
    risk.seed_positions("T1", "yes", 60, 10);
    risk.seed_positions("T1", "no", 40, 5);

    risk.reseed_position("T1", "yes", 50, 8);
    assert_eq!(risk.net_position("T1"), 8);
}

// ============================================================
// 11. Volume Filter Prevents Trading Illiquid Markets
// ============================================================

#[test]
fn low_volume_games_produce_no_signals() {
    let risk = test_risk();
    let mut gsm = GameStateManager::new();
    let mut books = HashMap::new();

    let gs = gsm.upsert("evt_low".into(), "HomeLow".into(), "AwayLow".into());
    gs.espn_home_win_prob = Some(0.70);
    gs.phase = GamePhase::Halftime;

    let mut mkt = KalshiMarketState::new("LOW-VOL".into(), true);
    mkt.volume = Some(500); // below min_volume of 1000
    gs.kalshi_markets.push(mkt);
    gsm.register_ticker("LOW-VOL", "evt_low");

    books.insert("LOW-VOL".to_string(), make_book("LOW-VOL", 48, 52));

    let state = BotState {
        started_at: chrono::Utc::now(),
        game_state: gsm,
        risk,
        order_manager: OrderManager::new(),
        last_espn_update: std::time::Instant::now(),
        last_kalshi_sync: std::time::Instant::now(),
        position_corrections: 0,
    };

    let registry = make_registry(20);
    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    assert!(signals.is_empty(), "Low-volume game should produce no signals");
}

// ============================================================
// 12. Price Filter Prevents Trading Extreme Prices
// ============================================================

#[test]
fn extreme_price_games_produce_no_signals() {
    let risk = test_risk();
    let mut gsm = GameStateManager::new();
    let mut books = HashMap::new();

    let gs = gsm.upsert("evt_extreme".into(), "HomeExt".into(), "AwayExt".into());
    gs.espn_home_win_prob = Some(0.98);
    gs.phase = GamePhase::Halftime;

    let mut mkt = KalshiMarketState::new("EXTREME".into(), true);
    mkt.volume = Some(50000);
    gs.kalshi_markets.push(mkt);
    gsm.register_ticker("EXTREME", "evt_extreme");

    // Book is at 97/99 — mid is 98c, outside max_price_cents=95
    books.insert("EXTREME".to_string(), make_book("EXTREME", 97, 99));

    let state = BotState {
        started_at: chrono::Utc::now(),
        game_state: gsm,
        risk,
        order_manager: OrderManager::new(),
        last_espn_update: std::time::Instant::now(),
        last_kalshi_sync: std::time::Instant::now(),
        position_corrections: 0,
    };

    let registry = make_registry(20);
    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    assert!(
        signals.is_empty(),
        "Extreme-priced market (mid=98c) should be filtered out"
    );
}

// ============================================================
// 14. Edge Threshold Prevents Low-Edge Trading
// ============================================================

#[test]
fn no_signal_when_edge_below_threshold() {
    let risk = test_risk(); // min_edge = 0.01
    let quoter = BreakEvQuoter::new(0.05, 20.0, 1); // strategy needs 5% edge

    // Fair = 54%, market mid = 50c → edge ~3c after ALO pricing
    // This should be below the 5% threshold
    let mut gs = GameState::new("evt".into(), "Home".into(), "Away".into());
    gs.espn_home_win_prob = Some(0.54);
    gs.phase = GamePhase::Halftime;
    let mut mkt = KalshiMarketState::new("TICKER".into(), true);
    mkt.volume = Some(50000);
    gs.kalshi_markets.push(mkt);

    let mut books = HashMap::new();
    books.insert("TICKER".to_string(), make_book("TICKER", 48, 52));

    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(
        signal.is_none(),
        "Should produce no signal when edge is below strategy threshold"
    );
}

// ============================================================
// 15. Fill Dedup Prevents Double State Updates
// ============================================================

#[test]
fn fill_dedup_double_buffer_rotation() {
    let mut om = OrderManager::new();

    // Fill the first buffer (500 entries)
    for i in 0..500 {
        om.mark_fill_processed(&format!("trade-{}", i));
    }

    // Entry 499 should still be in current buffer
    assert!(om.is_fill_processed("trade-499"));

    // One more triggers rotation
    om.mark_fill_processed("trade-500");

    // trade-499 should be in prev buffer
    assert!(om.is_fill_processed("trade-499"), "Recent fills should survive rotation");
    // trade-500 is in current
    assert!(om.is_fill_processed("trade-500"));

    // Fill another 500 to rotate again
    for i in 501..1001 {
        om.mark_fill_processed(&format!("trade-{}", i));
    }

    // trade-499 was in prev, now prev was overwritten
    assert!(
        !om.is_fill_processed("trade-499"),
        "Very old fills should be evicted after two rotations"
    );
}

// ============================================================
// 16. Stress Test: Many Games, Repeated Evaluations
// ============================================================

// ============================================================
// 17. net_game_home_risk: Signed Game-Level Exposure Helper
// ============================================================

#[test]
fn net_game_home_risk_returns_zero_when_flat() {
    let risk = test_risk();
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    assert_eq!(risk.net_game_home_risk(&markets), 0);
}

#[test]
fn net_game_home_risk_long_home_is_positive() {
    let mut risk = test_risk();
    risk.seed_positions("HOME", "yes", 50, 5);
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    assert_eq!(risk.net_game_home_risk(&markets), 5);
}

#[test]
fn net_game_home_risk_long_away_is_negative() {
    let mut risk = test_risk();
    risk.seed_positions("AWAY", "yes", 50, 5);
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    // YES on away ticker = long away team = negative home-aligned exposure
    assert_eq!(risk.net_game_home_risk(&markets), -5);
}

#[test]
fn net_game_home_risk_partial_hedge() {
    let mut risk = test_risk();
    risk.seed_positions("HOME", "yes", 50, 5); // +5 home units
    risk.seed_positions("AWAY", "yes", 50, 3); // -3 home units (long away)
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    assert_eq!(risk.net_game_home_risk(&markets), 2);
}

// ============================================================
// 18. is_reduce_order: Reduce Direction Detection
// ============================================================

#[test]
fn is_reduce_no_on_home_ticker_when_long_home() {
    let mut risk = test_risk();
    risk.seed_positions("HOME", "yes", 50, 7);
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    // Buying NO on HOME when long YES on HOME = reduce (same-ticker)
    assert!(risk.is_reduce_order(&markets, "HOME", &OrderSide::No));
}

#[test]
fn is_reduce_yes_on_away_ticker_when_long_home() {
    let mut risk = test_risk();
    risk.seed_positions("HOME", "yes", 50, 7);
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    // Buying YES on AWAY when long HOME = cross-ticker reduce (backing opposing team)
    assert!(risk.is_reduce_order(&markets, "AWAY", &OrderSide::Yes));
}

#[test]
fn is_not_reduce_yes_on_home_when_long_home() {
    let mut risk = test_risk();
    risk.seed_positions("HOME", "yes", 50, 7);
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    // Buying more YES on HOME = adding exposure, not reducing
    assert!(!risk.is_reduce_order(&markets, "HOME", &OrderSide::Yes));
}

#[test]
fn is_not_reduce_no_on_away_when_long_home() {
    let mut risk = test_risk();
    risk.seed_positions("HOME", "yes", 50, 7);
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    // NO on AWAY = backing home (away loses) = adding home exposure, not reducing
    assert!(!risk.is_reduce_order(&markets, "AWAY", &OrderSide::No));
}

#[test]
fn is_not_reduce_flat_position() {
    let risk = test_risk();
    let markets = vec![
        KalshiMarketState::new("HOME".into(), true),
        KalshiMarketState::new("AWAY".into(), false),
    ];
    // No position → nothing to reduce regardless of direction
    assert!(!risk.is_reduce_order(&markets, "HOME", &OrderSide::No));
    assert!(!risk.is_reduce_order(&markets, "AWAY", &OrderSide::Yes));
}

// ============================================================
// 19. Reduce Order Integration Tests
// ============================================================

/// Build a single halftime game with custom books, seeding `risk` with any pre-existing
/// positions already recorded.
fn make_single_game(
    home_prob: f64,
    home_book: (i64, i64),
    away_book: (i64, i64),
    risk: RiskManager,
) -> (BotState, HashMap<String, LocalOrderBook>) {
    let mut gsm = GameStateManager::new();
    let mut books = HashMap::new();

    let gs = gsm.upsert("evt_0".into(), "Home0".into(), "Away0".into());
    gs.espn_home_win_prob = Some(home_prob);
    gs.phase = GamePhase::Halftime;

    let mut home_mkt = KalshiMarketState::new("T0-HOME".into(), true);
    home_mkt.volume = Some(50000);
    gs.kalshi_markets.push(home_mkt);

    let mut away_mkt = KalshiMarketState::new("T0-AWAY".into(), false);
    away_mkt.volume = Some(50000);
    gs.kalshi_markets.push(away_mkt);

    gsm.register_ticker("T0-HOME", "evt_0");
    gsm.register_ticker("T0-AWAY", "evt_0");

    books.insert("T0-HOME".to_string(), make_book("T0-HOME", home_book.0, home_book.1));
    books.insert("T0-AWAY".to_string(), make_book("T0-AWAY", away_book.0, away_book.1));

    let state = BotState {
        started_at: chrono::Utc::now(),
        game_state: gsm,
        risk,
        order_manager: OrderManager::new(),
        last_espn_update: std::time::Instant::now(),
        last_kalshi_sync: std::time::Instant::now(),
        position_corrections: 0,
    };
    (state, books)
}

#[test]
fn same_ticker_reduce_signal_fires() {
    // Setup: originally long 7 YES on home. Home prob drops to 30% (away now favored).
    // With home market at 48/52, fair_home=30 < mid=50 → strategy generates NO on T0-HOME.
    // NO on T0-HOME when we're long YES on T0-HOME = reduce. Should fire.
    let mut risk = test_risk();
    risk.seed_positions("T0-HOME", "yes", 70, 7);

    let (state, books) = make_single_game(0.30, (48, 52), (48, 52), risk);
    let registry = make_registry(20);
    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    assert!(!signals.is_empty(), "Reduce signal on T0-HOME should fire when home prob drops");

    // The signal must be a reduce direction (NO on home, or YES on away)
    let signal = &signals[0];
    let is_reduce = snapshot.risk.is_reduce_order(
        &snapshot.games["evt_0"].kalshi_markets,
        &signal.kalshi_ticker,
        &signal.side,
    );
    assert!(is_reduce, "Signal {:?} {:?} should be a reduce order", signal.kalshi_ticker, signal.side);
}

#[test]
fn reduce_signal_capped_at_net_position_not_regular_remaining() {
    // 7 YES on home. Regular cap=20 → regular_remaining=13.
    // Reduce signal should be capped at 7 (reduce_cap = net position), not 13.
    let mut risk = test_risk();
    risk.seed_positions("T0-HOME", "yes", 70, 7);

    let (state, books) = make_single_game(0.30, (48, 52), (48, 52), risk);
    let registry = make_registry(20);
    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    assert!(!signals.is_empty(), "Should produce a reduce signal");
    for signal in &signals {
        let is_reduce = snapshot.risk.is_reduce_order(
            &snapshot.games["evt_0"].kalshi_markets,
            &signal.kalshi_ticker,
            &signal.side,
        );
        if is_reduce {
            assert_eq!(
                signal.max_contracts.unwrap_or(0), 7,
                "Reduce max_contracts should be 7 (net position), not regular_remaining=13"
            );
        }
    }
}

#[test]
fn reduce_fires_even_when_at_regular_cap() {
    // 10 YES on home, regular cap = 10 (exactly at cap).
    // regular_remaining = 0, but reduce_cap = 10 → reduce signal should still fire.
    let mut risk = test_risk();
    risk.seed_positions("T0-HOME", "yes", 70, 10);

    let (state, books) = make_single_game(0.30, (48, 52), (48, 52), risk);
    let registry = make_registry(10); // exactly at cap
    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    assert!(
        !signals.is_empty(),
        "Reduce signal should fire even when regular contract cap is exhausted"
    );
    let signal = &signals[0];
    assert_eq!(
        signal.max_contracts.unwrap_or(0), 10,
        "Reduce cap should equal the existing position (10)"
    );
}

#[test]
fn cross_ticker_reduce_bypasses_resting_on_other_market() {
    // Setup: 7 YES on T0-HOME. Resting YES order on T0-HOME.
    // Home prob drops to 30%. T0-HOME book 29/31 → NO on home has 0 raw edge (no signal).
    // T0-AWAY book 65/69 → YES on away has edge 0.70 - 0.68 = 0.02.
    // Strategy returns YES on T0-AWAY (cross-ticker reduce).
    // The resting order on T0-HOME should NOT block a signal on T0-AWAY.
    let mut risk = test_risk();
    risk.seed_positions("T0-HOME", "yes", 70, 7);

    // T0-HOME (1, 99): extreme spread → NO ALO = 98, edge_raw = 0.70 - 0.98 = -0.28 < 0,
    //   evaluate_market returns None. Mid = 50c still passes game-level tradeable filter.
    // T0-AWAY (60, 66): YES ALO = 65, edge_raw = 0.70 - 0.65 = 0.05,
    //   edge_after_fees = 0.04 >> 0.01. Clear signal.
    let (mut state, books) = make_single_game(0.30, (1, 99), (60, 66), risk);

    // Add resting YES order on T0-HOME
    state.order_manager.record_placed_order(
        make_order("resting-home-yes", "T0-HOME", 5), 5, "break_ev",
    );

    let registry = make_registry(20);
    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    assert!(
        !signals.is_empty(),
        "Cross-ticker reduce on T0-AWAY should fire despite resting YES order on T0-HOME"
    );
    let signal = &signals[0];
    assert_eq!(signal.kalshi_ticker, "T0-AWAY", "Should be a T0-AWAY signal");
    assert!(matches!(signal.side, OrderSide::Yes), "Should be YES on T0-AWAY (away team)");
    assert_eq!(
        signal.max_contracts.unwrap_or(0), 7,
        "Reduce cap should be 7 (net game home risk)"
    );
}

#[test]
fn regular_signal_still_blocked_when_no_position_to_reduce() {
    // No position → reduce_cap = 0. Resting order on game → regular also blocked.
    // This ensures existing resting-order-blocks-regular-orders behavior is preserved.
    let risk = test_risk();
    let (mut state, books) = make_bot_state_with_games(1, risk);
    let registry = make_registry(100);

    state.order_manager.record_placed_order(
        make_order("resting-1", "T0-HOME", 5), 5, "break_ev",
    );

    let snapshot = build_eval_snapshot(&state);
    let mut break_log = VecDeque::new();
    let signals = evaluate_strategies(&snapshot, &books, &mut break_log, &registry);

    // No position → reduce_cap = 0, and resting blocks regular → no signals
    assert!(
        signals.is_empty(),
        "Regular signal should be blocked by resting order when no reduce position exists, got {} signals",
        signals.len()
    );
}


// ============================================================
// 20. CLV Staleness Detection Tests
// ============================================================

/// Helper: build a LocalOrderBook with a single YES bid at `yes_bid_price`.
fn make_book_with_yes_bid(ticker: &str, yes_bid_price: i64) -> LocalOrderBook {
    let mut book = LocalOrderBook::new(ticker.to_string());
    book.apply_snapshot(&OrderBookSnapshot {
        yes: vec![PriceLevel { price: yes_bid_price, quantity: 100 }],
        no: vec![],
    });
    book
}

/// Helper: build a LocalOrderBook with a single NO bid at `no_bid_price`.
fn make_book_with_no_bid(ticker: &str, no_bid_price: i64) -> LocalOrderBook {
    let mut book = LocalOrderBook::new(ticker.to_string());
    book.apply_snapshot(&OrderBookSnapshot {
        yes: vec![],
        no: vec![PriceLevel { price: no_bid_price, quantity: 100 }],
    });
    book
}

/// Apply the staleness predicate from scoreboard.rs to a single CLV order.
fn is_clv_stale(
    side: &str,
    price_cents: i64,
    books: &HashMap<String, LocalOrderBook>,
    ticker: &str,
) -> bool {
    let best_bid = match side {
        "Yes" => books.get(ticker).and_then(|b| b.best_yes_bid()).map(|b| b.price),
        "No"  => books.get(ticker).and_then(|b| b.best_no_bid()).map(|b| b.price),
        _     => None,
    };
    best_bid.is_some_and(|bid| bid > price_cents)
}

/// Build an Order suitable for use as a resting CLV order.
fn make_clv_resting_order(id: &str, ticker: &str, side: OrderSide, price: i64) -> Order {
    let (yes_price, no_price) = match side {
        OrderSide::Yes => (Some(price), None),
        OrderSide::No  => (None, Some(price)),
    };
    Order {
        order_id: id.to_string(),
        ticker: ticker.to_string(),
        action: OrderAction::Buy,
        side,
        order_type: "limit".to_string(),
        status: "resting".to_string(),
        yes_price,
        no_price,
        remaining_count: 5,
        created_time: "2026-03-11T10:00:00Z".to_string(),
    }
}

#[test]
fn clv_order_not_stale_when_book_matches_our_price() {
    // Order at 20, best_yes_bid = 20 → best_bid == price → not stale
    let ticker = "CLV-GAME";
    let mut books = HashMap::new();
    books.insert(ticker.to_string(), make_book_with_yes_bid(ticker, 20));

    let stale = is_clv_stale("Yes", 20, &books, ticker);
    assert!(!stale, "Order at 20 with best_yes_bid=20 should NOT be stale (not outbid)");
}

#[test]
fn clv_order_stale_when_outbid() {
    // Order at 20, best_yes_bid = 22 → someone outbid us → stale
    let ticker = "CLV-GAME";
    let mut books = HashMap::new();
    books.insert(ticker.to_string(), make_book_with_yes_bid(ticker, 22));

    let stale = is_clv_stale("Yes", 20, &books, ticker);
    assert!(stale, "Order at 20 with best_yes_bid=22 should be stale (outbid)");
}

#[test]
fn clv_order_not_stale_when_book_below_our_price() {
    // Order at 20, best_yes_bid = 18 → we're still at the top of book → not stale
    let ticker = "CLV-GAME";
    let mut books = HashMap::new();
    books.insert(ticker.to_string(), make_book_with_yes_bid(ticker, 18));

    let stale = is_clv_stale("Yes", 20, &books, ticker);
    assert!(!stale, "Order at 20 with best_yes_bid=18 should NOT be stale (we're top of book)");
}

#[test]
fn clv_no_order_stale_when_outbid() {
    // No-side order at 75, best_no_bid = 77 → someone outbid us → stale
    let ticker = "CLV-GAME";
    let mut books = HashMap::new();
    books.insert(ticker.to_string(), make_book_with_no_bid(ticker, 77));

    let stale = is_clv_stale("No", 75, &books, ticker);
    assert!(stale, "No-side order at 75 with best_no_bid=77 should be stale (outbid)");
}

#[test]
fn clv_order_not_stale_when_book_absent() {
    // No book entry for ticker → best_bid = None → not stale
    let ticker = "CLV-GAME";
    let books: HashMap<String, LocalOrderBook> = HashMap::new();

    let stale = is_clv_stale("Yes", 20, &books, ticker);
    assert!(!stale, "Order with no book entry should NOT be stale (no information)");
}

#[test]
fn all_clv_orders_excludes_non_clv() {
    // CLV order in clv_orders but NOT in resting_orders → excluded by all_clv_orders()
    let mut om = OrderManager::new();

    // Register a CLV order without a corresponding resting order
    om.record_clv_order("order-ghost", "CLV-GAME", "Yes", 20);

    // all_clv_orders() filters to only those also in resting_orders
    let clv = om.all_clv_orders();
    assert!(
        clv.is_empty(),
        "CLV order not in resting_orders should be excluded by all_clv_orders(), got {} entries",
        clv.len()
    );
}

// ============================================================
// 21. CLV Recovery Tests
// ============================================================

#[test]
fn recover_clv_from_resting_basic() {
    // Resting order with strategy="clv_hunter" in order_strategies → recovered into clv_orders
    let mut om = OrderManager::new();

    let order = make_clv_resting_order("o-clv-1", "CLV-GAME", OrderSide::Yes, 35);
    om.record_placed_order(order, 5, "clv_hunter");

    // record_placed_order populates resting_orders and order_strategies but NOT clv_orders.
    // This simulates the post-restart state where clv_orders is empty.
    let before = om.all_clv_orders();
    assert!(before.is_empty(), "Before recovery, clv_orders should be empty");

    om.recover_clv_from_resting();

    let after = om.all_clv_orders();
    assert_eq!(after.len(), 1, "After recovery, the CLV order should be present");
    assert_eq!(after[0].order_id, "o-clv-1");
    assert_eq!(after[0].ticker, "CLV-GAME");
    assert_eq!(after[0].price_cents, 35);
}

#[test]
fn recover_clv_from_resting_skips_non_clv() {
    // Resting order with strategy="break_ev" should NOT be recovered into clv_orders
    let mut om = OrderManager::new();

    let order = make_clv_resting_order("o-break-1", "BREAK-GAME", OrderSide::Yes, 48);
    om.record_placed_order(order, 5, "break_ev");

    om.recover_clv_from_resting();

    let clv = om.all_clv_orders();
    assert!(
        clv.is_empty(),
        "break_ev order should NOT be recovered into clv_orders, got {} entries",
        clv.len()
    );
}

#[test]
fn recover_clv_from_resting_skips_already_present() {
    // Order already in clv_orders → recover_clv_from_resting skips it (no duplicate)
    let mut om = OrderManager::new();

    let order = make_clv_resting_order("o-clv-2", "CLV-GAME", OrderSide::Yes, 42);
    om.record_placed_order(order, 5, "clv_hunter");

    // Manually populate clv_orders (simulates order placed in-session, not via restart)
    om.record_clv_order("o-clv-2", "CLV-GAME", "Yes", 42);

    assert_eq!(om.all_clv_orders().len(), 1);

    // Recovery should skip already-present entries
    om.recover_clv_from_resting();

    let clv = om.all_clv_orders();
    assert_eq!(
        clv.len(), 1,
        "Should have exactly 1 CLV entry after recovery (no duplicate)"
    );
}

#[test]
fn recover_clv_from_resting_no_strategy() {
    // Resting order with no entry in order_strategies → NOT recovered
    let mut om = OrderManager::new();

    // apply_synced_orders adds to resting_orders but NOT order_strategies
    om.apply_synced_orders(vec![
        make_clv_resting_order("o-unknown", "CLV-GAME", OrderSide::Yes, 30),
    ]);

    om.recover_clv_from_resting();

    let clv = om.all_clv_orders();
    assert!(
        clv.is_empty(),
        "Order with no strategy entry should NOT be recovered into clv_orders"
    );
}

#[test]
fn recover_clv_from_resting_mixed_strategies() {
    // Mix of clv_hunter and break_ev orders — only clv_hunter gets recovered
    let mut om = OrderManager::new();

    let clv_order = make_clv_resting_order("o-clv-3", "CLV-GAME", OrderSide::Yes, 28);
    om.record_placed_order(clv_order, 3, "clv_hunter");

    let break_order = make_clv_resting_order("o-break-2", "BREAK-GAME", OrderSide::No, 55);
    om.record_placed_order(break_order, 4, "break_ev");

    om.recover_clv_from_resting();

    let clv = om.all_clv_orders();
    assert_eq!(clv.len(), 1, "Only the clv_hunter order should be recovered, got {}", clv.len());
    assert_eq!(clv[0].order_id, "o-clv-3");
}

#[test]
fn recover_clv_no_side_correct_for_no_order() {
    // Verify that a No-side CLV order gets the correct side string after recovery
    let mut om = OrderManager::new();

    let order = make_clv_resting_order("o-clv-no", "CLV-GAME-NO", OrderSide::No, 72);
    om.record_placed_order(order, 5, "clv_hunter");

    om.recover_clv_from_resting();

    let clv = om.all_clv_orders();
    assert_eq!(clv.len(), 1);
    // The side field is formatted via `format!("{:?}", order.side)` in recover_clv_from_resting
    assert_eq!(clv[0].side, "No", "Recovered No-side order should have side='No'");
    assert_eq!(clv[0].price_cents, 72);
    assert_eq!(clv[0].ticker, "CLV-GAME-NO");
}
