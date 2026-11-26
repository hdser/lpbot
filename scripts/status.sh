#!/usr/bin/env bash
# Shows whether the containers are up and how far the node has synced.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY="$ROOT/deploy"
RPC="${LPBOT_RPC:-http://127.0.0.1:8545}"
BEACON="${LPBOT_BEACON:-http://127.0.0.1:5052}"

rpc() {
  curl -s --max-time 5 -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":${2:-[]}}" "$RPC"
}

# Seconds as "40 s", "12 min", "5.2 h" or "4.3 days".
duration() {
  awk -v s="$1" 'BEGIN {
    if (s < 120) printf "%d s", s
    else if (s < 7200) printf "%d min", s / 60
    else if (s < 172800) printf "%.1f h", s / 3600
    else printf "%.1f days", s / 86400
  }'
}

# Pulls a "key":"value" or "key":value pair out of a small JSON reply.
field() {
  sed -n "s/.*\"$1\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p" | head -n 1
}

echo "Containers"
if [[ -f "$DEPLOY/.env" ]]; then
  docker compose --env-file "$DEPLOY/.env" -f "$DEPLOY/docker-compose.yml" ps \
    --format 'table {{.Service}}\t{{.State}}\t{{.Status}}' 2>/dev/null \
    || echo "  could not query Docker"
else
  echo "  not set up yet: run 'make up'"
fi

echo
echo "Execution node (Reth)"
syncing="$(rpc eth_syncing)"
if [[ -z "$syncing" ]]; then
  echo "  not reachable at $RPC"
elif [[ "$syncing" == *'"result":false'* ]]; then
  head_hex="$(rpc eth_blockNumber | field result)"
  echo "  synced, head block $((head_hex))"
else
  current="$(printf '%s' "$syncing" | field currentBlock)"
  highest="$(printf '%s' "$syncing" | field highestBlock)"
  if [[ -n "$current" && -n "$highest" ]]; then
    echo "  syncing: block $((current)) of $((highest))"
  else
    echo "  syncing"
  fi
fi
# A node cut off from its peers still calls itself synced, or names its own
# newest block as the highest: the age of that block shows it.
newest="$(rpc eth_getBlockByNumber '["latest",false]' | field timestamp)"
if [[ -n "$newest" ]]; then
  age=$(( $(date +%s) - newest ))
  echo "  newest block made $(duration "$age") ago"
  if (( age > 120 )); then
    echo "  warning: the node is behind the chain. Lighthouse below says by how much;" \
      "with no peers, check the machine's internet connection."
  fi
fi

echo
echo "Consensus node (Lighthouse)"
beacon="$(curl -s --max-time 5 "$BEACON/eth/v1/node/syncing")"
if [[ -z "$beacon" ]]; then
  echo "  not reachable at $BEACON"
else
  is_syncing="$(printf '%s' "$beacon" | field is_syncing)"
  distance="$(printf '%s' "$beacon" | field sync_distance)"
  slot="$(printf '%s' "$beacon" | field head_slot)"
  if [[ "$is_syncing" == "false" ]]; then
    echo "  synced, head slot $slot"
  else
    echo "  syncing: head slot $slot, $distance slots behind"
  fi
  peers="$(curl -s --max-time 5 "$BEACON/eth/v1/node/peer_count" | field connected)"
  [[ -n "$peers" ]] && echo "  $peers peer(s) connected"
fi

echo
echo "Disk"
if [[ -f "$DEPLOY/.env" ]]; then
  # shellcheck source=scripts/env.sh
  . "$ROOT/scripts/env.sh"
  data_dir="$(env_setting "$DEPLOY/.env" DATA_DIR)"
  case "$data_dir" in
    /*) ;;
    *) data_dir="$ROOT/${data_dir#./}" ;;
  esac
  if [[ -d "$data_dir" ]]; then
    df -Pk "$data_dir" | awk -v dir="$data_dir" 'NR==2 {
      free = $4 / 1024 / 1024; total = $2 / 1024 / 1024
      printf "  %s: %.0f GB free of %.0f GB (%s used)\n", dir, free, total, $5
      if (free < 150) print "  warning: less than 150 GB free. The node stops when the disk is full."
    }'
  else
    echo "  $data_dir does not exist yet"
  fi
else
  echo "  not set up yet"
fi

echo
echo "Agents"
if [[ -x "$ROOT/target/release/lpbot" ]] &&
  "$ROOT/target/release/lpbot" supervisor status --help >/dev/null 2>&1; then
  (cd "$ROOT" && target/release/lpbot supervisor status --brief 2>/dev/null) | sed 's/^/  /' ||
    echo "  could not read the agents' state"
else
  echo "  target/release/lpbot is not built with \`lpbot supervisor status\` yet"
fi
