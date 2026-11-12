# `make up` is the one command that sets everything up on the home machine.

COMPOSE = docker compose --env-file deploy/.env -f deploy/docker-compose.yml

.PHONY: up down restart status logs build test check

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

## Run the unit tests.
test:
	cargo test --workspace

## Everything that must pass before a commit.
check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	cargo run -q -p lpbot -- plan experiments/width-and-patience-eth.yml > /dev/null
