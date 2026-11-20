# lpbot: working notes for Claude sessions

Read `docs/DESIGN.md` before changing anything. It is the source of truth for what this bot does and why. This file covers how to work in the repo and what to build next.

## What this is

An autonomous Uniswap v3 liquidity-provision bot, Ethereum mainnet first. The play: enter a high-crossing price zone one-sided in the value token, earn the fee on each pass, exit back in the value token. When price runs beyond the range, an adaptive patience decision chooses between waiting, flipping and closing.

## Decisions that are settled

Do not reopen these without the owner asking.

- **Language:** all Rust. No Python, no notebooks. The only non-Rust piece will be a small TypeScript script that applies the Safe roles policy.
- **Experiments:** every run (backtest, paper, live) is one YAML file in `experiments/` over a base config in `config/`. No experiment-specific code.
- **Results:** every run writes the same six tables: `runs`, `campaigns`, `decisions`, `series`, `pool_rank`, `zone_map`. Besides them, two pool tables, keyed by pool and not by run, hold what each pool looked like over its stored history: `pool_series` and `pool_liquidity`, from `lpbot pool-stats` (added at the owner's request on 19 November 2026). Five live tables hold the followed pools now, kept current from the node by `lpbot watch-pools`: `live_pools`, `live_pool_series`, `live_liquidity`, `live_swaps`, `live_positions` (added at the owner's request on 20 November 2026). The viewer (Grafana) knows only these thirteen tables.
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
- Done, 17 to 19 November 2026: `make snapshot` (archive snapshot 26414278, 1.72 TB to download, 2.88 TB unpacked). What it taught, each now handled by the scripts or the runbooks:
  - The machine went off twice. At about 13:36 UTC on 17 November the journal just stops, with no shutdown: a power cut, not the button. At 17:51 a short press of the box's own button switched it off. `HandlePowerKey=ignore` works only once the login service reads it; `deploy/REMOTE.md` reloads it and checks. After a double start `journalctl -b -1` is the wrong boot: `deploy/RUNBOOK.md` lists the boots first.
  - Reth checks what is on disk for about an hour before downloading and prints nothing meanwhile. Ctrl-C then made the old script say "The snapshot is in place"; it now looks for `*.part` files left in `.download-cache`.
  - Reth v2.10.1 starts an archive again from zero when the server answers a resume with the whole file (its "Server does not support Range requests" line is wrong: the server serves ranges). The last archive, `rocksdb_indices.tar.zst` (252 GB), went back to zero three times. It was fetched with a `curl -C -` loop instead (curl refuses such an answer), unpacked into `DATA_DIR/reth`, and a last `make snapshot` verified all 3,176 archives (`needs_download=0`) and wrote `reth.toml`.
  - Downloads ran at 10 to 17 MB/s, an upstream limit (the link is gigabit). Reth's "GB" are GiB and its estimates run short.
