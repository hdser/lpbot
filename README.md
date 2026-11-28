# lpbot

An autonomous liquidity-provision bot for Uniswap v3, Ethereum mainnet first.

It automates one play: put the token you value into a price range that gets crossed often, collect the fee on each crossing, and take the position out when it is back in that token. The full design is in [docs/DESIGN.md](docs/DESIGN.md).

## Status

Backtests, and the pools followed live. The bot reads pools from a node, replays their history and follows them block by block; nothing here sends a transaction or holds funds yet.

| Works today | Not built yet |
| --- | --- |
| One-command setup of the node and services (`make up`) | Paper mode |
| Experiment files: base config, overrides, sweeps, config hashing | Differential test of the pool math against a mainnet fork |
| Run-input checks (capital and loss limit are required) | Live trading (`lpbot live`) |
| Uniswap v3 pool math, checked against the reference test vectors | The Safe roles policy as code, and its fork test |
| Pool-state replica: mints, burns, swaps and exact replay of swap events | Watchdog |
| Chain access: JSON-RPC client, chunked log fetching, pool snapshots, discovery | Scoring decisions against the options not taken |
| `lpbot pools`, `lpbot verify-pool` and `lpbot rank` (pools ranked by implied volatility) | The impact fill model (entries are capped instead) |
| Zone map: passes per day, time in range, return times and per-zone implied volatility (`lpbot zones`) | |
| Paper mode (`lpbot paper`): a forward experiment at the head with virtual positions, the engine fed block by block and checked against a replay of the same blocks | |
| The seam to the executor: every change the engine makes recorded as a step; the planner turns an action's steps into bounded Uniswap calls batched through MultiSend and the roles module, checked on a fork of mainnet against the deployed contracts | |
| The keeper's signer (`lpbot signer`), the executor's journal, its submitter (nonce, replacement of a stuck transaction within the gas cap) and reconciliation (the position read back after a transaction) | |
| Risk checks (width, price gaps, swap impact, slippage, gas, actions per day, node lag, replica against the chain, sleeve caps) and the pause flag (`lpbot pause`, `exit-all`, `resume`) | |
| `lpbot select list`, `approve` and `check`, and the ladder every night (`deploy/selection/nightly.sh`) | |
| Backfill: pool history into Parquet, one file per pool and day, with a replay check (`lpbot backfill --verify`) | |
| Daily update of the stored history and gas prices from where the store ends (`lpbot backfill --resume`, `deploy/history/daily.sh` from cron); run from the laptop against the node, not yet in the node's crontab | |
| Fee check: the replica's fees against what real positions collected, confirmed with the node's position records (`lpbot verify-fees`) | |
| One command for every check against real chain data, then the backtest, the load and the calibration (`make gates`) | |
| Simulated pool history to try everything without a node (`lpbot demo-data`) | |
| Replay engine: virtual position, exact fees, gas and swap costs, action latency, loss limit, decided and counted block by block | |
| `lpbot gas-history`: each block's base fee and priority fee from the node; backtests charge every action the gas of its block | |
| `lpbot run`: backtests of the zone round trip, written as the six result tables, with results in the value token and in dollars | |
| Entries one-sided or around the price (`entry.shape`), with an optional move back around the price once price has left (`exit.recenter_after`) | |
| Per-campaign exits (time limit, stop from entry, stop from the best, leaving on the return after a deep excursion), an entry signal on fast moves, ranges sized by volatility | |
| `lpbot lp-positions`: a provider's positions from the node's position manager records; backtests that follow them (`strategy.follow`) | |
| The agent mode: a run decided look by look by a person or a session (`lpbot agent next`, `lpbot agent decide`), replayed from its decisions; forward at the head of the chain (`window: { to: open }`), the store brought up to the node's head before each look and the next look waited for by the wall clock (`agent next --wait`) | |
| [Server Codex coordinator](docs/CODEX_RUNNER.md): due scheduling, fenced writers, isolated CLI deciders, durable recovery, blind-first manual-workflow reviews and daily reporting; peer handover accepts only completed score horizons | Server activation requires the pinned tools-off host, dedicated authentication and verified cutover |
| Panels of agents: one run per agent (`agent.name`, `agent.model`, `agent.peers`), a shared JSON record a look with what it saw, decided and expected, peers' records handed over only up to each look (`lpbot agent next`, `lpbot agent peers`), every decision scored against cash and holding (`lpbot agent evaluate`), the brief for each session (`lpbot agent brief`), the agents' looks and scores in the viewer (`agent_looks`, `agent_scores`, the `Agents` dashboard), and in forward and live panels agents asking each other at a look through a shared board (`lpbot agent ask`, `inbox --wait`, `answer`, `answers --wait`, `board`) | Reviews blind first, strategy notes, lockstep panels |
| `lpbot supervisor status`: every forward agent on the machine, whatever runs it: its state (waiting, deciding, late, no waiter, stopped, scheduler down, paused, ...), its next look's due time, what waits for it and what schedules it, from its files, `/proc` and the agent list (`data/agents/agents.yml`); `agent_status` and the agents dashboard's Now row, every minute | One supervisor running every agent the same way (designed); alerts |
| Long timeframe, short entry: `regime` gates on the trend, the straightness and the band of days to months (`regime.trend_min_pct`, `regime.max_efficiency`, `regime.band_min`, ...), and a trend exit (`exit.trend_min_pct`) | |
| Agent setups: a mandate in the brief (`agent.mandate`: trend and four other wordings of it, range, fade, patient; each a text file in `crates/cli/mandates/`), so a panel sets one model on several mandates or several models on one | |
| `lpbot select`: a rule's settings scored walk-forward against cash, the average and hindsight (`walk-forward`, also by the values of one swept setting and across pools); rules frozen and run forward (`window: { to: open }`); and the ladder that holds every forward run, rule or agent, against the owner's gates and shares a track's capital among those that pass (`ladder`) | The live rungs of the ladder; the ladder on a schedule and in the viewer |
| Adaptive patience decision: wait, flip or close by expected result, from the pool's own history | |
| `lpbot calibrate`: fits the adaptive decision walk-forward; it ships only if it beats the three fixed rules out of sample | |
| Viewer: `lpbot load-results` puts the tables in Postgres (and `lpbot load-looks` an agent panel's newest looks, every minute on the node); ten Grafana dashboards read them, in five subjects with the same menu on each: the Overview Grafana opens on (the node, the watcher, every agent, what needs attention, the busiest pools, the newest experiments); Agents, with each agent's state, result, scores, stances and every look's reasoning side by side; Pools: live and history; Runs: experiments, one run (the price, the range shaded by state, time in and out of range, APR and fee APR), compare, and pool and range choice; System: machine and node, and Reth internals. Alerts go to Telegram, each with what to do (`deploy/grafana/alerting`, `deploy/RUNBOOK.md`). `make dashboards-preview` shows a change before it reaches the node | |
| `lpbot pool-stats`: each pool's price, value held, volume and fees hour by hour, and the liquidity around the price day by day, for the pool dashboard | |
| `lpbot watch-pools` (the watcher service): every pool followed from the node block by block, with reorgs taken back; every position and position NFT with its owner, range, amounts, fees and when it opened and closed; the live tables and the live dashboard | |
| Implied-volatility metric, fixed patience rules, loss-limit check | |

The pool math and replica are tested against Uniswap's own test vectors, against themselves, and end to end against a simulated node. `make gates` is that check and every other one that needs real chain data, and it is the first thing to run once the node is synced. Its first run against a real node, on 19 November 2026, passed: the replica matched all 24 pools of the universe exactly, their stored history since 31 January replays to their state on the chain, and 6,135 real positions collected exactly the fees the replica computed. The backtest on that history lost money (every run stopped at its 5% loss limit), which is the next thing to understand.

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

To look at the results, Postgres and Grafana have to be up. `make viewer` starts just those two (and Prometheus, which stays empty without a node), which is what you want on a laptop; `make up` starts them along with the node.

```sh
make viewer             # Postgres, Prometheus and Grafana, no node
make load               # result tables into Postgres
```

Then open Grafana on http://127.0.0.1:3000: it opens on the Overview. Choose `Runs: experiments` from the Runs menu, click an experiment, then a run id to open that run in detail with the time range set to its window.

On the node machine the watcher service, which `make up` starts, keeps the dashboard `Pools: live` current: every pool's price, liquidity, volume and fees minute by minute, the liquidity around the price, recent swaps and every position with its owner, NFT, range, amounts and fees. It refreshes every ten seconds. Its first start replays the stored history, which takes a while; `make watch-logs` shows how far it is. `deploy/RUNBOOK.md` has the rest.

The machine that runs the node is set up with `deploy/REMOTE.md`: one script to prepare it, `make snapshot` to download the node's database, `make up` to start it. From a laptop, `make tunnel HOST=<user>@<machine>` brings its Grafana to http://127.0.0.1:3001; the same file has a table of what goes wrong and why. `deploy/RUNBOOK.md` is what to type on the machine: long jobs in `tmux`, and what to do after it was switched off.

Once the node is synced, one command runs every check against real chain data in order, then the backtest, the load into Grafana's database and the calibration:

```sh
make gates
```

It stops at the first step that fails and ends with a summary of what passed; `make gates STEP=<step>` carries on from a step. `deploy/README.md` describes the steps. They are these commands, which also work one by one:

```sh
# The universe's pools, read from the Uniswap v3 factory
cargo run --release -p lpbot -- pools

# Replay a pool's last 2,000 blocks of events and compare the replica with the chain
cargo run --release -p lpbot -- verify-pool <pool address from the list above>

# Store every universe pool's history from the end of January on under data/lake, then replay
# it against the chain (the example experiment starts on 1 May and looks back 90 days)
make history            # or: cargo run --release -p lpbot -- backfill --from 2026-01-31 --verify

# Compare the replica's fees with what real positions in a pool collected; it asks the node
# which positions started empty and what the pool still owes them
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
  chain/      RPC client, logs, headers, pool and position reads
  venues/     pool math, state replica, call encoding (Uniswap v3 first)
  data/       Parquet sinks, backfill, positions, the live state and its follower
  sim/        replay engine, cost models, result tables
  strategy/   strategy and patience-policy interfaces, built-in rules
  risk/       limits and exit paths
  exec/       planner, custody, signer client, submitter, journal
  cli/        the lpbot binary
deploy/       Docker Compose stack, machine setup (REMOTE.md) and runbook (RUNBOOK.md)
scripts/      setup and status scripts
config/       shared base configs (YAML)
experiments/  one YAML file per experiment or sweep
docs/         the design
```

## Rules of the house

- Everything is Rust. Experiments are YAML files. There are no notebooks.
- Data, results and secrets never go in Git.
- Contributor conventions are in [CLAUDE.md](CLAUDE.md). Codex starts from
  [AGENTS.md](AGENTS.md); [the agent workflow](docs/AGENT_WORKFLOW.md) covers
  development, code review, LP strategy research, deciders and strategy reviews.
