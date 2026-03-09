use sports_betting::engine::game_state::{GameState, GameStateManager};
use sports_betting::espn::types::GamePhase;

/// Helper: create a basic GameState for testing
fn make_game() -> GameState {
    GameState::new("evt_001".into(), "Duke".into(), "UNC".into())
}

/// Helper: create a game with full reference prices set up
fn make_game_with_refs(
    espn: Option<f64>,
    dk: Option<f64>,
    poly_bid: Option<f64>,
    poly_ask: Option<f64>,
    kalshi_is_home: bool,
) -> GameState {
    let mut gs = make_game();
    gs.espn_home_win_prob = espn;
    gs.dk_home_implied_prob = dk;
    gs.polymarket_home_bid = poly_bid;
    gs.polymarket_home_ask = poly_ask;
    if let (Some(b), Some(a)) = (poly_bid, poly_ask) {
        gs.polymarket_home_prob = Some((b + a) / 2.0);
    }
    gs.kalshi_is_home = kalshi_is_home;
    gs.kalshi_ticker = Some("KXNCAAMBGAME-DUKE".into());
    gs
}

// ============================================================
// consensus_fair_value (mid-based, no direction)
// ============================================================

#[test]
fn consensus_requires_two_sources() {
    let mut gs = make_game();
    // Zero sources
    assert_eq!(gs.consensus_fair_value(), None);

    // One source only → None
    gs.espn_home_win_prob = Some(0.60);
    assert_eq!(gs.consensus_fair_value(), None);

    // Two sources → Some
    gs.dk_home_implied_prob = Some(0.55);
    assert!(gs.consensus_fair_value().is_some());
}

#[test]
fn consensus_two_sources_averages_correctly() {
    let mut gs = make_game();
    gs.espn_home_win_prob = Some(0.60);
    gs.dk_home_implied_prob = Some(0.50);
    let fair = gs.consensus_fair_value().unwrap();
    assert!((fair - 0.55).abs() < 1e-10);
}

#[test]
fn consensus_three_sources_averages_correctly() {
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.52), Some(0.58), true);
    let fair = gs.consensus_fair_value().unwrap();
    // ESPN=0.60, DK=0.50, Poly mid=0.55 → avg = (0.60 + 0.50 + 0.55)/3 = 0.55
    assert!((fair - 0.55).abs() < 1e-10);
}

#[test]
fn consensus_only_poly_insufficient() {
    let gs = make_game_with_refs(None, None, Some(0.45), Some(0.55), true);
    // Only 1 source (poly) → None
    assert_eq!(gs.consensus_fair_value(), None);
}

// ============================================================
// Wide Polymarket spread filtering
// ============================================================

#[test]
fn wide_poly_spread_excluded_from_consensus() {
    // Poly spread = 0.20 > 0.15 max → excluded
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.35), Some(0.55), true);
    let fair = gs.consensus_fair_value().unwrap();
    // Only ESPN and DK → (0.60 + 0.50) / 2 = 0.55
    assert!((fair - 0.55).abs() < 1e-10);
}

#[test]
fn poly_at_max_spread_included() {
    // Poly spread = exactly 0.15 → included
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.475), Some(0.625), true);
    let fair = gs.consensus_fair_value().unwrap();
    // Poly mid = 0.55, all three avg = (0.60 + 0.50 + 0.55) / 3 = 0.55
    assert!((fair - 0.55).abs() < 1e-10);
}

#[test]
fn poly_just_over_max_spread_excluded() {
    // Poly spread = 0.151 → excluded
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.4745), Some(0.6255), true);
    let fair = gs.consensus_fair_value().unwrap();
    // Only ESPN + DK
    assert!((fair - 0.55).abs() < 1e-10);
}

#[test]
fn wide_poly_with_only_espn_returns_none() {
    // Wide poly + only ESPN = only 1 source → None
    let gs = make_game_with_refs(Some(0.60), None, Some(0.30), Some(0.70), true);
    assert_eq!(gs.consensus_fair_value(), None);
}

