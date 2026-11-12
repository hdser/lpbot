#!/usr/bin/env bash
# Downloads Reth's published database snapshot into DATA_DIR, so the node
# starts near the head of the chain instead of syncing from the first block.
# Run it once, before the first `make up`. Running it again resumes.
#
#   make snapshot        # or: ./scripts/snapshot.sh
#
# It downloads the archive snapshot, or the full-node one when
# RETH_EXTRA_ARGS in deploy/.env is --full.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY="$ROOT/deploy"
ENV_FILE="$DEPLOY/.env"

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

[[ -f "$ENV_FILE" ]] \
  || die "deploy/.env does not exist yet. Run 'LPBOT_DRY_RUN=1 ./scripts/up.sh' to create it, set DATA_DIR, then run this again."
command -v docker >/dev/null 2>&1 || die "Docker is not installed. Run ./scripts/host-setup.sh first."

set -a
# shellcheck disable=SC1090
. "$ENV_FILE"
set +a
case "$DATA_DIR" in
  /*) ;;
  *) DATA_DIR="$ROOT/${DATA_DIR#./}" ;;
esac
RETH_DIR="$DATA_DIR/reth"
mkdir -p "$RETH_DIR"

if [[ "${RETH_EXTRA_ARGS:-}" == *--full* ]]; then
  profile=--full
else
  profile=--archive
fi

compose() {
  docker compose --env-file "$ENV_FILE" -f "$DEPLOY/docker-compose.yml" "$@"
}

# The database must not be open while it is being replaced.
if [[ -n "$(compose ps -q reth 2>/dev/null)" ]]; then
  say "Stopping the node while its database is downloaded."
  compose stop lighthouse reth
fi

if [[ -n "$(ls -A "$RETH_DIR" 2>/dev/null)" && "${LPBOT_SNAPSHOT_RESUME:-0}" != 1 ]]; then
  die "$RETH_DIR is not empty. If it holds an interrupted download, resume with:
    LPBOT_SNAPSHOT_RESUME=1 make snapshot
If it holds a synced node, there is nothing to download. To start over, empty that folder first."
fi

avail_gb="$(df -Pk "$RETH_DIR" | awk 'NR==2 {print int($4 / 1024 / 1024)}')"
say "Downloading the ${profile#--} snapshot into $RETH_DIR (${avail_gb} GB free). This takes hours."

# A terminal lets Reth draw its progress display; without one it logs lines.
tty_flags=()
if [[ -t 0 && -t 1 ]]; then
  tty_flags=(-it)
fi

docker run --rm ${tty_flags[@]+"${tty_flags[@]}"} \
  -v "$RETH_DIR:/root/.local/share/reth/mainnet" \
  "ghcr.io/paradigmxyz/reth:${RETH_VERSION}" \
  download --chain mainnet "$profile"

cat <<EOF

The snapshot is in place. Start the stack with:

  make up

The node then syncs the blocks since the snapshot, which takes far less time
than a sync from the first block.
EOF
