# Sports Betting Bot

Rust-based automated CBB (college basketball) betting bot on Kalshi. Uses ESPN + DraftKings as reference prices, Polymarket as secondary reference.

## Agent Instructions

**IMPORTANT**: If you notice anything in this file that is wrong, outdated, or missing — update it immediately. This file must stay in sync with the actual codebase. When you add new files, change config fields, fix bugs, or alter behavior, update the relevant sections here before finishing your task. Future agents depend on this file being accurate.

**IMPORTANT**: Do NOT run the bot locally unless explicitly asked. The bot runs on the DigitalOcean droplet. Local builds and tests are fine, but never `cargo run` locally.

## Quick Reference

- **Build**: `cargo build` (or `cargo build --release`)
- **Run**: `cargo run` (reads `config.toml` from cwd)
- **Test**: `cargo test`
- **Lint**: `cargo clippy`
- **Single binary**: `cargo run --bin cancel_orders` or `cargo run --bin debug_kalshi`
- **Rust edition**: 2024

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for detailed diagrams of data flow, strategies, and order lifecycle.

### Module Layout

```
src/
├── main.rs              — Event loop (tokio::select!), startup, WS connections
├── lib.rs               — Re-exports for multi-binary support
├── config.rs            — TOML config deserialization
├── bin/
│   ├── cancel_orders.rs — Utility to cancel all resting orders
│   └── debug_kalshi.rs  — Utility to debug Kalshi API/markets
├── engine/
│   ├── bot.rs           — BotState, SharedState, SharedOrderBooks, SharedBreakLog, SharedLogger, SharedMapper types
│   ├── game_state.rs    — GameState, KalshiMarketState, GameStateManager
│   ├── market_mapper.rs — Fuzzy team matching + LLM fallback + direction flags
│   ├── market_prep.rs   — Shared fetch/filter/build helpers, BookPrices
│   ├── order_manager.rs — Order tracking, dedup, fill sync watermark
│   ├── risk.rs          — Kelly sizing, position limits, fee calc
│   ├── executor.rs      — Signal → Kalshi REST order placement
│   ├── logger.rs        — SQLite trade logging
│   ├── notifier.rs      — Telegram push notifications
│   ├── dashboard.rs     — Web dashboard (axum, port 3030)
│   └── handlers/
│       ├── mod.rs           — Re-exports + handle_maintenance_tick() orchestration
│       ├── scoreboard.rs    — ESPN poll → strategy evaluation → order signals
│       ├── kalshi_ws.rs     — Kalshi WebSocket event handler
│       ├── polymarket_ws.rs — Polymarket WebSocket event handler
│       ├── cleanup.rs       — Internal cleanup for finished games (no REST calls)
│       ├── discovery.rs     — Discover new Kalshi markets
│       ├── order_sync.rs    — Sync resting orders from Kalshi REST
│       ├── fill_sync.rs     — Sync fills from Kalshi REST
│       └── position_sync.rs — Reconcile positions with Kalshi REST
├── strategies/
│   ├── mod.rs           — Strategy trait
│   ├── common.rs        — evaluate_market(), compute_edge_and_alo(), ALO price calc
│   └── passive_espn.rs  — Unified strategy: CLV hunting (pre-game), break quoting, live closing
├── kalshi/              — Auth (RSA-PSS), REST, WebSocket, orderbook, types
├── espn/                — Scoreboard poller, game info types
└── polymarket/          — REST + WS client, event types
```

### Key Types

