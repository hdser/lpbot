# Runbook for the node machine

What to type on the node machine for the things that come up: starting a long job so that a dropped connection does not stop it, coming back to it, and getting going again after the machine was off. Setting the machine up is in [REMOTE.md](REMOTE.md); what the services are and what the checks do is in [README.md](README.md). A Claude Code session that checks, operates or debugs the machine starts with [NODE.md](NODE.md): how to connect, what runs there, where everything is, and the rules it keeps.

## Two kinds of things run here

| | Services | Jobs |
| --- | --- | --- |
| What | The containers: Reth, Lighthouse, Postgres, Prometheus, Grafana, and the watcher that keeps the live tables current | Commands that run until they are done: `make snapshot`, `make gates`, `make history`, `make backtest`, `make calibrate` |
| Started with | `make up`, or `make viewer` for Postgres, Prometheus and Grafana alone | Typing them, inside `tmux` |
| When your SSH connection drops | Keep running | Keep running if they were started inside `tmux`; stop otherwise |
| When the machine restarts | Come back by themselves | Are gone. Start them again: each carries on from what it already stored |

A third kind runs on a schedule from the user's crontab (`crontab -l` lists it) and comes back after a restart by itself: the refresh of the agents dashboard, "Keeping the agents dashboard current" below.

## tmux

`tmux` keeps a terminal alive on the machine after you disconnect. Use one session per job, named after it.

| To | Type |
| --- | --- |
| Start a session called `node` | `tmux new -s node` |
| Leave it running and get your shell back | Ctrl-b, then d |
| See which sessions exist | `tmux ls` |
| Come back to it | `tmux attach -t node` |
| Scroll up in its output | Ctrl-b, then `[`; arrow keys or Page Up; `q` to stop scrolling |
| Close the session once the job is done | `exit` inside it |

- Closing the laptop or losing the connection only detaches you. Ctrl-c inside the session stops the job.
- `no sessions` or `no server running` means no job is running: the machine restarted, or the job finished and its session was closed.
- Do not run `tmux kill-server`. It ends every session and the jobs in them.
- Start `tmux` only after you have logged out and back in once following `scripts/host-setup.sh`. A session from before that login cannot use Docker; `exit` it and start a new one.

## Start a long job

```sh
tmux new -s node        # `node` for the download, `gates` for the checks
cd ~/lpbot
make snapshot           # or the job you want
```

Then Ctrl-b, d. `tmux attach -t node` shows how far it is.

| Job | When | To carry on after an interruption |
| --- | --- | --- |
| `make snapshot` | Once, before the first `make up` | `LPBOT_SNAPSHOT_RESUME=1 make snapshot`. It carries on with the snapshot it began with, written down in `DATA_DIR/reth-snapshot-manifest`, and Reth keeps the files it has already verified. Before downloading, Reth checks what is on disk and prints nothing until `Startup integrity summary`: tens of minutes after an interruption, about 90 for 2 TB on disk. It is not stuck; see "Is the snapshot download working?" below before pressing Ctrl-C. If it is stopped before it finishes, the script says so and gives the resume command |
| `make gates` | Once the node is synced | `make gates` again, or `make gates STEP=<step>` to start at the step that was running. What is stored is not fetched again. A day file a power cut left empty or cut short is renamed to `<name>.damaged`, named in the output, and fetched again; the `.damaged` files can be deleted |
| `make history`, `make backtest`, `make calibrate`, `make load` | The steps of `make gates`, one by one | The same command again |
| `make gas-history FROM=<day>` | After `make history`, from the same first day, before any backtest: each block's base fee and priority fee, about 30 minutes for two years of blocks. `make gates` runs it as its `gas` step | The same command again: only the newest chunk of 100,000 blocks is read again |
| `make pool-stats` | After `make history`, for the pool dashboard (price, value held, volume, fees, the liquidity around the price); `make load` then loads it | The same command again: it replaces each pool's tables |
| `make watch` | Only to run the watcher by hand instead of as a service: stop the service first (`make watch-stop`), the two must not run at once. See "The live tables" below | The same command again: it carries on from its checkpoint |
| A forward run in the agent mode | A Claude Code session deciding `experiments/agent-usdc-weth-forward.yml` at the head of the chain. See "A forward run in the agent mode" below | Start the session again with the same brief: it carries on from the decisions file, and the next look is shown at the head as it is then |

### Is the snapshot download working?

Its output stays still for long stretches: through the check of what is on disk, and between progress lines. Look at the disk instead, from a second SSH window, not the `node` session:

