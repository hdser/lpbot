#!/usr/bin/env bash
# One command to bring the whole stack up: Ethereum node, Postgres, Prometheus
# and Grafana. Safe to run again at any time; it never overwrites secrets or data.
#
#   make up            # or: ./scripts/up.sh
#
# Set LPBOT_DRY_RUN=1 to prepare everything and validate the compose file
# without starting any container.
#
# Set LPBOT_SERVICES to start only some services, without what they depend
# on. `make viewer` uses it to start the viewer without the node. Prometheus
# is part of it so the node dashboard has a source to ask; until the node
# runs it has nothing to collect and the dashboard is empty:
#
#   LPBOT_SERVICES="postgres prometheus grafana" ./scripts/up.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY="$ROOT/deploy"
ENV_FILE="$DEPLOY/.env"
JWT_FILE="$DEPLOY/jwt/jwt.hex"

say() { printf '\n==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- prerequisites -----------------------------------------------------------

command -v docker >/dev/null 2>&1 \
  || die "Docker is not installed. Install Docker Engine (https://docs.docker.com/engine/install/) and run this again."
docker compose version >/dev/null 2>&1 \
  || die "The Docker Compose plugin is missing. Install it (https://docs.docker.com/compose/install/) and run this again."
command -v openssl >/dev/null 2>&1 \
  || die "openssl is needed to generate secrets. Install it and run this again."

# --- deploy/.env -------------------------------------------------------------

if [[ ! -f "$ENV_FILE" ]]; then
  cp "$DEPLOY/.env.example" "$ENV_FILE"
  chmod 600 "$ENV_FILE"
  say "Created deploy/.env from the example. Edit DATA_DIR there to point at your NVMe drive."
fi

# Fills KEY= with a value only while it is still empty.
fill() {
  local key="$1" value="$2"
  if grep -q "^${key}=\$" "$ENV_FILE"; then
    sed -i.bak "s|^${key}=\$|${key}=${value}|" "$ENV_FILE"
    rm -f "$ENV_FILE.bak"
  fi
}

# `make env DATA_DIR=/data/lpbot` sets where the data lives without an editor.
if [[ -n "${LPBOT_SET_DATA_DIR:-}" ]]; then
  sed -i.bak "s|^DATA_DIR=.*|DATA_DIR=${LPBOT_SET_DATA_DIR}|" "$ENV_FILE"
  rm -f "$ENV_FILE.bak"
fi

fill POSTGRES_PASSWORD "$(openssl rand -hex 16)"
fill GRAFANA_PASSWORD "$(openssl rand -hex 12)"
fill HOST_UID "$(id -u)"
fill HOST_GID "$(id -g)"

# shellcheck source=scripts/env.sh
. "$ROOT/scripts/env.sh"
env_load "$ENV_FILE"

# --- data directories --------------------------------------------------------

# A relative DATA_DIR is relative to the repository root.
case "$DATA_DIR" in
  /*) ;;
  *) DATA_DIR="$ROOT/${DATA_DIR#./}" ;;
esac
export DATA_DIR

SERVICES="${LPBOT_SERVICES:-}"
# The node runs unless a list of services leaves it out.
with_node=1
if [[ -n "$SERVICES" && " $SERVICES " != *" reth "* ]]; then
  with_node=0
fi

if [[ "${RETH_EXTRA_ARGS:-}" == *--full* ]]; then
  need_gb=1500
  kind="a full"
else
  # The archive snapshot of November 2026 unpacks to about 2,900 GB, and
  # the consensus client and the pool history come on top.
  need_gb=3200
  kind="an archive"
fi

# Free space where DATA_DIR is, or will be: the nearest folder that exists.
probe="$DATA_DIR"
while [[ ! -d "$probe" ]]; do
  probe="$(dirname "$probe")"
done
avail_gb="$(df -Pk "$probe" | awk 'NR==2 {print int($4 / 1024 / 1024)}')"
if [[ "$with_node" == 1 && -z "$(ls -A "$DATA_DIR/reth" 2>/dev/null)" && "$avail_gb" -lt "$need_gb" ]]; then
  warn "$DATA_DIR has ${avail_gb} GB free; ${kind} node needs about ${need_gb} GB."
  warn "Point DATA_DIR in deploy/.env at a larger drive (make env DATA_DIR=<folder>), or set RETH_EXTRA_ARGS=--full."
fi

# --- engine secret shared by Reth and Lighthouse -----------------------------

mkdir -p "$DEPLOY/jwt"
if [[ ! -s "$JWT_FILE" ]]; then
  openssl rand -hex 32 | tr -d '\n' > "$JWT_FILE"
  chmod 600 "$JWT_FILE"
  say "Generated the engine secret."
fi

# --- start -------------------------------------------------------------------

compose() {
  docker compose --env-file "$ENV_FILE" -f "$DEPLOY/docker-compose.yml" "$@"
}

if [[ "${LPBOT_DRY_RUN:-0}" == "1" ]]; then
  compose config --quiet
  say "Dry run: configuration is valid. Nothing was started."
  echo "Data will live in $DATA_DIR (${avail_gb} GB free there)."
  exit 0
fi

# Docker is installed, but can this shell use it?
docker info >/dev/null 2>&1 \
  || die "this shell cannot talk to Docker. If you just ran scripts/host-setup.sh, log out and back in (or run 'newgrp docker') and try again. A tmux session started before that login keeps the old permissions."

mkdir -p "$DATA_DIR"/{reth,lighthouse,postgres,prometheus,grafana}

# Written when the node is first started. `make snapshot` refuses to run
# once it exists: from then on the database is the node's own, synced past
# any snapshot, and a download into it would overwrite it.
NODE_MARKER="$DATA_DIR/reth-node-started"

# Reth must not open a database that a snapshot download is still filling.
if [[ "$with_node" == 1 ]]; then
  reth_image="ghcr.io/paradigmxyz/reth:${RETH_VERSION}"
  if docker ps --filter "ancestor=$reth_image" --format '{{.Command}}' | grep -q download; then
    die "a snapshot download (make snapshot) is still running. Wait for it to finish, then run this again."
  fi
  partial=0
  if [[ -d "$DATA_DIR/reth/.download-cache" ]]; then
    partial="$(find "$DATA_DIR/reth/.download-cache" -maxdepth 1 -name '*.part' | wc -l | tr -d ' ')"
  fi
  if [[ "$partial" != 0 ]]; then
    die "the snapshot download in $DATA_DIR/reth did not finish ($partial archives are partly downloaded). Finish it first:
    LPBOT_SNAPSHOT_RESUME=1 make snapshot
Starting the node now would open a half-unpacked database."
  fi
fi

mark_node_started() {
  if [[ "$with_node" == 1 && ! -f "$NODE_MARKER" ]]; then
    date -u +%Y-%m-%dT%H:%M:%SZ > "$NODE_MARKER"
  fi
}

# Waits until Postgres accepts connections, then makes sure the result
# tables exist, so the dashboards of a database without runs show "no data"
# rather than an error. Safe to repeat: it only adds what is missing.
prepare_database() {
  local ready=0
  printf 'Waiting for Postgres'
  for _ in $(seq 1 60); do
    if compose exec -T postgres pg_isready -U lpbot -d lpbot >/dev/null 2>&1; then
      ready=1
      break
    fi
    printf '.'
    sleep 1
  done
  printf '\n'
  [[ "$ready" == 1 ]] \
    || die "Postgres did not come up within a minute. See: docker compose -f deploy/docker-compose.yml logs postgres"
  compose exec -T -e PGOPTIONS=--client-min-messages=warning postgres \
    psql -q -v ON_ERROR_STOP=1 -U lpbot -d lpbot \
    < "$DEPLOY/postgres/schema.sql" >/dev/null \
    || die "could not create the result tables. See: docker compose -f deploy/docker-compose.yml logs postgres"
}

if [[ -n "$SERVICES" ]]; then
  say "Starting only: $SERVICES"
  # shellcheck disable=SC2086
  compose up -d --no-deps $SERVICES
  mark_node_started

  if [[ " $SERVICES " == *" postgres "* ]]; then
    prepare_database
  fi

  cat <<EOF

Started: $SERVICES. Data lives in $DATA_DIR.

  make load              load the result tables under runs/ into Postgres
  make grafana-password  print the password for the admin user
  make down              stop everything (data is kept)

Grafana:  http://127.0.0.1:3000   user admin
From another machine: make tunnel HOST=<user>@<this machine>, then http://127.0.0.1:3001
EOF
  exit 0
fi

say "Starting ${kind} node, Postgres, Prometheus and Grafana."
compose up -d
mark_node_started
prepare_database

cat <<EOF

The stack is starting. Data lives in $DATA_DIR.

  make status            sync progress of the node, and free disk space
  make logs              follow the node logs
  make grafana-password  print the password for the admin user
  make down              stop everything (data is kept)

Grafana:  http://127.0.0.1:3000   user admin
RPC:      http://127.0.0.1:8545 and ws://127.0.0.1:8546 (this machine only)
From another machine: make tunnel HOST=<user>@<this machine>, then http://127.0.0.1:3001

After a snapshot the node catches up in hours. Without one, the first sync
takes days. The bot's data commands wait for it. Once \`make status\` says
both clients are synced:

  make gates             every check against real chain data, then the
                         backtest, the load into Grafana and the calibration
EOF
