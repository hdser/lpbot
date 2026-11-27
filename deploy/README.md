# Running the stack at home

Setting up a fresh machine for it, reaching it from a laptop, and what to do when that does not work are in [REMOTE.md](REMOTE.md). What to type on that machine day to day, how to run long jobs in `tmux`, and what to do after it was off are in [RUNBOOK.md](RUNBOOK.md).

One command starts everything the bot needs on the home machine:

```sh
make up
```

It checks for Docker, creates `deploy/.env` with generated passwords, generates the engine secret shared by Reth and Lighthouse, creates the data folders, and starts six containers. The first time, it also builds the watcher's image from this repository, which takes a few minutes.

| Service | What it is | Reachable at |
| --- | --- | --- |
| reth | Ethereum execution node, archive by default | `http://127.0.0.1:8545`, `ws://127.0.0.1:8546` |
| lighthouse | Ethereum consensus node | `http://127.0.0.1:5052` |
| postgres | Result tables for the viewer | `127.0.0.1:5432`, database and user `lpbot` |
| prometheus | Node metrics | `http://127.0.0.1:9090` |
| grafana | Viewer and monitoring | `http://127.0.0.1:3000` |
| watcher | `lpbot watch-pools`: follows the stored pools from the node block by block and keeps the live tables current, for the `Pools: live` dashboard. See "The live tables" in [RUNBOOK.md](RUNBOOK.md) | Nothing: it writes into Postgres |

Only the peer-to-peer ports (30303 and 9000, TCP and UDP) are open to the network. Forward those on the router. Never forward the others.

## Before the first run

1. Install Docker Engine with the Compose plugin.
2. Create `deploy/.env` and point it at the NVMe drive without starting anything: `make env DATA_DIR=/data/lpbot`. Values are read literally, as Compose reads them: quotes around a value are dropped, and spaces stay part of it.
3. Download the database snapshot with `make snapshot` (in `tmux`, see [RUNBOOK.md](RUNBOOK.md)), and only then run `make up`. A `make up` before the snapshot starts an archive sync from the first block; a `make up` during it refuses to start the node.

An archive node needs about 3.2 TB free and is what the replay checks assume. For a pruned full node (about 1.5 TB), set `RETH_EXTRA_ARGS=--full` in `deploy/.env`.

## Day to day

| Command | Does |
| --- | --- |
| `make status` | Container state and sync progress |
| `make logs` | Follows the Reth and Lighthouse logs |
| `make down` | Stops everything; data is kept |
| `make up` | Starts again, or applies changes made in `deploy/.env` |

## Sync time

Lighthouse starts from a recent checkpoint and is ready within minutes. Reth then syncs the blocks since the snapshot that `make snapshot` downloaded, which takes hours rather than the days of a sync from the first block. Once the node has been started, `make snapshot` refuses to run: the database is the node's own from then on.

## First check after the sync

When `make status` shows both clients synced, one command runs every check against real chain data, then the backtest, the load into Grafana's database and the calibration:

```sh
make gates
```

Run it inside `tmux`: the first run stores months of history for every pool of the universe, and the calibration replays it many times. Everything it stores is kept, so a second run only fetches what is new.

| Step | What it does | Passes when |
| --- | --- | --- |
| `node` | Asks the node whether it is synced, and the results database whether it is there | The node says it is synced, its newest block is less than ten minutes old, and the experiment's window lies in the past |
| `pools` | Lists the universe's pools from the Uniswap v3 factory | There is at least one |
| `replica` | For every pool: reads its state 2,000 blocks back, replays every event since and compares with its state now | Every pool matches, down to every tick |
| `history` | Stores every pool's events from the longest zone lookback before the experiment's window, and replays them from the first day's state | Every pool replays to its state on the chain |
| `fees` | Follows every position opened inside the stored history of the traded pool and compares what it collected with what the replica says it was owed | At least 20 emptied positions agree within 0.1% and none contradicts the replica |
| `backtest` | Runs the experiment file | Every run writes its six tables |
| `load` | Loads the tables into Postgres | The runs are in the database; open the `Runs: experiments` dashboard |
| `calibrate` | Fits the adaptive patience decision walk-forward and scores it against the fixed rules | It finishes; the output says which one ships |

It stops at the first step that fails, so nothing is built on a check that did not hold, and ends with a summary: one line per step with `pass`, `open`, `FAIL`, `left out` or `not run`. `open` means nothing wrong was found but the check could not be decided, for example when too few positions were emptied; the steps after it still run. The whole output is kept under `logs/`.

```sh
make gates STEP=fees                    # carry on from a step
make gates SKIP=fees,calibrate          # leave steps out
make gates EXPERIMENT=<file>            # another experiment file
make gates ARGS="--blocks 7200"         # other options; `lpbot gates --help` lists them
```

