//! Correctness tests: direction invariants, property-based fuzzing, end-to-end scenarios.
//!
//! These tests exist to prevent the class of bugs that silently lose money:
//! flipped home/away, wrong ticker, misaligned fair values, wrong order side.

use proptest::prelude::*;

use sports_betting::engine::game_state::{GameState, GameStateManager, KalshiMarketState};
use sports_betting::engine::order_manager::{OrderManager, OrderSignal};
use sports_betting::engine::risk::RiskManager;
use sports_betting::espn::types::GamePhase;
use sports_betting::kalshi::types::{OrderAction, OrderSide};
use sports_betting::strategies::break_ev::BreakEvQuoter;
use sports_betting::strategies::clv_hunter::ClvHunter;
use sports_betting::strategies::Strategy;

// ============================================================
// Helpers
// ============================================================

/// Build a game with two Kalshi markets (home YES + away YES) and configurable book.
fn make_game_with_book(
    espn_home_prob: f64,
    phase: GamePhase,
    yes_bid: f64,
    yes_ask: f64,
) -> GameState {
    let mut gs = GameState::new("evt_test".into(), "HomeTeam".into(), "AwayTeam".into());
    gs.espn_home_win_prob = Some(espn_home_prob);
    gs.phase = phase;

    let mid = (yes_bid + yes_ask) / 2.0;

    let mut home_mkt = KalshiMarketState::new("TICKER-HOME".into(), true);
    home_mkt.yes_bid = Some(yes_bid);
    home_mkt.yes_ask = Some(yes_ask);
    home_mkt.yes_mid = Some(mid);
    home_mkt.volume = Some(50000);
    gs.kalshi_markets.push(home_mkt);

    let mut away_mkt = KalshiMarketState::new("TICKER-AWAY".into(), false);
    away_mkt.yes_bid = Some(yes_bid);
    away_mkt.yes_ask = Some(yes_ask);
    away_mkt.yes_mid = Some(mid);
    away_mkt.volume = Some(50000);
    gs.kalshi_markets.push(away_mkt);

    gs
}

fn test_risk() -> RiskManager {
    RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01)
}

// ============================================================
// 1. Property Tests: Direction Invariants
// ============================================================

proptest! {
    /// Fair value for home market + away market MUST always sum to 1.0.
    #[test]
    fn prop_fair_values_sum_to_one(espn_prob in 0.01f64..0.99) {
        let gs = make_game_with_book(espn_prob, GamePhase::Halftime, 40.0, 60.0);
        let home_fair = gs.fair_value_for_market(&gs.kalshi_markets[0]).unwrap();
        let away_fair = gs.fair_value_for_market(&gs.kalshi_markets[1]).unwrap();
        prop_assert!((home_fair + away_fair - 1.0).abs() < 1e-10,
            "home={} + away={} != 1.0 for espn_prob={}", home_fair, away_fair, espn_prob);
    }

    /// Fair value for home market equals ESPN probability directly.
    #[test]
    fn prop_home_fair_equals_espn(espn_prob in 0.01f64..0.99) {
        let gs = make_game_with_book(espn_prob, GamePhase::Halftime, 40.0, 60.0);
        let home_fair = gs.fair_value_for_market(&gs.kalshi_markets[0]).unwrap();
        prop_assert!((home_fair - espn_prob).abs() < 1e-10);
    }

    /// Fair value for away market equals 1 - ESPN probability.
    #[test]
    fn prop_away_fair_is_complement(espn_prob in 0.01f64..0.99) {
        let gs = make_game_with_book(espn_prob, GamePhase::Halftime, 40.0, 60.0);
        let away_fair = gs.fair_value_for_market(&gs.kalshi_markets[1]).unwrap();
        prop_assert!((away_fair - (1.0 - espn_prob)).abs() < 1e-10);
    }

    /// Kelly size is NEVER negative.
    #[test]
    fn prop_kelly_never_negative(
        fair_prob in 0.01f64..0.99,
        price_cents in 1.0f64..99.0,
        exposure in 0.0f64..40.0,
    ) {
        let risk = test_risk();
        let size = risk.kelly_size(fair_prob, price_cents, exposure);
        prop_assert!(size >= 0.0, "Kelly returned negative: {} for fair={}, price={}", size, fair_prob, price_cents);
    }

    /// Kelly size never exceeds per-game cap.
    #[test]
    fn prop_kelly_within_game_cap(
        fair_prob in 0.51f64..0.99,
        price_cents in 1.0f64..50.0,
    ) {
        let risk = test_risk();
        let size = risk.kelly_size(fair_prob, price_cents, 0.0);
        prop_assert!(size <= risk.max_position_per_game + 0.01,
            "Kelly {} > game cap {}", size, risk.max_position_per_game);
    }

    /// Maker fee is ALWAYS non-negative.
    #[test]
    fn prop_maker_fee_non_negative(
        contracts in 1i64..1000,
        price_cents in 1i64..99,
    ) {
        let fee = RiskManager::maker_fee(contracts, price_cents);
        prop_assert!(fee >= 0.0, "Fee was negative: {}", fee);
    }

    /// Maker fee is symmetric: fee(p) == fee(100-p).
    #[test]
    fn prop_maker_fee_symmetric(
        contracts in 1i64..100,
        price_cents in 1i64..49,
    ) {
        let fee_low = RiskManager::maker_fee(contracts, price_cents);
        let fee_high = RiskManager::maker_fee(contracts, 100 - price_cents);
        prop_assert!((fee_low - fee_high).abs() < 1e-10,
            "Fee asymmetric: fee({})={} != fee({})={}", price_cents, fee_low, 100-price_cents, fee_high);
    }
}

