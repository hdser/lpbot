# lpbot

An autonomous liquidity-provision bot for Uniswap v3, Ethereum mainnet first.

It automates one play: put the token you value into a price range that gets crossed often, collect the fee on each crossing, and take the position out when it is back in that token. The full design is in [docs/DESIGN.md](docs/DESIGN.md).

## Status

Scaffold only. Nothing here touches a chain or holds funds yet.

| Works today | Not built yet |
| --- | --- |
| Experiment files: base config, overrides, sweeps, config hashing | Pool math and state replica |
| Run-input checks (capital and loss limit are required) | Backfill, pool ranking, zone map |
| Implied-volatility metric | Replay engine and result tables |
| Fixed patience rules (wait, flip, close) | Adaptive patience decision and calibrator |
| Loss-limit check | Live ingestion, executor, Safe roles policy, viewer |

## Try it

```sh
cargo test --workspace

# Expand an experiment file into its runs
cargo run -p lpbot -- plan experiments/width-and-patience-eth.yml

# Implied volatility from fee tier, daily volume and value within 1% of the price
cargo run -p lpbot -- iv --fee-pct 0.05 --volume 100000000 --depth-1pct 5000000
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
config/       shared base configs (YAML)
experiments/  one YAML file per experiment or sweep
docs/         the design
```

## Rules of the house

- Everything is Rust. Experiments are YAML files. There are no notebooks.
- Data, results and secrets never go in Git.
- Contributor and agent conventions are in [CLAUDE.md](CLAUDE.md).
