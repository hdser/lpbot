#!/usr/bin/env bash
# Works out the ladder every night, after the stored history is current
# (docs/PLAN.md, piece 3; deploy/RUNBOOK.md, "The ladder every night"):
#
#   1. replays every frozen rule that runs forward (an experiment under
#      experiments/ whose window is open at the end and that has no agent)
#      into runs/forward-rules, up to the newest stored block;
#   2. runs `lpbot select ladder` on the owner's ladder file, which writes
#      the plan to data/selection/<ladder>.json.
#
#   deploy/selection/nightly.sh [ladder.yml]     default selection/ladder.yml
#
# Agent panels are replayed by deploy/agents/refresh.sh, hourly. The plan
# moves nothing: the owner approves it (`lpbot select approve`) before the
# trader takes it. In cron, after the history job:
#
#   13 5 * * *  $HOME/lpbot/deploy/selection/nightly.sh >> $HOME/lpbot/logs/select-nightly.log 2>&1
#
# The log gets a line when a run starts and one when it ends; the full
# output of the last run is in logs/select-nightly-last.log.
set -uo pipefail
cd "$(dirname "$(realpath "$0")")/../.." || exit 1

stamp() { date -u +%FT%TZ; }
ladder="${1:-selection/ladder.yml}"

mkdir -p logs
exec 9>"logs/.select-nightly.lock"
if command -v flock >/dev/null && ! flock -n 9; then
  echo "$(stamp) the previous run is still going; skipped"
  exit 0
fi

lpbot="${LPBOT_BIN:-target/release/lpbot}"
if [ ! -x "$lpbot" ]; then
  echo "$(stamp) FAILED: $lpbot is not built: CARGO_BUILD_JOBS=2 cargo build --release -p lpbot"
  exit 1
fi
if [ ! -f "$ladder" ]; then
  echo "$(stamp) FAILED: no ladder file at $ladder: copy selection/ladder.example.yml there and set the gates"
  exit 1
fi

env_value() { sed -n "s/^$1=//p" deploy/.env 2>/dev/null | head -n 1 | tr -d "\"'"; }
data_dir=$(env_value DATA_DIR)
export LPBOT_DATA_DIR="${LPBOT_DATA_DIR:-${data_dir:-data}/lake}"

last=logs/select-nightly-last.log
echo "$(stamp) started, ladder $ladder"
failed=
rules=0
{
  for exp in experiments/*.yml; do
    grep -qE '\bto:[[:space:]]*open\b' "$exp" || continue
    # An agent's run names its decisions file; deploy/agents/refresh.sh replays those.
    grep -q 'decisions' "$exp" && continue
    rules=$((rules + 1))
    echo "== $(stamp) lpbot run $exp"
    nice -n 10 "$lpbot" run "$exp" --results-dir runs/forward-rules || failed="${failed:+$failed, }$exp"
  done
  echo "== $(stamp) lpbot select ladder $ladder"
  "$lpbot" select ladder "$ladder" || failed="${failed:+$failed, }the ladder"
} >"$last" 2>&1

if [ -n "$failed" ]; then
  echo "$(stamp) FAILED: $failed. See $last"
  exit 1
fi
echo "$(stamp) ok: $rules forward rule experiment(s) replayed; $(grep -m1 -oE 'Written to [^ ]+' "$last" || echo 'no plan written')"