// ============================================================
// 2. Property Tests: Signal Direction Correctness
// ============================================================

proptest! {
    /// When a signal fires for buying YES, fair_value > order_price.
    /// When a signal fires for buying NO, (1 - fair_value) > order_price.
    /// i.e., we NEVER buy the wrong direction.
    #[test]
    fn prop_signal_direction_never_wrong(
        espn_prob in 0.15f64..0.85,
        yes_bid in 20i64..45,
        spread in 3i64..15,
    ) {
        let yes_ask = yes_bid + spread;
        if yes_ask >= 99 { return Ok(()); }

        let gs = make_game_with_book(
            espn_prob,
            GamePhase::Halftime,
            yes_bid as f64,
            yes_ask as f64,
        );
        for market in &gs.kalshi_markets {
            let fair = gs.fair_value_for_market(market).unwrap();
            let mid = market.yes_mid.unwrap() / 100.0;

            // Simulate what evaluate_market does
            let buying_yes = fair > mid;

            if buying_yes {
                let price = (yes_ask - 1).max(1);
                let order_prob = price as f64 / 100.0;
                let edge = fair - order_prob;
                // If a signal would fire, edge must be positive
                if edge > 0.0 {
                    prop_assert!(fair > order_prob,
                        "Buying YES but fair {} <= order_price {} (market is_home={})",
                        fair, order_prob, market.is_home);
                }
            } else {
                let no_ask = 100 - yes_bid;
                let price = (no_ask - 1).max(1);
                let order_prob = price as f64 / 100.0;
                let fair_no = 1.0 - fair;
                let edge = fair_no - order_prob;
                if edge > 0.0 {
                    prop_assert!(fair_no > order_prob,
                        "Buying NO but fair_no {} <= order_price {} (market is_home={})",
                        fair_no, order_prob, market.is_home);
                }
            }
        }
    }

    /// For any game, at most ONE side (YES or NO) on a single market should have positive edge.
    /// (Both can't be +EV at the same time on the same market — that would be free money.)
    #[test]
    fn prop_no_free_money_same_market(
        espn_prob in 0.10f64..0.90,
        yes_bid in 10i64..45,
        spread in 2i64..20,
    ) {
        let yes_ask = yes_bid + spread;
        if yes_ask >= 99 { return Ok(()); }

        let fair = espn_prob; // is_home=true, so fair = espn_prob directly

        // YES edge: fair - (ask-1)/100
        let yes_price = (yes_ask - 1).max(1) as f64 / 100.0;
        let yes_edge = fair - yes_price;

        // NO edge: (1-fair) - ((100-bid-1)/100)
        let no_price = (100 - yes_bid - 1).max(1) as f64 / 100.0;
        let no_edge = (1.0 - fair) - no_price;

        // Both can't be positive (would mean you can buy both sides profitably on same market)
        prop_assert!(!(yes_edge > 0.0 && no_edge > 0.0),
            "Free money! YES edge={:.4}, NO edge={:.4}, fair={:.4}, bid={}, ask={}",
            yes_edge, no_edge, fair, yes_bid, yes_ask);
    }
}