```sh
vmstat 5          # one line every 5 seconds; Ctrl-c stops it, not the download
```

| `vmstat` shows | It is |
| --- | --- |
| `bi` in the hundreds of thousands (KB read per second), `bo` near 0, `wa` high | Checking what is on disk. 400,000 is about 400 MB/s, so 2 TB takes about 90 minutes |
| `bo` large | Downloading and unpacking |
| `bi` and `bo` both near 0 for several minutes | Waiting, or stopped. Look at the `node` session; if it shows a prompt, the download is not running |

`vmstat` only watches. It can run, or not, without changing the download.

## The live tables (the watcher)

The watcher is the service that runs `lpbot watch-pools`. It follows every stored pool from the local node, block by block, and keeps the live tables in Postgres current; the Grafana dashboard `Pools: live` reads them. It is a container like the others: `make up` builds and starts it, and it comes back by itself after a restart.

| To | Type |
| --- | --- |
| See what it is doing | `make watch-logs` (Ctrl-c leaves; it keeps running) |
| Stop it, start it again | `make watch-stop`, `make watch-start` |
| Run a new version after `git pull` | `make watch-build`: it rebuilds the image (a few minutes, two compile jobs) and restarts it, which carries on from its checkpoint |
| Check that it keeps up | The dashboard's "Block age" (how old the last block it applied is: up to about 12 seconds), "Blocks behind the head" (0 or 1) and "Written" (seconds since its last write). A block age of minutes with 0 blocks behind is the node itself behind the chain: see "After the internet was down" |

**The first start takes a while.** With no checkpoint it builds its state from the stored history, and says so line by line in `make watch-logs`:

1. `storing the position manager's logs from ... to ...`: the NFT events of the stored days, read from the node and kept under `DATA_DIR/lake/manager`. Four days at a time, a few seconds per day: about 15 minutes for a year. Only the first time; later starts find them stored.
2. `rebuilding from the stored history`, then one line per day: every pool and position replayed from the first snapshot. Positions it meets for the first time are read from the node at the block before.
3. `following ... pool(s) from block ...`: it catches up with the node, 500 blocks per step, then follows the head.

It saves a checkpoint of its state at the finalized block every ten minutes, `DATA_DIR/lake/live/checkpoint-ethereum.json`. A restart, of the container or the machine, carries on from there: `carrying on from the checkpoint at block ...`, then it catches up on what it missed. A stop of hours or days is caught up the same way, from the node.

**Its messages.**

| It says | It means |
| --- | --- |
| `reorg: N block(s) taken back` | The chain replaced its newest blocks. It rebuilt its state from the finalized block and wrote the affected rows again. Nothing to do |
| `... reading again` | The node's answers did not fit together, usually because a block arrived meanwhile. It tries again at once |
| `the node did not answer` | Reth is down or busy. It waits and tries again, longer each time; it carries on when Reth is back |
| `could not write to the database` | Postgres is down or refused a write. It keeps following, and writes everything again once Postgres is back |
| `note: block N: ...` | A position did not add up, most often one that held liquidity before the stored history began and that nobody touched since. It carries on; the count is in `live_pools.anomalies` |
| `the chain reorganized below the finalized block` | Should not happen. Rebuild, as below |

**Rebuild** when it stopped on an error that names the checkpoint, or to follow positions from another day:

```sh
make watch-stop
rm "$(sed -n 's/^DATA_DIR=//p' deploy/.env | tr -d '"')/lake/live/checkpoint-ethereum.json"
make watch-start          # no checkpoint: it rebuilds from the stored history
```

To follow positions from a later day, which is quicker but sees fewer positions, set `LPBOT_WATCH_ARGS="--from 2026-01-31"` in `deploy/.env` before `make watch-start`; the replay starts at the newest snapshot on or before that day.

**Running it by hand.** `make watch` runs the same command in the foreground, from the source; use it inside `tmux`, with the service stopped:

```sh
make watch-stop
tmux new -s watch
make watch                     # or: make watch ARGS="--once" to catch up, write once and stop
```

## A forward run in the agent mode

For the separately owned Codex panel, the server scheduler, dedicated login,
single-writer cutover and recovery procedure are in
[CODEX_RUNNER.md](../docs/CODEX_RUNNER.md). Do not reuse Claude's session or
decisions paths. A deployed CLI serializes finite lake jobs and releases
its locks while waiting for a future look or board answer; older jobs already
running at deployment must finish naturally before the new shared locking
can protect a Codex cycle.

