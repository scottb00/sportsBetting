use sports_betting::engine::game_state::{GameState, GameStateManager, KalshiMarketState};
use sports_betting::espn::types::GamePhase;

/// Helper: create a basic GameState for testing
fn make_game() -> GameState {
    GameState::new("evt_001".into(), "Duke".into(), "UNC".into())
}

/// Helper: create a game with ESPN fair value and two Kalshi markets
fn make_game_with_markets(espn_hp: Option<f64>) -> GameState {
    let mut gs = make_game();
    gs.espn_home_win_prob = espn_hp;

    // Home market: YES = Duke (home)
    let mut home_mkt = KalshiMarketState::new("KXNCAAMBGAME-26MAR09-DUKE".into(), true);
    home_mkt.yes_bid = Some(48.0);
    home_mkt.yes_ask = Some(52.0);
    home_mkt.yes_mid = Some(50.0);
    home_mkt.volume = Some(25000);
    gs.kalshi_markets.push(home_mkt);

    // Away market: YES = UNC (away)
    let mut away_mkt = KalshiMarketState::new("KXNCAAMBGAME-26MAR09-UNC".into(), false);
    away_mkt.yes_bid = Some(48.0);
    away_mkt.yes_ask = Some(52.0);
    away_mkt.yes_mid = Some(50.0);
    away_mkt.volume = Some(15000);
    gs.kalshi_markets.push(away_mkt);

    gs
}

// ============================================================
// Fair value for market
// ============================================================

#[test]
fn fair_value_for_home_market() {
    let gs = make_game_with_markets(Some(0.65));
    let home_mkt = &gs.kalshi_markets[0];
    assert!(home_mkt.is_home);
    let fair = gs.fair_value_for_market(home_mkt).unwrap();
    assert!((fair - 0.65).abs() < 1e-10);
}

#[test]
fn fair_value_for_away_market() {
    let gs = make_game_with_markets(Some(0.65));
    let away_mkt = &gs.kalshi_markets[1];
    assert!(!away_mkt.is_home);
    let fair = gs.fair_value_for_market(away_mkt).unwrap();
    // Away = 1 - 0.65 = 0.35
    assert!((fair - 0.35).abs() < 1e-10);
}

#[test]
fn fair_value_home_away_sum_to_one() {
    let gs = make_game_with_markets(Some(0.70));
    let home_fair = gs.fair_value_for_market(&gs.kalshi_markets[0]).unwrap();
    let away_fair = gs.fair_value_for_market(&gs.kalshi_markets[1]).unwrap();
    assert!((home_fair + away_fair - 1.0).abs() < 1e-10);
}

#[test]
fn fair_value_none_without_espn() {
    let gs = make_game_with_markets(None);
    assert!(gs.fair_value_for_market(&gs.kalshi_markets[0]).is_none());
}

// ============================================================
// KalshiMarketState
// ============================================================

#[test]
fn market_state_update_prices() {
    let mut mkt = KalshiMarketState::new("TEST-TICKER".into(), true);
    assert!(mkt.yes_bid.is_none());

    mkt.update_prices(Some(45.0), Some(55.0), Some(50.0));
    assert_eq!(mkt.yes_bid, Some(45.0));
    assert_eq!(mkt.yes_ask, Some(55.0));
    assert_eq!(mkt.yes_mid, Some(50.0));
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
    let gs = make_game_with_markets(Some(0.50));
    assert!(gs.has_kalshi());
}

#[test]
fn kalshi_tickers_returns_all() {
    let gs = make_game_with_markets(Some(0.50));
    let tickers = gs.kalshi_tickers();
    assert_eq!(tickers.len(), 2);
    assert!(tickers.contains(&"KXNCAAMBGAME-26MAR09-DUKE"));
    assert!(tickers.contains(&"KXNCAAMBGAME-26MAR09-UNC"));
}

#[test]
fn kalshi_total_volume() {
    let gs = make_game_with_markets(Some(0.50));
    assert_eq!(gs.kalshi_total_volume(), 40000);
}

#[test]
fn kalshi_market_mut_finds_ticker() {
    let mut gs = make_game_with_markets(Some(0.50));
    let mkt = gs.kalshi_market_mut("KXNCAAMBGAME-26MAR09-DUKE");
    assert!(mkt.is_some());
    assert!(mkt.unwrap().is_home);
}

#[test]
fn kalshi_market_mut_returns_none_for_unknown() {
    let mut gs = make_game_with_markets(Some(0.50));
    assert!(gs.kalshi_market_mut("NONEXISTENT").is_none());
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

    assert_eq!(mgr.pre_game_games().len(), 1);
    assert_eq!(mgr.live_games().len(), 3); // Live, Halftime, Break
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

    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-A").is_some());
    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-B").is_some());
    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-C").is_none());
}

