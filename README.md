# lpbot

An autonomous liquidity-provision bot for Uniswap v3, Ethereum mainnet first.

It automates one play: put the token you value into a price range that gets crossed often, collect the fee on each crossing, and take the position out when it is back in that token. The full design is in [docs/DESIGN.md](docs/DESIGN.md).

## Status

Backtests only so far. The bot reads pools from a node and replays their history; nothing here sends a transaction or holds funds yet.

| Works today | Not built yet |
| --- | --- |
| One-command setup of the node and services (`make up`) | Paper mode |
| Experiment files: base config, overrides, sweeps, config hashing | Paper mode |
| Run-input checks (capital and loss limit are required) | Live subscriptions, reorg handling |
| Uniswap v3 pool math, checked against the reference test vectors | Executor, Safe roles policy |
| Pool-state replica: mints, burns, swaps and exact replay of swap events | Watchdog |
| Chain access: JSON-RPC client, chunked log fetching, pool snapshots, discovery | Scoring decisions against the options not taken |
| `lpbot pools`, `lpbot verify-pool` and `lpbot rank` (pools ranked by implied volatility) | |
| Zone map: passes per day, time in range, return times and per-zone implied volatility (`lpbot zones`) | |
| Backfill: pool history into Parquet, one file per pool and day, with a replay check (`lpbot backfill --verify`) | |
| Fee check: the replica's fees against what real positions collected (`lpbot verify-fees`) | |
| Simulated pool history to try everything without a node (`lpbot demo-data`) | |
| Replay engine: virtual position, exact fees, gas and swap costs, action latency, loss limit | |
| `lpbot run`: backtests of the zone round trip, written as the six result tables | |
| Adaptive patience decision: wait, flip or close by expected result, from the pool's own history | |
| `lpbot calibrate`: fits the adaptive decision walk-forward; it ships only if it beats the three fixed rules out of sample | |
| Viewer: `lpbot load-results` puts the tables in Postgres; three Grafana dashboards read them (runs, run detail, pools and zones) | |
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
cargo run --release -p lpbot -- run experiments/demo-patience.yml --data-dir data/demo
```

The last command runs twelve backtests (three range widths, each with the adaptive decision and with always wait, always flip and always close) and writes their result tables under `runs/`. To fit the adaptive decision and see whether it beats the fixed rules on windows it was not fitted on:

```sh
cargo run --release -p lpbot -- calibrate experiments/demo-patience.yml --data-dir data/demo --fit 8 --score 5
```

On this made-up price waiting wins, because the price always comes back; the calibrator says so and ships the fixed rule.

To look at the results, Postgres and Grafana have to be up. `make viewer` starts just those two, which is what you want on a laptop; `make up` starts them along with the node.

```sh
make viewer             # Postgres and Grafana only
make load               # result tables into Postgres
```

Then open Grafana on http://127.0.0.1:3000, folder `lpbot`, dashboard `lpbot: runs`. Click a run id to open that run in detail with the time range set to its window.

Once the node is synced:

```sh
# The universe's pools, read from the Uniswap v3 factory
cargo run --release -p lpbot -- pools

# Replay a pool's last 2,000 blocks of events and compare the replica with the chain
cargo run --release -p lpbot -- verify-pool <pool address from the list above>

# Store every universe pool's history from February on under data/lake, then replay it
# against the chain (the example experiment starts in May and looks back 90 days)
make history            # or: cargo run --release -p lpbot -- backfill --from 2026-02-01 --verify

# Compare the replica's fees with what real positions in a pool collected
cargo run --release -p lpbot -- verify-fees <pool address>

# The stored pools ranked by implied volatility over the last 7 days
cargo run --release -p lpbot -- rank

# A pool's zone map on 7, 30 and 90 day lookbacks
cargo run --release -p lpbot -- zones <pool address>

# Backtest an experiment on the stored history; its window must lie inside what is stored
cargo run --release -p lpbot -- run experiments/width-and-patience-eth.yml

# Fit the adaptive decision walk-forward and score it against the fixed rules
cargo run --release -p lpbot -- calibrate experiments/width-and-patience-eth.yml
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