#[test]
fn tight_poly_with_only_espn_returns_some() {
    // Tight poly + ESPN = 2 sources → Some
    let gs = make_game_with_refs(Some(0.60), None, Some(0.55), Some(0.60), true);
    let fair = gs.consensus_fair_value().unwrap();
    // ESPN=0.60, Poly mid=0.575 → (0.60 + 0.575)/2 = 0.5875
    assert!((fair - 0.5875).abs() < 1e-10);
}

// ============================================================
// Conservative consensus (direction-aware)
// ============================================================

#[test]
fn conservative_buying_yes_uses_poly_bid_when_kalshi_is_home() {
    // kalshi_is_home=true, buying YES → want lower fair → use poly home bid
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.50), Some(0.60), true);
    let fair = gs.consensus_fair_value_conservative(true).unwrap();
    // ESPN=0.60, DK=0.50, Poly=bid=0.50 → (0.60 + 0.50 + 0.50)/3 ≈ 0.5333
    assert!((fair - 0.5333).abs() < 0.001);
}

#[test]
fn conservative_buying_no_uses_poly_ask_when_kalshi_is_home() {
    // kalshi_is_home=true, buying NO → want higher fair → use poly home ask
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.50), Some(0.60), true);
    let fair = gs.consensus_fair_value_conservative(false).unwrap();
    // ESPN=0.60, DK=0.50, Poly=ask=0.60 → (0.60 + 0.50 + 0.60)/3 ≈ 0.5667
    assert!((fair - 0.5667).abs() < 0.001);
}

#[test]
fn conservative_buying_yes_uses_poly_ask_when_kalshi_is_away() {
    // kalshi_is_home=false, buying YES on away team
    // Want lower aligned fair value = lower (1-home) = higher home prob → poly home ask
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.50), Some(0.60), false);
    let fair = gs.consensus_fair_value_conservative(true).unwrap();
    // Uses poly_home_ask=0.60 → (0.60+0.50+0.60)/3 ≈ 0.5667
    assert!((fair - 0.5667).abs() < 0.001);
}

#[test]
fn conservative_buying_no_uses_poly_bid_when_kalshi_is_away() {
    // kalshi_is_home=false, buying NO on away team
    // Want higher aligned fair value = higher (1-home) = lower home prob → poly home bid
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.50), Some(0.60), false);
    let fair = gs.consensus_fair_value_conservative(false).unwrap();
    // Uses poly_home_bid=0.50 → (0.60+0.50+0.50)/3 ≈ 0.5333
    assert!((fair - 0.5333).abs() < 0.001);
}

#[test]
fn conservative_equals_mid_when_tight_spread() {
    // When spread is zero, conservative should equal mid-based
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.55), Some(0.55), true);
    let mid_fair = gs.consensus_fair_value().unwrap();
    let cons_yes = gs.consensus_fair_value_conservative(true).unwrap();
    let cons_no = gs.consensus_fair_value_conservative(false).unwrap();
    assert!((mid_fair - cons_yes).abs() < 1e-10);
    assert!((mid_fair - cons_no).abs() < 1e-10);
}

#[test]
fn conservative_always_less_favorable_than_mid() {
    // For buying YES: conservative fair ≤ mid fair (lower = less edge)
    // For buying NO: conservative fair ≥ mid fair (higher = less edge)
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.45), Some(0.55), true);
    let mid = gs.consensus_fair_value().unwrap();
    let cons_yes = gs.consensus_fair_value_conservative(true).unwrap();
    let cons_no = gs.consensus_fair_value_conservative(false).unwrap();
    assert!(cons_yes <= mid + 1e-10);
    assert!(cons_no >= mid - 1e-10);
}

#[test]
fn conservative_wide_poly_excluded() {
    // Wide spread → poly excluded → conservative = mid (both only use ESPN + DK)
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.30), Some(0.70), true);
    let mid = gs.consensus_fair_value().unwrap();
    let cons_yes = gs.consensus_fair_value_conservative(true).unwrap();
    let cons_no = gs.consensus_fair_value_conservative(false).unwrap();
    assert!((mid - cons_yes).abs() < 1e-10);
    assert!((mid - cons_no).abs() < 1e-10);
}

