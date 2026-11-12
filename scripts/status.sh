#!/usr/bin/env bash
# Shows whether the containers are up and how far the node has synced.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEPLOY="$ROOT/deploy"
RPC="${LPBOT_RPC:-http://127.0.0.1:8545}"
BEACON="${LPBOT_BEACON:-http://127.0.0.1:5052}"

rpc() {
  curl -s --max-time 5 -H 'content-type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":[]}" "$RPC"
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
fi
