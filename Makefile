# `make up` is the one command that sets everything up on the home machine.

COMPOSE = docker compose --env-file deploy/.env -f deploy/docker-compose.yml

# Pool history is stored beside the node's data: under DATA_DIR from
# deploy/.env once that file exists, else under ./data.
-include deploy/.env
export LPBOT_DATA_DIR ?= $(if $(DATA_DIR),$(DATA_DIR),data)/lake

.PHONY: up env viewer snapshot tunnel grafana-password grafana-reset-password down restart status logs build test check demo history rank backtest calibrate load

## Start the node, Postgres, Prometheus and Grafana. Safe to run again.
up:
	./scripts/up.sh

## Create deploy/.env and say where the data lives, without starting anything.
##   make env DATA_DIR=/data/lpbot
env:
	LPBOT_DRY_RUN=1 LPBOT_SET_DATA_DIR="$(DATA_DIR)" ./scripts/up.sh

## Download Reth's published database snapshot, so the node starts near the
## head of the chain. Once, before the first `make up`.
snapshot:
	./scripts/snapshot.sh

## From a laptop: bring the node machine's Grafana and RPC to this machine.
##   make tunnel HOST=<user>@<machine>
## For as long as the command runs, the node machine's Grafana is on
## http://127.0.0.1:3001 and its RPC on http://127.0.0.1:8545. Port 3001, so
## that it does not clash with a Grafana running on the laptop itself.
tunnel:
	@test -n "$(HOST)" || { echo "usage: make tunnel HOST=<user>@<machine>"; exit 1; }
	@echo "Grafana of $(HOST): http://127.0.0.1:3001   RPC: http://127.0.0.1:8545"
	@echo "Leave this running. Ctrl-C closes the tunnel."
	ssh -N -L 3001:127.0.0.1:3000 -L 8545:127.0.0.1:8545 -L 8546:127.0.0.1:8546 $(HOST)

## Print the password of Grafana's admin user on this machine. Every machine
## generates its own.
grafana-password:
	@sed -n 's/^GRAFANA_PASSWORD=//p' deploy/.env

## Set Grafana's admin password back to the one in deploy/.env, for when the
## login is refused.
grafana-reset-password:
	@$(COMPOSE) exec grafana grafana cli admin reset-admin-password "$$(sed -n 's/^GRAFANA_PASSWORD=//p' deploy/.env)"

## Start only Postgres and Grafana, to look at results without a node.
## For a laptop: `make demo`, `make viewer`, `make load`.
viewer:
	LPBOT_SERVICES="postgres grafana" ./scripts/up.sh

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
	cargo run --release -p lpbot -- run experiments/demo-patience.yml --data-dir data/demo
	cargo run --release -p lpbot -- calibrate experiments/demo-patience.yml --data-dir data/demo --fit 8 --score 5

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

## Fit the adaptive patience decision walk-forward and score it against the
## fixed rules, for the same experiment file.
calibrate:
	cargo run --release -p lpbot -- calibrate $(EXPERIMENT)

## Load the result tables under RESULTS into Postgres, for the dashboards.
## Safe to run again: a run that is loaded twice replaces itself.
RESULTS ?= runs
load:
	@LPBOT_DB_URL="postgres://lpbot:$(POSTGRES_PASSWORD)@127.0.0.1:5432/lpbot" \
		cargo run --release -p lpbot -- load-results --results-dir $(RESULTS)

## Run the unit tests.
test:
	cargo test --workspace

## Everything that must pass before a commit.
check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	cargo run -q -p lpbot -- plan experiments/width-and-patience-eth.yml > /dev/null
