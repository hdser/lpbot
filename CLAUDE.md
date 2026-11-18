# lpbot: working notes for Claude sessions

Read `docs/DESIGN.md` before changing anything. It is the source of truth for what this bot does and why. This file covers how to work in the repo and what to build next.

## What this is

An autonomous Uniswap v3 liquidity-provision bot, Ethereum mainnet first. The play: enter a high-crossing price zone one-sided in the value token, earn the fee on each pass, exit back in the value token. When price runs beyond the range, an adaptive patience decision chooses between waiting, flipping and closing.

## Decisions that are settled

Do not reopen these without the owner asking.

- **Language:** all Rust. No Python, no notebooks. The only non-Rust piece will be a small TypeScript script that applies the Safe roles policy.
- **Experiments:** every run (backtest, paper, live) is one YAML file in `experiments/` over a base config in `config/`. No experiment-specific code.
- **Results:** every run writes the same six tables: `runs`, `campaigns`, `decisions`, `series`, `pool_rank`, `zone_map`. The viewer (Grafana) knows only these tables.
- **Universe:** WETH, WBTC, USDC, USDT. A pool qualifies only if both tokens are listed.
- **Ranking:** implied volatility from fee, volume and in-range liquidity (not total liquidity), at the price and per zone.
- **Run inputs:** capital, loss limit (percent, in the value token) and autonomy (`auto` or `approve`). Capital and loss limit have no default; a run refuses to start without them. Backtests always run as `auto`.
- **Adaptive settings:** zone choice, width, fee tier and the patience decision are calibrated walk-forward and must beat the fixed rules (always wait, always flip, always close) out of sample, or the fixed rule ships.
- **Custody:** a Safe with the Zodiac Roles module. No custom contract.
- **Chain access:** a local Reth node feeds every decision. Public RPC endpoints are for cross-checks, exit-only fallback and one-time backfill. No paid RPC.
- **No arbitrage-loss theory in the decision logic.** It may appear only as an optional diagnostic in results.

