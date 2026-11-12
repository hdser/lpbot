# lpbot

An autonomous liquidity-provision bot for Uniswap v3, Ethereum mainnet first.

It automates one play: put the token you value into a price range that gets crossed often, collect the fee on each crossing, and take the position out when it is back in that token. The full design is in [docs/DESIGN.md](docs/DESIGN.md).

## Status

Backtests only so far. The bot reads pools from a node and replays their history; nothing here sends a transaction or holds funds yet.

| Works today | Not built yet |
| --- | --- |
| One-command setup of the node and services (`make up`) | Loading the result tables into Postgres |
| Experiment files: base config, overrides, sweeps, config hashing | Check of simulated fees against real positions |
| Run-input checks (capital and loss limit are required) | Adaptive patience decision and calibrator |
| Uniswap v3 pool math, checked against the reference test vectors | Live subscriptions, reorg handling |
| Pool-state replica: mints, burns, swaps and exact replay of swap events | Executor, Safe roles policy |
| Chain access: JSON-RPC client, chunked log fetching, pool snapshots, discovery | Viewer dashboards |
| `lpbot pools`, `lpbot verify-pool` and `lpbot rank` (pools ranked by implied volatility) | |
| Zone map: passes per day, time in range, return times and per-zone implied volatility (`lpbot zones`) | |
| Backfill: pool history into Parquet, one file per pool and day, with a replay check (`lpbot backfill --verify`) | |
| Simulated pool history to try everything without a node (`lpbot demo-data`) | |
| Replay engine: virtual position, exact fees, gas and swap costs, action latency, loss limit | |
| `lpbot run`: backtests of the zone round trip with the three fixed patience rules, written as the six result tables | |
| Implied-volatility metric, fixed patience rules, loss-limit check | |

The pool math and replica are tested against Uniswap's own test vectors, against themselves, and end to end against a simulated node. They have not yet been run against a real pool. `lpbot verify-pool` is that check, and it is the first thing to run once the node is synced.

## Try it

```sh
make up      # on the home machine: node, Postgres, Prometheus, Grafana (see deploy/README.md)

cargo test --workspace

# Expand an experiment file into its runs
cargo run -p lpbot -- plan experiments/width-and-patience-eth.yml

# Implied volatility from fee tier, daily volume and value within 1% of the price
cargo run -p lpbot -- iv --fee-pct 0.05 --volume 100000000 --depth-1pct 5000000
```

Without a node, on a simulated pool (the price is made up; it shows the commands, not a strategy). `make demo` runs the first and the last of these:

```sh
cargo run --release -p lpbot -- demo-data
cargo run --release -p lpbot -- rank --data-dir data/demo
cargo run --release -p lpbot -- zones 0x5157000000000000000000000000000000000001 --data-dir data/demo --days 7,30
cargo run --release -p lpbot -- run experiments/demo-fixed-rules.yml --data-dir data/demo
```

The last command runs nine backtests (three range widths against always wait, always flip and always close) and writes their result tables under `runs/`.

Once the node is synced:

```sh
# The universe's pools, read from the Uniswap v3 factory
cargo run --release -p lpbot -- pools

# Replay a pool's last 2,000 blocks of events and compare the replica with the chain
cargo run --release -p lpbot -- verify-pool <pool address from the list above>

# Store every universe pool's history from February on under data/lake, then replay it
# against the chain (the example experiment starts in May and looks back 90 days)
make history            # or: cargo run --release -p lpbot -- backfill --from 2026-02-01 --verify

# The stored pools ranked by implied volatility over the last 7 days
cargo run --release -p lpbot -- rank

# A pool's zone map on 7, 30 and 90 day lookbacks
cargo run --release -p lpbot -- zones <pool address>

# Backtest an experiment on the stored history; its window must lie inside what is stored
cargo run --release -p lpbot -- run experiments/width-and-patience-eth.yml
```

`rank`, `zones` and `run` read the stored history. For `rank` and `zones`, add `--node` to read the node directly, without a backfill.

## Layout

```text
crates/
  core/       types, config, experiment files, metrics
  chain/      RPC pool, node ingestion, reorg handling
  venues/     pool math, state replica, call encoding (Uniswap v3 first)
  data/       Parquet sinks, exchange feeds, backfill
  sim/        replay engine, cost models, result tables
  strategy/   strategy and patience-policy interfaces, built-in rules
  risk/       limits and exit paths
  exec/       planner, custody, signer client, submitter, journal
  cli/        the lpbot binary
deploy/       Docker Compose stack and runbook
scripts/      setup and status scripts
config/       shared base configs (YAML)
experiments/  one YAML file per experiment or sweep
docs/         the design
```

## Rules of the house

- Everything is Rust. Experiments are YAML files. There are no notebooks.
- Data, results and secrets never go in Git.
- Contributor and agent conventions are in [CLAUDE.md](CLAUDE.md).
