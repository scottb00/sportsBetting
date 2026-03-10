# Sports Betting Bot — Architecture

## System Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                          main.rs EVENT LOOP                        │
│                                                                     │
│  tokio::select! {                                                   │
│    scoreboard_tick  ──► ESPN poll → strategy eval → order exec      │
│    cleanup_tick     ──► remove finished games                       │
│    discovery_tick   ──► find new Kalshi/Poly markets                │
│    kalshi_ws_rx     ──► orderbook updates, fills                    │
│    poly_ws_rx       ──► price updates (logged, not yet in signals)  │
│  }                                                                  │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                    ┌────────────▼────────────┐
                    │   SharedState            │
                    │   Arc<Mutex<BotState>>   │
                    ├─────────────────────────┤
                    │ GameStateManager        │  ← all games + markets
                    │ MarketMapper            │  ← ESPN↔Kalshi↔Poly
                    │ OrderManager            │  ← open orders + intents
                    │ RiskManager             │  ← position limits, P&L
                    │ LocalOrderBooks (map)   │  ← per-ticker book
                    │ TradeLogger (SQLite)    │  ← audit trail
                    └─────────────────────────┘
```

## Data Sources & Flow

```
  ┌──────────────┐    ┌──────────────────┐    ┌────────────────────┐
  │    ESPN       │    │     Kalshi        │    │   Polymarket       │
  │  (reference)  │    │  (execution)      │    │  (secondary ref)   │
  └──────┬───────┘    └────────┬─────────┘    └────────┬───────────┘
         │                     │                       │
   ┌─────▼──────┐    ┌────────▼────────┐    ┌─────────▼──────────┐
   │ REST: score │    │ REST: events,   │    │ REST: CBB events,  │
   │ board every │    │ series, orders  │    │ moneyline markets  │
   │ ~10 seconds │    │                 │    │                    │
   │             │    │ WS: orderbook   │    │ WS: price updates  │
   │ REST: game  │    │ snapshots,      │    │                    │
   │ summary on  │    │ deltas, fills,  │    │                    │
   │ breaks      │    │ trades          │    │                    │
   └─────┬───────┘    └────────┬────────┘    └─────────┬──────────┘
         │                     │                       │
         ▼                     ▼                       ▼
  ┌──────────────────────────────────────────────────────────────┐
  │                    GameState (per game)                       │
  │                                                              │
  │  espn_home_win_prob ◄── ESPN summary (DraftKings moneyline)  │
  │  kalshi_markets[]   ◄── Kalshi WS (bid/ask/mid per ticker)   │
  │  polymarket price   ◄── Polymarket WS                        │
  │  phase, scores      ◄── ESPN scoreboard                      │
  └──────────────────────────────────────────────────────────────┘
```

## Market Mapping (Startup)

```
  ESPN games          Kalshi events           Polymarket events
  ─────────           ─────────────           ──────────────────
  "Duke @ UNC"        "KXNCAAMBGAME-..."      "Duke vs. UNC"
  event_id: 401x      title: "Duke at         token_id: 0x...
                       UNC Winner?"
       │                     │                       │
       └─────────┬───────────┘                       │
                 ▼                                   │
       ┌─────────────────┐                           │
       │  Fuzzy Matching  │ ◄────────────────────────┘
       │                  │
       │ 1. Normalize     │   "Appalachian St." → "Appalachian State"
       │    team names    │   Strip mascots ("Tigers", "Eagles", etc.)
       │                  │
       │ 2. Match "at"    │   ESPN "A @ B" ↔ Kalshi "A at B Winner?"
       │    separator     │   Poly "A vs. B" ↔ ESPN "A @ B"
       │                  │
       │ 3. LLM fallback  │   Claude Haiku for remaining unmatched
       │    + validation  │   Validate returned tickers against list
       └────────┬────────┘
                │
                ▼
       ┌─────────────────┐
       │  Cached Mappings  │   espn_id → (kalshi_tickers[], poly_token)
       │  (per-day file)   │   + direction flags (is_home per market)
       └──────────────────┘
```

## Team Direction Alignment (Critical)

```
                     ESPN says: Home win prob = 65%

  ┌─────────────────────────────────────────────────────────────┐
  │                    Which team is YES?                        │
  │                                                             │
  │  Kalshi ticker: KXNCAAMBGAME-...-UNC                        │
  │    suffix "UNC" = home team  →  is_home = true              │
  │    fair_value = 0.65 (ESPN home prob used directly)          │
  │                                                             │
  │  Kalshi ticker: KXNCAAMBGAME-...-DUKE                       │
  │    suffix "DUKE" = away team →  is_home = false             │
  │    fair_value = 0.35 (flip: 1 - 0.65)                       │
  │                                                             │
  │  Polymarket token: "Duke vs. UNC"                           │
  │    YES = Duke (away)  →  polymarket_is_home = false         │
  │    fair_value = 0.35 (flip)                                  │
  └─────────────────────────────────────────────────────────────┘
