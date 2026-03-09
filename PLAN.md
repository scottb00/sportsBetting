# Sports Betting Bot — System Plan

## Overview
Fully automated, small-size college basketball betting bot on Kalshi. Built in Rust, deployed to cloud (Chicago region for lowest latency to Kalshi's matching engine). Mid-frequency strategies focused on market microstructure and flow, not model-driven.

---

## Strategies

### 1. Break-Based +EV Quoting
- **When**: Halftime, TV timeouts, end-of-half — clearly defined pauses in gameplay
- **What**: Post limit orders at top of book in the +EV direction
- **Edge**: Compare Kalshi price against multiple fair value references (ESPN win prob, DK odds, Polymarket). When Kalshi is mispriced relative to consensus, post passive orders to capture the spread
- **Sizing**: Fractional Kelly based on confidence in the reference price vs Kalshi price dislocation
- **Key detail**: Maker fees are ~4x cheaper than taker fees, so we want to be passive whenever possible

### 2. Cross-Market Arb Scanner (Kalshi vs references)
- **What**: Compare Kalshi implied probability vs multiple reference prices (DK moneyline from ESPN, Polymarket)
- **When**: Continuously during live games
- **Action**: When Kalshi is significantly mispriced vs reference consensus (accounting for fees), post limit orders to capture the dislocation
- **Note**: Not true riskless arb (can't trade DK programmatically), but DK + Polymarket together give a strong fair value signal

### 3. CLV (Closing Line Value) Hunting
- **What**: Pre-game, compare Kalshi prices to sharp sportsbook lines
- **When**: Hours/minutes before tip-off
- **Action**: If Kalshi price diverges significantly from DK/Polymarket/sharp consensus, post limit orders early and let the market come to you
- **Edge**: Kalshi markets are less efficient pre-game than traditional sportsbooks. Prices should converge toward sharp lines by game time
- **Sizing**: Fractional Kelly, smaller size due to less confidence in pre-game edge

### Next Steps (not in initial build)
- **Flow / Impact Detection** — Monitor Kalshi order book for large orders, sweeps, and book imbalance as directional signals
- **SharpAPI.io integration ($79/mo)** — Add Pinnacle-derived fair odds (vig-stripped) as an additional reference price source via SSE streaming

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        CLOUD (Chicago VPS)                       │
│                                                                   │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌─────────────┐  │
│  │  ESPN Poller  │  │ Polymarket   │  │ Kalshi WS    │  │ Kalshi REST │  │
│  │  (scores,     │  │ WS + REST    │  │ (orderbook   │  │ (order      │  │
│  │   win prob,   │  │ (prices,     │  │  deltas,     │  │  entry,     │  │
│  │   DK odds)    │  │  book,       │  │  trades,     │  │  positions, │  │
│  │  on breaks    │  │  trades)     │  │  fills)      │  │  balance)   │  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  └──────┬──────┘  │
│         │                 │                  │                  │         │
│         ▼                 ▼                  ▼                  ▲         │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                     Core Engine                              │ │
│  │                                                              │ │
│  │  ┌────────────┐  ┌────────────┐  ┌─────────────────────┐   │ │
│  │  │ Game State │  │ Order Book │  │  Strategy Manager    │   │ │
│  │  │ Manager    │  │ Tracker    │  │                      │   │ │
│  │  │            │  │            │  │  - Break EV Quoter   │   │ │
│  │  │ - scores   │  │ - Kalshi   │  │  - Arb Scanner       │   │ │
│  │  │ - clock    │  │   local    │  │  - CLV Hunter        │   │ │
│  │  │ - win prob │  │   book     │  │                      │   │ │
│  │  │ - DK odds  │  │ - Poly     │  │  Each strategy emits │   │ │
│  │  │ - Poly     │  │   prices   │  │  Order signals →     │   │ │
│  │  │   price    │  │            │  │  Risk Manager        │   │ │
│  │  │ - game     │  │            │  │                      │   │ │
│  │  │   phase    │  │            │  │                      │   │ │
│  │  └────────────┘  └────────────┘  └─────────────────────┘   │ │
│  │                                                              │ │
│  │  ┌────────────────────┐  ┌──────────────────────────────┐   │ │
│  │  │ Risk Manager       │  │ Order Manager                │   │ │
│  │  │                    │  │                               │   │ │
│  │  │ - position limits  │  │ - place/cancel/amend orders  │   │ │
│  │  │ - Kelly sizing     │  │ - track open orders          │   │ │
│  │  │ - max exposure     │  │ - handle fills               │   │ │
│  │  │ - per-game limits  │  │ - batch operations           │   │ │
│  │  │ - daily loss limit │  │ - post_only enforcement      │   │ │
│  │  └────────────────────┘  └──────────────────────────────┘   │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                   │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │                     Logging / Telemetry                      │ │
│  │  - All orders, fills, cancels logged with timestamps         │ │
│  │  - P&L tracking per strategy                                 │ │
│  │  - Latency metrics (ESPN poll → signal → order placed)       │ │
│  │  - Alerting (Discord/Telegram webhook for fills, errors)     │ │
│  └─────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

---

## Data Sources

### ESPN Free API (no auth required)
| Endpoint | Data | Poll Frequency |
|----------|------|----------------|
| `site.api.espn.com/.../scoreboard` | Event IDs, game status, schedules | Every 30s |
| `site.api.espn.com/.../summary?event={id}` | Win probability, DK odds (pickcenter), plays, box score | On break detection (not continuous) |

### Polymarket (no auth for reads)
| Source | Data | Method |
|--------|------|--------|
| Gamma API `GET /events?tag_id=101178` | CBB market discovery, prices, volume | REST poll |
| Gamma API `GET /events?tag_id=100149` | March Madness markets | REST poll |
| CLOB API `GET /order-book?token_id={id}` | Full bid/ask depth | REST poll |
| WebSocket `wss://ws-subscriptions-clob.polymarket.com/ws/market` | Real-time book, trades, best bid/ask | Streaming |
| **Rate limits**: 1,500 req/10s for price data, 500/10s for market queries | | |
| **Rust SDK**: `polymarket-client-sdk` (official, on crates.io) | | |

### Kalshi WebSocket (authenticated)
| Channel | Data | Use |
|---------|------|-----|
| `orderbook_delta` | Price level changes + snapshots | Local book replica, imbalance, large order detection |
| `trade` | Trades with price, size, taker side | Flow detection, tape reading |
| `fill` | Our fills with position updates | Execution confirmation |
| `user_orders` | Our order status changes | Order management |
| `ticker` | Best bid/ask, volume, OI | Quick market summary |

### Kalshi REST (authenticated)
| Endpoint | Use |
|----------|-----|
| `GET /markets/{ticker}/orderbook` | Initial book snapshot on startup |
| `POST /portfolio/orders` | Place orders |
| `DELETE /portfolio/orders/{id}` | Cancel orders |
| `POST /portfolio/orders/{id}/amend` | Amend price/size |
| `POST /portfolio/orders/batched` | Batch place (up to 20) |
| `GET /portfolio/positions` | Current positions |
| `GET /portfolio/balance` | Account balance |

---

## Tech Stack

### Language: Rust
- **Why**: Sub-second order book processing, async WebSocket handling, memory safety
- **Runtime**: Tokio async runtime
- **Key crates**:
  - `tokio` — async runtime
  - `tokio-tungstenite` — WebSocket client
  - `reqwest` — HTTP client (ESPN polling, Kalshi REST)
  - `rsa` + `sha2` — Kalshi auth signing (RSA-PSS + SHA-256)
  - `serde` / `serde_json` — JSON parsing
  - `polymarket-client-sdk` — official Polymarket API client (REST + WebSocket)
  - `tracing` — structured logging
  - `sqlx` or `rusqlite` — trade/order logging to SQLite or Postgres

### Deployment: Chicago VPS
- **Why**: Kalshi matching engine is in Chicago. ~1-2ms latency from colocated VPS vs 50-200ms residential
- **Provider**: AWS us-east-2 (Ohio) or a Chicago-based VPS provider
- **Setup**: Single binary, systemd service, auto-restart on crash

---

## Module Breakdown (Build Order)

### Phase 1: Foundation
1. **Kalshi Auth Module** — RSA-PSS signing, API key management
2. **Kalshi REST Client** — Order CRUD, positions, balance
3. **Kalshi WebSocket Client** — Connect, subscribe, handle orderbook_delta + trade channels
4. **Local Order Book** — Maintain real-time book replica from WS deltas, sequence gap detection + resync

### Phase 2: Data & Game State
5. **ESPN Poller** — Poll scoreboard for event IDs and game status. Poll summary for win prob + DK odds only on break detection
6. **Polymarket Client** — WebSocket connection for real-time CBB prices. REST for market discovery and order book depth. Uses official `polymarket-client-sdk` crate
7. **Game State Manager** — Track all live games: score, clock, phase (pre-game, live, halftime, break, final), win probability, DK reference price, Polymarket price
8. **Market Mapper** — Map Kalshi market tickers ↔ Polymarket token IDs ↔ ESPN event IDs (match by team names + date)

### Phase 3: Strategies
9. **Break EV Quoter** — When game enters break state, compare ESPN win prob / DK odds / Polymarket price to Kalshi mid. If dislocation > threshold, emit order signal to post at top of book
10. **Arb Scanner** — Continuously compare Kalshi implied prob vs Polymarket + DK. Flag dislocations above fee-adjusted threshold
11. **CLV Hunter** — Pre-game: compare Kalshi price vs DK / Polymarket. Post limit orders where divergence exceeds threshold

### Phase 4: Execution & Risk
12. **Risk Manager** — Position limits (per game, total), Kelly sizing calculator, daily loss limit, max open order exposure
13. **Order Manager** — Translate strategy signals into Kalshi orders. Handle order lifecycle (place → open → fill/cancel). Use post_only for maker orders. Batch operations where possible

### Phase 5: Ops & Monitoring
14. **Trade Logger** — Persist all orders, fills, positions, P&L to database
15. **Alerting** — Discord/Telegram webhooks for fills, errors, daily P&L summary
16. **Dashboard** (optional, later) — Simple web UI showing live books, positions, strategy signals

---

## Risk Controls

| Control | Setting |
|---------|---------|
| Max position per game | Configurable (start: $50) |
| Max total exposure | Configurable (start: $500) |
| Daily loss limit | Configurable (start: $200) — halt all trading if hit |
| Order type | post_only by default (maker fees only) |
| Kelly fraction | 0.5 Kelly (half Kelly to start) |
| Min edge threshold | Fee-adjusted: only trade when expected edge > 2x fees |
| Stale data kill switch | If ESPN data > 60s stale, pull all quotes |
| Exchange halt | If Kalshi exchange status != active, pull all quotes |

---

## Key Decisions & Open Questions

1. **Market ticker mapping**: Need to figure out how Kalshi names CBB markets and reliably map them to ESPN event IDs. This is a critical early task.
2. **Win probability as fair value**: ESPN's BPI win prob is a model — it could be wrong. DK moneyline is market-derived and probably sharper. May want to weight DK more heavily.
3. **Fee calculation**: Maker fee = ceil(0.0175 × C × P × (1-P)). Need to bake this into every edge calculation.
4. **Kalshi rate limits**: Starting at Basic tier (20 read/s, 10 write/s). Need to be efficient with REST calls and rely on WebSocket for real-time data.
5. **Multiple simultaneous games**: March Madness can have 8+ games at once. Need to handle multiple WS subscriptions and game states concurrently.
6. **Backtesting**: No historical Kalshi order book data readily available. May need to paper trade for a period to validate strategies before sizing up.