- Running there since 19 November 2026: the node (`make up` at 07:22 UTC, synced at 13:34 after executing the 33,241 blocks since the snapshot), Lighthouse (checkpoint sync, then old history in the background), Postgres, Prometheus and Grafana, reached from the laptop with `make tunnel` on port 3001. `make up` wrote `DATA_DIR/reth-node-started`, so `make snapshot` now refuses to run.
- `make gates` ran there on 19 November 2026; see Phase 1 for the results and Phase 2 for what the backtest showed. The pool history is in `DATA_DIR/lake`.
- The start-up script creates the six result tables empty (`deploy/postgres/schema.sql`, generated from `lpbot_sim::tables` and kept in step by a test), so dashboards on a database without runs show no data instead of an error.
- Running there since 20 November 2026, 01:17 UTC: the watcher (`lpbot watch-pools`), following all 24 pools from the history since 31 January 2025. Its first start took about 25 minutes, most of it storing the position manager's 2025 days, then about 5 to replay 658 days and 3 to read every NFT; a restart from its checkpoint takes seconds. It holds about 13,000 pool positions and 30,000 NFTs in 250 MB, attributes 99.8 to 100% of every pool's active liquidity, and what it says NFT owners could collect matched the position manager's own `collect` to the last unit. Until the pull request merges it runs from an image built in `~/lpbot-live`, a worktree of the branch; after the merge, `make watch-build` in `~/lpbot` rebuilds it from there, and `git worktree remove ~/lpbot-live` tidies up.
- The owner chose to keep the full archive for now and prune later. Do not reopen that; do keep an eye on disk space, which is tighter than first estimated.
- Manage the containers only with the compose file in `~/lpbot`. Compose resolves `./jwt` and the other relative mounts against the compose file's own folder, so running a branch's compose file from another checkout (on 20 November 2026, `~/lpbot-live`) recreates Reth with an empty engine-secret folder: Lighthouse can no longer reach it. Recreating Reth from `~/lpbot` fixed it; Reth then spent several minutes unwinding its last blocks before it served again. To try a branch's service there, build its image and start the container on its own (`docker run`, or `--no-deps` with the right `--project-directory`), never `up` with dependencies.
- Known hardware limits: the drive is a Kingston NV2 (no memory cache; it measured 45k read and 15k write IOPS, falling to about half within ten minutes), and the box runs hot. The CPU sits at 90 to 100 °C and throttles under a load of two or three cores even with turbo off (`no_turbo=1`, set by hand, lost on a restart); the motherboard's sensor reaches 100 °C, and the kernel switches the machine off at 105 °C. The NVMe went from about 70 to 52 °C once the owner added ventilation on 19 November, but the CPU did not change, so its own cooler (fan, dust, the BIOS fan profile) is the suspect. Build there with `CARGO_BUILD_JOBS=2`. If the node falls behind, suspect these before the software.

On the owner's laptop, `make demo`, `make viewer` and `make load` are tested, and the dashboards were opened in Grafana.

## Phase 1: what to build next

In this order. Each item lists the gate it must pass.

Status: all seven items are written and unit tested, and `experiments/demo-patience.yml` runs end to end on a simulated pool history (`lpbot demo-data`, `lpbot_data::synth`). On 19 November 2026 they ran against the owner's synced node with `make gates`, and items 1 to 3, 6 and 7 pass: the replica matched all 24 pools over 2,000 blocks, the stored history (24 pools from 31 January, 13.9 million logs) replays to every pool's state on the chain, and 6,135 emptied positions on WETH/USDT 0.3% collected exactly what the replica computed. `make gates` (`lpbot gates`, in `crates/cli/src/gates_cmd.rs`) runs them all in order once the node is synced, then the backtest, the load and the calibration; it stops at the first step that fails and keeps its output under `logs/`. Its steps against a node are tested with a mock node only; ask the owner for the summary it prints.

- Items 1 and 2: `lpbot verify-pool`; the `replica` step runs it on every pool of the universe.
- Item 3: `lpbot backfill --verify` (the `history` step), which stores the history and replays every pool to its state on the chain. The backfill asks for each day of all pools in one request (`backfill_pools`). A row-count against a public endpoint was removed on 19 November 2026 at the owner's decision: keyless endpoints no longer serve older logs, and the replay already shows that nothing is missing.
- Item 6: `lpbot verify-fees <pool>` on a busy pool with a few months stored. It follows every position that the pool's `positions()` shows empty at the start of the stored history and checks that what it collected plus what the pool still owes it at the end equals the replica's fee growth inside its range (`lpbot_data::fee_check`). It needs the node (an archive node for an old history); without one, agreements count as unverified and the gate stays open. This checks the replica's fee accounting, which the engine's fee share is built on.
- Item 7: `lpbot run experiments/width-and-patience-eth.yml` on real history.

The viewer exists too: `lpbot load-results` (`lpbot_sim::loader`) creates the Postgres tables from the Parquet files and replaces a run's rows on reload; the dashboards are in `deploy/grafana/dashboards/`. All three were opened in Grafana 13.3 on the owner's laptop with the demo runs loaded (`make viewer`, `make load`), and every panel renders. Open a run from its id on the runs dashboard: the link sets the time range to the run's window.

