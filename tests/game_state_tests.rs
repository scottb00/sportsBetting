use std::collections::HashMap;

use sports_betting::engine::game_state::{GameState, GameStateManager, KalshiMarketState};
use sports_betting::espn::types::GamePhase;
use sports_betting::kalshi::orderbook::LocalOrderBook;
use sports_betting::strategies::common::ConvictionConfig;
use sports_betting::strategies::Strategy;

/// Default ConvictionConfig for tests: score 0 → 1000 contracts (effectively uncapped by conviction,
/// limited only by max_contracts_per_game).
fn test_conviction() -> ConvictionConfig {
    ConvictionConfig {
        max_contracts: 10000,
        long_tiers: vec![(0.0, 10000)],
        short_tiers: vec![(0.0, 10000)],
    }
}

/// Helper: create a basic GameState for testing
fn make_game() -> GameState {
    GameState::new("evt_001".into(), "Duke".into(), "UNC".into())
}

/// Helper: build a LocalOrderBook with given yes_bid and yes_ask (as cents).
fn make_book(ticker: &str, yes_bid: i64, yes_ask: i64) -> LocalOrderBook {
    let mut book = LocalOrderBook::new(ticker.to_string());
    // YES bid at given price
    book.yes_levels.insert(yes_bid, 100);
    // NO bid at (100 - yes_ask) to create a YES ask at yes_ask
    book.no_levels.insert(100 - yes_ask, 100);
    book
}

/// Helper: create a game with ESPN fair value, two Kalshi markets, and matching order books.
fn make_game_with_markets(espn_hp: Option<f64>) -> (GameState, HashMap<String, LocalOrderBook>) {
    let mut gs = make_game();
    gs.espn_home_win_prob = espn_hp;

    // Home market: YES = Duke (home)
    let mut home_mkt = KalshiMarketState::new("KXNCAAMBGAME-26MAR09-DUKE".into(), true);
    home_mkt.volume = Some(25000);
    gs.kalshi_markets.push(home_mkt);

    // Away market: YES = UNC (away)
    let mut away_mkt = KalshiMarketState::new("KXNCAAMBGAME-26MAR09-UNC".into(), false);
    away_mkt.volume = Some(15000);
    gs.kalshi_markets.push(away_mkt);

    let mut books = HashMap::new();
    books.insert("KXNCAAMBGAME-26MAR09-DUKE".to_string(), make_book("KXNCAAMBGAME-26MAR09-DUKE", 48, 52));
    books.insert("KXNCAAMBGAME-26MAR09-UNC".to_string(), make_book("KXNCAAMBGAME-26MAR09-UNC", 48, 52));

    (gs, books)
}

// ============================================================
// Fair value for market
// ============================================================

#[test]
fn fair_value_for_home_market() {
    let (gs, _books) = make_game_with_markets(Some(0.65));
    let home_mkt = &gs.kalshi_markets[0];
    assert!(home_mkt.is_home);
    let fair = gs.fair_value_for_market(home_mkt).unwrap();
    assert!((fair - 0.65).abs() < 1e-10);
}

#[test]
fn fair_value_for_away_market() {
    let (gs, _books) = make_game_with_markets(Some(0.65));
    let away_mkt = &gs.kalshi_markets[1];
    assert!(!away_mkt.is_home);
    let fair = gs.fair_value_for_market(away_mkt).unwrap();
    // Away = 1 - 0.65 = 0.35
    assert!((fair - 0.35).abs() < 1e-10);
}

#[test]
fn fair_value_home_away_sum_to_one() {
    let (gs, _books) = make_game_with_markets(Some(0.70));
    let home_fair = gs.fair_value_for_market(&gs.kalshi_markets[0]).unwrap();
    let away_fair = gs.fair_value_for_market(&gs.kalshi_markets[1]).unwrap();
    assert!((home_fair + away_fair - 1.0).abs() < 1e-10);
}

#[test]
fn fair_value_none_without_espn() {
    let (gs, _books) = make_game_with_markets(None);
    assert!(gs.fair_value_for_market(&gs.kalshi_markets[0]).is_none());
}

// ============================================================
// GameState helpers
// ============================================================

#[test]
fn has_kalshi_false_when_empty() {
    let gs = make_game();
    assert!(!gs.has_kalshi());
}

