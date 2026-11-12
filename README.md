# lpbot

An autonomous liquidity-provision bot for Uniswap v3, Ethereum mainnet first.

It automates one play: put the token you value into a price range that gets crossed often, collect the fee on each crossing, and take the position out when it is back in that token. The full design is in [docs/DESIGN.md](docs/DESIGN.md).

## Status

Scaffold only. Nothing here touches a chain or holds funds yet.

| Works today | Not built yet |
| --- | --- |
| One-command setup of the node and services (`make up`) | Backfill to Parquet, pool ranking, zone map |
| Experiment files: base config, overrides, sweeps, config hashing | Replay engine and result tables |
| Run-input checks (capital and loss limit are required) | Adaptive patience decision and calibrator |
| Uniswap v3 pool math, checked against the reference test vectors | Live subscriptions, reorg handling |
| Pool-state replica: mints, burns, swaps and exact replay of swap events | Executor, Safe roles policy |
| Chain access: JSON-RPC client, chunked log fetching, pool snapshots, discovery | Viewer dashboards |
| `lpbot pools` and `lpbot verify-pool` | |
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

Once the node is synced:

```sh
# The universe's pools, read from the Uniswap v3 factory
cargo run --release -p lpbot -- pools

# Replay a pool's last 2,000 blocks of events and compare the replica with the chain
cargo run --release -p lpbot -- verify-pool <pool address from the list above>
```

`lpbot run <experiment.yml>` resolves and validates the file, then stops with a clear message, because the replay engine is the next thing to build.

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
