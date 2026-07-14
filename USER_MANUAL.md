# polyStrat3 — User Manual

## Config structure

Strategy config (`config/taker.toml` / `config/maker.toml`):

| Section | Purpose |
|---|---|
| `[general]` | symbol (BTC/ETH), strategy (taker/maker), dry_run, log_level, endpoints, order_throttle_ms, max_orders_per_token, pin_core |
| `[taker]` | order size, price band, take-profit / stop-loss offsets, max position, cooldown, per-UTC-hour overrides |
| `[maker]` | half/min/max spread, quote size, requote triggers, inventory cap & skew, spread multipliers (tte / skew / momentum / fee), loss stop, aggressive-amend limiter |
| `[fair]` | vol window, default annual vol, `vol_override` |
| `[spot]` | Binance leg (default on), Chainlink leg (default off), staleness threshold |
| `[alpha]` | 100 ms alpha signal (order-book imbalance etc.), disabled by default |
| `[tick]` | tick-size rule: 0.01 inside [0.04, 0.96], else 0.001 |
| `[journal]` | event-journal recording on/off and output dir |

Credentials (`config/credentials.toml`, live only, must be `chmod 600`): private key + funder address. Env `POLYMARKET_PK` / `POLYMARKET_FUNDER` override the file. Never committed.

## Commands

```sh
# Build (release, with live-trading support)
cargo build --release

# Build without the live executor (dry-run only)
cargo build --release --no-default-features

# Run all unit tests
cargo test

# Run live connectivity tests (network + credentials required)
cargo test --test connectivity -- --ignored --test-threads=1

# Dry-run (simulated fills, no orders sent)
./target/release/polyStrat3 --config config/taker.toml --dry-run

# Live trading (set dry_run = false in config; credentials required)
./target/release/polyStrat3 --config config/maker.toml

# Replay a recorded journal deterministically (forces dry-run)
./target/release/polyStrat3 --config config/maker.toml --replay output/journal/journal_btc_<ts>.jsonl
```

Useful flags (each overrides the config): `--symbol BTC|ETH`, `--strategy taker|maker`, `--dry-run`, `--log-level info`, `--pin-core N`.

Replay tip: use the same config the journal was recorded with — replaying maker data through a taker config yields no orders.

## Outputs

| Path | Content |
|---|---|
| `output/<sym>_trades.csv` | every fill (price, size, fee, position after) |
| `output/btc_trader_pnl.txt` | per-window PnL summary |
| `output/journal/*.jsonl` | event journal for replay |

Stopping: `Ctrl-C` cancels all resting orders before exit (awaited).
