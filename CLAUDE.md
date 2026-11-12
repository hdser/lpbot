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

## Phase 1: what to build next

In this order. Each item lists the gate it must pass.

Status: items 1 and 2 are written and unit tested, including an end-to-end run against a simulated node. Their gates against a real pool are still open: run `lpbot verify-pool` on the owner's machine. The log fetching and pool discovery parts of item 3 exist in `crates/chain`; the Parquet sink does not. Item 4 works straight from the node (`lpbot rank`, with the ranking itself in `lpbot_core::ranking`); it does not yet write the `pool_rank` table or read from local Parquet.

1. **Uniswap v3 math** in `crates/venues` (`uniswap_v3::math`): TickMath, FullMath, SqrtPriceMath, SwapMath, LiquidityAmounts, fee growth inside a range.
   - Gate: unit tests from the reference vectors in the Uniswap v3 test suite pass; a differential test against the deployed pool on a mainnet fork agrees exactly on randomized swaps and mints.
2. **Pool-state replica** in `crates/venues`: rebuild `slot0`, active liquidity, the tick map and fee growth from Initialize, Mint, Burn, Swap and Collect events, with snapshots and an undo log for reorgs.
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

The adaptive patience decision and the calibrator are phase 2 and build on the tables from items 5 and 6.