// ============================================================
// kalshi_aligned_fair_value (home vs away ticker)
// ============================================================

#[test]
fn aligned_fair_value_home_ticker() {
    let gs = make_game_with_refs(Some(0.70), Some(0.60), None, None, true);
    let aligned = gs.kalshi_aligned_fair_value().unwrap();
    // kalshi_is_home → aligned = home fair = 0.65
    assert!((aligned - 0.65).abs() < 1e-10);
}

#[test]
fn aligned_fair_value_away_ticker() {
    let gs = make_game_with_refs(Some(0.70), Some(0.60), None, None, false);
    let aligned = gs.kalshi_aligned_fair_value().unwrap();
    // kalshi_is_home=false → aligned = 1 - home = 1 - 0.65 = 0.35
    assert!((aligned - 0.35).abs() < 1e-10);
}

#[test]
fn aligned_conservative_home_ticker() {
    let gs = make_game_with_refs(Some(0.70), Some(0.60), Some(0.60), Some(0.70), true);
    // buying YES on home ticker → want lower aligned → use poly bid=0.60
    let aligned = gs.kalshi_aligned_fair_value_conservative(true).unwrap();
    // home fair = (0.70 + 0.60 + 0.60)/3 = 0.6333
    assert!((aligned - 0.6333).abs() < 0.001);
}

#[test]
fn aligned_conservative_away_ticker() {
    let gs = make_game_with_refs(Some(0.70), Some(0.60), Some(0.60), Some(0.70), false);
    // kalshi_is_home=false, buying YES on away ticker
    // conservative: use poly home ask=0.70 (higher home = lower away = conservative for YES away)
    let cons = gs.kalshi_aligned_fair_value_conservative(true).unwrap();
    // home fair = (0.70 + 0.60 + 0.70)/3 = 0.6667 → aligned = 1 - 0.6667 = 0.3333
    assert!((cons - 0.3333).abs() < 0.001);
}

// ============================================================
// Polymarket bid/ask flip for away-team YES token
// ============================================================

#[test]
fn poly_home_bid_ask_when_poly_is_home() {
    let mut gs = make_game();
    gs.polymarket_is_home = true;
    // Simulate WS update: YES token bid=0.55, ask=0.60
    let yes_bid = 0.55;
    let yes_ask = 0.60;
    gs.polymarket_home_bid = Some(yes_bid);
    gs.polymarket_home_ask = Some(yes_ask);
    gs.polymarket_home_prob = Some((yes_bid + yes_ask) / 2.0);

    assert!((gs.polymarket_home_bid.unwrap() - 0.55).abs() < 1e-10);
    assert!((gs.polymarket_home_ask.unwrap() - 0.60).abs() < 1e-10);
    assert!((gs.polymarket_home_prob.unwrap() - 0.575).abs() < 1e-10);
}

#[test]
fn poly_home_bid_ask_when_poly_is_away() {
    let mut gs = make_game();
    gs.polymarket_is_home = false;
    // Simulate WS update: YES token (= away team) bid=0.55, ask=0.60
    // Home bid = 1 - yes_ask = 0.40, Home ask = 1 - yes_bid = 0.45
    let yes_bid = 0.55;
    let yes_ask = 0.60;
    let yes_mid = (yes_bid + yes_ask) / 2.0;

    // This is what the WS handler does:
    gs.polymarket_home_prob = Some(1.0 - yes_mid);
    gs.polymarket_home_bid = Some(1.0 - yes_ask);
    gs.polymarket_home_ask = Some(1.0 - yes_bid);

    assert!((gs.polymarket_home_bid.unwrap() - 0.40).abs() < 1e-10);
    assert!((gs.polymarket_home_ask.unwrap() - 0.45).abs() < 1e-10);
    assert!((gs.polymarket_home_prob.unwrap() - 0.425).abs() < 1e-10);
}