- `SharedState` = `Arc<Mutex<BotState>>` — core mutable state (game_state, risk, order_manager)
- `SharedOrderBooks` = `Arc<RwLock<HashMap<String, LocalOrderBook>>>` — order books in separate RwLock; WS writes, strategies/dashboard read concurrently
- `SharedBreakLog` = `Arc<std::sync::Mutex<VecDeque<BreakEvalLog>>>` — dashboard-only break eval data, separated to reduce state lock contention
- `SharedLogger` = `Arc<std::sync::Mutex<TradeLogger>>` — independent SQLite logger lock (std::sync, no .await while held)
- `SharedMapper` = `Arc<tokio::sync::Mutex<MarketMapper>>` — independent mapper lock (tokio::sync, held across .await in discovery)
- `GameState` — per-game: ESPN probs, Kalshi markets, Polymarket price, phase, scores
- `OrderSignal` — output of strategy evaluation, fed to executor
- `LocalOrderBook` — per-ticker bid/ask from Kalshi WS snapshots + deltas
- `StrategyRegistry` — holds single `MarketMaker` strategy instance, created at startup via `create_strategies()`

### Lock Ordering
When multiple locks are needed, acquire in this order to prevent deadlocks: **mapper → state → order_books → break_log → logger**

## Critical Domain Knowledge

### Team Direction Alignment
This is the #1 source of bugs. Each venue defines "YES" differently:
- **ESPN/DK**: Probabilities are for the HOME team
- **Kalshi**: Ticker suffix = team code for YES side. Could be home OR away. Use `suffix_matches_team()` to determine.
- **Polymarket**: YES token = first team listed (usually away, but varies)
- `kalshi_is_home` and `polymarket_is_home` flags on GameState control probability flipping

### Order Mechanics
- All orders use `post_only: true` (add-liquidity-only) for maker fees (1.75% vs 7% taker)
- Post-only must post at `bid + 1` or `(100 - ask) + 1`, NOT at the existing bid/ask
- `expiration_ts` takes unix SECONDS (not milliseconds)
- Kalshi does NOT send WS notifications for expired orders — `prune_expired()` handles cleanup locally
- **break_ev order safety**: Four-layer protection against stale orders and stale book data:
  1. **REST book cross-check**: `execute_signal()` in `executor.rs` fetches a fresh REST orderbook before placing any break_ev order. If the REST-derived ALO price differs from the WS-derived price by ≥3c, re-evaluates edge using fresh REST book. Skips the order if edge disappears; reprices if edge still exists. Always updates `SharedOrderBooks` with the fresh REST snapshot so subsequent ticks use accurate data. Logs drift: `"Book drift: {ticker} WS_alo={x}c REST_alo={y}c drift={z}c"`.
  2. **Active cancellation**: `cancel_break_orders()` in `scoreboard.rs` fires when ESPN detects break→live transition (via `breaks_ended()`), immediately cancels ALL resting orders on the game's tickers (not just break_ev tagged — handles bot restart where strategy tags are lost)
  3. **Fixed break expiration**: `break_expires_at` is computed once on break entry (start + duration - safety buffer) and stored on `GameState`. All orders during the same break share the same absolute expiration. TV timeouts: 90s effective, halftime: 840s effective. Falls back to config `order_ttl_secs` if `break_expires_at` is None (e.g. bot restart mid-break).
  4. **Late-break order cutoff**: `break_has_time_for_order(30)` in executor blocks new break_ev ADD orders when <30s remain before expiration. Prevents stale late-break orders after fills. CLOSE orders (position unwinding) are always allowed.
- **REST orderbook seeding at startup**: After WS connects in `main.rs`, fetches orderbooks via REST for all initial tickers and applies snapshots to `SharedOrderBooks`. Ensures books have fresh data immediately rather than waiting for WS snapshots (which may be delayed).