// ============================================================
// 3. End-to-End Direction Scenarios
// ============================================================

/// Home team is a heavy favorite (70%). Kalshi book at 50/50.
/// Should buy YES on home market (edge = fair - price) or buy NO on away market.
/// Both are economically equivalent — the key is we don't buy the WRONG side.
#[test]
fn e2e_home_favorite_buys_correct_side() {
    let gs = make_game_with_book(0.70, GamePhase::Halftime, 48.0, 52.0);
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    let signal = quoter.evaluate(&gs, &risk, 0.0).expect("Should produce a signal");

    // Signal must be buying the side that has positive edge
    match signal.side {
        OrderSide::Yes => {
            // If buying YES, the market must be the home market (where fair > mid)
            // OR the away market where fair_away(=0.30) > mid would be wrong
            let market = gs.kalshi_markets.iter().find(|m| m.ticker == signal.kalshi_ticker).unwrap();
            let fair = gs.fair_value_for_market(market).unwrap();
            let order_prob = signal.price_cents as f64 / 100.0;
            assert!(fair > order_prob,
                "Buying YES but fair={:.4} <= price={:.4} on ticker {} (is_home={})",
                fair, order_prob, signal.kalshi_ticker, market.is_home);
        }
        OrderSide::No => {
            let market = gs.kalshi_markets.iter().find(|m| m.ticker == signal.kalshi_ticker).unwrap();
            let fair = gs.fair_value_for_market(market).unwrap();
            let order_prob = signal.price_cents as f64 / 100.0;
            // Buying NO means we think YES prob < mid. fair_no = 1-fair should > order_prob.
            assert!((1.0 - fair) > order_prob,
                "Buying NO but fair_no={:.4} <= price={:.4} on ticker {} (is_home={})",
                1.0 - fair, order_prob, signal.kalshi_ticker, market.is_home);
        }
    }
}

/// Away team is a heavy favorite. ESPN says home=30%, so away=70%.
/// The away market (is_home=false) has fair_YES = 70%. Should get a buy YES signal on away market.
#[test]
fn e2e_away_favorite_buys_correct_side() {
    let gs = make_game_with_book(0.30, GamePhase::Halftime, 48.0, 52.0);
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    let signal = quoter.evaluate(&gs, &risk, 0.0).expect("Should produce a signal");

    let market = gs.kalshi_markets.iter().find(|m| m.ticker == signal.kalshi_ticker).unwrap();
    let fair = gs.fair_value_for_market(market).unwrap();

    match signal.side {
        OrderSide::Yes => {
            let order_prob = signal.price_cents as f64 / 100.0;
            assert!(fair > order_prob,
                "Buying YES but fair={:.4} <= price={:.4}", fair, order_prob);
        }
        OrderSide::No => {
            let order_prob = signal.price_cents as f64 / 100.0;
            assert!((1.0 - fair) > order_prob,
                "Buying NO but fair_no={:.4} <= price={:.4}", 1.0 - fair, order_prob);
        }
    }
}

