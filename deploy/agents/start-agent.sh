#!/usr/bin/env bash
# Starts the Claude Code session that decides one agent of a forward panel,
# from the agent's brief, with the agent's own model (`agent.model`). Run it
# in its own tmux window on the node machine:
#
#   tmux new-window -d -t agent -n a deploy/agents/start-agent.sh experiments/agent-panel-forward.yml a
#
# Only `target/release/lpbot agent ...` runs in the session. Reading files,
# the web and every other command are denied without a prompt, so the session
# never stops on a question to the owner and cannot open what the brief puts
# off limits. When the session ends, the window stays open and says how to
# start it again: the decisions file keeps the run, and `agent next` carries
# on from it. `deploy/RUNBOOK.md`, "A forward run in the agent mode", has the
# rest.
set -euo pipefail
usage="usage: start-agent.sh <experiment.yml> <agent>"
exp="${1:?$usage}"
agent="${2:?$usage}"
self=$(realpath "$0")
cd "$(dirname "$self")/../.."

env_value() { sed -n "s/^$1=//p" deploy/.env 2>/dev/null | head -n 1 | tr -d "\"'"; }
data_dir=$(env_value DATA_DIR)
export LPBOT_DATA_DIR="${LPBOT_DATA_DIR:-${data_dir:-data}/lake}"

brief=$(target/release/lpbot agent brief "$exp" --agent "$agent")
model=$(printf '%s\n' "$brief" | sed -n '1s/^# Brief for agent .* (\(.*\))$/\1/p')
if [ -z "$model" ]; then
  echo "the brief of agent $agent names no model: set agent.model in $exp" >&2
  exit 1
fi
note="The binary is built: do not run cargo. In this session only target/release/lpbot agent commands run, written as the brief writes them, from the repository root, without cd, pipes, redirects or &&. Reading files and every other command are turned off. Keep \$ and backticks out of the text you pass. When a background command finishes, run it again without --wait to see what it shows.

"
claude=$(command -v claude || echo "$HOME/.local/bin/claude")
"$claude" --model "$model" --permission-mode dontAsk \
  --allowedTools "Bash(target/release/lpbot agent:*)" "Bash(target/release/lpbot agent *)" \
  --disallowedTools Read Glob Grep Edit Write NotebookEdit WebFetch WebSearch Agent Task \
  -- "$note$brief" || true
echo "The session of agent $agent ended. Start it again with: $self $exp $agent"
exec bash -l