```

---

## Strategy 1: Break-Based +EV Quoter

```
  WHEN: Game is at halftime or TV timeout (phase.is_break())

  WHY:  During stoppages, Kalshi market makers are slow to update.
        ESPN/DK odds update faster → temporary mispricings.

  ┌──────────────┐     ┌──────────────┐
  │  ESPN Summary │     │ Kalshi Book   │
  │  home_prob=65%│     │ bid=60 ask=68 │
  └──────┬───────┘     └──────┬───────┘
         │                     │
         ▼                     ▼
  ┌──────────────────────────────────────────┐
  │           evaluate_market()              │
  │                                          │
  │  fair_value = 0.65  (aligned to YES)     │
  │                                          │
  │  YES side:  ALO price = ask - 1 = 67     │
  │    edge = |0.65 - 0.67| = 0.02           │
  │    Wait — fair < price → would BUY NO    │
  │                                          │
  │  NO side:   ALO price = (100-bid)-1 = 39 │
  │    fair_no  = 1 - 0.65 = 0.35            │
  │    edge = |0.35 - 0.39| = 0.04           │
  │    fair < price → BUY NO ✓               │
  │                                          │
  │  edge_after_fees = 0.04 - maker_fee      │
  │  If edge_after_fees > min_edge → SIGNAL  │
  └──────────────────────┬───────────────────┘
                         │
                         ▼
  ┌──────────────────────────────────────────┐
  │  OrderSignal                             │
  │    ticker: KXNCAAMBGAME-...-UNC          │
  │    side: No,  action: Buy                │
  │    price: 39 cents                       │
  │    post_only: true (maker fees only)     │
  │    expiration: now + order_ttl           │
  │    size: Kelly-sized, capped by risk     │
  └──────────────────────────────────────────┘
```

## Strategy 2: Cross-Market Arb Scanner

```
  WHEN: Game is live, at halftime, or on break
        (broader than Break EV — runs continuously)

  WHY:  Same edge logic as Break EV, but catches mispricings
        that appear mid-play when scores change rapidly.

  ┌────────────────────────────────────────────────────────┐
  │  Same evaluate_market() pipeline as Break EV           │
  │                                                        │
  │  Key difference: fires during LIVE play, not just      │
  │  breaks. Captures real-time arb when Kalshi hasn't     │
  │  fully priced in a scoring run or momentum shift.      │
  │                                                        │
  │  Order placement: identical (passive ALO, post_only)   │
  └────────────────────────────────────────────────────────┘
```

## Strategy 3: CLV Hunter (Pre-Game)

```
  WHEN: Game phase == PreGame (before tipoff)

  WHY:  Post passive orders at current ESPN fair value.
        As tipoff approaches, market converges to sharp price.
        If our resting order IS the sharp price, we get filled
        with positive expected value. Orders auto-cancel at tipoff.

  TIMELINE:
  ────────────────────────────────────────────────────────►
  T-2hrs        T-30min        Tipoff         Post-game
    │              │              │               │
    │  Post order  │  Market      │  Order        │
    │  at ALO      │  converges   │  expires      │
    │  price       │  to ESPN     │  (auto-cancel │
    │              │  fair value  │   by Kalshi)  │
    │              │              │               │
    │              │  If filled:  │  CLV check:   │
    │              │  got edge!   │  mid vs price │

  ┌──────────────────────────────────────────┐
  │  Unique features:                        │
  │                                          │
  │  • expiration_ts = game start time       │
  │    (Kalshi auto-cancels unfilled orders) │
  │                                          │
  │  • CLV Validation at tipoff:             │
  │    closing_mid = book mid at game start  │
  │    CLV = closing_mid - order_price       │
  │    CLV > 0 → "CAPTURED" (good trade)     │
  │    CLV < 0 → "MISSED" (bad entry)        │
  │                                          │
  │  • Same edge calc as other strategies    │
  └──────────────────────────────────────────┘
