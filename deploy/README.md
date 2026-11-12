# Running the stack at home

One command starts everything the bot needs on the home machine:

```sh
make up
```

It checks for Docker, creates `deploy/.env` with generated passwords, generates the engine secret shared by Reth and Lighthouse, creates the data folders, and starts five containers.

| Service | What it is | Reachable at |
| --- | --- | --- |
| reth | Ethereum execution node, archive by default | `http://127.0.0.1:8545`, `ws://127.0.0.1:8546` |
| lighthouse | Ethereum consensus node | `http://127.0.0.1:5052` |
| postgres | Result tables for the viewer | `127.0.0.1:5432`, database and user `lpbot` |
| prometheus | Node metrics | `http://127.0.0.1:9090` |
| grafana | Viewer and monitoring | `http://127.0.0.1:3000` |

Only the peer-to-peer ports (30303 and 9000, TCP and UDP) are open to the network. Forward those on the router. Never forward the others.

## Before the first run

1. Install Docker Engine with the Compose plugin.
2. Run `make up` once, then edit `DATA_DIR` in `deploy/.env` to point at the NVMe drive and run `make up` again. Or create `deploy/.env` from `deploy/.env.example` yourself first.

An archive node needs about 3 TB free and is what the replay checks assume. For a pruned full node (about 1.5 TB), set `RETH_EXTRA_ARGS=--full` in `deploy/.env`.

## Day to day

| Command | Does |
| --- | --- |
| `make status` | Container state and sync progress |
| `make logs` | Follows the Reth and Lighthouse logs |
| `make down` | Stops everything; data is kept |
| `make up` | Starts again, or applies changes made in `deploy/.env` |

## Sync time

Lighthouse starts from a recent checkpoint and is ready within minutes. Reth then syncs the execution chain, which takes many hours to a few days depending on disk and connection. Reth also offers snapshot downloads that shorten this; see the Reth documentation for the current procedure before using one.

## First check after the sync

When `make status` shows both clients synced, check that the bot's pool replica agrees with the chain:

```sh
cargo run --release -p lpbot -- pools
cargo run --release -p lpbot -- verify-pool <pool address from the list>
```

`pools` lists every Uniswap v3 pool whose two tokens are both in the universe. `verify-pool` reads a pool's state 2,000 blocks back, replays every event since, and compares the result with the pool's current state, down to every tick. It prints PASS or the list of differences. Run it on several pools and with a larger `--blocks` before trusting anything built on top.

Then store the history the backtests run on:

```sh
make history
```

That is `lpbot backfill --from 2026-02-01 --verify`: every universe pool from 1 February on, under `lake` in your `DATA_DIR`, as one Parquet file per pool and day plus a snapshot of each pool before the first day. `make history FROM=2026-06-01` starts later; `--pool` and `--data-dir` on the command itself narrow it further. The example experiment starts on 1 May and looks back 90 days, which is where 1 February comes from. `--verify` replays the stored events from the snapshot and compares the result with the pool on the chain. A second run only fetches the days that are new. The snapshot is the pool's state months back, so this step needs the archive node; on a pruned node, start with a recent date.

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

It trades the pool that ranks highest for the value token, once per combination in the file's `sweep`, and writes six result tables per run under `runs/`. Capital and the loss limit come from the file's `run` block or from `--capital` and `--loss-limit`. The runs with `patience.mode: adaptive` are skipped for now; the three fixed rules run.

To see the results, load them into Postgres and open Grafana:

```sh
make load
```

Grafana is on http://127.0.0.1:3000 (user `admin`, password `GRAFANA_PASSWORD` from `deploy/.env`). The folder `lpbot` has three dashboards: every run of an experiment side by side, one run in detail, and what a run saw of pools and zones. Backtests lie in the past, so set the time range to the run's window. Loading is safe to repeat; a run loaded twice replaces itself.

`make rank` and `make backtest` are the short forms of the last two steps (`make backtest EXPERIMENT=<file>` for another file). The `make` targets find the stored history through `DATA_DIR` in `deploy/.env`. When you call `lpbot` directly and `DATA_DIR` is not `./data`, set `LPBOT_DATA_DIR=<DATA_DIR>/lake` or pass `--data-dir`.

All of these commands only read the chain. They need no keys and send no transactions.

## Upgrades

Client versions are pinned in `deploy/.env` (`RETH_VERSION`, `LIGHTHOUSE_VERSION`). To upgrade, change the version and run `make up`. Check the release notes first: a network upgrade needs both clients updated before its activation date.

The pinned versions were current when this file was written. Confirm they still exist and are still supported before the first run.

## What is not committed

`deploy/.env`, `deploy/jwt/` and the data folder are ignored by Git. If you lose `deploy/.env`, the Postgres password inside the existing database no longer matches a newly generated one; keep a copy.
