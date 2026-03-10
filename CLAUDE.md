# Sports Betting Bot

Rust-based automated CBB (college basketball) betting bot on Kalshi. Uses ESPN + DraftKings as reference prices, Polymarket as secondary reference.

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
│   ├── dashboard.rs     — Web dashboard (axum)
│   └── handlers/        — Event loop handlers (scoreboard, kalshi_ws, polymarket_ws, cleanup, discovery, order_sync)
├── strategies/
│   ├── common.rs        — evaluate_edge(), evaluate_market(), ALO price calc
│   ├── break_ev.rs      — Break-based +EV quoter (halftime/TV timeout)
│   ├── arb_scanner.rs   — Cross-market arb (live play)
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
- **ESPN dates**: Sends truncated ISO like `"2026-03-08T19:00Z"` (no seconds). Must normalize before parsing.
- **Polymarket `outcomes`**: JSON string field, not native array. Needs explicit parsing.
- **LLM market matching**: Claude Haiku often modifies ticker suffixes. Always validate returned tickers against the known valid set.

## Config

`config.toml` contains secrets (Kalshi key ID, Anthropic API key) — it is gitignored. Sections: `[kalshi]`, `[anthropic]`, `[risk]`, `[strategy]`, `[polling]`, `[logging]`, `[intervals]`, `[notify]`.

Key settings: `kalshi.dry_run = true` for paper trading. Risk params are at 0.1x for testing.

## Testing

- `tests/correctness_tests.rs` — Property-based tests (proptest) for direction invariants + e2e scenarios
- `tests/game_state_tests.rs` — GameState unit tests
- Run with `cargo test` — no external services needed (all mocked/unit)

## Common Patterns

- Strategies return `Vec<OrderSignal>`, deduped to best-per-ticker in `scoreboard.rs` handler
- `has_strategy_order()` on OrderManager prevents duplicate orders across ticks
- `evaluate_market()` in `strategies/common.rs` is the shared edge calculation used by all 3 strategies
- Error handling uses `anyhow::Result` throughout; `thiserror` for typed errors in kalshi module