Where things ended up: the ranking is in `lpbot_core::ranking`, the entry-range choice in `lpbot_strategy::zone_round_trip`, the engine and tables in `lpbot_sim`. The run's `pool_rank` and `zone_map` tables hold what the engine looked at, not the full maps. Gas is charged at the price of the block an action lands in (`costs.gas: chain`, from `lpbot gas-history`; the runs before 20 November 2026 paid a fixed 1 gwei), and positions are marked at the pool price until exchange feeds are stored. The engine decides and counts block by block; what runs on a clock is the strategy's own pace (`entry.check_every`, `patience.decide_every`) and the signals (`signal_step`, `entry.vol_window`, zone lookbacks as durations), each settable down to one block. Intraday runs set them to a minute; an experiment's `set` block shortens `sim.series_every` for the charts.

1. **Uniswap v3 math** in `crates/venues` (`uniswap_v3::math`): TickMath, FullMath, SqrtPriceMath, SwapMath, LiquidityAmounts, fee growth inside a range.
   - Gate: unit tests from the reference vectors in the Uniswap v3 test suite pass; a differential test against the deployed pool on a mainnet fork agrees exactly on randomized swaps and mints. The fork test is not written yet; until it is, `lpbot verify-pool` against the node is the only check independent of the code.
2. **Pool-state replica** in `crates/venues`: rebuild `slot0`, active liquidity, the tick map and fee growth from Initialize, Mint, Burn, Swap and Collect events, with snapshots and a way back from reorgs. Stored days close only at the finalized block, where no reorg reaches; the live follower (`lpbot_data::live`) keeps the finalized state and the blocks past it, and rebuilds from there on a reorg.
   - Gate: after replaying a block range, state equals on-chain `slot0`, `liquidity` and `ticks()` at the end block.
3. **Backfill** in `crates/data`: pool discovery from the factory, events and block headers for qualifying pools into Parquet, partitioned by pool and day. Chunked `eth_getLogs` first; node-local extraction later.
   - Gate: a re-run is idempotent, and the stored history replays to every pool's state on the chain at the end block.
4. **Pool ranking** in `crates/data`: the `pool_rank` table from fee, volume (24 h and 7 d) and in-range liquidity, with the floors from `universe.floors`.
   - Gate: `lpbot status` (or a `rank` subcommand) prints the ranked universe from local data.
5. **Zone map** in `crates/data`: passes per day, time in range and return-time distribution per candidate range on 7, 30 and 90 day lookbacks, plus per-zone implied volatility.
   - Gate: unit tests on synthetic price paths with known pass counts.
6. **Replay engine and result tables** in `crates/sim`: virtual positions with exact fee accrual, gas and swap costs, one-block-or-more action latency, and the six result tables written as Parquet.
   - Gate: fees collected by real on-chain positions in the pool are reproduced within 0.1%.
7. **`lpbot run`** wired to the replay engine for `mode: backtest`, with the three fixed patience rules.
   - Gate: `experiments/width-and-patience-eth.yml` runs end to end for the fixed rules and writes all six tables.

## Live pools

Added on 20 November 2026 at the owner's request: every stored pool followed from the local node block by block, with every position, for the `lpbot: live` dashboard. It runs as the `watcher` service of `deploy/docker-compose.yml` (`make up`, `make watch-logs`, `make watch-build`); `deploy/RUNBOOK.md` has the rest.

- `lpbot_data::positions`: the position book. Pool positions by owner and range, and position NFTs by token, followed as the contracts do; NFT events pair with the manager's pool event before them in the same transaction. A position first met is read from the node at the block before (`Wanted`, `Seed`), except one minting into a range no one holds.
- `lpbot_data::manager`: the position manager's logs, stored by day under `<lake>/manager`, fetched four days at a time.
- `lpbot_data::live`: the live state (replicas, book, minute bars, swaps), `replay_history`, the `Follower` (finalized base, tail of blocks with hashes, rebuild on a reorg) and the checkpoint under `<lake>/live`.
- `lpbot_sim::live_tables` and `lpbot_sim::live_db`: the five tables, their keys and upserts; `deploy/postgres/schema.sql` carries them, kept in step by the same test.
- `crates/cli/src/watch_cmd.rs`: the command, the dollar prices (the pool-stats rules, with WBTC/WETH priced through USDC/WETH), the rows and the writes.