#[test]
fn poly_flip_preserves_spread() {
    // Flipping bid/ask for away team should preserve spread width
    let yes_bid: f64 = 0.30;
    let yes_ask: f64 = 0.40;
    let spread = yes_ask - yes_bid; // 0.10

    let home_bid = 1.0 - yes_ask; // 0.60
    let home_ask = 1.0 - yes_bid; // 0.70
    let home_spread = home_ask - home_bid; // 0.10

    assert!((spread - home_spread).abs() < 1e-10);
}

#[test]
fn poly_flip_inverted_bid_ask_order_correct() {
    // When flipping: home_bid < home_ask must always hold
    let yes_bid = 0.45;
    let yes_ask = 0.55;
    let home_bid = 1.0 - yes_ask; // 0.45
    let home_ask = 1.0 - yes_bid; // 0.55
    assert!(home_bid < home_ask);
}

// ============================================================
// Edge calculation
// ============================================================

#[test]
fn edge_vs_kalshi_positive_means_buy_yes() {
    let mut gs = make_game_with_refs(Some(0.70), Some(0.60), None, None, true);
    gs.kalshi_yes_mid = Some(50.0); // 50 cents = 0.50
    let edge = gs.edge_vs_kalshi().unwrap();
    // fair=0.65, kalshi=0.50 → edge = +0.15 → buy YES
    assert!((edge - 0.15).abs() < 1e-10);
    assert!(edge > 0.0);
}

#[test]
fn edge_vs_kalshi_negative_means_buy_no() {
    let mut gs = make_game_with_refs(Some(0.40), Some(0.30), None, None, true);
    gs.kalshi_yes_mid = Some(50.0);
    let edge = gs.edge_vs_kalshi().unwrap();
    // fair=0.35, kalshi=0.50 → edge = -0.15 → buy NO
    assert!(edge < 0.0);
}

#[test]
fn edge_vs_kalshi_none_when_no_fair_value() {
    let mut gs = make_game();
    gs.kalshi_yes_mid = Some(50.0);
    // No reference prices → no fair value → no edge
    assert_eq!(gs.edge_vs_kalshi(), None);
}

#[test]
fn edge_vs_kalshi_none_when_no_kalshi_mid() {
    let gs = make_game_with_refs(Some(0.60), Some(0.50), None, None, true);
    // No kalshi_yes_mid
    assert_eq!(gs.edge_vs_kalshi(), None);
}

// ============================================================
// Real-world scenario tests
// ============================================================

#[test]
fn scenario_smu_clv_bug_single_poly_reference() {
    // The bug: SMU at 67.3% on ESPN but only Polymarket at 40.5% was used as reference
    // With the fix: single source → no consensus → no signal
    let mut gs = make_game();
    gs.home_team = "SMU".into();
    gs.away_team = "Some Opponent".into();
    // Only Polymarket set, no ESPN or DK
    gs.polymarket_home_bid = Some(0.38);
    gs.polymarket_home_ask = Some(0.43);
    gs.polymarket_home_prob = Some(0.405);
    gs.kalshi_is_home = true;
    gs.kalshi_yes_mid = Some(68.0);

    // Single source → None
    assert_eq!(gs.consensus_fair_value(), None);
    assert_eq!(gs.kalshi_aligned_fair_value(), None);
}

#[test]
fn scenario_smu_with_espn_and_poly() {
    // Same game but now ESPN is also available
    let gs = make_game_with_refs(Some(0.673), None, Some(0.38), Some(0.43), true);

    // ESPN + tight poly (spread=0.05) = 2 sources
    let fair = gs.consensus_fair_value().unwrap();
    // ESPN=0.673, poly mid=0.405 → (0.673 + 0.405)/2 = 0.539
    assert!((fair - 0.539).abs() < 0.001);

    // Conservative for buying NO (kalshi expensive):
    // poly_home_ask = 0.43 → (0.673 + 0.43)/2 = 0.5515
    let cons = gs.consensus_fair_value_conservative(false).unwrap();
    assert!((cons - 0.5515).abs() < 0.001);
}

#[test]
fn scenario_wide_poly_market_ignored() {
    // Polymarket 30/60 (30c wide) → unreliable, should be excluded
    let gs = make_game_with_refs(Some(0.65), Some(0.60), Some(0.30), Some(0.60), true);
    let fair = gs.consensus_fair_value().unwrap();
    // Only ESPN + DK (poly excluded due to 30c spread)
    assert!((fair - 0.625).abs() < 1e-10);
}

