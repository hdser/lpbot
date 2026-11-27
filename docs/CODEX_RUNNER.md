# Codex forward-panel runner

`lpbot agent-runner <config.yml>` keeps an authorized Codex forward simulation
running on the node machine. It schedules due looks, starts clean Codex
deciders and records their returned fields through `lpbot agent decide`.
It uses virtual positions; it does not fund a wallet or submit transactions.
Claude Code's existing panel remains a separate run with separate identities
and decision paths.

The runner is infrastructure. It does not make peer scores safe by moving
them to the server. A passive reader may run only after the CLI's full-horizon
peer-score gate is deployed and verified. An unfinished `1d` score is not a
mature one because its stored end is before the reader's `as_of`. A `next`
score requires the next decision to exist. Keep Reader blocked until those
checks pass; Solo can continue independently.

## Decider boundary

Each due look starts a fresh, stateless Codex context with tools disabled. The
runner does not resume the laptop chat or a prior worker session. It supplies
only the generated brief, the assigned look and permitted peer records printed
by `lpbot agent`. User and project configuration, repository instructions,
skills, MCP servers and repository documents must not enter that context. The
coordinator and runner may inspect operational state; they must not pass their
own development, research, review or outcome context to a decider. Continuity
comes from the run state and permitted CLI output, not a persistent model chat.

Stock `codex exec` is insufficient for this boundary: model metadata can expose
native tools even when shell and Code Mode feature flags are disabled. Inference
therefore uses a separate pinned `codex-exec` build with an unconditional empty
startup tool allowlist. The official Codex CLI remains unchanged for login and
version/capability checks. See [the host build](../deploy/codex-host/README.md).
Both its SHA-256 and a build marker must match before inference; missing or
ordinary executables fail closed. The marker identifies the reviewed build;
the actual protection is the startup `ToolPolicy`, tested inside upstream
session and tool-registry code.

This comparison is passive-peer only. Do not enable `ask`, `inbox`, `answer`,
`answers` or board advice, even when a generic brief mentions them. Do not give
an active decider raw decision, reasoning or evaluation files, result tables,
prices, news or Grafana access. The coordinator handles review and evaluation
separately under [AGENT_WORKFLOW.md](AGENT_WORKFLOW.md).

The model returns structured decision fields; the runner validates the look
and `as_of` and forwards the fields unchanged to `agent decide`. Its model
process cannot run arbitrary commands or write a decision directly. The
runner schedules its separate review process for entries, exits and one
decision each UTC day. Do not enable automatic approval of commands,
repository changes or deployment work. Those changes keep their normal review
and authorization process.

Review is the manual blind-first workflow automated by the coordinator, not a
native `agent review` command. A fresh reviewer first forms an independent
decision from the same permitted input. The runner seals that response and its
checksum before a second fresh reviewer context sees the author's response.
Stage two assesses the reasoning without outcomes. The LP decision is recorded
before review so review latency does not age its observation. A failed review
stays durably queued and pauses further decisions for that identity until the
review completes. The assessment does not replace the author's fields or act
as a semantic veto. Records live under `data/codex/reviews`, outside the active
run's files and decider prompts.