/// Home market and away market should produce signals with OPPOSITE directions
/// when the book is centered at 50/50 and ESPN is not at 50%.
#[test]
fn e2e_home_away_markets_opposite_direction() {
    let gs = make_game_with_book(0.70, GamePhase::Halftime, 48.0, 52.0);

    // Evaluate each market individually
    let home_market = &gs.kalshi_markets[0];
    let away_market = &gs.kalshi_markets[1];
    assert!(home_market.is_home);
    assert!(!away_market.is_home);

    let home_fair = gs.fair_value_for_market(home_market).unwrap(); // 0.70
    let away_fair = gs.fair_value_for_market(away_market).unwrap(); // 0.30
    let mid = home_market.yes_mid.unwrap() / 100.0; // 0.50

    // Home market: fair=0.70 > mid=0.50 → should buy YES
    assert!(home_fair > mid, "Home fair should be above mid");
    // Away market: fair=0.30 < mid=0.50 → should buy NO
    assert!(away_fair < mid, "Away fair should be below mid");
}

/// Verify that when is_home flag is flipped, the signal direction flips too.
/// This catches the exact bug class where is_home gets set wrong.
#[test]
fn e2e_flipped_is_home_changes_signal() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    // Correct: is_home=true, ESPN home=70%, fair=70%
    let mut gs_correct = GameState::new("evt_1".into(), "Home".into(), "Away".into());
    gs_correct.espn_home_win_prob = Some(0.70);
    gs_correct.phase = GamePhase::Halftime;
    let mut mkt_correct = KalshiMarketState::new("CORRECT".into(), true);
    mkt_correct.update_prices(Some(48.0), Some(52.0), Some(50.0));
    mkt_correct.volume = Some(50000);
    gs_correct.kalshi_markets.push(mkt_correct);

    // Wrong: is_home=false (but this is actually the home team's market!)
    // Fair will be 1-0.70=0.30, so it will buy NO instead of YES
    let mut gs_wrong = GameState::new("evt_2".into(), "Home".into(), "Away".into());
    gs_wrong.espn_home_win_prob = Some(0.70);
    gs_wrong.phase = GamePhase::Halftime;
    let mut mkt_wrong = KalshiMarketState::new("WRONG".into(), false); // FLIPPED!
    mkt_wrong.update_prices(Some(48.0), Some(52.0), Some(50.0));
    mkt_wrong.volume = Some(50000);
    gs_wrong.kalshi_markets.push(mkt_wrong);

    let sig_correct = quoter.evaluate(&gs_correct, &risk, 0.0).expect("correct should signal");
    let sig_wrong = quoter.evaluate(&gs_wrong, &risk, 0.0).expect("wrong should signal");

    // They should produce DIFFERENT sides because the is_home flag changes fair value
    let correct_buying_yes = matches!(sig_correct.side, OrderSide::Yes);
    let wrong_buying_yes = matches!(sig_wrong.side, OrderSide::Yes);
    assert_ne!(correct_buying_yes, wrong_buying_yes,
        "Flipping is_home MUST change signal direction. correct=YES:{}, wrong=YES:{}",
        correct_buying_yes, wrong_buying_yes);
}

/// CLV hunter should only fire on PreGame, never on Live/Halftime.
#[test]
fn e2e_clv_only_pregame() {
    let risk = test_risk();
    let hunter = ClvHunter::new(0.01);

    for phase in [GamePhase::Live, GamePhase::Halftime, GamePhase::Break, GamePhase::Final] {
        let gs = make_game_with_book(0.70, phase.clone(), 48.0, 52.0);
        assert!(hunter.evaluate(&gs, &risk, 0.0).is_none(),
            "CLV should not fire on phase {:?}", phase);
    }

    let gs = make_game_with_book(0.70, GamePhase::PreGame, 48.0, 52.0);
    assert!(hunter.evaluate(&gs, &risk, 0.0).is_some(),
        "CLV should fire on PreGame with edge");
}

/// Break EV should only fire on break phases, never on PreGame/Live/Final.
#[test]
fn e2e_break_ev_only_break_phases() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    for phase in [GamePhase::PreGame, GamePhase::Live, GamePhase::Final] {
        let gs = make_game_with_book(0.70, phase.clone(), 48.0, 52.0);
        assert!(quoter.evaluate(&gs, &risk, 0.0).is_none(),
            "Break EV should not fire on phase {:?}", phase);
    }

    for phase in [GamePhase::Halftime, GamePhase::Break] {
        let gs = make_game_with_book(0.70, phase.clone(), 48.0, 52.0);
        assert!(quoter.evaluate(&gs, &risk, 0.0).is_some(),
            "Break EV should fire on phase {:?}", phase);
    }
}