```

## Order Lifecycle

```
  Strategy fires signal
         │
         ▼
  ┌─────────────┐    NO     ┌──────────────┐
  │ Risk check:  ├─────────►│ Signal        │
  │ can_trade()? │          │ dropped       │
  └──────┬──────┘          └──────────────┘
     YES │
         ▼
  ┌─────────────┐
  │ Record       │  ← blocks same strategy from re-firing
  │ intent       │     on this ticker until expiry
  └──────┬──────┘
         │
         ├── dry_run? ──► log only, intent still blocks
         │
         ▼
  ┌─────────────────┐       ┌──────────────────┐
  │ Kalshi REST:     │──ERR─►│ Intent remains,  │
  │ create_order()   │       │ expires after TTL │
  │ (RSA-PSS signed) │       └──────────────────┘
  └──────┬──────────┘
     OK  │
         ▼
  ┌─────────────────┐
  │ Track order in   │
  │ OrderManager     │
  │ Log to SQLite    │
  │ Push notification│
  └──────┬──────────┘
         │
         ├── Fill (WS) ────► update remaining, record P&L
         │                    full fill → remove order, clear intent
         │
         ├── Expire (Kalshi) ► no WS notification!
         │                     local prune_expired() cleans up
         │
         └── Cancel (REST) ─► remove order, clear intent
```

## Risk Management

```
  ┌───────────────────────────────────────────────────────┐
  │                    RiskManager                         │
  │                                                       │
  │  Guards (checked before every order):                  │
  │  ┌─────────────────────────────────────────────────┐  │
  │  │ daily_pnl > -daily_loss_limit     (e.g., -$50)  │  │
  │  │ total_exposure + new < max_total   (e.g., $200) │  │
  │  │ game_exposure + new < max_per_game (e.g., $10)  │  │
  │  │ NOT halted                                      │  │
  │  └─────────────────────────────────────────────────┘  │
  │                                                       │
  │  Kelly Sizing:                                        │
  │  ┌─────────────────────────────────────────────────┐  │
  │  │ b = (1 - price) / price      (payout ratio)     │  │
  │  │ f = (b * fair - (1-fair)) / b (Kelly fraction)  │  │
  │  │ size = f * kelly_mult * max_total_exposure       │  │
  │  │ size = min(size, game_remaining, total_remaining)│  │
  │  └─────────────────────────────────────────────────┘  │
  │                                                       │
  │  Fee Calculation:                                     │
  │  ┌─────────────────────────────────────────────────┐  │
  │  │ fee = ceil(rate * contracts * p * (1-p))         │  │
  │  │ maker_rate = 1.75%    taker_rate = 7.0%         │  │
  │  │ All strategies use post_only → maker fees only   │  │
  │  └─────────────────────────────────────────────────┘  │
  │                                                       │
  │  Daily Reset: midnight → reset daily_pnl, un-halt    │
  └───────────────────────────────────────────────────────┘
```

## Module Map

```
  src/
  ├── main.rs                    Event loop, startup, WS connections
  ├── lib.rs                     Re-exports for multi-binary
  │
  ├── engine/
  │   ├── bot.rs                 BotState, SharedState, populate_game_states
  │   ├── game_state.rs          GameState, KalshiMarketState, GameStateManager
  │   ├── market_mapper.rs       Fuzzy matching, LLM fallback, direction flags
  │   ├── market_prep.rs         Shared fetch/filter/build helpers, BookPrices
  │   ├── order_manager.rs       Order tracking, intents, dedup, pruning
  │   ├── risk.rs                Kelly sizing, position limits, fee calc
  │   ├── executor.rs            Signal → Kalshi REST order
  │   ├── logger.rs              SQLite trade logging
  │   ├── notifier.rs            ntfy.sh push notifications
  │   └── handlers/
  │       ├── mod.rs             Re-exports
  │       ├── scoreboard.rs      ESPN tick → strategy eval → order exec
  │       ├── kalshi_ws.rs       Orderbook updates, fills
  │       ├── polymarket_ws.rs   Price updates (logged only)
  │       ├── cleanup.rs         Remove finished games
  │       └── discovery.rs       Find new markets mid-session
  │
  ├── strategies/
  │   ├── common.rs              evaluate_edge(), evaluate_market(), ALO calc
  │   ├── break_ev.rs            Break-based +EV quoter
  │   ├── arb_scanner.rs         Cross-market arb (live play)
  │   └── clv_hunter.rs          Pre-game CLV hunting
  │
  ├── kalshi/
  │   ├── auth.rs                RSA-PSS request signing
  │   ├── rest.rs                REST client (events, orders, cancel)
  │   ├── websocket.rs           WS client (orderbook, fills, trades)
  │   ├── orderbook.rs           LocalOrderBook (snapshot + delta)
  │   └── types.rs               API response types
  │
  ├── espn/
  │   ├── poller.rs              Scoreboard + summary fetching, phase detection
  │   └── types.rs               GameInfo, odds extraction
  │
  ├── polymarket/
  │   ├── client.rs              REST + WS client
  │   └── types.rs               Event/market types
  │
  └── config.rs                  TOML config (strategies, risk, intervals)
```
