# `make up` is the one command that sets everything up on the home machine.

COMPOSE = docker compose --env-file deploy/.env -f deploy/docker-compose.yml

# Pool history is stored beside the node's data: under DATA_DIR from
# deploy/.env once that file exists, else under ./data.
-include deploy/.env
export LPBOT_DATA_DIR ?= $(if $(DATA_DIR),$(DATA_DIR),data)/lake

.PHONY: up down restart status logs build test check demo history rank backtest

## Start the node, Postgres, Prometheus and Grafana. Safe to run again.
up:
	./scripts/up.sh

## Stop everything. Data is kept.
down:
	$(COMPOSE) down

## Restart after changing versions or settings in deploy/.env.
restart:
	./scripts/up.sh

## Show container state and node sync progress.
status:
	./scripts/status.sh

## Follow the node logs.
logs:
	$(COMPOSE) logs -f --tail 100 reth lighthouse

## Build the lpbot binary in release mode.
build:
	cargo build --release -p lpbot

## Try the whole pipeline on a simulated pool. Needs no node.
demo:
	cargo run --release -p lpbot -- demo-data
	cargo run --release -p lpbot -- run experiments/demo-fixed-rules.yml --data-dir data/demo

## Once the node is synced: store pool history from FROM on, and check it
## against the chain. Safe to run again; it only fetches what is new.
FROM ?= 2026-02-01
history:
	cargo run --release -p lpbot -- backfill --from $(FROM) --verify

## Rank the stored pools by implied volatility.
rank:
	cargo run --release -p lpbot -- rank

## Backtest an experiment file on the stored history.
EXPERIMENT ?= experiments/width-and-patience-eth.yml
backtest:
	cargo run --release -p lpbot -- run $(EXPERIMENT)

## Run the unit tests.
test:
	cargo test --workspace

## Everything that must pass before a commit.
check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	cargo run -q -p lpbot -- plan experiments/width-and-patience-eth.yml > /dev/null