## Commands

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p lpbot -- plan experiments/width-and-patience-eth.yml
```

All four must pass before a commit. Clippy runs with warnings as errors.

## Conventions

- **Exact math is integer math.** Pool math (ticks, sqrt prices, liquidity, fee growth) is ported bit-for-bit from the Uniswap v3 contracts using 256-bit integers. Floating point is allowed only for signals, metrics and reporting, never for anything that decides token amounts.
- **Mind token order.** Uniswap orders a pool's tokens by address, and ticks are in token1-per-token0. On mainnet USDC/WETH has USDC as token0, so "ETH priced higher in dollars" is a lower tick. Code works in tick terms (`EntrySide::Above` or `Below`); convert at the edges and test both orders.
- **Strategies declare, they do not act.** A strategy returns `Intent`s. Sizing limits, transaction building and safety live outside it.
- **Same code in every mode.** Anything that would behave differently in backtest and live is a bug unless it is the clock, the data source or the executor.
- **Tests come with the code.** Every public function gets unit tests. Pool math additionally gets differential tests against the deployed contracts on a fork.
- **Typed config, tolerant parsing.** Type the settings the code uses; carry the rest through `extra` maps so the YAML can grow ahead of the code.
- **Errors say what to do.** A refusal to start names the missing input.
- **Never commit** data, results, keystores, `.env` files or real addresses of the owner's Safe. Config uses `"0x..."` placeholders.

## Workflow

- Work on a branch and open a pull request against `main`. Do not push to `main` directly.
- Keep `docs/DESIGN.md` in step: a change of behaviour updates the design in the same pull request.
- Update the status table in `README.md` when something moves from "not built" to "works".

## What can run where

- **Cloud sessions** can build, lint and run unit tests. They cannot reach RPC endpoints or exchange APIs, so they cannot backfill data or run real backtests.
- **The owner's machine** runs everything that needs chain data, the local node, or funds.

Write code so the data-dependent parts take their input from files or traits, and keep unit tests free of network access.

## The node machine

The owner runs the stack on a NUC (Ubuntu Server 26.04, 4 TB NVMe, 32 GB of memory), set up with `deploy/REMOTE.md`. Cloud sessions cannot reach it: the owner runs the commands and pastes the output. `deploy/RUNBOOK.md` is what he types there: long jobs run in `tmux`, services come back after a restart and jobs do not. Give him commands from it, and keep it in step when a job or its resume command changes.

- Done there, November 2026: `scripts/host-setup.sh`, a 3.4 TB data volume at `/data`, `make env`.
- Under way: `make snapshot` (archive, 1.72 TB to download, 2.88 TB unpacked). On 17 November 2026 the machine went off in the middle of it at about 13:36 UTC. Its journal just stops there, with no shutdown and no heat or hardware warning, on a nearly idle machine: the power was cut (a power cut, a loose plug or the button held down), not a short press. It stayed off until 17:50. The first resume, with a script that did not name a snapshot, started on a newer one (26428655 instead of 26414278); the owner stopped it and resumed 26414278 by hand with `--manifest-url`. `make snapshot` now writes the manifest URL it began with to `DATA_DIR/reth-snapshot-manifest` and resumes that one. At 17:51:19, 48 seconds after it was switched on, a short press of the box's own button (no keyboard was attached) switched it off in an orderly way (`Power key pressed short`), and it was on again at 17:51:44; the owner does not remember switching it off. Both stops were first put down to the button: after the double start, `journalctl -b -1` showed the 50-second start, not the one that went off at 13:36. The `HandlePowerKey=ignore` setting from `deploy/REMOTE.md` was not applied then. The owner wrote it at 18:58, after that boot, and the login service was not told to read it, so on 18 November it still answered `poweroff` until the owner reloaded it at about 16:20 UTC; `deploy/REMOTE.md` now reloads the service and checks the answer. The setting does nothing against a power cut. Three resumes after that were stopped with Ctrl-C during Reth's silent check of the 2.1 TB on disk, and the script then said "The snapshot is in place"; it now says when the download did not finish (`*.part` files left in `.download-cache`). The resume of 26414278 started at 18:50 UTC: the check took about an hour, reading at about 400 MB/s, and kept what was on disk; downloading then ran at 10 to 17 MB/s (the server's link is gigabit, so the limit is upstream). By 14:35 UTC on 18 November only the last archive was left, `rocksdb_indices.tar.zst` (252 GB, 303 GB unpacked), about 90% down. At 15:24 the connection dropped and Reth started that archive again from zero ("Archive attempt failed, retrying from scratch"). The server does answer range requests (206, `accept-ranges: bytes`), whatever Reth's "Server does not support Range requests" line says: at 16:21 another drop resumed from 35 GB ("Download interrupted, retrying"). Reth retries by itself, ten times. It was 31 of 252 GB at 16:15 UTC at 10 MB/s, about six hours to go plus the unpacking. Reth's "GB" are GiB and its estimate runs short. `make snapshot` now refuses once the node has been started (`make up` writes `DATA_DIR/reth-node-started`), and `make up` refuses to start the node while a download is running or unfinished.
- Running there: Postgres, Prometheus and Grafana (`make viewer`), reached from the laptop with `make tunnel` on port 3001.
- Not yet run there: `make up` with the node, and `make gates` (every check against real chain data).
- The start-up script creates the six result tables empty (`deploy/postgres/schema.sql`, generated from `lpbot_sim::tables` and kept in step by a test), so dashboards on a database without runs show no data instead of an error.
- The owner chose to keep the full archive for now and prune later. Do not reopen that; do keep an eye on disk space, which is tighter than first estimated.
- Known hardware limits: the drive is a Kingston NV2 (no memory cache; it measured 45k read and 15k write IOPS, falling to about half within ten minutes), and the box runs hot (NVMe near its critical temperature when idle). If the node falls behind, suspect these before the software.

On the owner's laptop, `make demo`, `make viewer` and `make load` are tested, and the dashboards were opened in Grafana.

## Phase 1: what to build next

In this order. Each item lists the gate it must pass.

Status: all seven items are written and unit tested, and `experiments/demo-patience.yml` runs end to end on a simulated pool history (`lpbot demo-data`, `lpbot_data::synth`). None of it has run against a real node yet, so these gates are still open on the owner's machine. `make gates` (`lpbot gates`, in `crates/cli/src/gates_cmd.rs`) runs them all in order once the node is synced, then the backtest, the load and the calibration; it stops at the first step that fails and keeps its output under `logs/`. Its steps against a node are tested with a mock node only; ask the owner for the summary it prints.

- Items 1 and 2: `lpbot verify-pool`; the `replica` step runs it on every pool of the universe.
- Item 3: `lpbot backfill --verify`, and the `row-count` step, which compares the newest complete stored day of the traded pool log for log with a public endpoint (`lpbot_data::cross_check`). It stays open when no endpoint answers.
- Item 6: `lpbot verify-fees <pool>` on a busy pool with a few months stored. It follows every position that the pool's `positions()` shows empty at the start of the stored history and checks that what it collected plus what the pool still owes it at the end equals the replica's fee growth inside its range (`lpbot_data::fee_check`). It needs the node (an archive node for an old history); without one, agreements count as unverified and the gate stays open. This checks the replica's fee accounting, which the engine's fee share is built on.
- Item 7: `lpbot run experiments/width-and-patience-eth.yml` on real history.

The viewer exists too: `lpbot load-results` (`lpbot_sim::loader`) creates the Postgres tables from the Parquet files and replaces a run's rows on reload; the dashboards are in `deploy/grafana/dashboards/`. All three were opened in Grafana 13.3 on the owner's laptop with the demo runs loaded (`make viewer`, `make load`), and every panel renders. Open a run from its id on the runs dashboard: the link sets the time range to the run's window.

Where things ended up: the ranking is in `lpbot_core::ranking`, the entry-range choice in `lpbot_strategy::zone_round_trip`, the engine and tables in `lpbot_sim`. The run's `pool_rank` and `zone_map` tables hold what the engine looked at, not the full maps. Gas is charged at the fixed price in the `costs` block, and positions are marked at the pool price, until block headers and exchange feeds are stored.

1. **Uniswap v3 math** in `crates/venues` (`uniswap_v3::math`): TickMath, FullMath, SqrtPriceMath, SwapMath, LiquidityAmounts, fee growth inside a range.
   - Gate: unit tests from the reference vectors in the Uniswap v3 test suite pass; a differential test against the deployed pool on a mainnet fork agrees exactly on randomized swaps and mints. The fork test is not written yet; until it is, `lpbot verify-pool` against the node is the only check independent of the code.
2. **Pool-state replica** in `crates/venues`: rebuild `slot0`, active liquidity, the tick map and fee growth from Initialize, Mint, Burn, Swap and Collect events, with snapshots and an undo log for reorgs. The undo log is not built: stored days close only at the finalized block, where no reorg reaches.
   - Gate: after replaying a block range, state equals on-chain `slot0`, `liquidity` and `ticks()` at the end block.
3. **Backfill** in `crates/data`: pool discovery from the factory, events and block headers for qualifying pools into Parquet, partitioned by pool and day. Chunked `eth_getLogs` first; node-local extraction later.
   - Gate: a re-run is idempotent, and row counts match an independent source for a sample day.
4. **Pool ranking** in `crates/data`: the `pool_rank` table from fee, volume (24 h and 7 d) and in-range liquidity, with the floors from `universe.floors`.
   - Gate: `lpbot status` (or a `rank` subcommand) prints the ranked universe from local data.
5. **Zone map** in `crates/data`: passes per day, time in range and return-time distribution per candidate range on 7, 30 and 90 day lookbacks, plus per-zone implied volatility.
   - Gate: unit tests on synthetic price paths with known pass counts.
6. **Replay engine and result tables** in `crates/sim`: virtual positions with exact fee accrual, gas and swap costs, one-block-or-more action latency, and the six result tables written as Parquet.
   - Gate: fees collected by real on-chain positions in the pool are reproduced within 0.1%.
7. **`lpbot run`** wired to the replay engine for `mode: backtest`, with the three fixed patience rules.
   - Gate: `experiments/width-and-patience-eth.yml` runs end to end for the fixed rules and writes all six tables.

## Phase 2: where it stands

A first adaptive patience decision and a walk-forward calibrator exist and run on simulated history.

- The comparison is in `lpbot_strategy::adaptive`; the figures it compares are worked out in `lpbot_sim::engine` (`adaptive_decision`); the odds come from `lpbot_data::odds` (how often price made a move of that size within the horizon, counted over the lookback).
- `lpbot calibrate` fits horizon, margin and reinvest credit on one window and scores on the next against the three fixed rules. The adaptive decision ships only if it wins out of sample, summed over the folds; a tie ships the fixed rule.

Known gaps, in the order they matter:

1. The odds are unconditional. They do not depend on how far price already is from the range, how long it has been away, or the long-horizon state. Condition them before trusting the decision on real data.
2. Decisions are not scored afterwards against the two options not taken (the `decisions` table has the expectations, not the outcomes).
3. The calibrator fits the patience settings only. Width and fee tier are compared by sweeping them; zone-ranking weights and entry shape are not searched.
4. A run trades one pool with one campaign at a time; `allocator` settings in the config are not used yet.

Nothing in phase 2 has seen real data. Expect the defaults to be wrong.