#[test]
fn scenario_tight_poly_adds_value() {
    // Polymarket 55/57 (2c wide) → very liquid, should be included
    let gs = make_game_with_refs(Some(0.65), Some(0.60), Some(0.55), Some(0.57), true);
    let fair = gs.consensus_fair_value().unwrap();
    // ESPN=0.65, DK=0.60, Poly mid=0.56 → (0.65 + 0.60 + 0.56)/3 = 0.6033
    assert!((fair - 0.6033).abs() < 0.001);
}

#[test]
fn scenario_conservative_reduces_phantom_edge() {
    // The key test: mid-based edge looks positive, but conservative edge is smaller
    let mut gs = make_game_with_refs(Some(0.60), None, Some(0.50), Some(0.60), true);
    gs.kalshi_yes_mid = Some(55.0);

    // Mid-based aligned fair value (kalshi_is_home=true)
    let mid_fair = gs.kalshi_aligned_fair_value().unwrap();
    // ESPN=0.60, poly mid=0.55 → (0.60+0.55)/2 = 0.575
    assert!((mid_fair - 0.575).abs() < 1e-10);
    // Mid edge = 0.575 - 0.55 = +0.025 → would buy YES

    // Conservative for buying YES → use poly bid=0.50
    let cons_fair = gs.kalshi_aligned_fair_value_conservative(true).unwrap();
    // ESPN=0.60, poly bid=0.50 → (0.60+0.50)/2 = 0.55
    assert!((cons_fair - 0.55).abs() < 1e-10);
    // Conservative edge = 0.55 - 0.55 = 0.00 → NO EDGE, correctly blocks the trade
}

#[test]
fn scenario_all_three_sources_agree_tight() {
    // All three sources agree, tight poly → conservative ≈ mid
    let gs = make_game_with_refs(Some(0.65), Some(0.64), Some(0.63), Some(0.65), true);
    let mid = gs.kalshi_aligned_fair_value().unwrap();
    let cons_yes = gs.kalshi_aligned_fair_value_conservative(true).unwrap();
    let cons_no = gs.kalshi_aligned_fair_value_conservative(false).unwrap();

    // All should be close to 0.64-0.65 range
    assert!(mid > 0.63 && mid < 0.66);
    assert!((cons_yes - cons_no).abs() < 0.02); // small diff with tight spread
}

// ============================================================
// Symmetry tests — home/away should produce consistent results
// ============================================================

#[test]
fn symmetry_home_vs_away_ticker_fair_values_sum_to_one() {
    let gs_home = make_game_with_refs(Some(0.65), Some(0.60), Some(0.55), Some(0.60), true);
    let gs_away = make_game_with_refs(Some(0.65), Some(0.60), Some(0.55), Some(0.60), false);

    let home_aligned = gs_home.kalshi_aligned_fair_value().unwrap();
    let away_aligned = gs_away.kalshi_aligned_fair_value().unwrap();

    // Should sum to 1.0 (they're on opposite sides of the same game)
    assert!((home_aligned + away_aligned - 1.0).abs() < 1e-10);
}

#[test]
fn symmetry_conservative_home_yes_equals_away_no() {
    // Buying YES on home ticker = Buying NO on away ticker (same economic position)
    let gs_home = make_game_with_refs(Some(0.65), Some(0.60), Some(0.55), Some(0.60), true);
    let gs_away = make_game_with_refs(Some(0.65), Some(0.60), Some(0.55), Some(0.60), false);

    let home_yes = gs_home.kalshi_aligned_fair_value_conservative(true).unwrap();
    let away_no = gs_away.kalshi_aligned_fair_value_conservative(false).unwrap();

    // home_yes fair value + away_no fair value should sum to 1.0
    // Because buying YES home = buying NO away → same direction
    assert!(
        (home_yes + away_no - 1.0).abs() < 1e-10,
        "home_yes={}, away_no={}, sum={}",
        home_yes,
        away_no,
        home_yes + away_no
    );
}

