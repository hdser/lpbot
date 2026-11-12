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

Both commands only read. They need no keys and send no transactions.

## Upgrades

Client versions are pinned in `deploy/.env` (`RETH_VERSION`, `LIGHTHOUSE_VERSION`). To upgrade, change the version and run `make up`. Check the release notes first: a network upgrade needs both clients updated before its activation date.

The pinned versions were current when this file was written. Confirm they still exist and are still supported before the first run.

## What is not committed

`deploy/.env`, `deploy/jwt/` and the data folder are ignored by Git. If you lose `deploy/.env`, the Postgres password inside the existing database no longer matches a newly generated one; keep a copy.