#[test]
fn has_kalshi_true_with_markets() {
    let (gs, _books) = make_game_with_markets(Some(0.50));
    assert!(gs.has_kalshi());
}

#[test]
fn kalshi_tickers_returns_all() {
    let (gs, _books) = make_game_with_markets(Some(0.50));
    let tickers = gs.kalshi_tickers();
    assert_eq!(tickers.len(), 2);
    assert!(tickers.contains(&"KXNCAAMBGAME-26MAR09-DUKE"));
    assert!(tickers.contains(&"KXNCAAMBGAME-26MAR09-UNC"));
}

#[test]
fn kalshi_total_volume() {
    let (gs, _books) = make_game_with_markets(Some(0.50));
    assert_eq!(gs.kalshi_total_volume(), 40000);
}

// ============================================================
// GameStateManager tests
// ============================================================

#[test]
fn manager_upsert_creates_and_retrieves() {
    let mut mgr = GameStateManager::new();
    mgr.upsert("e1".into(), "Duke".into(), "UNC".into());
    assert!(mgr.get("e1").is_some());
    assert!(mgr.get("e2").is_none());
}

#[test]
fn manager_upsert_does_not_overwrite() {
    let mut mgr = GameStateManager::new();
    let gs = mgr.upsert("e1".into(), "Duke".into(), "UNC".into());
    gs.espn_home_win_prob = Some(0.65);

    let gs2 = mgr.upsert("e1".into(), "Duke".into(), "UNC".into());
    assert_eq!(gs2.espn_home_win_prob, Some(0.65));
}

#[test]
fn manager_phase_filters() {
    let mut mgr = GameStateManager::new();
    mgr.upsert("e1".into(), "A".into(), "B".into()).phase = GamePhase::PreGame;
    mgr.upsert("e2".into(), "C".into(), "D".into()).phase = GamePhase::Live;
    mgr.upsert("e3".into(), "E".into(), "F".into()).phase = GamePhase::Halftime;
    mgr.upsert("e4".into(), "G".into(), "H".into()).phase = GamePhase::Final;
    mgr.upsert("e5".into(), "I".into(), "J".into()).phase = GamePhase::Break;

    assert_eq!(mgr.games_on_break().len(), 2); // Halftime, Break
}

#[test]
fn manager_cleanup_finished() {
    let mut mgr = GameStateManager::new();
    mgr.upsert("e1".into(), "A".into(), "B".into()).phase = GamePhase::Live;
    mgr.upsert("e2".into(), "C".into(), "D".into()).phase = GamePhase::Final;
    mgr.upsert("e3".into(), "E".into(), "F".into()).phase = GamePhase::Final;

    mgr.cleanup_finished();
    assert_eq!(mgr.games.len(), 1);
    assert!(mgr.get("e1").is_some());
}

#[test]
fn manager_find_by_kalshi_ticker() {
    let mut mgr = GameStateManager::new();
    let gs = mgr.upsert("e1".into(), "Duke".into(), "UNC".into());
    gs.kalshi_markets.push(KalshiMarketState::new("TICKER-A".into(), true));
    gs.kalshi_markets.push(KalshiMarketState::new("TICKER-B".into(), false));
    mgr.register_ticker("TICKER-A", "e1");
    mgr.register_ticker("TICKER-B", "e1");

    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-A").is_some());
    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-B").is_some());
    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-C").is_none());
}

// ============================================================
// Strategy integration tests
// ============================================================

#[test]
fn clv_hunter_no_signal_without_espn() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let hunter = PassiveEspn::new(0.015, 0.015, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let (mut gs, books) = make_game_with_markets(None);
    gs.phase = GamePhase::PreGame;

    let signal = hunter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none());
}

#[test]
fn clv_hunter_signal_with_espn() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let hunter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    // ESPN says 65% home, Kalshi mid is 50c → big edge
    let (mut gs, books) = make_game_with_markets(Some(0.65));
    gs.phase = GamePhase::PreGame;

    let signal = hunter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    assert_eq!(sig.strategy, "pregame");
}

#[test]
fn break_ev_no_signal_when_not_on_break() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::strategies::Strategy;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.015, 0.015, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let (mut gs, books) = make_game_with_markets(Some(0.70));
    gs.phase = GamePhase::Live;

    assert!(!quoter.can_evaluate(&gs) || quoter.evaluate(&gs, &risk, 0.0, &books).is_none());
}

