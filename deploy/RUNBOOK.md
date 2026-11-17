# Runbook for the node machine

What to type on the node machine for the things that come up: starting a long job so that a dropped connection does not stop it, coming back to it, and getting going again after the machine was off. Setting the machine up is in [REMOTE.md](REMOTE.md); what the services are and what the checks do is in [README.md](README.md).

## Two kinds of things run here

| | Services | Jobs |
| --- | --- | --- |
| What | The containers: Reth, Lighthouse, Postgres, Prometheus, Grafana | Commands that run until they are done: `make snapshot`, `make gates`, `make history`, `make backtest`, `make calibrate` |
| Started with | `make up`, or `make viewer` for all but the node | Typing them, inside `tmux` |
| When your SSH connection drops | Keep running | Keep running if they were started inside `tmux`; stop otherwise |
| When the machine restarts | Come back by themselves | Are gone. Start them again: each carries on from what it already stored |

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
| `make snapshot` | Once, before the first `make up` | `LPBOT_SNAPSHOT_RESUME=1 make snapshot`. Reth keeps the files it has already verified. With no snapshot named it takes the newest one published, so after a pause of days part of it may be downloaded again |
| `make gates` | Once the node is synced | `make gates` again, or `make gates STEP=<step>` to start at the step that was running. What is stored is not fetched again |
| `make history`, `make backtest`, `make calibrate`, `make load` | The steps of `make gates`, one by one | The same command again |

## After the machine was off

1. Log in and look:

   ```sh
   cd ~/lpbot
   make status       # which containers are up, and how far the node is
   tmux ls           # which jobs are running: none after a restart
   ```

2. **Services need nothing.** Docker starts at boot and brings back every container that was running. If `make status` lists none, they had been stopped with `make down`: start them with `make up`, or with `make viewer` if the node has not been started yet.

3. **Jobs have to be started again**, in a new `tmux` session, with the command from the table above. For a snapshot download that was under way:

   ```sh
   tmux new -s node
   cd ~/lpbot
   LPBOT_SNAPSHOT_RESUME=1 make snapshot
   ```

   Not sure whether the download had finished? Run it: Reth passes over what it already has. Do not run it once the node has been started with `make up`. The database is complete by then, and the command would stop the node to download into it.

4. **Find out why it went off**, if you do not know. The log of the previous start says how it ended, and what asked for it:

   ```sh
   sudo journalctl -b -1 -n 40 --no-pager
   sudo journalctl -b -1 --no-pager | grep -iE "power key|powering (off|down)|critical temp|hardware protection|COMMAND=.*(poweroff|shutdown|halt|reboot)" | tail -n 20
   ```

   The times in the log are the machine's, which `date` shows. Ubuntu Server runs on UTC unless it was told otherwise.

   | The log shows | It was |
   | --- | --- |
   | It ends with `Reached target poweroff.target` and `System Power Off` | An orderly shutdown: something asked for it, and the disks were closed properly. The second command says what |
   | `Power key pressed short` | The power button. "Power" under "The machine" in [REMOTE.md](REMOTE.md) says how to make a short press do nothing |
   | `critical temperature reached` or `HARDWARE PROTECTION shutdown` | The kernel switching the machine off to protect it from heat. Deal with the heat before starting the job again: see "Heat" under "The machine" in [REMOTE.md](REMOTE.md) |
   | A `sudo` line with `COMMAND=` and `poweroff` or `shutdown` | Someone typed it |
   | It stops in the middle of ordinary lines, with none of the above | A power cut, a held power button, or the firmware cutting the power. Nothing was closed properly; Reth's database is built to survive that and may re-sync a little |

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
| Sync progress and free disk space | `make status` |
| The node's own log | `make logs`; Ctrl-c leaves it and the node keeps running |
| Temperatures | `sensors` (package `lm-sensors`); `sudo nvme smart-log /dev/nvme0` (package `nvme-cli`) |
| Dashboards | From the laptop: `make tunnel HOST=<user>@<node machine>`, then http://127.0.0.1:3001 |