A Claude Code session decides a run look by look at the head of the chain, on blocks no model has seen: `experiments/agent-usdc-weth-forward.yml`, USDC/WETH 0.05%, $10,000 in USDC, loss limit 20%. Before every look `lpbot agent next` brings the pool's events and the gas prices up to the node's head, and it shows a look only while the node is at the head. What it does and why is in `docs/DESIGN.md`, "Forward runs in the agent mode". The brief for the deciding session is `data/agent/BRIEF-forward.md` on the machine it runs on; like everything under `data/`, it is not committed.

**Before it starts.**

1. The node is at the head: `make status` shows the newest block seconds old, and Lighthouse synced. Otherwise `agent next` refuses with `the node is behind the chain` and shows no look.
2. `from:` in the experiment is the day the session starts. The first look is due at 00:00 UTC that day and is shown at the head when `agent next` first runs. Once there are decisions, change nothing in the experiment: they answer the looks of this run as it is.
3. The binary is built: `cargo build --release -p lpbot` (`CARGO_BUILD_JOBS=2 cargo build --release -p lpbot` on the node machine, which runs hot).

**Where it runs.** The same commands work in both places. The node machine is the better one for a run of more than a day or two: it is always on and needs no tunnel.

| | On the node machine | On the laptop |
| --- | --- | --- |
| The node's RPC | Local: the config's `http://127.0.0.1:8545` | Through a tunnel left open: `ssh -N -L 18545:127.0.0.1:8545 <user>@<node machine>`, and `export LPBOT_RPC=http://127.0.0.1:18545` in the session's shell |
| The pool history | `/data/lpbot/lake`: the make targets read it from `deploy/.env`; for `target/release/lpbot` itself, `export LPBOT_DATA_DIR=/data/lpbot/lake` | `data/lake`, the copy of the history. The first catch-up stores what the copy lacks since it was made, a minute or two through the tunnel |
| The session | Claude Code in `tmux` (`tmux new -s agent`, then `claude` in `~/lpbot`), or the desktop app's SSH connection to the machine | The desktop app, on the repository |
| What stops it | A restart: start the session again | Sleep, a closed lid or a dropped tunnel: open it again and start the session again |

Decide a run from one place only: the run is its decisions file, `data/agent/usdc-weth-forward.jsonl`, next to the store it was decided on.

**How the session comes back at each look.** `agent decide` records the decision and prints when the next look is due by the wall clock, as `Next look due: 2026-11-25T04:00:00Z`.