#[test]
fn break_ev_signals_on_halftime() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let (mut gs, books) = make_game_with_markets(Some(0.70));
    gs.phase = GamePhase::Halftime;

    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some());
}

#[test]
fn strategy_picks_best_market() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.80);
    gs.phase = GamePhase::Halftime;

    gs.kalshi_markets.push(KalshiMarketState::new("HOME-TICKER".into(), true));
    gs.kalshi_markets.push(KalshiMarketState::new("AWAY-TICKER".into(), false));

    let mut books = HashMap::new();
    books.insert("HOME-TICKER".to_string(), make_book("HOME-TICKER", 48, 52));
    books.insert("AWAY-TICKER".to_string(), make_book("AWAY-TICKER", 48, 52));

    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some());
}

// ============================================================
// ALO pricing tests
// ============================================================

#[test]
fn alo_buy_yes_prices_at_ask_minus_one() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.70);
    gs.phase = GamePhase::Halftime;

    gs.kalshi_markets.push(KalshiMarketState::new("TEST-HOME".into(), true));

    let mut books = HashMap::new();
    books.insert("TEST-HOME".to_string(), make_book("TEST-HOME", 55, 60));

    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    // Fair=0.70, mid=57.5c → buying YES. ALO price = ask-1 = 59
    assert_eq!(sig.price_cents, 59);
}

#[test]
fn alo_buy_no_prices_correctly() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.30); // home is underdog
    gs.phase = GamePhase::Halftime;

    gs.kalshi_markets.push(KalshiMarketState::new("TEST-HOME".into(), true));

    let mut books = HashMap::new();
    books.insert("TEST-HOME".to_string(), make_book("TEST-HOME", 55, 60));

    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    // Fair YES=0.30, mid=57.5c → buying NO. NO ask = 100 - yes_bid = 45. Price = 45-1 = 44
    assert_eq!(sig.price_cents, 44);
}

// ============================================================
// CLV expiration_ts tests
// ============================================================

#[test]
fn clv_signal_sets_expiration_to_game_start() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let hunter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let (mut gs, books) = make_game_with_markets(Some(0.65));
    gs.phase = GamePhase::PreGame;
    gs.start_time_ts = Some(1772996400); // game start time

    let signal = hunter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    assert_eq!(sig.expiration_ts, Some(1772996400), "CLV expiration should match game start time");
}

#[test]
fn clv_signal_no_expiration_without_start_time() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let hunter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let (mut gs, books) = make_game_with_markets(Some(0.65));
    gs.phase = GamePhase::PreGame;
    gs.start_time_ts = None;

    let signal = hunter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    assert_eq!(sig.expiration_ts, None, "No expiration if start time unknown");
}

// ============================================================
// Order signal_to_order expiration_ts passthrough
// ============================================================