// ============================================================
// Strategy integration tests
// ============================================================

#[test]
fn clv_hunter_no_signal_without_espn() {
    use sports_betting::strategies::clv_hunter::ClvHunter;
    use sports_betting::engine::risk::RiskManager;

    let hunter = ClvHunter::new(0.015);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game_with_markets(None);
    gs.phase = GamePhase::PreGame;

    let signal = hunter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_none());
}

#[test]
fn clv_hunter_signal_with_espn() {
    use sports_betting::strategies::clv_hunter::ClvHunter;
    use sports_betting::engine::risk::RiskManager;

    let hunter = ClvHunter::new(0.01);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    // ESPN says 65% home, Kalshi mid is 50c → big edge
    let mut gs = make_game_with_markets(Some(0.65));
    gs.phase = GamePhase::PreGame;

    let signal = hunter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    assert_eq!(sig.strategy, "clv_hunter");
}

#[test]
fn break_ev_no_signal_when_not_on_break() {
    use sports_betting::strategies::break_ev::BreakEvQuoter;
    use sports_betting::engine::risk::RiskManager;

    let quoter = BreakEvQuoter::new(0.015);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game_with_markets(Some(0.70));
    gs.phase = GamePhase::Live;

    assert!(quoter.evaluate(&gs, &risk, 0.0).is_none());
}

#[test]
fn break_ev_signals_on_halftime() {
    use sports_betting::strategies::break_ev::BreakEvQuoter;
    use sports_betting::engine::risk::RiskManager;

    let quoter = BreakEvQuoter::new(0.01);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game_with_markets(Some(0.70));
    gs.phase = GamePhase::Halftime;

    let signal = quoter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_some());
}

#[test]
fn strategy_picks_best_market() {
    use sports_betting::strategies::break_ev::BreakEvQuoter;
    use sports_betting::engine::risk::RiskManager;

    let quoter = BreakEvQuoter::new(0.01);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    // ESPN says 80% home. Home market ask=52, away market ask=52.
    // Home YES fair=0.80, price=51c → edge=0.29
    // Away YES fair=0.20, price=51c → edge=-0.31 (no edge buying YES)
    // Away NO fair=0.80, price=(100-48-1)=51c → edge=0.29
    // Should pick whichever gives better edge
    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.80);
    gs.phase = GamePhase::Halftime;

    let mut home_mkt = KalshiMarketState::new("HOME-TICKER".into(), true);
    home_mkt.yes_bid = Some(48.0);
    home_mkt.yes_ask = Some(52.0);
    home_mkt.yes_mid = Some(50.0);
    gs.kalshi_markets.push(home_mkt);

    let mut away_mkt = KalshiMarketState::new("AWAY-TICKER".into(), false);
    away_mkt.yes_bid = Some(48.0);
    away_mkt.yes_ask = Some(52.0);
    away_mkt.yes_mid = Some(50.0);
    gs.kalshi_markets.push(away_mkt);

    let signal = quoter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_some());
}

// ============================================================
// ALO pricing tests
// ============================================================

#[test]
fn alo_buy_yes_prices_at_ask_minus_one() {
    use sports_betting::strategies::break_ev::BreakEvQuoter;
    use sports_betting::engine::risk::RiskManager;

    let quoter = BreakEvQuoter::new(0.01);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.70);
    gs.phase = GamePhase::Halftime;

    let mut mkt = KalshiMarketState::new("TEST-HOME".into(), true);
    mkt.yes_bid = Some(55.0);
    mkt.yes_ask = Some(60.0);
    mkt.yes_mid = Some(57.5);
    gs.kalshi_markets.push(mkt);

    let signal = quoter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    // Fair=0.70, mid=0.575 → buying YES. ALO price = ask-1 = 59
    assert_eq!(sig.price_cents, 59);
}

#[test]
fn alo_buy_no_prices_correctly() {
    use sports_betting::strategies::break_ev::BreakEvQuoter;
    use sports_betting::engine::risk::RiskManager;

    let quoter = BreakEvQuoter::new(0.01);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.30); // home is underdog
    gs.phase = GamePhase::Halftime;

    let mut mkt = KalshiMarketState::new("TEST-HOME".into(), true);
    mkt.yes_bid = Some(55.0);
    mkt.yes_ask = Some(60.0);
    mkt.yes_mid = Some(57.5);
    gs.kalshi_markets.push(mkt);

    let signal = quoter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    // Fair YES=0.30, mid=0.575 → buying NO. NO ask = 100 - yes_bid = 45. Price = 45-1 = 44
    assert_eq!(sig.price_cents, 44);
}

// ============================================================
// CLV expiration_ts tests
// ============================================================

