#!/usr/bin/env bash
# Keeps forward agent panels current on the `lpbot: agents` dashboard.
#
#   deploy/agents/refresh.sh [experiment.yml ...]
#       scores, replays and loads each panel: `lpbot agent evaluate`, then
#       `lpbot run` into runs/<experiment without "agent-">, then
#       `lpbot load-results`. A minute or two a panel: hourly.
#   deploy/agents/refresh.sh --looks [experiment.yml ...]
#       loads only the looks decided since the last load, with their
#       reasoning (`lpbot load-looks`), without replays: every minute.
#
# With no experiment named, every experiment under experiments/ whose window
# is open at the end (`to: open`) and one of whose decisions files exists.
# It reads only the stored history, which the agents bring up to the head at
# each look: it never runs `agent next` or `agent decide`. The database is
# LPBOT_DB_URL, else the one in deploy/.env, as `make load` does. In cron
# (`deploy/RUNBOOK.md`, "Keeping the agents dashboard current"):
#
#   * * * * *  $HOME/lpbot/deploy/agents/refresh.sh --looks >> $HOME/lpbot/logs/agent-refresh.log 2>&1
#   17 * * * * $HOME/lpbot/deploy/agents/refresh.sh >> $HOME/lpbot/logs/agent-refresh.log 2>&1
#
# The per-minute job writes a line only when it loads something. The full
# output of the last full refresh is in logs/agent-refresh-last.log.
set -uo pipefail
cd "$(dirname "$(realpath "$0")")/../.." || exit 1

stamp() { date -u +%FT%TZ; }

mode=full
if [ "${1:-}" = "--looks" ]; then
  mode=looks
  shift
fi

# One refresh of each mode at a time. Without flock (macOS), no lock.
mkdir -p logs
exec 9>"logs/.agent-refresh-$mode.lock"
if command -v flock >/dev/null && ! flock -n 9; then
  [ "$mode" = full ] && echo "$(stamp) the previous refresh is still running; skipped"
  exit 0
fi

lpbot=target/release/lpbot
if [ ! -x "$lpbot" ]; then
  echo "$(stamp) $lpbot is not built: CARGO_BUILD_JOBS=2 cargo build --release -p lpbot"
  exit 1
fi

env_value() { sed -n "s/^$1=//p" deploy/.env 2>/dev/null | head -n 1 | tr -d "\"'"; }
data_dir=$(env_value DATA_DIR)
export LPBOT_DATA_DIR="${LPBOT_DATA_DIR:-${data_dir:-data}/lake}"
if [ -z "${LPBOT_DB_URL:-}" ]; then
  password=$(env_value POSTGRES_PASSWORD)
  if [ -z "$password" ]; then
    echo "$(stamp) no database: set LPBOT_DB_URL, or POSTGRES_PASSWORD in deploy/.env"
    exit 1
  fi
  export LPBOT_DB_URL="postgres://lpbot:${password}@127.0.0.1:5432/lpbot"
  unset password
fi

# The forward panels with decisions on disk.
if [ $# -eq 0 ]; then
  for exp in experiments/*.yml; do
    grep -qE '\bto:[[:space:]]*open\b' "$exp" || continue
    # `decisions: <path>` in a panel, `agent.decisions: [<path>, ...]` in a
    # single-agent run.
    for decisions in $(grep -oE 'decisions:[[:space:]]*(\[[^]]*\]|[^ ,}]+)' "$exp" |
      sed -E "s/decisions:[[:space:]]*//; s/[][\"']//g; s/,/ /g"); do
      if [ -e "$decisions" ]; then
        set -- "$@" "$exp"
        break
      fi
    done
  done
fi

status=0
if [ "$mode" = looks ]; then
  for exp in "$@"; do
    stem=$(basename "$exp" .yml)
    if ! out=$("$lpbot" load-looks "$exp" 2>&1); then
      status=1
    fi
    printf '%s\n' "$out" | grep -v 'already loaded' | grep . |
      while IFS= read -r line; do echo "$(stamp) $stem: $line"; done
  done
  exit $status
fi

last=logs/agent-refresh-last.log
: >"$last"
for exp in "$@"; do
  stem=$(basename "$exp" .yml)
  results="runs/${stem#agent-}"
  if nice -n 10 "$lpbot" agent evaluate "$exp" --jobs 2 >>"$last" 2>&1 &&
    nice -n 10 "$lpbot" run "$exp" --results-dir "$results" >>"$last" 2>&1 &&
    "$lpbot" load-results --results-dir "$results" >>"$last" 2>&1; then
    echo "$(stamp) $stem: $(grep '^loaded ' "$last" | tail -n 1)"
  else
    echo "$(stamp) $stem: failed; see $last"
    status=1
  fi
done
exit $status