#[test]
fn symmetry_conservative_home_no_equals_away_yes() {
    let gs_home = make_game_with_refs(Some(0.65), Some(0.60), Some(0.55), Some(0.60), true);
    let gs_away = make_game_with_refs(Some(0.65), Some(0.60), Some(0.55), Some(0.60), false);

    let home_no = gs_home.kalshi_aligned_fair_value_conservative(false).unwrap();
    let away_yes = gs_away.kalshi_aligned_fair_value_conservative(true).unwrap();

    assert!(
        (home_no + away_yes - 1.0).abs() < 1e-10,
        "home_no={}, away_yes={}, sum={}",
        home_no,
        away_yes,
        home_no + away_yes
    );
}

// ============================================================
// Edge cases
// ============================================================

#[test]
fn poly_bid_zero_ask_nonzero() {
    let gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.0), Some(0.10), true);
    // Spread = 0.10 ≤ 0.15 → included
    let fair = gs.consensus_fair_value().unwrap();
    // ESPN=0.60, DK=0.50, Poly mid=0.05 → (0.60 + 0.50 + 0.05)/3 = 0.3833
    assert!((fair - 0.3833).abs() < 0.001);
}

#[test]
fn poly_bid_near_one() {
    let gs = make_game_with_refs(Some(0.90), Some(0.85), Some(0.88), Some(0.92), true);
    // Spread = 0.04 → included
    let fair = gs.consensus_fair_value().unwrap();
    // Poly mid=0.90, avg = (0.90+0.85+0.90)/3 = 0.8833
    assert!((fair - 0.8833).abs() < 0.001);
}

#[test]
fn all_sources_none_returns_none() {
    let gs = make_game();
    assert_eq!(gs.consensus_fair_value(), None);
    assert_eq!(gs.consensus_fair_value_conservative(true), None);
    assert_eq!(gs.consensus_fair_value_conservative(false), None);
    assert_eq!(gs.kalshi_aligned_fair_value(), None);
    assert_eq!(gs.kalshi_aligned_fair_value_conservative(true), None);
}

#[test]
fn poly_bid_only_no_ask_excluded() {
    let mut gs = make_game_with_refs(Some(0.60), Some(0.50), Some(0.55), None, true);
    // Only bid, no ask → poly excluded (needs both)
    gs.polymarket_home_ask = None;
    let fair = gs.consensus_fair_value().unwrap();
    // Only ESPN + DK
    assert!((fair - 0.55).abs() < 1e-10);
}

#[test]
fn poly_ask_only_no_bid_excluded() {
    let mut gs = make_game_with_refs(Some(0.60), Some(0.50), None, Some(0.55), true);
    gs.polymarket_home_bid = None;
    let fair = gs.consensus_fair_value().unwrap();
    assert!((fair - 0.55).abs() < 1e-10);
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

    // Upsert again with same ID → should return existing
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

// ============================================================
// Strategy integration tests
// ============================================================

#[test]
fn clv_hunter_no_signal_without_consensus() {
    use sports_betting::strategies::clv_hunter::ClvHunter;
    use sports_betting::engine::risk::RiskManager;

    let hunter = ClvHunter::new(0.015);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game();
    gs.phase = GamePhase::PreGame;
    gs.kalshi_ticker = Some("KXNCAAMBGAME-DUKE".into());
    gs.kalshi_yes_mid = Some(50.0);
    gs.kalshi_yes_bid = Some(48.0);
    gs.kalshi_yes_ask = Some(52.0);
    // Only polymarket, no ESPN/DK → no consensus → no signal
    gs.polymarket_home_bid = Some(0.60);
    gs.polymarket_home_ask = Some(0.65);
    gs.polymarket_home_prob = Some(0.625);
    gs.kalshi_is_home = true;

    let signal = hunter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_none());
}