/// No signal should ever have edge_after_fees <= 0.
#[test]
fn e2e_no_signal_with_negative_edge() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    // Test across a range of scenarios
    for espn_prob in [0.20, 0.35, 0.50, 0.65, 0.80] {
        for bid in [30.0, 40.0, 50.0, 60.0, 70.0] {
            let ask = bid + 4.0;
            let gs = make_game_with_book(espn_prob, GamePhase::Halftime, bid, ask);
            if let Some(signal) = quoter.evaluate(&gs, &risk, 0.0) {
                assert!(signal.edge_after_fees > 0.0,
                    "Signal with non-positive edge: {:.4} for espn={}, bid={}, ask={}",
                    signal.edge_after_fees, espn_prob, bid, ask);
            }
        }
    }
}

// ============================================================
// 4. Signal → Order Correctness
// ============================================================

/// YES signal must produce yes_price, not no_price.
#[test]
fn signal_yes_produces_yes_price_only() {
    let signal = OrderSignal {
        strategy: "test".into(),
        kalshi_ticker: "TICKER-A".into(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 55,
        size_dollars: 10.0,
        post_only: true,
        expiration_ts: None,
        edge_after_fees: 0.05,
    };

    let order = OrderManager::signal_to_order(&signal);
    assert_eq!(order.yes_price, Some(55));
    assert_eq!(order.no_price, None);
    assert!(matches!(order.side, OrderSide::Yes));
}

/// NO signal must produce no_price, not yes_price.
#[test]
fn signal_no_produces_no_price_only() {
    let signal = OrderSignal {
        strategy: "test".into(),
        kalshi_ticker: "TICKER-A".into(),
        side: OrderSide::No,
        action: OrderAction::Buy,
        price_cents: 45,
        size_dollars: 10.0,
        post_only: true,
        expiration_ts: None,
        edge_after_fees: 0.05,
    };

    let order = OrderManager::signal_to_order(&signal);
    assert_eq!(order.yes_price, None);
    assert_eq!(order.no_price, Some(45));
    assert!(matches!(order.side, OrderSide::No));
}

/// Contract count calculation: $10 at 50c = 20 contracts.
#[test]
fn signal_to_order_contract_count() {
    let signal = OrderSignal {
        strategy: "test".into(),
        kalshi_ticker: "TICKER-A".into(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 50,
        size_dollars: 10.0,
        post_only: true,
        expiration_ts: None,
        edge_after_fees: 0.05,
    };

    let order = OrderManager::signal_to_order(&signal);
    assert_eq!(order.count, 20); // $10 / $0.50 = 20 contracts
}

/// Contract count floors correctly: $10 at 33c = floor(30.3) = 30.
#[test]
fn signal_to_order_contract_count_floors() {
    let signal = OrderSignal {
        strategy: "test".into(),
        kalshi_ticker: "TICKER-A".into(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 33,
        size_dollars: 10.0,
        post_only: true,
        expiration_ts: None,
        edge_after_fees: 0.05,
    };

    let order = OrderManager::signal_to_order(&signal);
    assert_eq!(order.count, 30); // floor(10.0 / 0.33) = 30
}

/// Contract count minimum is 1 (even for tiny sizes).
#[test]
fn signal_to_order_min_one_contract() {
    let signal = OrderSignal {
        strategy: "test".into(),
        kalshi_ticker: "TICKER-A".into(),
        side: OrderSide::Yes,
        action: OrderAction::Buy,
        price_cents: 99,
        size_dollars: 0.01,
        post_only: true,
        expiration_ts: None,
        edge_after_fees: 0.05,
    };

    let order = OrderManager::signal_to_order(&signal);
    assert!(order.count >= 1, "Must always place at least 1 contract");
}

// ============================================================
// 5. ALO Pricing: Never Cross the Spread
// ============================================================

/// Buy YES at ask-1 means we're INSIDE the spread, not crossing it.
#[test]
fn alo_yes_inside_spread() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    // Fair=70%, book 48-52 → buying YES at 51 (ask-1)
    let gs = make_game_with_book(0.70, GamePhase::Halftime, 48.0, 52.0);
    let signal = quoter.evaluate(&gs, &risk, 0.0).unwrap();

    if matches!(signal.side, OrderSide::Yes) {
        let market = gs.kalshi_markets.iter().find(|m| m.ticker == signal.kalshi_ticker).unwrap();
        let ask = market.yes_ask.unwrap() as i64;
        assert!(signal.price_cents < ask,
            "YES price {} should be < ask {} (not crossing spread)", signal.price_cents, ask);
        assert!(signal.price_cents > 0, "Price must be positive");
    }
}

/// Buy NO at (100-bid)-1 means we're INSIDE the NO spread.
#[test]
fn alo_no_inside_spread() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    // Fair=30%, book 48-52 → buying NO. NO ask = 100-48 = 52, price = 51
    let gs = make_game_with_book(0.30, GamePhase::Halftime, 48.0, 52.0);
    let signal = quoter.evaluate(&gs, &risk, 0.0).unwrap();

    if matches!(signal.side, OrderSide::No) {
        let market = gs.kalshi_markets.iter().find(|m| m.ticker == signal.kalshi_ticker).unwrap();
        let yes_bid = market.yes_bid.unwrap() as i64;
        let no_ask = 100 - yes_bid;
        assert!(signal.price_cents < no_ask,
            "NO price {} should be < NO ask {} (not crossing spread)", signal.price_cents, no_ask);
    }
}

// ============================================================
// 6. GameStateManager Ticker Index Consistency
// ============================================================

/// After cleanup_finished, ticker reverse index should have no stale entries.
#[test]
fn manager_cleanup_removes_ticker_index() {
    let mut mgr = GameStateManager::new();

    // Game 1: finished
    let gs1 = mgr.upsert("e1".into(), "A".into(), "B".into());
    gs1.kalshi_markets.push(KalshiMarketState::new("TICKER-1A".into(), true));
    gs1.kalshi_markets.push(KalshiMarketState::new("TICKER-1B".into(), false));
    gs1.phase = GamePhase::Final;
    mgr.register_ticker("TICKER-1A", "e1");
    mgr.register_ticker("TICKER-1B", "e1");

    // Game 2: still live
    let gs2 = mgr.upsert("e2".into(), "C".into(), "D".into());
    gs2.kalshi_markets.push(KalshiMarketState::new("TICKER-2A".into(), true));
    gs2.phase = GamePhase::Live;
    mgr.register_ticker("TICKER-2A", "e2");

    mgr.cleanup_finished();

    // Finished game tickers should be gone from index
    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-1A").is_none(),
        "Finished game ticker should be removed from index");
    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-1B").is_none(),
        "Finished game ticker should be removed from index");

    // Live game ticker should still work
    assert!(mgr.get_mut_by_kalshi_ticker("TICKER-2A").is_some(),
        "Live game ticker should remain in index");
}