Every step talks only to your own node and the results database.

The rest of this section is the same steps by hand. Check that the bot's pool replica agrees with the chain:

```sh
cargo run --release -p lpbot -- pools
cargo run --release -p lpbot -- verify-pool <pool address from the list>
```

`pools` lists every Uniswap v3 pool whose two tokens are both in the universe. `verify-pool` reads a pool's state 2,000 blocks back, replays every event since, and compares the result with the pool's current state, down to every tick. It prints PASS or the list of differences. Run it on several pools and with a larger `--blocks` before trusting anything built on top.

Then store the history the backtests run on:

```sh
make history
```

That is `lpbot backfill --from 2026-01-31 --verify`: every universe pool from 31 January on, under `lake` in your `DATA_DIR`, as one Parquet file per pool and day plus a snapshot of each pool before the first day. `make history FROM=2026-06-01` starts later; `--pool` and `--data-dir` on the command itself narrow it further. The example experiment starts on 1 May and looks back 90 days, which is where 31 January comes from. `--verify` replays the stored events from the snapshot and compares the result with the pool on the chain. A second run only fetches the days that are new. The snapshot is the pool's state months back, so this step needs the archive node; on a pruned node, start with a recent date.

Check the fee accounting on the busiest pool you stored:

```sh
cargo run --release -p lpbot -- verify-fees <pool address>
```

It follows every position that was opened inside the stored history, works out from the replica what the pool owed it, and compares that with what its owner collected. It passes when at least 20 emptied positions match and none contradicts the replica.

Rank the universe and look at the zone map of a pool from the top of the ranking:

```sh
cargo run --release -p lpbot -- rank
cargo run --release -p lpbot -- zones <pool address>
```

`rank` prints the stored pools ordered by implied volatility over the last 7 days (`--days` changes that), with volume and the value within 1% of the price. Pools under the floors in `universe.floors` are listed last with the reason. `zones` lists, for 7, 30 and 90 day lookbacks, the price ranges that were crossed most: full passes per day, the fee a position there earned per day, the share of time price spent inside, and how long price stayed away after leaving. `--widths` sets the range widths to try, in tick spacings. Both read the stored history; with `--node` they read the node directly instead.

Then run a backtest. The window of `experiments/width-and-patience-eth.yml` must lie inside the days you stored:

```sh
cargo run --release -p lpbot -- run experiments/width-and-patience-eth.yml
```

It trades the pool that ranks highest for the value token, once per combination in the file's `sweep`, and writes six result tables per run under `runs/`. Capital and the loss limit come from the file's `run` block or from `--capital` and `--loss-limit`. `make calibrate` then fits the adaptive patience decision on one stretch of the window and scores it on the next, fold after fold, against always wait, always flip and always close, and says which one ships.

To see the results, load them into Postgres and open Grafana:

```sh
make load
```

On a machine without the node, `make viewer` starts Postgres, Prometheus and Grafana, which is enough to load and look at results. The `System: Reth internals` dashboard stays empty until a node runs. Grafana is on http://127.0.0.1:3000 (user `admin`, password `GRAFANA_PASSWORD` from `deploy/.env`). Grafana opens on the Overview, and the folder `lpbot` has nine dashboards in five subjects, with the same menu at the top of each: the Overview; Agents; Pools: live (filled by the watcher on the node machine) and Pools: history; Runs: experiments, Runs: one run, Runs: compare and Runs: pool and range choice; System: Reth internals. Start from Runs: experiments, click an experiment and then a run id: that opens the run in detail with the time range set to its window, which matters because backtests lie in the past. To look at a changed dashboard before it reaches the node, `make dashboards-preview` starts a read-only copy at http://127.0.0.1:3002 with this computer's data (`HOST=<user>@<node>` for the node's). Loading is safe to repeat; a run loaded twice replaces itself.

`make rank` and `make backtest` are the short forms of the last two steps (`make backtest EXPERIMENT=<file>` for another file). The `make` targets find the stored history through `DATA_DIR` in `deploy/.env`. When you call `lpbot` directly and `DATA_DIR` is not `./data`, set `LPBOT_DATA_DIR=<DATA_DIR>/lake` or pass `--data-dir`.

All of these commands only read the chain. They need no keys and send no transactions.

## Upgrades

Client versions are pinned in `deploy/.env` (`RETH_VERSION`, `LIGHTHOUSE_VERSION`). To upgrade, change the version and run `make up`. Check the release notes first: a network upgrade needs both clients updated before its activation date.

The pinned versions were current when this file was written. Confirm they still exist and are still supported before the first run.

## What is not committed

`deploy/.env`, `deploy/jwt/` and the data folder are ignored by Git. If you lose `deploy/.env`, the Postgres password inside the existing database no longer matches a newly generated one; keep a copy.