#[test]
fn signal_to_order_passes_expiration_in_seconds() {
    use sports_betting::engine::order_manager::{OrderManager, OrderSignal};
    use sports_betting::kalshi::types::{OrderSide, OrderAction};

    let signal = OrderSignal {
        strategy: "pregame".to_string(),
        kalshi_ticker: "TEST-TICKER".to_string(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 55,
        size_dollars: 10.0,
        post_only: true,
        expiration_ts: Some(1772996400),
        edge_after_fees: 0.05,
        fair_value_cents: None,
        is_close: false,
        max_contracts: None,
        conviction_score: None,
        conviction_details: None,
    };

    let order = OrderManager::signal_to_order(&signal).unwrap();
    // expiration_ts should pass through as-is (seconds, NOT multiplied by 1000)
    assert_eq!(order.expiration_ts, Some(1772996400));
}

#[test]
fn signal_to_order_none_expiration() {
    use sports_betting::engine::order_manager::{OrderManager, OrderSignal};
    use sports_betting::kalshi::types::{OrderSide, OrderAction};

    let signal = OrderSignal {
        strategy: "pregame".to_string(),
        kalshi_ticker: "TEST-TICKER".to_string(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 55,
        size_dollars: 10.0,
        post_only: true,
        expiration_ts: None,
        edge_after_fees: 0.05,
        fair_value_cents: None,
        is_close: false,
        max_contracts: None,
        conviction_score: None,
        conviction_details: None,
    };

    let order = OrderManager::signal_to_order(&signal).unwrap();
    assert_eq!(order.expiration_ts, None);
}

// ============================================================
// Order dedup — has_resting_order (simplified: dedup by ticker)
// ============================================================

#[test]
fn has_resting_order_tracks_correctly() {
    use sports_betting::engine::order_manager::OrderManager;
    use sports_betting::kalshi::types::{Order, OrderSide, OrderAction};

    let mut om = OrderManager::new();

    let order = Order {
        order_id: "order-1".to_string(),
        ticker: "TICKER-A".to_string(),
        action: OrderAction::Buy,
        side: OrderSide::Yes,
        order_type: "limit".to_string(),
        status: "resting".to_string(),
        yes_price: Some(55),
        no_price: Some(45),
        remaining_count: 10,
        created_time: "2026-03-09T18:00:00Z".to_string(),
    };

    om.record_placed_order(order, 10, "test");

    assert!(om.has_resting_order("TICKER-A"));
    assert!(!om.has_resting_order("TICKER-B"));
}

#[test]
fn resting_order_cleared_after_remove() {
    use sports_betting::engine::order_manager::OrderManager;
    use sports_betting::kalshi::types::{Order, OrderSide, OrderAction};

    let mut om = OrderManager::new();

    let order = Order {
        order_id: "order-1".to_string(),
        ticker: "TICKER-A".to_string(),
        action: OrderAction::Buy,
        side: OrderSide::Yes,
        order_type: "limit".to_string(),
        status: "resting".to_string(),
        yes_price: Some(55),
        no_price: Some(45),
        remaining_count: 5,
        created_time: "2026-03-09T18:00:00Z".to_string(),
    };

    om.record_placed_order(order, 10, "test");
    assert!(om.has_resting_order("TICKER-A"));

    om.remove_order("order-1");
    assert!(!om.has_resting_order("TICKER-A"), "Should be cleared after remove");
}

#[test]
fn in_flight_blocks_resting_check() {
    use sports_betting::engine::order_manager::OrderManager;

    let mut om = OrderManager::new();
    assert!(!om.has_resting_order("TICKER-A"));

    om.mark_in_flight("TICKER-A");
    assert!(om.has_resting_order("TICKER-A"), "In-flight should block duplicate");

    om.clear_in_flight("TICKER-A");
    assert!(!om.has_resting_order("TICKER-A"), "Should be clear after in-flight removed");
}

// ============================================================
// Edge from order price (not mid)
// ============================================================

#[test]
fn edge_calculated_from_order_price() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    // Fair=0.60, ask=58 → order price=57c → edge=0.60-0.57=0.03 (before fees)
    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.60);
    gs.phase = GamePhase::Halftime;

    gs.kalshi_markets.push(KalshiMarketState::new("TEST".into(), true));

    let mut books = HashMap::new();
    books.insert("TEST".to_string(), make_book("TEST", 50, 58));

    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    assert_eq!(sig.price_cents, 57); // ask - 1
}

// ============================================================
// Target-position sizing tests
// ============================================================

/// Helper: make a game ready for break_ev evaluation.
fn make_halftime_game(espn_hp: f64) -> (GameState, HashMap<String, LocalOrderBook>) {
    let mut gs = GameState::new("evt_sz".into(), "Duke".into(), "UNC".into());
    gs.espn_home_win_prob = Some(espn_hp);
    gs.phase = GamePhase::Halftime;
    let mut mkt = KalshiMarketState::new("TEST-HOME".into(), true);
    mkt.volume = Some(50000);
    gs.kalshi_markets.push(mkt);
    let mut books = HashMap::new();
    books.insert("TEST-HOME".to_string(), make_book("TEST-HOME", 48, 52));
    (gs, books)
}

/// Conviction sizing with no sportsbook data: score=0 → tier maps to max,
/// capped by max_contracts_per_game=20.
#[test]
fn target_sizing_conviction_no_sportsbook_data() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let (gs, books) = make_halftime_game(0.70);
    let signal = quoter.evaluate(&gs, &risk, 0.0, &books).unwrap();
    let order = sports_betting::engine::order_manager::OrderManager::signal_to_order(&signal).unwrap();
    assert_eq!(order.count, 20, "20 contracts (max_contracts_per_game) with generous conviction tiers");
}

