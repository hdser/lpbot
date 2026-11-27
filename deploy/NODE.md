# The node machine: a handout for Claude sessions

For a development or operations session that has to check, operate or debug the owner's node machine: how to reach it, what runs there, where every piece of information lives, and the rules. Read it once; the other files say the rest. [REMOTE.md](REMOTE.md) is how the machine was set up, [RUNBOOK.md](RUNBOOK.md) is what to type there for each job, [README.md](README.md) is what the services and the checks are, and `CLAUDE.md`, "The node machine", is its history and its hardware limits.

**Not for agent-mode deciders.** A session that decides an agent run reads only what `lpbot agent` prints. Point it at `lpbot agent brief <experiment> --agent <name>`, never at this file.

The repository is public, so this file uses placeholders: `<user>@<node machine>` for the SSH login. The real one is on the owner's laptop only, in `data/NODE.local.md` of the main checkout (`data/` is ignored by Git). Ask the owner if it is missing. Never write the real host, user or machine name into a committed file, a commit message or a pull request.

What follows was checked against the machine on 25 November 2026. Figures dated that day are what it showed then.

## The rules

1. **sudo is the owner's.** It asks for the owner's password (`sudo -n true` answers `interactive authentication is required`). Give the owner the command to type, with what it does. Setting turbo off after a restart is the usual one (see "How to check health").
2. **Secrets stay unprinted.** `deploy/.env` holds `POSTGRES_PASSWORD` and `GRAFANA_PASSWORD`, and `deploy/jwt/jwt.hex` is the engine secret shared by Reth and Lighthouse. Read them only inside the command that uses them, as `$(sed -n 's/^POSTGRES_PASSWORD=//p' deploy/.env)`, and never `cat`, `grep`, `echo` or print them. `make grafana-password` prints one: it is for the owner, not for a session.
3. **Never type the Grafana password into a browser**, the app's own or Chrome. If a dashboard has to be looked at, ask the owner to log in, or read the same tables with SQL (below). `/api/health` needs no login.
4. **New Postgres tables need the owner's yes.** The viewer knows the sixteen tables listed below and no others.
5. **Long jobs go in `tmux`**, one session per job, named after it. Never `tmux kill-server`, and never type into, kill or attach to a session you did not start. The `agent` session belongs to a run in progress.
6. **Containers only through the compose file in `~/lpbot`.** Never `up` the compose file of another checkout: it recreates Reth with an empty engine-secret folder (it happened on 20 November 2026). To try a branch's service, build its image and start that container on its own (`docker run`, or `--no-deps` with `--project-directory ~/lpbot/deploy`). Ask the owner before anything that stops or recreates Reth, Lighthouse or Postgres (`make down`, `make up` after a change to `deploy/.env`, an `up` without `--no-deps`).
7. **The CPU runs hot.** Build with `CARGO_BUILD_JOBS=2`; the variable is not set in any shell there, so put it on the command each time. Keep turbo off, and do not start a heavy job while another runs.
8. **Leave agent runs alone.** Write, move or delete nothing under `data/agent/` of a run in progress, and read its files only when the owner asks. The decisions file is the run.
9. **Disk is tight.** The owner chose to keep the full archive and prune later; do not reopen that. Say so when a job would write tens of GB.

Cloud sessions cannot reach the machine at all. Sessions on the laptop can, over SSH.

## Connecting

### SSH

From the laptop:

```sh
ssh <user>@<node machine>
```