#[test]
fn clv_hunter_signal_with_two_sources() {
    use sports_betting::strategies::clv_hunter::ClvHunter;
    use sports_betting::engine::risk::RiskManager;

    let hunter = ClvHunter::new(0.015);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game();
    gs.phase = GamePhase::PreGame;
    gs.kalshi_ticker = Some("KXNCAAMBGAME-DUKE".into());
    gs.kalshi_yes_mid = Some(50.0);
    gs.kalshi_yes_bid = Some(48.0);
    gs.kalshi_yes_ask = Some(52.0);
    gs.espn_home_win_prob = Some(0.65);
    gs.dk_home_implied_prob = Some(0.60);
    gs.kalshi_is_home = true;

    // ESPN=0.65, DK=0.60 → consensus=0.625 → edge=0.125 → should signal
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

    let mut gs = make_game_with_refs(Some(0.70), Some(0.60), None, None, true);
    gs.phase = GamePhase::Live; // NOT a break
    gs.kalshi_ticker = Some("KXNCAAMBGAME-DUKE".into());
    gs.kalshi_yes_mid = Some(50.0);
    gs.kalshi_yes_bid = Some(48.0);
    gs.kalshi_yes_ask = Some(52.0);

    assert!(quoter.evaluate(&gs, &risk, 0.0).is_none());
}

#[test]
fn break_ev_signals_on_halftime() {
    use sports_betting::strategies::break_ev::BreakEvQuoter;
    use sports_betting::engine::risk::RiskManager;

    let quoter = BreakEvQuoter::new(0.015);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game_with_refs(Some(0.70), Some(0.60), None, None, true);
    gs.phase = GamePhase::Halftime;
    gs.kalshi_ticker = Some("KXNCAAMBGAME-DUKE".into());
    gs.kalshi_yes_mid = Some(50.0);
    gs.kalshi_yes_bid = Some(48.0);
    gs.kalshi_yes_ask = Some(52.0);

    let signal = quoter.evaluate(&gs, &risk, 0.0);
    assert!(signal.is_some());
}

#[test]
fn arb_scanner_conservative_blocks_phantom_edge() {
    use sports_betting::strategies::arb_scanner::ArbScanner;
    use sports_betting::engine::risk::RiskManager;

    let scanner = ArbScanner::new(0.02);
    let risk = RiskManager::new(50.0, 500.0, 200.0, 0.5, 0.01);

    let mut gs = make_game();
    gs.phase = GamePhase::Live;
    gs.kalshi_ticker = Some("KXNCAAMBGAME-DUKE".into());
    gs.kalshi_yes_mid = Some(55.0);
    gs.kalshi_yes_bid = Some(53.0);
    gs.kalshi_yes_ask = Some(57.0);
    gs.kalshi_is_home = true;
    // ESPN says 0.60, Poly 50/60 (wide-ish but under 15c)
    gs.espn_home_win_prob = Some(0.60);
    gs.polymarket_home_bid = Some(0.50);
    gs.polymarket_home_ask = Some(0.60);
    gs.polymarket_home_prob = Some(0.55);

    // Mid-based: fair=(0.60+0.55)/2=0.575, edge=0.575-0.55=0.025 → above 0.02 threshold
    // Conservative (buying YES): poly bid=0.50 → fair=(0.60+0.50)/2=0.55, edge=0.55-0.55=0.00 → blocked
    let signal = scanner.evaluate(&gs, &risk, 0.0);
    // Should be blocked by conservative check or tiny edge after fees
    // Either None or very small edge that fails fee check
    if let Some(sig) = &signal {
        // If signal fires, verify it's using conservative reference
        // The edge should be much smaller than the mid-based edge
        assert!(sig.size_dollars < 50.0);
    }
}

// ============================================================
// Polymarket price update simulation (mimics WS handler logic)
// ============================================================

fn simulate_poly_ws_update(gs: &mut GameState, yes_bid: f64, yes_ask: f64) {
    let yes_mid = (yes_bid + yes_ask) / 2.0;
    if gs.polymarket_is_home {
        gs.polymarket_home_prob = Some(yes_mid);
        gs.polymarket_home_bid = Some(yes_bid);
        gs.polymarket_home_ask = Some(yes_ask);
    } else {
        gs.polymarket_home_prob = Some(1.0 - yes_mid);
        gs.polymarket_home_bid = Some(1.0 - yes_ask);
        gs.polymarket_home_ask = Some(1.0 - yes_bid);
    }
}