### API Gotchas
- **Kalshi API v2 dollar-string migration (2026-03)**: Kalshi migrated all prices from integer cents to decimal dollar strings, and counts from integers to FP strings. Field names changed: `price` → `price_dollars`, `delta` → `delta_fp`, `count` → `count_fp`, `remaining_count` → `remaining_count_fp`, `yes_price`/`no_price` → `yes_price_dollars`/`no_price_dollars`. Custom deserializers in `types.rs` handle both old (int) and new (string) formats, converting to i64 cents internally. CreateOrderRequest still uses old field names (Kalshi accepts both).
- **Kalshi WS deltas**: Use `ts` field (ISO string), not `timestamp` (i64). Wrong field = silently dropped deltas.
- **Kalshi positions API**: Returns `position` field (signed int: positive=YES, negative=NO). Does NOT return `yes_amount`/`no_amount`.
- **Kalshi fills API**: `fee_cost` is a JSON string (e.g. `"0.0900"`), not float — needs custom deserializer. Value is in **dollars** (stored in DB column `fee_cents` — legacy misnomer, code uses `fee_dollars`). Also sends both `ticker` and `market_ticker` fields — cannot use `serde(alias)` or it fails with "duplicate field". `min_ts` param expects unix timestamp integer, not ISO string.
- **Kalshi market `result` field**: Returns `""` (empty string) for active markets, not absent/null. Must match `"yes"` or `"no"` explicitly — treating empty as "not yes" = "no" will incorrectly settle active fills.
- **ESPN dates**: Sends truncated ISO like `"2026-03-08T19:00Z"` (no seconds). Must normalize before parsing.
- **Polymarket `outcomes`**: JSON string field, not native array. Needs explicit parsing.
- **LLM market matching**: Claude Haiku often modifies ticker suffixes. Always validate returned tickers against the known valid set.
- **Kalshi API field casing**: Kalshi REST and WS return lowercase `side` ("yes"/"no") and `action` ("buy"). The executor logs orders with capitalized values ("Yes"/"No"/"Buy") via `format!("{:?}", enum)`. SQL queries must use `LOWER()` or case-insensitive comparison when matching these fields across fills/orders.

## Config

`config.toml` contains secrets (Kalshi key ID, Anthropic API key) — it is gitignored. Sections: `[kalshi]`, `[anthropic]`, `[risk]`, `[strategy]`, `[polling]`, `[logging]`, `[intervals]`, `[notify]`.

Key settings: `kalshi.dry_run = true` for paper trading. Risk params are at 0.1x for testing.

### Strategy Config Fields
Required: `break_ev_min_edge`, `clv_hunter_min_edge` (used as pregame_min_edge)
Optional (with defaults): `live_strategies` (["pregame", "break_ev"]), `min_volume` (20000), `min_price_cents` (10.0), `max_price_cents` (90.0), `order_ttl_secs` (60), `max_contracts_per_game` (20), `contracts_per_pct_edge` (20.0), `min_trade_contracts` (5), `max_contracts_per_order` (30)

**Note**: There is NO `arb_scanner_min_edge` field. The arb_scanner strategy was planned but never implemented.
**Note**: The `summary_on_break_only` polling config field was removed (was parsed but never used).

### Sizing Model & Trading Rules
See [TRADING_RULES.md](TRADING_RULES.md) for the complete trading rules and strategy logic.

Summary: target-position model. ADD orders sized by edge above threshold. CLOSE orders fire when edge disappears but close-direction edge is positive — closes as much as possible (`min(exposure, max_contracts_per_order)`). No live trading. No forced unwinds. No negative-edge closes. Cross-ticker closing allowed (same-game tickers are equivalent).

## Deployment

The bot runs on a **DigitalOcean Droplet** (1 vCPU, 2GB RAM, Ubuntu 24.04, NYC3 region).
Server IP: `165.227.117.108`

### Cross-compile & Deploy
```bash
# Build for Linux (from macOS)
PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH" cargo build --release --target x86_64-unknown-linux-musl

# Upload binary
scp target/x86_64-unknown-linux-musl/release/sports-betting root@165.227.117.108:/home/bot/app/sports-betting

# Restart
ssh root@165.227.117.108 systemctl restart sports-betting
```

Requires `x86_64-unknown-linux-musl` rustup target and `musl-cross` brew package (already installed).
Cargo cross-compilation config is in `.cargo/config.toml`.