The login is by key; the laptop's key is accepted. A session runs commands there one at a time, without a terminal:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=8 <user>@<node machine> 'cd ~/lpbot && make status'
```

Such a command runs in a shell that reads no `~/.profile`, so `cargo` and `claude` are not on its PATH. Use a login shell (`bash -lc 'cargo --version'`) or the full paths, `~/.cargo/bin/cargo` and `~/.local/bin/claude`. `make`, `docker`, `tmux`, `git`, `curl` and `sensors` are found either way.

The machine's clocks and logs run on UTC.

### Tunnels from the laptop

RPC, Postgres and Grafana listen on the machine itself (`127.0.0.1`), never on the network. The laptop reaches them through SSH port forwarding:

| On the laptop | On the machine | What |
| --- | --- | --- |
| `127.0.0.1:3001` | `3000` | Grafana |
| `127.0.0.1:18545` | `8545` | Reth's HTTP RPC |
| `127.0.0.1:15432` | `5432` | Postgres |

The laptop ports differ from the machine's because the laptop runs its own viewer (`make viewer`, under Docker) on 3000 and 5432, and the forward runs in [RUNBOOK.md](RUNBOOK.md) take the RPC on 18545. All three at once:

```sh
ssh -N -L 3001:127.0.0.1:3000 -L 18545:127.0.0.1:8545 -L 15432:127.0.0.1:5432 <user>@<node machine>
```

`make tunnel HOST=<user>@<node machine>` is the short form in the Makefile, with other ports: Grafana on 3001, RPC on 8545 and WebSocket on 8546, and no Postgres.

The owner often has a tunnel of their own open on 3001. Before opening one, see what is already there, and open only what is missing:

```sh
lsof -nP -iTCP -sTCP:LISTEN | grep -E ':(3001|18545|15432) '
ssh -f -N -o ExitOnForwardFailure=yes -L 18545:127.0.0.1:8545 -L 15432:127.0.0.1:5432 <user>@<node machine>
```

`-f` puts it in the background once the ports are open; `ExitOnForwardFailure` makes it fail instead of half-working when a port is taken. Close only the tunnel you opened, by its exact command line (`pkill -f "<the command as typed>"`), and leave the owner's alone.

Through the tunnels:

```sh
curl -s http://127.0.0.1:3001/api/health                       # Grafana, no login needed
curl -s -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' http://127.0.0.1:18545
LPBOT_RPC=http://127.0.0.1:18545 target/release/lpbot ...       # lpbot on the laptop against the node
PGPASSWORD="$(ssh <user>@<node machine> "sed -n 's/^POSTGRES_PASSWORD=//p' ~/lpbot/deploy/.env")" \
  psql -h 127.0.0.1 -p 15432 -U lpbot -d lpbot                 # the password is read, not printed
```

On the machine itself Postgres needs no password from inside its container:

```sh
cd ~/lpbot && docker compose --env-file deploy/.env -f deploy/docker-compose.yml exec -T postgres psql -U lpbot -d lpbot
```

### Claude Code on the machine

- Version 2.1.332, at `~/.local/bin/claude` (a link into `~/.local/share/claude/versions/`), on the PATH of login shells through `~/.profile`. The owner is signed in; user settings are in `~/.claude/settings.json`. `~/lpbot` has no `.claude/` folder.
- A session there runs in `tmux`: `tmux new -s <name>`, then `cd ~/lpbot` and `claude`. It reaches everything locally: no tunnel, and the RPC at the config's `http://127.0.0.1:8545`.
- The desktop app has no saved SSH connection to the machine yet, so a session cannot be started there from the app. The owner starts it in `tmux`, or adds the connection in the app first.

## What runs there

### The services

Seven containers, all from `deploy/docker-compose.yml` in `~/lpbot` (compose project `lpbot`, containers `lpbot-<service>-1`). Docker brings them back after a restart.