OpenAI documents `codex exec` for scheduled, non-interactive jobs and supports
schema-constrained final output. Its ordinary CLI defaults alone do not
establish this project's decider boundary: the runner must also prevent
configuration, tool and repository-context loading. See the official
[non-interactive mode documentation](https://learn.chatgpt.com/docs/non-interactive-mode).

## Prepare the node

Read [NODE.md](../deploy/NODE.md) and [RUNBOOK.md](../deploy/RUNBOOK.md) before
operating the node. Do not touch Claude's `tmux` session or recreate Reth,
Lighthouse or Postgres for this runner. Deployment of a changed binary is a
separate authorized operation. Build only after checking load and disk, with
`CARGO_BUILD_JOBS=2`, and use a named `tmux` session for a long build.

The following are required before cutover:

- The reviewed binary is `target/release/lpbot` in the node checkout, including
  shared command locking and the peer-score maturity fix. Existing Claude
  commands must invoke that binary too; locks in only the Codex wrapper cannot
  coordinate an older binary. Already-running CLI processes retain the old
  code. Let them drain naturally before relying on the new lock guarantee;
  do not interrupt or stop Claude's session.
  The current readiness gate compares executable inodes, so it also waits for
  an older compatible binary. A long `next --wait` can delay cutover until its
  due look. Plan upgrades around those waiters; do not substitute an unverified
  compatibility exception.
- `LPBOT_DATA_DIR=/data/lpbot/lake` and `RAYON_NUM_THREADS=2` are set for every
  runner invocation. The supplied user service sets both.
- The owner has pinned the exact Codex CLI build reviewed for the worker's
  isolation options and authenticated a dedicated `codex_home` with a
  file-backed `auth.json`. Do not import another chat, global configuration or
  personal authentication automatically. If authentication is unavailable,
  stop before enabling the service and let the owner set it up. `--check`
  verifies executable availability and login status; it does not run a worker
  or establish option compatibility. Verify the pinned worker in an isolated
  compatibility check before activating a real due cycle.
- The dedicated tools-off `codex_exec` has been built from the pinned upstream
  revision and reviewed patch, its upstream regressions passed, and its exact
  SHA-256 is set in `codex_exec_sha256`. `make check` validates lpbot, not that
  separate native host build. Never substitute stock `codex exec` to bypass a
  missing host or a failed native integration check.
- The unchanged experiment and each configured decisions path match the
  existing run. `experiment_sha256` pins the exact experiment bytes. Never
  change the experiment to fit the runner after a decision exists. Activation
  also pins each agent's resolved settings in coordinator state; a later base
  configuration change that changes those settings is refused.
- Heavy evaluation and replay jobs follow the owner's agreed cadence and
  share the maintenance lock. Check cron auto-discovery too: adding a panel
  must not silently add hourly evaluation when its policy is daily. The runner
  marks its panel as managed so the hourly full refresh skips it. The existing
  `refresh.sh --looks` job still discovers managed panels and loads their
  decided looks every minute without replaying or scoring them.
- `tmux` is available for the runner's own named long jobs. The runner must not
  attach to, type into or stop Claude's existing sessions.

Copy [codex-panel.example.yml](../deploy/agents/codex-panel.example.yml) to
`data/codex/panel-runner.yml`, then replace the placeholders. This local config
and all runtime data stay uncommitted. The relative paths are from the
repository root; use absolute paths for `codex` and `codex_home`. Keep the
dedicated Codex home private, and never print or commit authentication tokens.
OpenAI treats saved CLI authentication as a secret; see
[authentication in automation](https://learn.chatgpt.com/docs/non-interactive-mode#authenticate-in-automation).

The runner fields are operational settings:

- `state_dir` stores coordinator state and job artifacts. Its example value is
  `data/codex/runner`.
- `results_dir` selects the daily replay output; its default is
  `runs/codex-panel-forward`. Use a separate directory for another panel.
- `poll_seconds` defaults to `30` between scheduler checks.
- `timeout_seconds` defaults to `300` per inference.
- `codex_exec` is the absolute path of the separately built tools-off host;
  `codex_exec_sha256` pins its exact binary. `codex` continues to select the
  official login CLI. The host checksum is rechecked for each model invocation.
- `min_disk_gb` defaults to `150` free before a due cycle.
- `max_load` defaults to `4` for host load.
- `max_cpu_c` defaults to `80`; missing CPU measurements also defer the job.

## Check and cut over

Run these from the node checkout using the reviewed binary:

```sh
export LPBOT_DATA_DIR=/data/lpbot/lake
export RAYON_NUM_THREADS=2
target/release/lpbot agent-runner data/codex/panel-runner.yml --check
target/release/lpbot agent-runner data/codex/panel-runner.yml --status
```

`--check` validates configuration, immutable identities, executable and
authentication prerequisites, and resource limits. It runs no inference,
synchronization or decision; its resource check may create the shared
operations lock file. `--status` validates the config and prints persisted
state without starting workers. Neither claims ownership or permits another
writer. `--once` executes one scheduler cycle and may infer, review and record
actual decisions for due agents, then run due daily coordinator work. It is
not a dry run.

Cut over only after the checks pass and the current decision is settled:

1. Fence the laptop's writer for these Codex identities, including its
   heartbeat. Wait for an in-flight decision to finish or reconcile its
   canonical state. Leave Claude's writers alone.
2. Verify the latest committed decision and pending look through the CLI.
   Preserve the experiment, decision paths and runner state. The per-agent
   runner fence and command lock must allow only one writer.
3. Record the cutover time, binary revision, Codex version, actual model
   identifier when exposed, runtime and context transition in local
   operational provenance. Preserve existing `codex-inherited` experiment
   metadata. Do not rewrite it or claim that a new server context is the same
   laptop session or a controlled comparison of named models.
4. Run `--once` to verify one due cycle, or enable the service to wait for the
   next due look. A not-yet-due cycle must not invent a decision.
5. Confirm the CLI acknowledged any decision and reports its next due time.
   Keep the laptop writer fenced after the server takes ownership.

## User service

The supplied service assumes the checkout is `~/lpbot` and the local config is
`~/lpbot/data/codex/panel-runner.yml`. If those paths differ, adapt the unit
before installing it. `ExecStart` runs the loop, which coordinates
decisions, reviews and daily reporting. It does not authenticate Codex, build
or change containers.

For dashboard loading, the owner must provide `LPBOT_DB_URL` in the private
local file `data/codex/coordinator.env`, read by the service's optional
`EnvironmentFile`. Keep its permissions private and never print its contents.
Resolve existing Compose database credentials privately into this dedicated
environment during owner operations; do not expose them in terminal output.
Without a database URL, the daily load fails and needs inspection. That
coordinator environment is cleared before any model worker starts.

After the cutover checks, install the unit as the ordinary node user:

```sh
mkdir -p ~/.config/systemd/user
cp deploy/agents/codex-panel.service ~/.config/systemd/user/codex-panel.service
systemctl --user daemon-reload
systemctl --user enable --now codex-panel.service
systemctl --user status codex-panel.service
journalctl --user -u codex-panel.service -n 50 --no-pager
```

To keep a user service running across logout and start it at boot, the owner
must enable user lingering if it is not already enabled. Check with
`loginctl show-user "$USER" -p Linger`. This procedure does not invoke sudo or
change host login policy on the owner's behalf. Without lingering, do not
claim that the service survives logout or a reboot.

The service stops only its coordinator process (`KillMode=process`). Its owned
tmux jobs deliberately finish after a stop or restart, including when the
coordinator started the first tmux server after boot. New jobs use the private
`lpbot-codex-runner` tmux socket; retained older jobs keep their recorded/default
server for recovery. Do not kill a tmux server or assume that stopping the
service has drained its jobs. Keep the ownership fence until reconciliation
and `--release` have verified every owned exit record.
On restart, the coordinator waits for any retained command job before rotating
its owner token, so a delayed child can still authenticate and finish. The
retained command record then goes through normal canonical reconciliation.

The journal holds service messages; `state_dir/state.json` holds coordinator
state and `state_dir/jobs` holds CLI job arguments, stdout, stderr and exit
records. `state_dir/active` and `state_dir/last` name each job slot's running
and last finished job (see "Recovery and maintenance"). Worker directories
retain the permitted prompt, schema, response,
event stream and runtime provenance. Keep those records out of decider
context. Monitor scheduling lateness, health deferrals, model failures and
canonical decision acknowledgments. Successful nonterminal recovery clears
the old error in status; inspection stops retain their error. A running process
alone does not prove that a due look was decided.

## Daily coordinator work

The runner schedules `agent evaluate --horizons next,1d --jobs 2`, a reporting
replay into `results_dir`, and `load-results` once per UTC day. It skips the
activation day to avoid repeating an earlier coordinator evaluation during
cutover. Resource deferrals can retry later; once a day is reserved, a failed
or uncertain job is retained for inspection instead of being relaunched on
every poll. A `--once` cycle may perform this work too.

The coordinator records complete and unfinished score counts separately.
It first requires five canonical Solo decisions, including an entry, whose
observation boundaries are at least one day old. It then reserves that UTC day
and scores once, even if no previous evaluation exists. Reporting replay and
loading require five proven full stored one-day horizons, including an entry.
Insufficient stored coverage is retained as `scored_waiting_for_maturity` and
reconsidered the next UTC day; wall-clock age alone does not prove coverage.
Terminal partial horizons remain labelled, and raw evaluator summaries can
include them; do not call those full-horizon results. Daily reporting and its
outcomes never enter active decider context. The separate blind-first review
jobs assess the permitted look and reasoning without outcome data.

## Recovery and maintenance

The decision files remain the run's record. If `agent decide` times out or its
connection is lost, command success is unknown until canonical state is
checked. The runner reconciles that state under the agent lock before it
retries. Do not resend a saved response by hand, edit JSONL files, delete a
pending look or discard runner state to force progress.

Each job runs in a slot of its own. Every agent has one for its commands
(`next`, `brief`, `decide`) and one for its model calls, decisions and
reviews alike, and the panel has one for its daily scoring, replay and load.
A slot's running job is recorded in `state_dir/active/<key>.json` and its
last finished job in `state_dir/last/<key>.json`; the key is the agent's
name, `<agent>.model` or `panel.daily`. Agent names have no dots, so no two
slots share a record, whatever the agents are called. Before `decide` starts,
the saved attempt names that job's folder. Reconciliation first waits for a
job the agent's command slot still holds, then reads the attempt's own
`job.json`, `stdout.txt` and `exit.json`, never whatever the slot ran since.
A slot that still holds a job starts no other.

If the look or `as_of` changed while inference ran, discard the stale response
and obtain the current permitted look. Never apply it to the next look or
backdate a decision. An undecided run keeps its state under the engine's
limits; restarting the runner does not justify a fabricated `hold`.

The runner serializes shared lake work with other participating CLI commands
and separates it from maintenance jobs. Do not launch a second heavy replay
or evaluator alongside one already running. Evaluation files and dashboards
may contain terminal windows; mature peer handover still requires the full
requested span. Blind reviews follow the manual two-stage workflow until
`agent review` exists, and review feedback never enters an active decider's
context.

For planned runner maintenance, use `systemctl --user stop codex-panel.service`
and check its status. The persistent `.runner-owner.json` fence survives a
service stop and rejects ordinary commands from another writer. To transfer a
run back to the laptop, stop the daemon, reconcile canonical state and release
ownership through the CLI:

```sh
target/release/lpbot agent-runner data/codex/panel-runner.yml --release
```

`--release` requires the runner to be stopped and its owned tmux jobs to have
durable exit records. A job without an exit record needs inspection before
ownership can move. Keep the laptop heartbeat fenced until release succeeds,
then record another runtime/context transition before re-enabling its writer.
Preserve the experiment and decision paths.
Do not bypass ownership by modifying run or fence files. Stopping this service
does not authorize stopping another session or service.
