# `make up` is the one command that sets everything up on the home machine.

COMPOSE = docker compose --env-file deploy/.env -f deploy/docker-compose.yml

# Pool history is stored beside the node's data: under DATA_DIR from
# deploy/.env once that file exists, else under ./data.
-include deploy/.env
# Make reads quotes around a value as part of it; Compose and the scripts
# drop them. Drop them here too, so DATA_DIR="/data/lpbot" means the same.
data_dir = $(strip $(subst ',,$(subst ",,$(DATA_DIR))))
export LPBOT_DATA_DIR ?= $(if $(data_dir),$(data_dir),data)/lake

.PHONY: up env viewer snapshot tunnel grafana-password grafana-reset-password down restart status logs build test check demo gates history rank backtest calibrate load

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
	LPBOT_SERVICES="postgres prometheus grafana" ./scripts/up.sh

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

## Once the node is synced: every check against real chain data in one go,
## then the backtest, the load into Grafana's database and the calibration,
## for EXPERIMENT. It stops at the first step that fails and ends with a
## summary; the whole output is kept under logs/. Safe to run again: it
## only fetches what is new. The targets below are its steps one by one.
##   make gates                            everything
##   make gates STEP=fees                  carry on from a step
##   make gates SKIP=fees,calibrate        leave steps out
##   make gates ARGS="--blocks 7200"       other options of `lpbot gates`
## Steps: node, pools, replica, history, fees, backtest, load, calibrate.
STEP ?=
SKIP ?=
ARGS ?=
gates:
	./scripts/gates.sh $(EXPERIMENT) --results-dir $(RESULTS) $(if $(STEP),--start-at $(STEP)) $(if $(SKIP),--skip $(SKIP)) $(ARGS)

## Store pool history from FROM on, and check it against the chain. Safe to
## run again; it only fetches what is new. The default is what the example
## experiment needs: its window starts on 1 May and it looks back 90 days.
FROM ?= 2026-01-31
history:
	cargo run --release -p lpbot -- backfill --from $(FROM) --verify

## Rank the stored pools by implied volatility.
rank:
	cargo run --release -p lpbot -- rank

## Sample every stored pool for the pool dashboard: price, value held,
## volume and fees hour by hour, the liquidity around the price day by day.
## Then `make load` puts them in Grafana's database.
pool-stats:
	cargo run --release -p lpbot -- pool-stats --results-dir $(RESULTS)

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
