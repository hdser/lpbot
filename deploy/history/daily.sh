#!/usr/bin/env bash
# Keeps the stored pool history up to date: every pool of the universe and
# the gas prices, from where the store ends to the newest final block.
#
#   deploy/history/daily.sh            fetch what is missing
#   deploy/history/daily.sh --verify   fetch, then replay the whole store
#                                      and compare every pool with the chain
#
# It carries on from the store itself (`lpbot backfill --resume`, then
# `lpbot gas-history` without a first day), so a run that was missed, or a
# machine that was off for a week, leaves no gap: the next run fetches all of
# it. Running it twice does no harm. The store must exist already:
# `make history FROM=<day>` and `make gas-history FROM=<day>` make it.
#
# Both commands hold the store's lock while they run, as every command that
# reads or writes the store does. An agent's look that falls in that time
# waits, and the server runner puts its job off to its next turn. Without
# --verify that is a minute or two; with it, the minutes a replay of the
# whole store takes, at low priority. In cron (`deploy/RUNBOOK.md`,
# "Keeping the stored history current"):
#
#   23 3 * * *  $HOME/lpbot/deploy/history/daily.sh >> $HOME/lpbot/logs/history-daily.log 2>&1
#   43 4 * * 0  $HOME/lpbot/deploy/history/daily.sh --verify >> $HOME/lpbot/logs/history-daily.log 2>&1
#
# The node is LPBOT_RPC, else the config's local one; the binary is LPBOT_BIN,
# else target/release/lpbot. The log gets one line
# when a run starts and one when it ends, `ok` or `FAILED`; the full output
# of the last run is in logs/history-daily-last.log.
set -uo pipefail
cd "$(dirname "$(realpath "$0")")/../.." || exit 1

stamp() { date -u +%FT%TZ; }

verify=
case "${1:-}" in
  "") ;;
  --verify) verify=--verify ;;
  *)
    echo "usage: deploy/history/daily.sh [--verify]"
    exit 2
    ;;
esac

# One run at a time. Without flock (macOS), no lock.
mkdir -p logs
exec 9>"logs/.history-daily.lock"
if command -v flock >/dev/null && ! flock -n 9; then
  echo "$(stamp) the previous run is still going; skipped"
  exit 0
fi

lpbot="${LPBOT_BIN:-target/release/lpbot}"
if [ ! -x "$lpbot" ]; then
  echo "$(stamp) FAILED: $lpbot is not built: CARGO_BUILD_JOBS=2 cargo build --release -p lpbot"
  exit 1
fi

env_value() { sed -n "s/^$1=//p" deploy/.env 2>/dev/null | head -n 1 | tr -d "\"'"; }
data_dir=$(env_value DATA_DIR)
export LPBOT_DATA_DIR="${LPBOT_DATA_DIR:-${data_dir:-data}/lake}"

last=logs/history-daily-last.log
echo "$(stamp) started${verify:+ with the replay}, store $LPBOT_DATA_DIR"
failed=
{
  echo "== $(stamp) lpbot backfill --resume $verify"
  # shellcheck disable=SC2086
  nice -n 10 "$lpbot" backfill --resume $verify || failed="the backfill"
  echo "== $(stamp) lpbot gas-history"
  nice -n 10 "$lpbot" gas-history || failed="${failed:+$failed and }the gas prices"
} >"$last" 2>&1

# What the run stored, in the commands' own words.
days=$(grep -cE '^  [0-9]{4}-[0-9]{2}-[0-9]{2}  ' "$last" || true)
checked=$(grep -c 'verify: PASS' "$last" || true)
differ=$(grep -c 'verify: FAIL' "$last" || true)
summary="$days day(s) fetched"
[ -n "$verify" ] && summary="$summary, $checked pool(s) replay to the chain, $differ differ"
if [ -n "$failed" ] || [ "$differ" -gt 0 ]; then
  echo "$(stamp) FAILED: ${failed:-a pool differs from the chain}; $summary. See $last: $(grep -iE 'error|failed|FAIL|stopped' "$last" | tail -n 1)"
  exit 1
fi
echo "$(stamp) ok: $summary"