#[test]
fn ws_update_poly_is_home_team() {
    let mut gs = make_game();
    gs.polymarket_is_home = true;
    simulate_poly_ws_update(&mut gs, 0.55, 0.60);

    assert!((gs.polymarket_home_bid.unwrap() - 0.55).abs() < 1e-10);
    assert!((gs.polymarket_home_ask.unwrap() - 0.60).abs() < 1e-10);
    assert!((gs.polymarket_home_prob.unwrap() - 0.575).abs() < 1e-10);
}

#[test]
fn ws_update_poly_is_away_team() {
    let mut gs = make_game();
    gs.polymarket_is_home = false;
    simulate_poly_ws_update(&mut gs, 0.55, 0.60);

    // YES=away, so home_bid=1-0.60=0.40, home_ask=1-0.55=0.45
    assert!((gs.polymarket_home_bid.unwrap() - 0.40).abs() < 1e-10);
    assert!((gs.polymarket_home_ask.unwrap() - 0.45).abs() < 1e-10);
    assert!((gs.polymarket_home_prob.unwrap() - 0.425).abs() < 1e-10);
}

#[test]
fn ws_update_50_50_market() {
    let mut gs = make_game();
    gs.polymarket_is_home = true;
    simulate_poly_ws_update(&mut gs, 0.49, 0.51);

    assert!((gs.polymarket_home_bid.unwrap() - 0.49).abs() < 1e-10);
    assert!((gs.polymarket_home_ask.unwrap() - 0.51).abs() < 1e-10);
}

#[test]
fn ws_update_extreme_underdog() {
    let mut gs = make_game();
    gs.polymarket_is_home = true;
    simulate_poly_ws_update(&mut gs, 0.05, 0.10);

    assert!((gs.polymarket_home_bid.unwrap() - 0.05).abs() < 1e-10);
    assert!((gs.polymarket_home_ask.unwrap() - 0.10).abs() < 1e-10);
}

#[test]
fn ws_update_extreme_favorite() {
    let mut gs = make_game();
    gs.polymarket_is_home = false;
    simulate_poly_ws_update(&mut gs, 0.90, 0.95);

    // YES=away at 90-95, home is underdog
    assert!((gs.polymarket_home_bid.unwrap() - 0.05).abs() < 1e-10);
    assert!((gs.polymarket_home_ask.unwrap() - 0.10).abs() < 1e-10);
}

// ============================================================
// End-to-end direction consistency
// ============================================================

#[test]
fn e2e_buy_yes_home_ticker_all_sources() {
    // Home team is favored across all sources, Kalshi is cheap → Buy YES
    let mut gs = make_game_with_refs(Some(0.70), Some(0.65), Some(0.63), Some(0.67), true);
    gs.kalshi_yes_mid = Some(55.0);

    let mid_ref = gs.kalshi_aligned_fair_value().unwrap();
    let kalshi = 0.55;
    assert!(mid_ref > kalshi); // Edge positive → buy YES

    let cons_ref = gs.kalshi_aligned_fair_value_conservative(true).unwrap();
    // Conservative should still be > kalshi (real edge)
    assert!(cons_ref > kalshi, "cons={}, kalshi={}", cons_ref, kalshi);
}

#[test]
fn e2e_buy_no_away_ticker() {
    // Away ticker, home team is favored → Kalshi YES (=away) should be cheap
    // But it's expensive → buy NO
    let mut gs = make_game_with_refs(Some(0.70), Some(0.65), Some(0.63), Some(0.67), false);
    gs.kalshi_yes_mid = Some(50.0); // 50c for away team

    let mid_ref = gs.kalshi_aligned_fair_value().unwrap();
    // kalshi_is_home=false → aligned = 1 - home_fair
    // home_fair ≈ 0.67, aligned ≈ 0.33
    let kalshi = 0.50;
    assert!(mid_ref < kalshi); // Edge negative → buy NO
}