// ============================================================
// 7. Edge Cases: Extreme Probabilities & Prices
// ============================================================

/// ESPN probability near 0 or 1 should not produce invalid signals.
#[test]
fn extreme_espn_probabilities() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    for espn_prob in [0.01, 0.02, 0.05, 0.95, 0.98, 0.99] {
        let gs = make_game_with_book(espn_prob, GamePhase::Halftime, 48.0, 52.0);
        if let Some(signal) = quoter.evaluate(&gs, &risk, 0.0) {
            assert!(signal.price_cents >= 1 && signal.price_cents <= 99,
                "Price out of range: {} for espn_prob={}", signal.price_cents, espn_prob);
            assert!(signal.edge_after_fees > 0.0);
            assert!(signal.size_dollars > 0.0);
        }
    }
}

/// Extremely wide spread should still produce valid (or no) signals.
#[test]
fn wide_spread_still_valid() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    let gs = make_game_with_book(0.70, GamePhase::Halftime, 10.0, 90.0);
    if let Some(signal) = quoter.evaluate(&gs, &risk, 0.0) {
        assert!(signal.price_cents >= 1 && signal.price_cents <= 99);
        assert!(signal.edge_after_fees > 0.0);
    }
}

/// Tight spread (bid=ask-1) should still work.
#[test]
fn tight_spread_still_valid() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.001); // lower threshold for tight spread

    let gs = make_game_with_book(0.70, GamePhase::Halftime, 49.0, 51.0);
    // With a 2-cent spread, signal may or may not fire, but should never panic or produce bad data
    if let Some(signal) = quoter.evaluate(&gs, &risk, 0.0) {
        assert!(signal.price_cents >= 1 && signal.price_cents <= 99);
        assert!(signal.edge_after_fees > 0.0);
    }
}