| Service | Image on 25 November 2026 | Listens on the machine | Keeps its data in |
| --- | --- | --- | --- |
| `reth` | `ghcr.io/paradigmxyz/reth:v2.10.1` (archive) | 8545 HTTP RPC, 8546 WebSocket, 9001 metrics; 30303 peer-to-peer, open to the internet | `DATA_DIR/reth` |
| `lighthouse` | `sigp/lighthouse:v8.2.1` | 5052 beacon API; 9000 peer-to-peer, open to the internet | `DATA_DIR/lighthouse` |
| `postgres` | `postgres:17`, database and user `lpbot` | 5432 | `DATA_DIR/postgres` (owned by the container's user: `du` and `ls` cannot read it) |
| `prometheus` | `prom/prometheus:v3.16.0`, scraping `reth:9001` and `lighthouse:5054` | 9090 | `DATA_DIR/prometheus` |
| `grafana` | `grafana/grafana:13.3.1` | 3000 | `DATA_DIR/grafana` |
| `watcher` | `lpbot:local`, built from the repository (`deploy/lpbot.Dockerfile`); runs `lpbot watch-pools --config config/mainnet.yml` | nothing; it writes the live tables | `DATA_DIR/lake` |

Reth's and Lighthouse's versions are `RETH_VERSION` and `LIGHTHOUSE_VERSION` in `deploy/.env`. The watcher reaches Reth and Postgres inside the compose network (`http://reth:8545`, `postgres:5432`).

From `~/lpbot`, with `C="docker compose --env-file deploy/.env -f deploy/docker-compose.yml"` (the Makefile's `COMPOSE`):

| To | Type |
| --- | --- |
| See the containers, the sync and the disk | `make status` |
| Read the last lines of a log | `$C logs --no-color --tail 100 reth` (or `lighthouse`, `watcher`, `grafana`, `postgres`) |
| Follow the logs | `make logs` (Reth and Lighthouse), `make watch-logs`. Both follow until Ctrl-c, so a session that cannot press it uses `--tail` instead |
| Stop or start the watcher | `make watch-stop`, `make watch-start` |
| Run a new watcher after `git pull` | `make watch-build` (rebuilds the image with two jobs, restarts only the watcher) |
| Restart Grafana, when a changed dashboard does not show | `$C restart grafana` |

### The `tmux` sessions

On 25 November 2026:

| Session | Holds |
| --- | --- |
| `agent` | Claude Code deciding the two-agent forward panel `experiments/agent-panel-forward.yml`, one window an agent (`a`, `b`), each started with `deploy/agents/start-agent.sh`. Leave it alone |
| `gates` | The shell `make gates` ran in on 19 November 2026; idle |
| `history` | The shell `make history` ran in on 19 November 2026; idle |

`tmux ls` lists them; `tmux list-panes -a -F '#{session_name} #{pane_current_command}'` shows what runs in each without attaching. A session without a terminal of its own starts and reads a job like this:

```sh
tmux new-session -d -s <job> -c ~/lpbot
tmux send-keys -t <job> '<the command>' Enter
tmux capture-pane -p -t <job> | tail -n 30
```

## Where everything is

### The repository: `~/lpbot`

On `main`, at the commit the owner last pulled. It fetches from GitHub over SSH with a deploy key (`git config core.sshCommand` is `ssh -i ~/.ssh/lpbot_deploy`), which [REMOTE.md](REMOTE.md) adds read-only: pull there, push from the laptop. A `git pull` takes effect at once for what is mounted from the checkout: Grafana's dashboards, Prometheus's config, the compose file. The binary and the watcher's image need a rebuild.

| Path | Holds |
| --- | --- |
| `deploy/.env` | Settings for compose, the scripts and the Makefile. Keys: `DATA_DIR`, `RETH_VERSION`, `LIGHTHOUSE_VERSION`, `RETH_EXTRA_ARGS`, `CHECKPOINT_SYNC_URL`, `POSTGRES_PASSWORD`, `GRAFANA_PASSWORD`, `HOST_UID`, `HOST_GID`, and for alerts `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, `GRAFANA_ALERTING`, `GRAFANA_ROOT_URL` ([RUNBOOK.md](RUNBOOK.md), "Alerts on Telegram"; the token is a secret, rule 2). Compose also reads `LPBOT_WATCH_ARGS` and `CARGO_BUILD_JOBS` when set; neither is. List the keys with `sed -n 's/^\([A-Z_][A-Z0-9_]*\)=.*/\1/p' deploy/.env`, never the file. `DATA_DIR` is `/data/lpbot` |
| `deploy/jwt/jwt.hex` | The engine secret. Mounted as `./jwt`, relative to the compose file, which is why compose runs only from here |
| `target/release/lpbot` | The binary. Build it with `CARGO_BUILD_JOBS=2 cargo build --release -p lpbot` (cargo 1.100.0 in `~/.cargo/bin`). `lpbot --version` prints only `0.1.0`: compare `ls -l target/release/lpbot` with `git log -1 --format=%ci` to see whether it is older than the checkout |
| `logs/` | `gates-<time>.log`, the whole output of each `make gates` (UTC time in the name). Beside them, logs kept by hand: `make-gates-<time>.log`, `history-<time>.log`, `build-lpbot.log` |
| `runs/` | Result tables as Parquet, one folder per table (`runs/runs`, `runs/campaigns`, `runs/decisions`, `runs/series`, `runs/pool_rank`, `runs/zone_map`), one file per run, `<experiment>-<nnn>-<hash>.parquet`; `runs/calibration/` holds the YAML reports of `make calibrate`. Other runs write to `--results-dir runs/<name>` |
| `data/agent/` | Agent runs, one set of files per agent beside its decisions file `<name>.jsonl`: `<name>.jsonl.pending` (the look waiting for a decision), `<name>.log.md` (the reasoning log), `<name>.log.jsonl` (the shared record of each look), `<name>.eval.jsonl` (the scores from `agent evaluate`). A panel has its own folder, `data/agent/<panel>/`, with the panel's `board.jsonl` (questions and answers). On 25 November 2026 only `data/agent/panel-forward/` is there, for a run in progress |
| `data/agents/agents.yml` | The agent list for `lpbot supervisor status`: every forward panel and agent, what runs each one and where, whether a session here keeps `agent next --wait`, and agents paused or ended with why. The example is `deploy/agents/agents.example.yml`. Without it the status still shows every open panel with decisions |
| `logs/.agent-status-state.json` | The status's memory between passes: when each look was first seen shown, and the states last logged |

`lpbot` itself takes its inputs from flags or the environment: `LPBOT_DATA_DIR` for the stored history (else `data/lake`), `LPBOT_RPC` for the node (else the config's `http://127.0.0.1:8545`), `LPBOT_DB_URL` for Postgres. The make targets set `LPBOT_DATA_DIR` and `LPBOT_DB_URL` from `deploy/.env`; for `target/release/lpbot` run by hand, `export LPBOT_DATA_DIR=/data/lpbot/lake`.

### The data volume: `DATA_DIR`, `/data/lpbot`

`/data` is a 3.4 TB ext4 volume (LVM `ubuntu-vg/data`). Sizes on 25 November 2026:

| Path | Holds |
| --- | --- |
| `reth/` | Reth's archive database (`db`, `static_files`, `rocksdb`, `reth.toml`, ...), 2.9 TB |
| `lighthouse/` | Lighthouse's database, 19 GB |
| `postgres/`, `prometheus/`, `grafana/` | Those services' data; Prometheus 0.5 GB, Grafana 0.3 GB |
| `lake/` | The stored pool history, 3.2 GB; below |
| `reth-node-started` | Written by the first `make up`. While it exists, `make snapshot` refuses to run |
| `reth-snapshot-manifest` | Which Reth snapshot was downloaded |
| `manual-download/` | `take-over.sh` and its log: the last snapshot archive fetched with `curl` instead of Reth (November 2026). Done; kept for the record |

The lake, `/data/lpbot/lake`:

| Path | Holds |
| --- | --- |
| `events/chain=ethereum/pool=<address>/day=<YYYY-MM-DD>/` | Every universe pool's events, one Parquet file per pool and day: 24 pools from 31 January 2025, about 15,800 files, 2.9 GB. `make history` and the forward runs' catch-up add to it |
| `snapshots/chain=ethereum/pool=<address>/day=<day>.json` | Each pool's state before its first stored day (2025-01-31 and 2026-01-31) |
| `gas/chain=ethereum/blocks=<first block>[.partial].parquet` | Each block's base fee and priority fee, in chunks of 100,000 blocks. Only the newest chunk is stored here (`blocks=26400000.partial.parquet`, from the forward runs' catch-up); `make gas-history FROM=<day>` stores earlier blocks |
| `manager/events/chain=ethereum/pool=<position manager>/` | The NFT position manager's logs by day, for the watcher, 177 MB |
| `live/checkpoint-ethereum.json` | The watcher's checkpoint at the finalized block, every ten minutes, 55 MB. Deleting it makes the watcher rebuild ([RUNBOOK.md](RUNBOOK.md), "Rebuild") |

### Postgres

Database and user `lpbot`. Sixteen tables, created empty by the start-up script from `deploy/postgres/schema.sql` (generated from `lpbot_sim::tables` and kept in step by a test):

| Tables | Written by |
| --- | --- |
| The six run tables: `runs`, `campaigns`, `decisions`, `series`, `pool_rank`, `zone_map` | `make load` (`lpbot load-results`), from `runs/` |
| The two pool tables: `pool_series`, `pool_liquidity` | `make pool-stats`, then `make load` |
| The five live tables: `live_pools`, `live_pool_series`, `live_liquidity`, `live_swaps`, `live_positions` | The watcher, continuously |
| The two agent tables: `agent_looks`, `agent_scores` | `lpbot run` on an agent experiment, then `make load RESULTS=runs/<name>`. For the forward panels, cron does it (`crontab -l`): `deploy/agents/refresh.sh --looks` every minute (the looks, from the logs) and `deploy/agents/refresh.sh` hourly (scores and results); its log is `logs/agent-refresh.log` |
| The supervisor table: `agent_status` | `lpbot supervisor status --load`, at the end of every per-minute `deploy/agents/refresh.sh --looks`: every forward agent's state now, all rows replaced each time |

### Grafana

Dashboards are the JSON files in `deploy/grafana/dashboards/`, provisioned into the folder `lpbot` and read again from disk every 30 seconds (`dashboards.yml`). Change a dashboard in its file; on the machine, `git pull` is enough. Grafana opens on the Overview (`GF_USERS_HOME_PAGE`), and every dashboard starts with the same menu: Overview, Agents, and the Pools, Runs and System lists. Before a change reaches the machine, `make dashboards-preview HOST=<user>@<node machine>` shows the working copy's dashboards on the laptop, read-only, at http://127.0.0.1:3002, with this machine's data; `make dashboards-preview-stop` removes it.

| Subject | Dashboard | uid | File |
| --- | --- | --- | --- |
| Overview | `Overview` | `lpbot-overview` | `lpbot-overview.json` |
| Agents | `Agents` | `lpbot-agents` | `lpbot-agents.json` |
| Pools | `Pools: live` | `lpbot-live` | `lpbot-live.json` |
| Pools | `Pools: history` | `lpbot-pool` | `lpbot-pool.json` |
| Runs | `Runs: experiments` | `lpbot-runs` | `lpbot-runs.json` |
| Runs | `Runs: one run` | `lpbot-run` | `lpbot-run.json` |
| Runs | `Runs: compare` | `lpbot-compare` | `lpbot-compare.json` |
| Runs | `Runs: pool and range choice` | `lpbot-zones` | `lpbot-zones.json` |
| System | `System: machine and node` | `lpbot-machine` | `lpbot-machine.json` |
| System | `System: Reth internals` | `lpbot-reth-node` | `reth-node.json` |

Two data sources, from `deploy/grafana/datasources/datasources.yml`: `Prometheus` (uid `prometheus`) and `Results` (uid `results`, the Postgres database). Grafana's user is `admin`; the password is `GRAFANA_PASSWORD` (rule 2 and 3).

## How to check health

What it looked like on 25 November 2026 is the yardstick.

| Check | How | Healthy |
| --- | --- | --- |
| Everything at once | `make status` | Seven containers `running`; Reth `synced, head block N`; `newest block made N s ago` under half a minute (a warning past two minutes); Lighthouse `synced`, about 160 to 170 peers; the disk line |
| The newest block's age, alone | `curl -s -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["latest",false]}' http://127.0.0.1:8545`: its `timestamp` against `date +%s` (18545 from the laptop) | Up to about 12 seconds |
| The watcher keeps up | The `Pools: live` dashboard: "Block age" up to about 12 s, "Blocks behind the head" 0 or 1, "Written" a few seconds. The same in SQL: `select pool_name, extract(epoch from now())::bigint - time as block_age_s, head_block - block as behind, extract(epoch from now())::bigint - updated as written_s from live_pools order by pool_name;` | 24 rows, ages of seconds, 0 behind |
| Reth's log | `$C logs --no-color --tail 50 reth` | `Block added to canonical chain` and `Canonical chain committed` every 12 s. While catching up, batches of `Headers`, `Bodies`, `Execution`, `MerkleExecute`, `Finish` instead |
| Lighthouse's log | `$C logs --no-color --tail 50 lighthouse` | `Synced` with a peer count, `New block received`. `Low peer count` or `Stalled` means the internet is down |
| The watcher's log | `$C logs --no-color --tail 20 watcher` | `block N (0 behind the head, M past the finalized), ...` |
| Every agent waiting, deciding or paused | `make agents` (or the Agents section of `make status`, or the Now row of the `Agents` dashboard). The same in SQL: `select agent, state, detail, extract(epoch from now())::bigint - checked as checked_s from agent_status order by agent;` | No agent in red (late, no waiter, stopped, failing, scheduler down); checked within the last minute or two. [RUNBOOK.md](RUNBOOK.md), "Is every agent working?", says what to do for each state |
| Metrics | `curl -s http://127.0.0.1:9090/api/v1/targets` | All three targets (`reth`, `lighthouse`, `node`) `"health":"up"` |
| Alerts | The Overview's "Alerts firing"; the daily check-in on Telegram. Whether Telegram took the last message: `curl -s -K - http://127.0.0.1:3000/api/alertmanager/grafana/config/api/v1/receivers` with `user = "admin:<password>"` on its standard input, the password read from `deploy/.env` inside the command (rule 2). It never shows the token | Nothing firing; a check-in every day; `owner`'s `telegram` with a recent `lastNotifyAttempt` and no `lastNotifyAttemptError`. Rules in `deploy/grafana/alerting/rules.yml`, [RUNBOOK.md](RUNBOOK.md), "When something is red" and "Alerts on Telegram" |
| Heat | `sensors` | The `coretemp` package at about 50 °C idle, 90 to 100 °C under load. It prints `ALARM (CRIT)` beside every core even at 50 °C: read the temperatures, not the flag. NVMe `Composite` about 40 °C |
| Turbo | `cat /sys/devices/system/cpu/intel_pstate/no_turbo` | `1`. A restart sets it back to `0`; the owner turns it off again with `echo 1 \| sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo` |
| Disk | `df -h /data` | 466 GB free of 3.4 TB (87% used) on 25 November 2026, shrinking by a few GB a day. `make status` warns below 150 GB; the node stops when it is full |
| Restarts | `uptime`, `journalctl --list-boots --no-pager \| tail -n 5` | Up since 17 November 2026, 17:51 UTC. The journal is readable without sudo; [RUNBOOK.md](RUNBOOK.md), "After the machine was off", reads why a boot ended |

When the node falls behind, suspect the internet first (Lighthouse's peers; [RUNBOOK.md](RUNBOOK.md), "After the internet was down"), then the heat and the drive (`CLAUDE.md`, "Known hardware limits"), and only then the software.

## Where to read more

| For | Read |
| --- | --- |
| Setup, ports, and what goes wrong with the tunnel | [REMOTE.md](REMOTE.md) |
| Each job and how to resume it, the watcher, forward runs and panels, after a restart or an outage | [RUNBOOK.md](RUNBOOK.md) |
| The services and the gates | [README.md](README.md) |
| The machine's history and hardware limits | `CLAUDE.md`, "The node machine" |
| What the bot does and why | `docs/DESIGN.md` |
| The real SSH login | `data/NODE.local.md` on the laptop, not committed |