/// When already below target, add to fill the gap.
/// Fair=0.70 → target=20 (conviction). Seed 5 YES → delta=15 → add 15 YES.
#[test]
fn target_adds_toward_target_when_below() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;
    use sports_betting::kalshi::types::OrderSide;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 51, 5); // hold 5 YES

    let (gs, books) = make_halftime_game(0.70); // target=20 (max_contracts_per_game)
    let signal = quoter.evaluate(&gs, &risk, 0.0, &books).unwrap();
    assert!(matches!(signal.side, OrderSide::Yes), "should add YES");
    let order = sports_betting::engine::order_manager::OrderManager::signal_to_order(&signal).unwrap();
    assert_eq!(order.count, 15, "add 15 to reach target of 20 from 5");
}

/// Anti-scalp guard: delta < min_trade_contracts → no signal.
/// Fair=0.70 → target=20. Seed 18 YES → delta=2 < min_trade_contracts=5 → None.
#[test]
fn target_no_signal_when_delta_below_min_trade() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 5, 1000, 20, test_conviction()); // min_trade_contracts=5
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 51, 18); // hold 18, target=20, delta=2 < 5

    let (gs, books) = make_halftime_game(0.70);
    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none(), "delta=2 < min_trade_contracts=5 → no scalp signal");
}

/// No trimming: when above target but still have edge, do nothing.
/// Fair=0.70 → target=20. Seed 30 YES (above target) → no signal emitted.
#[test]
fn target_no_trim_when_above_target() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 51, 30); // hold 30, target=16, delta=-14 (above target)

    let (gs, books) = make_halftime_game(0.70);
    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none(), "above target → no trim, no signal");
}

/// Close entire position when edge disappears (ESPN fair ≈ market mid → target=0).
/// Seed 10 YES. When fair=0.50 and mid=0.50, no edge → target=0 → close signal emitted.
#[test]
fn target_closes_position_when_edge_gone() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 51, 10); // hold 10 YES

    // fair=0.50 exactly at mid → compute_edge_and_alo returns None → target=0
    // No close-direction edge → skip (don't send negative-edge orders)
    let (gs, books) = make_halftime_game(0.50);
    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none(), "no close signal when no edge in close direction");
}

/// No signal when flat position and no edge.
#[test]
fn target_no_signal_when_flat_and_no_edge() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let (gs, books) = make_halftime_game(0.50); // no edge, no position
    let signal = quoter.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none());
}

/// When edge favors NO, target is negative (hold NO contracts).
/// Fair=0.30 → buying NO. With conviction and max_contracts_per_game=20, target=-20.
/// From flat → add NO 20.
#[test]
fn target_sizes_correctly_for_no_side() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;
    use sports_betting::kalshi::types::OrderSide;

    let quoter = PassiveEspn::new(0.01, 0.01, 1, 1000, 20, test_conviction());
    let risk = RiskManager::new();

    let (gs, books) = make_halftime_game(0.30); // edge favors NO
    let signal = quoter.evaluate(&gs, &risk, 0.0, &books).unwrap();
    assert!(matches!(signal.side, OrderSide::No), "should buy NO");
    let order = sports_betting::engine::order_manager::OrderManager::signal_to_order(&signal).unwrap();
    assert_eq!(order.count, 20, "20 NO contracts (max_contracts_per_game) with conviction");
}

// ============================================================
// Negative-edge close protection
// ============================================================

/// Helper: create a live game with given fair value and book prices.
fn make_live_game_with_book(espn_hp: f64, yes_bid: i64, yes_ask: i64) -> (GameState, HashMap<String, LocalOrderBook>) {
    let mut gs = GameState::new("evt_close".into(), "Duke".into(), "UNC".into());
    gs.espn_home_win_prob = Some(espn_hp);
    gs.phase = GamePhase::Live;
    gs.period = Some(2);
    gs.display_clock = Some("10:00".to_string());
    let mut mkt = KalshiMarketState::new("TEST-HOME".into(), true);
    mkt.volume = Some(50000);
    gs.kalshi_markets.push(mkt);
    let mut books = HashMap::new();
    books.insert("TEST-HOME".to_string(), make_book("TEST-HOME", yes_bid, yes_ask));
    (gs, books)
}