### Server Layout
- **Binary**: `/home/bot/app/sports-betting`
- **Config**: `/home/bot/app/config.toml` (chmod 600)
- **Key**: `/home/bot/app/kalshi_private_key.pem` (chmod 600)
- **Service**: `sports-betting.service` (systemd, runs as `bot` user)
- **Dashboard**: Port 3030 (direct access via `http://165.227.117.108:3030` or HTTPS via Caddy when configured)

### Server Commands
- **Logs**: `ssh root@165.227.117.108 journalctl -u sports-betting -f`
- **Status**: `ssh root@165.227.117.108 systemctl status sports-betting`
- **Restart**: `ssh root@165.227.117.108 systemctl restart sports-betting`
- **Stop**: `ssh root@165.227.117.108 systemctl stop sports-betting`

### Updating Config/Key
```bash
scp config.toml root@165.227.117.108:/home/bot/app/config.toml
ssh root@165.227.117.108 'chown bot:bot /home/bot/app/config.toml && chmod 600 /home/bot/app/config.toml && systemctl restart sports-betting'
```

**IMPORTANT**: When config.rs changes (adding/removing fields), you MUST also update `config.toml` on the server. The binary will crash on startup if the config doesn't match the expected schema.

## Testing

- `tests/correctness_tests.rs` — Property-based tests (proptest) for direction invariants + e2e scenarios
- `tests/game_state_tests.rs` — GameState unit tests
- `tests/risk_constraint_tests.rs` — Risk constraint and CLV order tests
- Run with `cargo test` — no external services needed (all mocked/unit)

## Common Patterns

- Single `PassiveEspn` strategy trades during PreGame and Breaks only — never during live play, never in final 5 minutes. Signals tagged `"pregame"` or `"break_ev"` by phase. Position closing is not a separate strategy — it's the `is_close` flag on OrderSignal, emitted when `target == 0` and game-level exposure exists with +EV close opportunity.
- Close orders use game-level `net_game_aligned` (not ticker-specific net), allowing closing via ANY ticker in the game (best close price wins). See [TRADING_RULES.md](TRADING_RULES.md).
- Close orders (`is_close: true`) bypass the `live_strategies` config check — they're always allowed since they only reduce existing exposure
- Strategies return `Vec<OrderSignal>`, deduped to best-per-ticker in `scoreboard.rs` handler
- `has_resting_order()` on OrderManager prevents duplicate orders per-ticker across ticks (same-ticker only — orders on different tickers in the same game are allowed)
- `evaluate_market()` in `strategies/common.rs` is the shared edge calculation
- `compute_edge_and_alo()` in `strategies/common.rs` is the shared edge/ALO helper used by strategies, executor, and dashboard
- The executor checks `can_evaluate()` before calling `evaluate()` — strategies do NOT self-guard
- Error handling uses `anyhow::Result` throughout

## Main Event Loop (src/main.rs)

```
tokio::select! {
    scoreboard_interval.tick()   => handle_scoreboard_tick()    // ESPN poll + strategy eval
    maintenance_interval.tick()  => handle_maintenance_tick()   // cleanup + discovery + order/fill sync
    kalshi_rx.recv()             => handle_kalshi_event()       // WS orderbook updates + fills
    poly_rx.recv()               => handle_polymarket_event()   // WS price updates
}
```

The maintenance tick (default 30s, configurable via `intervals.maintenance_interval_secs`) runs sequentially:
1. `cleanup_finished_games()` — internal state cleanup: removes orders, risk positions, and order books for finished tickers; backfills settlement PnL (Kalshi auto-cancels settled orders)
2. `discover_new_markets()` — find new Kalshi markets, map them, subscribe WS
3. `sync_orders()` + `sync_fills()` — reconcile with Kalshi REST
4. `reconcile_positions()` — compare local risk positions with Kalshi API, auto-correct drift

Dashboard server runs as a separate tokio task on port 3030.
