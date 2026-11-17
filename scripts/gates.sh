#!/usr/bin/env bash
# Every check against real chain data in one go, then the backtest, the
# load into the viewer's database and the calibration: `lpbot gates`, with
# the whole output kept under logs/. `make gates` calls this.
#
#   ./scripts/gates.sh [experiment.yml] [options of `lpbot gates`]
#
# The node has to be synced. It stops at the first check that fails and
# ends with a summary. Everything it stores is kept, so running it again
# only fetches what is new.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$ROOT/deploy/.env"
cd "$ROOT"

# A setting from deploy/.env, empty when the file or the setting is missing.
setting() {
  [[ -f "$ENV_FILE" ]] || return 0
  sed -n "s/^$1=//p" "$ENV_FILE" | head -n 1
}

# The pool history lives beside the node's data, as for the other targets.
if [[ -z "${LPBOT_DATA_DIR:-}" ]]; then
  data_dir="$(setting DATA_DIR)"
  export LPBOT_DATA_DIR="${data_dir:-data}/lake"
fi

# The results go into the Postgres that `make up` or `make viewer` started.
# The address holds the password, so it is passed on without being shown.
if [[ -z "${LPBOT_DB_URL:-}" ]]; then
  password="$(setting POSTGRES_PASSWORD)"
  if [[ -n "$password" ]]; then
    export LPBOT_DB_URL="postgres://lpbot:${password}@127.0.0.1:5432/lpbot"
  fi
fi

cargo build --release -p lpbot

mkdir -p logs
log="logs/gates-$(date -u +%Y%m%dT%H%M%SZ).log"
status=0
./target/release/lpbot gates "$@" 2>&1 | tee "$log" || status=$?

echo
echo "The whole output is in $log"
exit "$status"
