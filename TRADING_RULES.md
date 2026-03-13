# Trading Rules & Strategy Logic

This document defines the trading rules for the sports betting bot. All code changes to strategy logic must conform to these rules.

## Core Philosophy

- **Every trade must be +EV.** We never trade for non-edge reasons (settlement risk, position cleanup, etc.).
- **Passive only.** All orders use `post_only: true` (maker fees 1.75% vs 7% taker).
- **ESPN/DK as fair value.** Edge = divergence between ESPN/DK probability and Kalshi market price.

## When We Trade

| Game Phase | Opens (ADD) | Closes | Notes |
|---|---|---|---|
| **PreGame** | Yes, if edge > `pregame_min_edge` | Yes, if close edge > 0 | Expires at game start |
| **Break / Halftime** | Yes, if edge > `break_min_edge` | Yes, if close edge > 0 | Expires at break end. Blocked if <30s remain in break. |
| **Live** | **No** | **No** | No trades during live play. Period. |
| **Final 5 min (any phase)** | **No** | **No** | `can_evaluate()` returns false. Positions ride to settlement. |

## Edge Calculation

1. **Direction**: `buying_yes = fair_value > book_midpoint`
2. **ALO price**: Most aggressive passive price (post inside the spread)
   - Buy YES: `yes_ask - 1`
   - Buy NO: `(100 - yes_bid) - 1`
3. **Raw edge**: `fair_value - order_price` (for the side being bought)
4. **Edge after fees**: `raw_edge - maker_fee_per_contract`

## Position Sizing: Target-Position Model

### ADD orders (Case A: edge exists above threshold)

- `target = floor((edge_after_fees - min_edge) * 100 * contracts_per_pct_edge)`, minimum 1
- `delta = target - effective_net_for_market` (game-level, cross-ticker aware)
- Only add if delta has same sign as target (we're short of target)
- `contracts = min(delta.abs(), max_contracts_per_order)`
- Anti-scalp: skip if `delta.abs() < min_trade_contracts`

### CLOSE orders (Case B: no edge in position direction, but game-level exposure exists)

- Triggered when `target == 0` AND `net_game_aligned != 0`
- **Only closes when close-direction edge > 0 (after fees).** Never sends negative-edge close orders.
- **Closes as much as possible**: `contracts = min(exposure, max_contracts_per_order)`
- No edge-scaling on close size — if it's +EV to close, close the full position (up to per-order cap)
- The `has_resting_order` dedup naturally creates drip behavior for positions larger than `max_contracts_per_order`

### Cross-Ticker Closing

- Same-game tickers are treated as equivalent (YES-DUKE = NO-UNC)
- Close signals are evaluated on ALL tickers in the game, not just the one where position is held
- `evaluate_edge` picks the best signal across all markets, so the best close price wins
- Example: Hold 10 YES-DUKE, edge disappears. Both NO-DUKE and YES-UNC are evaluated for closing. Whichever has better close edge is selected.
- Cross-ticker closes may open a new position on a different ticker (e.g., buy YES-UNC to offset YES-DUKE). This ties up additional capital but is economically equivalent.

### Case C: No edge, no exposure — no signal

## Order Safety

- **Per-order cap**: `max_contracts_per_order` (default 30). Combined with `has_resting_order` dedup, creates TWAP-like drip behavior.
- **Per-game cap**: `max_contracts_per_game` — sum of (position + resting) across all game tickers.
- **Resting order dedup**: Same ticker with a resting order blocks new signals (add or close).
- **Break order expiration**: Computed once per break entry. TV timeouts ~90s, halftime ~840s.
- **Late-break cutoff**: ADD orders blocked when <30s remain in break.
- **REST book cross-check** (break orders): Fresh REST fetch before placement. If drift >= 3c from WS book, re-evaluates edge. Skips if edge disappears.
- **Break→Live cancellation**: ALL resting orders cancelled when ESPN detects break ending.

## What We Don't Do

- **No live trading.** No opens or closes during live play.
- **No forced unwinds.** No settlement-risk closes, no final-minute emergency dumps.
- **No negative-edge closes.** If the market doesn't give us a +EV exit, we hold.
- **No edge-scaled close sizing.** If closing is +EV, we close as much as possible.
- **No taker orders.** Everything is post-only.
