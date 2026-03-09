# Sports Betting Bot — TODO

## Priority 1: Correctness

- [x] **1. Fix two-market-per-game structure** — Store both tickers per game, add `yes_sub_title` to Market struct, evaluate both markets and pick best edge.
- [x] **2. Simplify fair value to ESPN-only** — Drop DK implied prob and Polymarket from consensus.
- [x] **3. Fix edge calculation** — Edge = `espn_fair - order_price`, not `espn_fair - kalshi_mid`.
- [x] **4. Fix ALO pricing** — Price at `ask - 1` (most aggressive passive) instead of `bid + 1`.
- [x] **5. Wire up PnL tracking from fills** — `risk.record_fill()` now called on fill events.
- [x] **6. Cancel stale orders with break-duration TTL** — Orders cancel when break ends or after configurable TTL (default 120s). `order_ttl_secs` in config.

## Priority 2: Signal Quality

- [x] **7. Make volume/price filters configurable** — `min_volume`, `min_price_cents`, `max_price_cents` in `[strategy]` config.
- [x] **8. Log near-miss signals** — `tracing::debug!` for near-miss (positive edge below threshold) and fees-eaten scenarios.
- [x] **9. CLV validation** — Pre-game orders compared to closing mid at game start, logged and stored in `clv_checks` table.

## Priority 3: Operational

- [ ] **Push + deploy** — unpushed commits on main.
- [ ] **Alerting** — notify on fills, loss limit hits, crashes (Slack/email/etc).
- [ ] **Graceful shutdown** — cancel resting orders on SIGTERM/SIGINT.
- [x] **Mid-session market discovery** — Re-fetches Kalshi events every 5 min, maps new ones, subscribes on live WS.

## Priority 4: Future Features

- [ ] **Spreads/totals on Polymarket** — currently moneyline only.
- [ ] **Taker orders for large dislocations** — currently passive only.
- [ ] **PnL snapshot logging** — table exists but never populated.
- [ ] **Performance dashboard** — query SQLite for fill history, win rate, edge captured.