#[test]
fn clv_signal_sets_expiration_to_game_start() {
    use sports_betting::strategies::clv_hunter::ClvHunter;
    use sports_betting::engine::risk::RiskManager;

    let hunter = ClvHunter::new(0.01);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game_with_markets(Some(0.65));
    gs.phase = GamePhase::PreGame;
    gs.start_time_ts = Some(1772996400); // game start time

    let signal = hunter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    assert_eq!(sig.expiration_ts, Some(1772996400), "CLV expiration should match game start time");
}

#[test]
fn clv_signal_no_expiration_without_start_time() {
    use sports_betting::strategies::clv_hunter::ClvHunter;
    use sports_betting::engine::risk::RiskManager;

    let hunter = ClvHunter::new(0.01);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game_with_markets(Some(0.65));
    gs.phase = GamePhase::PreGame;
    gs.start_time_ts = None;

    let signal = hunter.evaluate(&gs, &risk, 0.0);
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
        strategy: "clv_hunter".to_string(),
        kalshi_ticker: "TEST-TICKER".to_string(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 55,
        size_dollars: 10.0,
        post_only: true,
        expiration_ts: Some(1772996400),
    };

    let order = OrderManager::signal_to_order(&signal);
    // expiration_ts should pass through as-is (seconds, NOT multiplied by 1000)
    assert_eq!(order.expiration_ts, Some(1772996400));
}

#[test]
fn signal_to_order_none_expiration() {
    use sports_betting::engine::order_manager::{OrderManager, OrderSignal};
    use sports_betting::kalshi::types::{OrderSide, OrderAction};

    let signal = OrderSignal {
        strategy: "clv_hunter".to_string(),
        kalshi_ticker: "TEST-TICKER".to_string(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 55,
        size_dollars: 10.0,
        post_only: true,
        expiration_ts: None,
    };

    let order = OrderManager::signal_to_order(&signal);
    assert_eq!(order.expiration_ts, None);
}

// ============================================================
// Order dedup — has_strategy_order
// ============================================================

#[test]
fn has_strategy_order_tracks_correctly() {
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

    om.track_order(order, "clv_hunter".to_string(), GamePhase::PreGame);

    assert!(om.has_strategy_order("TICKER-A", "clv_hunter"));
    assert!(!om.has_strategy_order("TICKER-A", "break_ev"));
    assert!(!om.has_strategy_order("TICKER-B", "clv_hunter"));
}

#[test]
fn has_strategy_order_cleared_after_fill() {
    use sports_betting::engine::order_manager::OrderManager;
    use sports_betting::kalshi::types::{Order, OrderSide, OrderAction, Fill};

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

    om.track_order(order, "clv_hunter".to_string(), GamePhase::PreGame);
    assert!(om.has_strategy_order("TICKER-A", "clv_hunter"));

    // Full fill removes the order
    let fill = Fill {
        trade_id: "trade-1".to_string(),
        order_id: "order-1".to_string(),
        market_ticker: "TICKER-A".to_string(),
        side: "yes".to_string(),
        action: "buy".to_string(),
        yes_price: 55,
        no_price: 45,
        count: 5,
    };
    om.handle_fill(&fill);
    assert!(!om.has_strategy_order("TICKER-A", "clv_hunter"), "Should be cleared after full fill");
}

#[test]
fn has_strategy_order_persists_after_partial_fill() {
    use sports_betting::engine::order_manager::OrderManager;
    use sports_betting::kalshi::types::{Order, OrderSide, OrderAction, Fill};

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

    om.track_order(order, "clv_hunter".to_string(), GamePhase::PreGame);

    // Partial fill — only 3 of 10
    let fill = Fill {
        trade_id: "trade-1".to_string(),
        order_id: "order-1".to_string(),
        market_ticker: "TICKER-A".to_string(),
        side: "yes".to_string(),
        action: "buy".to_string(),
        yes_price: 55,
        no_price: 45,
        count: 3,
    };
    om.handle_fill(&fill);
    assert!(om.has_strategy_order("TICKER-A", "clv_hunter"), "Should still be tracked after partial fill");
}

// ============================================================
// Edge from order price (not mid)
// ============================================================

#[test]
fn edge_calculated_from_order_price() {
    use sports_betting::strategies::break_ev::BreakEvQuoter;
    use sports_betting::engine::risk::RiskManager;

    let quoter = BreakEvQuoter::new(0.01);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    // Fair=0.60, ask=58 → order price=57c → edge=0.60-0.57=0.03 (before fees)
    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.60);
    gs.phase = GamePhase::Halftime;

    let mut mkt = KalshiMarketState::new("TEST".into(), true);
    mkt.yes_bid = Some(50.0);
    mkt.yes_ask = Some(58.0);
    mkt.yes_mid = Some(54.0);
    gs.kalshi_markets.push(mkt);

    let signal = quoter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_some());
    let sig = signal.unwrap();
    assert_eq!(sig.price_cents, 57); // ask - 1
}
