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
# on. `make viewer` uses it to start Postgres and Grafana on a laptop:
#
#   LPBOT_SERVICES="postgres grafana" ./scripts/up.sh

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

fill POSTGRES_PASSWORD "$(openssl rand -hex 16)"
fill GRAFANA_PASSWORD "$(openssl rand -hex 12)"
fill HOST_UID "$(id -u)"
fill HOST_GID "$(id -g)"

set -a
# shellcheck disable=SC1090
. "$ENV_FILE"
set +a

# --- data directories --------------------------------------------------------

# A relative DATA_DIR is relative to the repository root.
case "$DATA_DIR" in
  /*) ;;
  *) DATA_DIR="$ROOT/${DATA_DIR#./}" ;;
esac
export DATA_DIR

mkdir -p "$DATA_DIR"/{reth,lighthouse,postgres,prometheus,grafana}

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
  need_gb=3000
  kind="an archive"
fi
avail_gb="$(df -Pk "$DATA_DIR" | awk 'NR==2 {print int($4 / 1024 / 1024)}')"
if [[ "$with_node" == 1 && -z "$(ls -A "$DATA_DIR/reth" 2>/dev/null)" && "$avail_gb" -lt "$need_gb" ]]; then
  warn "$DATA_DIR has ${avail_gb} GB free; ${kind} node needs about ${need_gb} GB."
  warn "Point DATA_DIR in deploy/.env at a larger drive, or set RETH_EXTRA_ARGS=--full."
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
  exit 0
fi

if [[ -n "$SERVICES" ]]; then
  say "Starting only: $SERVICES"
  # shellcheck disable=SC2086
  compose up -d --no-deps $SERVICES

  if [[ " $SERVICES " == *" postgres "* ]]; then
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
    [[ "${ready:-0}" == 1 ]] \
      || die "Postgres did not come up within a minute. See: docker compose -f deploy/docker-compose.yml logs postgres"
  fi

  cat <<EOF

Started: $SERVICES. Data lives in $DATA_DIR.

  make load       load the result tables under runs/ into Postgres
  make down       stop everything (data is kept)

Grafana:  http://127.0.0.1:3000   user admin, password GRAFANA_PASSWORD in deploy/.env
EOF
  exit 0
fi

say "Starting ${kind} node, Postgres, Prometheus and Grafana."
compose up -d

cat <<EOF

The stack is starting. Data lives in $DATA_DIR.

  make status     sync progress of the node
  make logs       follow the node logs
  make down       stop everything (data is kept)

Grafana:  http://127.0.0.1:3000   user admin, password GRAFANA_PASSWORD in deploy/.env
RPC:      http://127.0.0.1:8545 and ws://127.0.0.1:8546 (this machine only)

The first sync takes many hours to a few days. The bot's data commands wait for it.
EOF