Known gaps: a position that held liquidity before the stored history began and that nobody touched since is never seen, so its liquidity is unattributed (`live_pools.known_share` says how much is); the follower polls HTTP rather than taking ExEx or WebSocket notifications; it exports no Prometheus metrics yet, so "is it current" is read from the tables.

## Phase 2: where it stands

A first adaptive patience decision and a walk-forward calibrator exist and run on simulated history.

- The comparison is in `lpbot_strategy::adaptive`; the figures it compares are worked out in `lpbot_sim::engine` (`adaptive_decision`); the odds come from `lpbot_data::odds` (how often price made a move of that size within the horizon, counted over the lookback).
- `lpbot calibrate` fits horizon, margin and reinvest credit on one window and scores on the next against the three fixed rules. The adaptive decision ships only if it wins out of sample, summed over the folds; a tie ships the fixed rule.

Known gaps, in the order they matter:

1. The odds are unconditional. They do not depend on how far price already is from the range, how long it has been away, or the long-horizon state. Condition them before trusting the decision on real data.
2. Decisions are not scored afterwards against the two options not taken (the `decisions` table has the expectations, not the outcomes).
3. The calibrator fits the patience settings only. Width and fee tier are compared by sweeping them; zone-ranking weights and entry shape are not searched.
4. A run trades one pool with one campaign at a time; `allocator` settings in the config are not used yet.
5. Entries are one-sided or around the price (`entry.shape`), with an optional move back around the price once price has left (`exit.recenter_after`). Both still carry the trend against the value token while a position is wholly in the other token; hedges are designed but not built. A ladder of adjacent ranges is one range in v3 maths and is not built as a separate shape.
6. Four months of trading history (plus the lookback) is too little to fit and score the gates walk-forward: backfill earlier history first.

The first run on real data (`make gates`, 19 November 2026) traded WETH/USDT 0.3%, the ranked pool, from May to October: all 16 runs of `width-and-patience-eth` stopped at the 5% loss limit whatever the patience rule, and always-wait shipped in every calibration. The width sweep (10, 20, 40, 80 ticks) is only two widths on that pool, whose tick spacing is 60: widths round up to 60 and 120.

The first research round on that history is in `docs/RESEARCH.md` (about 800 runs, each counted in ETH and in USD). In short: a one-sided campaign is a directional bet against the value token that earns fees, and the trend moved results 15 to 20 points while fees moved them a few; exiting at every excursion or chasing the price is churn; a longest wait of three days is worse than waiting; the new gates (`entry.max_vol_to_width`, `exit.max_vol_to_width`, `entry.min_gas_multiple`, `patience.max_wait`) helped in sample and not out of sample, so they stay off by default and the plain `wait` rule ships. Expect the defaults to be wrong.

The second round (20 November 2026, `docs/RESEARCH.md`) ran 520 runs over May 2025 to October 2026 on the longer store (from 31 January 2025): around-price entries lost in every configuration, and moving them back around the price made it worse; stops and short holding horizons with immediate re-entry were churn; the one family that held up in both halves was entering after a fast move (3% in four hours) on the side that bets on it stalling, with a day's limit and a 2% stop. Following real providers (`lpbot lp-positions`, `strategy.follow`) is built.

The third round (20 November 2026, `docs/RESEARCH.md`) charged gas at each block's price and went intraday. Round 2's ranking held at real gas, a little worse (the strategies paid 1.7 to 6.2 gwei on average, more than the 1 gwei assumed: they act most in 2025 and at volatile hours); the fast-move entry still held up on USDC/WETH 0.05%. Intraday positions next to the price on a clock lost everywhere, at $100,000 and in the cheap-gas months too: price moving through the range cost more than the fees, before gas. The 18 revert.finance providers did not beat holding their deposits once priced at the block (the intraday ones by +0.01% and −0.04% a position; hourly prices had shown +0.15%), and copying them with one value token costs about 0.12% a position in swaps and gas. Copying an intraday provider would need a run that keeps both tokens between positions; it is not built.
