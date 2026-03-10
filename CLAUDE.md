# Sports Betting Bot

Rust-based automated CBB (college basketball) betting bot on Kalshi. Uses ESPN + DraftKings as reference prices, Polymarket as secondary reference.

## Agent Instructions

**IMPORTANT**: If you notice anything in this file that is wrong, outdated, or missing — update it immediately. This file must stay in sync with the actual codebase. When you add new files, change config fields, fix bugs, or alter behavior, update the relevant sections here before finishing your task. Future agents depend on this file being accurate.

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
│   ├── bot.rs           — BotState, SharedState (Arc<Mutex>), populate_game_states
│   ├── game_state.rs    — GameState, KalshiMarketState, GameStateManager
│   ├── market_mapper.rs — Fuzzy team matching + LLM fallback + direction flags
│   ├── market_prep.rs   — Shared fetch/filter/build helpers, BookPrices
│   ├── order_manager.rs — Order tracking, intents, dedup, pruning
│   ├── risk.rs          — Kelly sizing, position limits, fee calc
│   ├── executor.rs      — Signal → Kalshi REST order placement
│   ├── logger.rs        — SQLite trade logging
│   ├── notifier.rs      — Telegram push notifications
│   ├── dashboard.rs     — Web dashboard (axum, port 3030)
│   └── handlers/
│       ├── scoreboard.rs    — ESPN poll → strategy evaluation → order signals
│       ├── kalshi_ws.rs     — Kalshi WebSocket event handler
│       ├── polymarket_ws.rs — Polymarket WebSocket event handler
│       ├── cleanup.rs       — Cancel orders for finished games
│       ├── discovery.rs     — Discover new Kalshi markets
│       ├── order_sync.rs    — Sync resting orders + positions from Kalshi REST
│       └── fill_sync.rs     — Sync fills from Kalshi REST
├── strategies/
│   ├── mod.rs           — Strategy trait + StrategyRegistry
│   ├── common.rs        — evaluate_edge(), evaluate_market(), ALO price calc
│   ├── break_ev.rs      — Break-based +EV quoter (halftime/TV timeout)
│   └── clv_hunter.rs    — Pre-game CLV hunting
├── kalshi/              — Auth (RSA-PSS), REST, WebSocket, orderbook, types
├── espn/                — Scoreboard poller, game info types
└── polymarket/          — REST + WS client, event types
```

### Key Types

- `SharedState` = `Arc<Mutex<BotState>>` — the global mutable state passed everywhere
- `GameState` — per-game: ESPN probs, Kalshi markets, Polymarket price, phase, scores
- `OrderSignal` — output of strategy evaluation, fed to executor
- `LocalOrderBook` — per-ticker bid/ask from Kalshi WS snapshots + deltas
- `StrategyRegistry` — holds all strategy instances, created at startup via `create_strategies()`

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

### API Gotchas
- **Kalshi WS deltas**: Use `ts` field (ISO string), not `timestamp` (i64). Wrong field = silently dropped deltas.
- **Kalshi positions API**: Returns `position` field (signed int: positive=YES, negative=NO). Does NOT return `yes_amount`/`no_amount`.
- **Kalshi fills API**: `fee_cost` is a JSON string (e.g. `"0.0900"`), not float — needs custom deserializer. Also sends both `ticker` and `market_ticker` fields — cannot use `serde(alias)` or it fails with "duplicate field". `min_ts` param expects unix timestamp integer, not ISO string.
- **ESPN dates**: Sends truncated ISO like `"2026-03-08T19:00Z"` (no seconds). Must normalize before parsing.
- **Polymarket `outcomes`**: JSON string field, not native array. Needs explicit parsing.
- **LLM market matching**: Claude Haiku often modifies ticker suffixes. Always validate returned tickers against the known valid set.

## Config

`config.toml` contains secrets (Kalshi key ID, Anthropic API key) — it is gitignored. Sections: `[kalshi]`, `[anthropic]`, `[risk]`, `[strategy]`, `[polling]`, `[logging]`, `[intervals]`, `[notify]`.

Key settings: `kalshi.dry_run = true` for paper trading. Risk params are at 0.1x for testing.

### Strategy Config Fields
Required: `break_ev_min_edge`, `clv_hunter_min_edge`
Optional (with defaults): `live_strategies` (["clv_hunter"]), `min_volume` (20000), `min_price_cents` (10.0), `max_price_cents` (90.0), `order_ttl_secs` (120), `max_contracts_per_game` (20)

**Note**: There is NO `arb_scanner_min_edge` field. The arb_scanner strategy was planned but never implemented.

## Deployment

The bot runs on **Fly.io** (app: `sportsbetting-bot`, region: `iad` / US East).
Dashboard URL: **https://sportsbetting-bot.fly.dev**

- **Deploy**: `flyctl deploy` (builds via Dockerfile, multi-stage Rust build — takes several minutes)
- **Logs**: `flyctl logs`
- **SSH**: `flyctl ssh console`
- **Start/stop**: `flyctl machine start <id>` / `flyctl machine stop <id>`
- **Status**: `flyctl status`
- **Secrets**: `CONFIG_TOML` and `KALSHI_PRIVATE_KEY` are stored as Fly secrets (injected by `entrypoint.sh`)
- **Dashboard**: Exposed on port 3030 via Fly HTTP service (force HTTPS, auto_stop=false)
- **VM**: `shared-cpu-1x`, 256MB RAM

**CLI note**: On macOS, `fly` may not be in PATH. Use `/opt/homebrew/bin/flyctl` if `fly` is not found.

Key files:
- `Dockerfile` — Multi-stage build (rust:1.94-bookworm → debian:bookworm-slim)
- `fly.toml` — Fly.io app config
- `entrypoint.sh` — Writes secrets to disk, then execs the binary

To update secrets: `flyctl secrets set CONFIG_TOML="$(cat config.toml)" KALSHI_PRIVATE_KEY="$(cat kalshi_private_key.pem)"`

**IMPORTANT**: When config.rs changes (adding/removing fields), you MUST also update the CONFIG_TOML secret on Fly.io. The deployed binary will crash on startup if the config doesn't match the expected schema.

## Testing

- `tests/correctness_tests.rs` — Property-based tests (proptest) for direction invariants + e2e scenarios
- `tests/game_state_tests.rs` — GameState unit tests
- Run with `cargo test` — no external services needed (all mocked/unit)

## Common Patterns

- Strategies implement the `Strategy` trait; registered in `StrategyRegistry` at startup
- Strategies return `Vec<OrderSignal>`, deduped to best-per-ticker in `scoreboard.rs` handler
- `has_strategy_order()` on OrderManager prevents duplicate orders across ticks
- `evaluate_market()` in `strategies/common.rs` is the shared edge calculation used by both strategies
- Error handling uses `anyhow::Result` throughout; `thiserror` for typed errors in kalshi module

## Main Event Loop (src/main.rs)

```
tokio::select! {
    scoreboard_interval.tick()  => handle_scoreboard_tick()   // ESPN poll + strategy eval
    cleanup_interval.tick()     => cleanup_finished_games()   // cancel orders for ended games
    discovery_interval.tick()   => discover_new_markets()     // find new Kalshi markets
    order_sync_interval.tick()  => sync_orders()              // sync resting orders from REST
    kalshi_rx.recv()            => handle_kalshi_event()      // WS orderbook updates
    poly_rx.recv()              => handle_polymarket_event()  // WS price updates
}
```

Dashboard server runs as a separate tokio task on port 3030.