1. Best: the session starts `target/release/lpbot agent next experiments/agent-usdc-weth-forward.yml --wait 2d` as a background command (Claude Code's Bash tool with `run_in_background`). It sleeps until the look is due, brings the store up to the head, prints the look and exits, and Claude Code wakes the session when it exits. Nothing runs in the session meanwhile.
2. Otherwise `/loop` with no interval: each round runs `agent next` without `--wait`, decides the look if one is shown, and otherwise schedules its next wake-up for when the look is due. A wake-up is at most an hour away, so a look a day ahead costs up to 24 short checks.

**A panel of agents forward.** Several sessions decide one panel experiment, one agent each (`--agent <name>` on every command), on the same machine. They share the panel's folder under `data/agent/`, and so its `board.jsonl`, where they ask each other and answer. Each session starts in its own window of the `agent` session, with the agent's brief and model, and may run only `target/release/lpbot agent ...`: reading files, the web and every other command are denied without asking, so it never stops on a question and cannot open what its brief puts off limits:

```sh
tmux new -s agent            # once; then, for each agent of the panel:
tmux new-window -d -t agent -n a deploy/agents/start-agent.sh experiments/agent-panel-forward.yml a
tmux new-window -d -t agent -n b deploy/agents/start-agent.sh experiments/agent-panel-forward.yml b
```

The first start on a machine shows Claude Code's first-run screens in that window (theme, security notes, trust of the folder): answer them there once. Between its looks a session runs two commands in the background, `agent next ... --wait 2d` for its next look and `agent inbox ... --wait 2d` for questions to it; whichever returns first wakes it. After answering a question it starts `inbox --wait` again. At a look it may ask the others and wait for their answers before deciding. The decision acts no earlier than the newest answer it read, and no answer in time means it decides alone. `docs/DESIGN.md`, "Advice in forward and live runs", has the rest.

**What `agent-panel-forward` shows its agents changed on 26 November 2026 at 22:38:43 UTC,** when the node's `target/release/lpbot` was rebuilt with pull requests 24 and 25. From then on a and b are handed only the peer scores whose horizon is complete, and their commands take the command lock and the lake lock. Their looks before that time were shown under the old rules. The run files do not record this; this line does.

If the session ends, nothing is lost: the decisions file and the reasoning log beside it hold the run. Its window stays open and prints the command that starts it again. A new session with the same brief runs `agent next` and carries on; a look that came due meanwhile is shown at the head as it is then, and acted on there.

| To | Type |
| --- | --- |
| See the next look, after the catch-up | `make agent-next` (node machine), or `target/release/lpbot agent next experiments/agent-usdc-weth-forward.yml` |
| Wait for it | `make agent-next ARGS="--wait 1d"` |
| Decide it | `target/release/lpbot agent decide experiments/agent-usdc-weth-forward.yml --action stay_out\|hold\|exit\|enter [--lower <pct> --upper <pct>] --next <duration> --note "..." --reasoning "..."` |
| Ask the panel at a look | `target/release/lpbot agent ask <experiment> --agent <name> --to all --question "..." [--wait 10m]`, then `target/release/lpbot agent answers <experiment> --agent <name> --wait`, which returns once all have answered or the deadline has passed |
| Wait for questions between looks | `target/release/lpbot agent inbox <experiment> --agent <name> --wait 1d`, in the background: it returns as soon as a question arrives |
| Answer one | `target/release/lpbot agent answer <experiment> --agent <name> --ask <id> --answer "..." [--would stay_out\|hold\|exit\|enter] [--confidence <0-1>]` |
| See who asked and answered what | `target/release/lpbot agent board <experiment>` |
| Score the decisions and put the run on the dashboards so far | On the node machine cron does it, below. By hand: `deploy/agents/refresh.sh experiments/agent-usdc-weth-forward.yml`, which runs `target/release/lpbot agent evaluate` (each decision against cash and holding, into `<name>.eval.jsonl` beside the decisions), then `target/release/lpbot run ... --results-dir runs/usdc-weth-forward`, then `lpbot load-results`. `lpbot run` writes the agent tables too (`agent_looks`, `agent_scores`), which the `Agents` dashboard reads. A panel of several agents is scored and loaded the same way |
| Put the looks decided since on the dashboard, without the replays | `deploy/agents/refresh.sh --looks`, which runs `target/release/lpbot load-looks <experiment>`: each look with its reasoning, in seconds |

**Keeping the agents dashboard current.** Grafana reads Postgres, not the agents' files, so something has to load them. Two cron lines on the node machine do it for every forward panel with decisions on disk (an experiment under `experiments/` with `to: open`):

```sh
crontab -e     # then add these two lines
* * * * *  $HOME/lpbot/deploy/agents/refresh.sh --looks >> $HOME/lpbot/logs/agent-refresh.log 2>&1
17 * * * * $HOME/lpbot/deploy/agents/refresh.sh >> $HOME/lpbot/logs/agent-refresh.log 2>&1
```

- Every minute, `--looks` loads the looks decided since, with their reasoning and stances, from each agent's log (`lpbot load-looks`). It needs no replay, does nothing when nothing changed, and writes a line to the log only when it loads something.
- At 17 minutes past each hour, the full refresh scores every decision, replays each run into `runs/<experiment without "agent-">` and loads the results, the scores and the looks: a minute or two a panel, at low priority. Its full output is in `logs/agent-refresh-last.log`.
- Both read only what is stored. The agents bring the store up to the head at each look, so results and scores reach as far as the newest look of any agent on the pool; nothing runs `agent next` for them. The dashboard itself refreshes every minute.
- A new panel shows on the dashboard from the first full refresh after its first decision: the dashboard lists an experiment once its runs are loaded.

**Its messages.**

| It says | It means |
| --- | --- |
| `No look is due yet. Look N is due at ...` | Come back then, or wait for it with `--wait` |
| `block N, the newest the store reaches, was made ... ago: the node is behind the chain` | No look until the node is at the head again: `make status`, and "After the internet was down" below if the connection dropped |
| `note: the node is still syncing (block X of Y)` | Reth is catching up. The store goes no further than the block it has executed; looks are refused until it is at the head |
| `note: the catch-up failed (...); trying again in 12 s` | The node answered out of step for a moment, as Reth does while it commits a batch of blocks. It tries once more |
| `bringing the store up to the node's head before the look; is the node reachable ...` | No answer from the node: on the laptop, the tunnel is down or `LPBOT_RPC` points elsewhere; on the node machine, Reth is down (`make status`) |
| `Look N is due since ...; the store does not reach its block yet` | The look's block is made but not yet stored, a minute or so after it is due. Run it again; `--wait` asks again every block |
| `the look was shown ... ago, so answers due ... after it are already late` | A question is due `--wait` after the look was shown. Run `agent next` again to see the look at the head, then ask |
| `asking and answering are for forward and live panels` | The experiment's window has a last day: a backtest, where agents read each other's logs but do not ask |
| `It acts from the block at ..., the newest market the board's answers you read are as of` | An answer you read was as of a later market than your look, so the decision acts there, not where the look was shown |

### Is every agent working?

`make agents` prints every forward agent on the machine, whatever runs it: its state, when its next look is due (counted from its last decision, never from when a look was last shown), what waits for it and, in a column of its own, what schedules it. `make status` ends with the same in one line an agent, and the `Agents` dashboard has it in its Now row, written every minute by `refresh.sh --looks`. A change of state is a line in `logs/agent-refresh.log`, such as `agents: b (agent-panel-forward): waiting -> late (...)`.

The agent list `data/agents/agents.yml` (copy `deploy/agents/agents.example.yml`) says what the files cannot: agents that have not decided yet, what runs each one and where, whether a session here keeps `agent next --wait` (`waits_here: true`), and agents set aside on purpose (`state: paused` or `ended`, with a `reason`). Keep it in step when a panel starts or stops.

| State | It means | What to do |
| --- | --- | --- |
| `waiting` | Its next look is ahead, and something here will wake it: a session's `agent next --wait` lasting past the due time, or the runner | Nothing |
| `unverified` | Its next look is ahead, and it runs from elsewhere (the laptop, another machine): nothing here can show what will wake it | Nothing, unless it turns `late` |
| `deciding` | Its look is due and shown, or it is waiting for answers to its questions | Nothing, unless it turns `late` |
| `due` | Its look is due within the grace (10 minutes by default) and not shown yet | Nothing: the catch-up takes a minute or two |
| `late` | Its look was due more than the grace ago and is not decided | Look at the session (`tmux list-panes -a -F '#{session_name}:#{window_name} #{pane_current_command}'`, never attaching to the `agent` session unasked) or the runner; a session that ended prints how to start it again |
| `no waiter` | A session that waits here has no `agent next --wait` lasting until its look: it will not wake by itself | Its session has stopped or forgot to wait: start it again with `deploy/agents/start-agent.sh <experiment> <agent>` in its window, with the owner's yes |
| `stopped` | The runner stopped it for inspection (the phase says why) | Inspect as `docs/CODEX_RUNNER.md` says before releasing it |
| `failing` | Its decisions cannot be read, or the runner recorded an error, or the list names an agent its experiment does not have | Read the detail |
| `scheduler down` | A runner's fence holds it, and no runner process is running | Start the runner again, or release it (`docs/CODEX_RUNNER.md`) |
| `paused`, `ended` | Set aside in the agent list | Nothing |

"Not listening for questions" in the detail means a panel agent here has no `agent inbox --wait` running: a question to it waits until its own next look.

## After the machine was off

1. Log in and look:

   ```sh
   cd ~/lpbot
   make status       # which containers are up, and how far the node is
   tmux ls           # which jobs are running: none after a restart
   ```

2. **Services need nothing.** Docker starts at boot and brings back every container that was running. If `make status` lists none, they had been stopped with `make down`: start them with `make up`, or with `make viewer` if the node has not been started yet. While a snapshot download is unfinished, `make up` refuses to start the node and says to resume the download first.

3. **Jobs have to be started again**, in a new `tmux` session, with the command from the table above. The sessions of a forward agent panel are such jobs: start each again with `deploy/agents/start-agent.sh`, "A forward run in the agent mode" above; cron's refresh of the dashboard comes back by itself. For a snapshot download that was under way:

   ```sh
   tmux new -s node
   cd ~/lpbot
   LPBOT_SNAPSHOT_RESUME=1 make snapshot
   ```

   Not sure whether the download had finished? Run it: Reth passes over what it already has. If it refuses because `reth-snapshot-manifest` is missing, it says how to name the snapshot you began with. Reth lists the snapshots of the last two weeks or so, so a download should not be left paused longer than that. Once the node has been started with `make up`, the command refuses to run: the database is the node's own by then, and a download would overwrite it. `make up` writes `DATA_DIR/reth-node-started` the first time it starts the node, and that file is what tells. The other way round, `make up` refuses to start the node while a download is running or unfinished.

4. **Find out why it went off**, if you do not know. The log of the start that went off says how it ended, and what asked for it. `-b -1` is the start before the current one. If the machine was switched on more than once since, it is an earlier one: the first command lists the starts with their times, and `-b -2`, `-b -3` pick them.

   ```sh
   journalctl --list-boots --no-pager | tail -n 5
   sudo journalctl -b -1 -n 40 --no-pager
   sudo journalctl -b -1 --no-pager | grep -iE "power key|powering (off|down)|critical temp|hardware protection|COMMAND=.*(poweroff|shutdown|halt|reboot)" | tail -n 20
   ```

   The times in the log are the machine's, which `date` shows. Ubuntu Server runs on UTC unless it was told otherwise.

   | The log shows | It was |
   | --- | --- |
   | It ends with `Reached target poweroff.target` and `System Power Off` | An orderly shutdown: something asked for it, and the disks were closed properly. The second command says what |
   | `Power key pressed short` | The power button. "Power" under "The machine" in [REMOTE.md](REMOTE.md) says how to make a short press do nothing, and how to check that the setting is in force: a file that was written but never read leaves the button working |
   | `critical temperature reached` or `HARDWARE PROTECTION shutdown` | The kernel switching the machine off to protect it from heat. Deal with the heat before starting the job again: see "Heat" under "The machine" in [REMOTE.md](REMOTE.md) |
   | A `sudo` line with `COMMAND=` and `poweroff` or `shutdown` | Someone typed it |
   | It stops in the middle of ordinary lines, with none of the above | A power cut, a held power button, or the firmware cutting the power. Nothing was closed properly; Reth's database is built to survive that and may re-sync a little. The log reaches the disk every few minutes, so the cut can be up to five minutes after the last line |

## After the internet was down

The machine and the containers keep running without the internet, and nothing needs restarting when it comes back. While it is down:

- Lighthouse logs `Low peer count`, then `Sync state updated` with `new_state: Stalled`, and Reth stops at its last block.
- `make status` says how old the node's newest block is, with a warning past two minutes, and how many peers Lighthouse has. Reth's own line still reads `synced` or `block N of N`: it knows of no higher block.
- On the `Pools: live` dashboard, "Block age" grows while "Blocks behind the head" stays at 0 and "Written" stays fresh: the watcher keeps up with a node that is itself behind.

When the connection comes back, Lighthouse finds peers within a minute or two and syncs (`make status` gives how many slots it is behind), and Reth follows in batches that `make logs` shows stage by stage: Headers, Bodies, Execution, MerkleExecute, then Finish. The newest block moves only at the end of a batch, so it can stand still for ten minutes or more. The watcher follows Reth by itself. Days offline take hours to catch up: after 4 days and 6 hours without internet (20 to 24 November 2026, about 30,700 blocks), Reth's first batch of 500 blocks took 12 minutes, most of it in Execution and MerkleExecute, and later batches were larger.

Catching up keeps several cores busy for hours on a machine that runs hot. Check `sensors`, and that turbo is off: `cat /sys/devices/system/cpu/intel_pstate/no_turbo` prints 1, and a restart sets it back to 0. On 24 November 2026 it was 0, with the CPU at 100 °C; turning it off brought it to about 80 °C:

```sh
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo
```

## Switch it off on purpose

```sh
tmux ls           # a job still running? attach, then wait for it or stop it with Ctrl-c
make down         # stops the containers; Reth gets up to five minutes to close its database
sudo poweroff
```

`make down` removes the containers, so after switching the machine on again start them with `make up` (or `make viewer`), then start any job again as above.

## Keep an eye on

| What | How |
| --- | --- |
| Sync progress, the age of the node's newest block, peers and free disk space | `make status` |
| Whether the snapshot download is reading or writing | `vmstat 5`, see "Is the snapshot download working?" above |
| The node's own log | `make logs`; Ctrl-c leaves it and the node keeps running |
| Temperatures | `sensors` (package `lm-sensors`); `sudo nvme smart-log /dev/nvme0` (package `nvme-cli`) |
| Dashboards | From the laptop: `make tunnel HOST=<user>@<node machine>`, then http://127.0.0.1:3001 |
| The live tables | The `Pools: live` dashboard: "Block age", "Blocks behind the head" and "Written"; `make watch-logs` |
| The agents | `make agents`; the Now row of the `Agents` dashboard; changes of state in `logs/agent-refresh.log` |