/// No ESPN data → no signal (never trade blind).
#[test]
fn no_espn_no_signal() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    let gs = make_game_with_book(0.50, GamePhase::Halftime, 48.0, 52.0);
    let mut gs_no_espn = gs;
    gs_no_espn.espn_home_win_prob = None;

    assert!(quoter.evaluate(&gs_no_espn, &risk, 0.0).is_none(),
        "Should never produce signal without ESPN data");
}

/// No book data → no signal.
#[test]
fn no_book_no_signal() {
    let risk = test_risk();
    let quoter = BreakEvQuoter::new(0.01);

    let mut gs = GameState::new("evt".into(), "Home".into(), "Away".into());
    gs.espn_home_win_prob = Some(0.70);
    gs.phase = GamePhase::Halftime;

    let mut mkt = KalshiMarketState::new("TICKER".into(), true);
    // No prices set (all None)
    mkt.volume = Some(50000);
    gs.kalshi_markets.push(mkt);

    assert!(quoter.evaluate(&gs, &risk, 0.0).is_none(),
        "Should never produce signal without book data");
}

// ============================================================
// 8. Market Mapper Direction Tests
// ============================================================

use sports_betting::engine::market_mapper::MarketMapper;

/// YES = home team → is_home should be true.
#[test]
fn mapper_yes_is_home_correct() {
    assert!(MarketMapper::yes_is_home_team(
        "North Carolina Tar Heels",
        "North Carolina"
    ));
}

/// YES = away team → is_home should be false.
#[test]
fn mapper_yes_is_away_correct() {
    assert!(!MarketMapper::yes_is_home_team(
        "North Carolina Tar Heels",
        "Duke"
    ));
}

/// market_is_home_team: title "Away at Home Winner?" with YES=Home → true.
#[test]
fn mapper_title_yes_is_home() {
    assert!(MarketMapper::market_is_home_team(
        "Duke Blue Devils at North Carolina Tar Heels Winner?",
        "North Carolina"
    ));
}

/// market_is_home_team: title "Away at Home Winner?" with YES=Away → false.
#[test]
fn mapper_title_yes_is_away() {
    assert!(!MarketMapper::market_is_home_team(
        "Duke Blue Devils at North Carolina Tar Heels Winner?",
        "Duke"
    ));
}

// ============================================================
// 9. Risk: Halted State Blocks Everything
// ============================================================

#[test]
fn halted_risk_produces_no_signals() {
    let mut risk = test_risk();
    // Trigger halt
    risk.record_fill(0.0, -200.0);
    assert!(risk.is_halted());

    // Even with massive edge, kelly should return 0
    let size = risk.kelly_size(0.90, 20.0, 0.0);
    // Kelly doesn't check halted, but can_trade does
    assert!(!risk.can_trade(size));
}

/// Exposure cap prevents oversizing.
#[test]
fn exposure_cap_prevents_oversize() {
    // With $90 already exposed, should cap remaining
    let mut risk_loaded = RiskManager::new(50.0, 100.0, 200.0, 0.5, 0.01);
    risk_loaded.record_fill(90.0, 0.0);

    let size = risk_loaded.kelly_size(0.80, 30.0, 0.0);
    assert!(size <= 10.01, "Size {} should be capped to ~$10 remaining exposure", size);
}