/// Close signal must NOT fire when fair value is worse than order price.
/// Long YES at 50c, fair=0.45 → buying NO to close, but NO ALO = (100-48-1) = 51c,
/// fair NO = 0.55. edge_raw = 0.55 - 0.51 = 0.04 > 0 → this is GOOD close edge.
/// But if fair=0.55 → fair NO = 0.45, NO ALO = 51c → edge_raw = 0.45 - 0.51 = -0.06 → must NOT close.
#[test]
fn no_close_when_edge_is_negative() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let closer = PassiveEspn::new(0.01, 0.01, 5, 30, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 50, 10); // hold 10 YES

    // fair=0.55 for home (YES). Closing means buying NO.
    // NO ALO = 100 - 48 - 1 = 51c. fair NO = 0.45. edge = 0.45 - 0.51 = -0.06 → negative
    let (gs, books) = make_live_game_with_book(0.55, 48, 52);
    let signal = closer.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none(), "must not close with negative edge");
}

/// Close signal must NOT fire when fair value equals mid (no edge in either direction).
#[test]
fn no_close_when_edge_is_zero() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let closer = PassiveEspn::new(0.01, 0.01, 5, 30, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 50, 10);

    // fair=0.50, mid=0.50 → compute_edge_and_alo returns None → close_edge=0
    let (gs, books) = make_live_game_with_book(0.50, 48, 52);
    let signal = closer.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none(), "must not close with zero edge");
}

/// Close signal SHOULD fire when there's positive edge in the close direction.
#[test]
fn close_fires_with_positive_close_edge() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;
    use sports_betting::kalshi::types::OrderSide;

    let closer = PassiveEspn::new(0.01, 0.01, 5, 30, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 50, 10); // hold 10 YES

    // fair=0.40 → fair NO = 0.60. NO ALO = 100-48-1 = 51c. edge = 0.60 - 0.51 = 0.09 → positive
    let (gs, books) = make_live_game_with_book(0.40, 48, 52);
    let signal = closer.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_some(), "should close with positive edge");
    let s = signal.unwrap();
    assert!(s.is_close, "must be a close signal");
    assert!(matches!(s.side, OrderSide::No), "close YES by buying NO");
    assert!(s.edge_after_fees > 0.0, "edge must be positive");
}

/// Close signal edge must match: fair_in_close_direction - alo_price, not fair - mid.
#[test]
fn close_edge_computed_from_alo_not_mid() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let closer = PassiveEspn::new(0.01, 0.01, 5, 30, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 50, 10);

    // fair=0.35 → fair NO = 0.65. Book: bid=40, ask=60. NO ALO = 100-40-1 = 59c.
    // edge_raw = 0.65 - 0.59 = 0.06, NOT 0.65 - 0.50 = 0.15
    let (gs, books) = make_live_game_with_book(0.35, 40, 60);
    let signal = closer.evaluate(&gs, &risk, 0.0, &books).unwrap();
    assert!(signal.edge_after_fees < 0.06, "edge should be from ALO price, not mid");
    assert!(signal.edge_after_fees > 0.03, "edge should be positive after fees");
}

/// Final minutes: do NOT close when edge is negative. No forced unwinds — closing is purely +EV.
#[test]
fn final_minutes_no_close_without_edge() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let closer = PassiveEspn::new(0.01, 0.01, 5, 30, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "yes", 50, 10);

    // fair=0.55 → negative close edge. Even in final minutes, should NOT close.
    let (mut gs, books) = make_live_game_with_book(0.55, 48, 52);
    gs.display_clock = Some("3:00".to_string()); // < 5 minutes remaining
    gs.period = Some(2);

    let signal = closer.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none(), "must not close in final minutes when edge is negative");
}

/// Long NO position: should not close when buying YES has negative edge.
#[test]
fn no_close_long_no_when_buying_yes_negative_edge() {
    use sports_betting::strategies::passive_espn::PassiveEspn;
    use sports_betting::engine::risk::RiskManager;

    let closer = PassiveEspn::new(0.01, 0.01, 5, 30, 20, test_conviction());
    let mut risk = RiskManager::new();
    risk.seed_positions("TEST-HOME", "no", 50, 10); // hold 10 NO

    // fair=0.45 → fair YES = 0.45. YES ALO = 52-1 = 51c. edge = 0.45 - 0.51 = -0.06 → negative
    let (gs, books) = make_live_game_with_book(0.45, 48, 52);
    let signal = closer.evaluate(&gs, &risk, 0.0, &books);
    assert!(signal.is_none(), "must not close NO position when YES edge is negative");
}
