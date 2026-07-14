# polyStrat3

Low-latency Rust trading bot for **Polymarket 15-minute BTC/ETH Up/Down markets**. Two strategies (taker, maker) on one event-driven engine, with dry-run, live, and deterministic replay modes. See `USER_MANUAL.md` for config and commands.

## Features

- Log-normal fair value `Φ(ln(S/S_open) / (σ√tte))` from an aggregated Binance spot (Chainlink leg optional), rolling realized vol with config override
- 100 ms alpha signal (order-book imbalance) shifting the quote center
- Target-based OMS reconciliation: strategies declare the desired order set, the OMS diffs — unchanged quotes cost zero traffic
- Risk controls: position caps, per-market loss stop, exposure limit, order throttle, aggressive-amend limiter, post-only guard, cancel-all on shutdown
- Event journal + deterministic replay (`--replay`): same events + virtual clock ⇒ byte-identical decisions, used as a no-regression proof for every refactor
- Tick-to-trade latency histograms (dispatch / decision / tick-to-order / journal), reported every 60 s
- Official Polymarket Rust SDK for all exchange interaction (orders, cancels, user channel, positions); trade CSV + PnL logs

## System design

- **Single-threaded engine**: all state owned by one event loop (tokio current-thread); feeds funnel into one queue — no locks anywhere on the decision path
- **Pure strategy state machines**: `Snap` in → `Action`s out; DryRun and Live share one code path (fill simulation lives in the executor)
- **Executor facade**: the only code that knows DryRun from Live; order I/O is fire-and-forget, acks return as events
- **Safety-by-state**: PendingCancel handling and deferred amends close the un-acked-order double-fill race; placement is gated on the user-channel being connected (stop-loss exempt)

## Strategies

- **Taker**: passive entry at the bid inside a price band; TP and SL are placed as opposite-token buys at the complement price; recovery re-arms lost close orders; cooldown with buy-the-dip override; per-UTC-hour overrides
- **Maker**: buy-only quotes around the alpha-adjusted fair, spread widened by time-to-expiry × inventory × momentum plus a maker-fee unit; inventory skew and hard caps (reduce-only at the limit); loss-stop and exposure halts; pre-expiry quote pull

## Possible extension

- Connect to additional exchanges (cross-venue hedging / arbitrage)
- Alpha research: richer microstructure signals beyond book imbalance
- Pricing spread control: adaptive spread from realized fill toxicity and queue position
