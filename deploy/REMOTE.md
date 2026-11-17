# Setting up the node machine

How to take a fresh machine to a running stack, and how to reach it from a laptop. `deploy/README.md` covers day-to-day use once it runs, and [RUNBOOK.md](RUNBOOK.md) is what to type on the machine: long jobs in `tmux`, and getting going again after it was off.

## At a glance

Two machines, each with its own clone of this repository and its own `deploy/.env`:

| | Node machine | Laptop |
| --- | --- | --- |
| Runs | Reth, Lighthouse, Postgres, Prometheus, Grafana, and later the bot | Nothing permanent. Optionally its own Postgres and Grafana for demo runs (`make viewer`) |
| Holds | The chain database, the stored pool history, the results | A clone to read, and to run `make tunnel` and `make demo` from |
| Started with | `make up` (or `make viewer` for everything but the node) | `make tunnel HOST=<user>@<node machine>` |
| Grafana | Listens on the machine only, port 3000 | The node machine's Grafana appears on http://127.0.0.1:3001 while the tunnel runs |
| Passwords | Its own, in its `deploy/.env` (`make grafana-password`) | Its own, different ones |

Everything is done over SSH. No service on the node machine is open to the network except the node's peer-to-peer ports.

`host-setup.sh` and `make snapshot` have run on a real machine once (Ubuntu Server 26.04, November 2026). `make up` with the node has not. Tell the next Claude session what did not work.

## The machine

- **Disk:** 4 TB NVMe holds an archive node, without much to spare. The archive snapshot of mid-November 2026 was 1.72 TB to download and 2.88 TB unpacked; the consensus client, pool history and Postgres add a few hundred GB; the node grows by a few GB a day. On a 4 TB drive that leaves some months before old history has to be pruned or the drive replaced. `make status` shows the free space.
- **Which drive:** a mainstream TLC drive with its own memory cache. Budget drives without one (the Kingston NV2 is a common example) are on node operators' lists of drives that may not keep up. Measure before the big download:

  ```sh
  sudo apt-get install -y fio
  cd /data && fio --randrepeat=1 --ioengine=libaio --direct=1 --gtod_reduce=1 \
    --name=test --filename=test --bs=4k --iodepth=64 --size=150G \
    --readwrite=randrw --rwmixread=75; rm -f /data/test
  ```

  About 15,000 read and 5,000 write IOPS is comfortable; 10,000 and 2,500 is the floor.
- **Heat:** look at `sensors` (package `lm-sensors`) on the idle machine. A small box with a quiet fan profile can idle above 60 °C and push the NVMe past its warning temperature under load; the drive then slows itself down. Set the BIOS fan profile to its coolest setting and check that the drive's thermal pad makes contact.
- **Memory:** 32 GB is comfortable. 16 GB is the floor.
- **Network:** a wired connection. The first download is about 2 TB.
- **Power:** set the BIOS to power on after a power loss, so the machine comes back by itself. A short press of the power button switches Ubuntu off in an orderly way, which on a box within reach can happen by accident and ends every job that was running. To make a short press do nothing:

  ```sh
  sudo mkdir -p /etc/systemd/logind.conf.d
  printf '[Login]\nHandlePowerKey=ignore\n' | sudo tee /etc/systemd/logind.conf.d/power-key.conf
  sudo systemctl restart systemd-logind      # or leave it to the next boot
  ```

  `sudo poweroff` still switches the machine off, and holding the button still cuts the power.

## Standalone, or under a hypervisor

The scripts assume a machine that does this one job: Ubuntu Server LTS on the metal, Docker on top. That gives the node the NVMe drive directly, keeps the bot running when other servers are rebooted, and keeps the key that will later trade on a machine only you administer.

Under Proxmox it works too, as a VM with the NVMe drive passed through to it. Do not put the node's database on a thin-provisioned virtual disk or a ZFS volume: the write load of an archive node is what those handle worst.

## Steps

1. **Install Ubuntu Server LTS.** During install, enable the OpenSSH server and add your public key. With the installer's default layout the system gets a 100 GB volume and the rest of the drive stays unallocated (`sudo vgs` shows it as free). Give that space to a data volume at `/data`, leaving some for the system to grow:

   ```sh
   sudo lvcreate -n data -L 3.4T ubuntu-vg      # for a 4 TB drive
   sudo mkfs.ext4 -m 0 -L lpbot-data /dev/ubuntu-vg/data
   sudo mkdir -p /data
   echo '/dev/ubuntu-vg/data /data ext4 defaults,noatime 0 2' | sudo tee -a /etc/fstab
   sudo systemctl daemon-reload && sudo mount /data && sudo chown "$USER": /data
   ```

2. **Get the repository onto it.** The repository is private, so the machine needs its own read access. A deploy key is the narrowest way:

   ```sh
   ssh-keygen -t ed25519 -f ~/.ssh/lpbot_deploy -N ""
   cat ~/.ssh/lpbot_deploy.pub     # add under the repository's Settings, Deploy keys, read-only
   GIT_SSH_COMMAND="ssh -i ~/.ssh/lpbot_deploy" git clone git@github.com:hdser/lpbot.git
   cd lpbot && git config core.sshCommand "ssh -i ~/.ssh/lpbot_deploy"
   ```

3. **Prepare the machine.** One command installs Docker, Rust and the build tools, and closes the firewall to everything but SSH and the node's peer-to-peer ports:

   ```sh
   ./scripts/host-setup.sh
   ```

   `LPBOT_DRY_RUN=1 ./scripts/host-setup.sh` shows what it would do first. Log out and back in afterwards, before starting `tmux`: a shell or a tmux session from before that login cannot use Docker yet.

4. **Say where the data lives.**

   ```sh
   make env DATA_DIR=/data/lpbot      # creates deploy/.env with generated passwords
   ```

5. **Download the node's database.** This replaces days of syncing with a download of a few hours:

   ```sh
   make snapshot
   ```

   Run it inside `tmux` (`tmux new -s node`; Ctrl-b then d detaches; `tmux attach -t node` comes back), so a dropped SSH session does not stop it. Reth prints the size and an estimate of the time, which settles after a few minutes.

   If it is interrupted, or the machine restarts, the download does not come back by itself: start a new `tmux` session and run `LPBOT_SNAPSHOT_RESUME=1 make snapshot`, which carries on with the same snapshot. Reth publishes a new one every day; the first run writes down which one it took in `DATA_DIR/reth-snapshot-manifest`, so a resume does not switch to a newer one and download parts of it again. [RUNBOOK.md](RUNBOOK.md) has the details.

   Postgres, Prometheus and Grafana do not have to wait for it: `make viewer` in a second session starts them now.

6. **Start everything.**

   ```sh
   make up
   make status      # repeat until both clients say they are synced
   ```

7. **Run the checks.** Once both clients are synced, inside `tmux`:

   ```sh
   make gates
   ```

   It builds the bot, checks it against real chain data step by step, runs the example backtest, loads the results into Grafana's database and calibrates the adaptive decision. It stops at the first step that fails and ends with a summary; the whole output is kept under `logs/`. `deploy/README.md`, "First check after the sync", describes each step and how to carry on from one.

## Reaching it from your laptop

RPC, Postgres and Grafana listen on the node machine itself and are not reachable from the network. That is deliberate: the RPC has no authentication. From the laptop, in a clone of this repository:

```sh
make tunnel HOST=<user>@<node machine>
```

Leave it running. While it runs:

- The node machine's Grafana is on http://127.0.0.1:3001 on the laptop. Port 3001, not 3000, so it cannot be confused with a Grafana running on the laptop itself.
- The node machine's RPC is on http://127.0.0.1:8545.

Log in to Grafana as `admin` with the node machine's password. Print it there, not on the laptop:

```sh
make grafana-password
```

Without the repository on the laptop, the tunnel is one command: `ssh -N -L 3001:127.0.0.1:3000 <user>@<node machine>`.

The same works from outside the house over whatever already gets you an SSH session to the machine (a VPN such as WireGuard or Tailscale, or a jump host). Do not forward the SSH port on the router to this machine without key-only logins.

### When it does not work

| What you see | Why | What to do |
| --- | --- | --- |
| `ssh: ... No route to host` on a Mac, while SSH works from another terminal app | macOS lets each app onto the local network separately. That app is not allowed; it can still reach the router, which makes it confusing | Run the tunnel from the app where SSH works, or allow the other one under System Settings, Privacy & Security, Local Network, and restart it |
| Grafana refuses the password | The laptop's password was used, the browser filled in a saved one, or several wrong tries locked the login for five minutes | On the node machine: `make grafana-reset-password`, then `make grafana-password`, and type it by hand |
| `permission denied ... docker.sock` | The shell is from before `host-setup.sh` added you to the Docker group. A tmux window from before keeps the old permissions too | `newgrp docker` in that shell, or log in again |
| Dashboards say a table does not exist | The database was started by a version without the schema step | `git pull`, then `make viewer` (or `make up`) again; it creates the empty tables |
| The `Reth node` dashboard says `lookup prometheus ... server misbehaving` | Prometheus is not running: the viewer was started by a version that left it out | `git pull`, then `make viewer` again |
| The `Reth node` dashboard is empty | The node is not running yet | Nothing until the snapshot is in place; then `make up` |
| The lpbot dashboards are empty | No runs have been loaded | `make demo` then `make load` for demo runs; real runs come from `make backtest` |
| The address on port 3001 does not load | Grafana is not running on the node machine, or the tunnel was closed | On the node machine: `make viewer` (or `make up`); on the laptop: start the tunnel again |

## Ports

| Port | What | Open to |
| --- | --- | --- |
| 22/tcp | SSH | your network |
| 30303 tcp+udp | Reth peer-to-peer | the internet |
| 9000 tcp+udp | Lighthouse peer-to-peer | the internet |
| 8545, 8546 | RPC | the machine only |
| 5432 | Postgres | the machine only |
| 3000, 9090 | Grafana, Prometheus | the machine only |

The node finds peers without any router setup. Forwarding 30303 and 9000 (both TCP and UDP) on the router to the machine gives it more and better peers; it is optional.

## After a reboot or a power cut

Docker starts on boot and brings every container back. Check with `make status`. Reth shuts down cleanly given up to five minutes; a power cut can cost it a short re-sync, not the database.

Jobs that were running in `tmux`, such as the snapshot download or `make gates`, do not come back: `tmux` sessions end with the machine. [RUNBOOK.md](RUNBOOK.md), "After the machine was off", says how to carry on with each and how to find out why the machine went off.

## Watching the node

`make status` shows the containers, how far both clients have synced, and the free space on the data drive. In Grafana, the `Reth node` dashboard in the `lpbot` folder is Reth's own overview dashboard, fed by Prometheus.

## Later: pruning

The archive keeps the state at every past block, which the bot uses only to read a pool's state at the start of a backfill. Once it is clear how much history the backtests need, Reth can be told to keep only that window. Check the Reth documentation of the version running for the prune settings and for how much space pruning an existing archive gives back; that has not been looked into yet.

## Not set up yet

- An alert when the node falls behind or the disk fills. Prometheus already collects the numbers, and `make status` warns below 150 GB free.
- Backups. Everything except `deploy/.env`, the `runs/` folder and, later, the bot's key can be rebuilt from the chain. Keep a copy of `deploy/.env` somewhere safe.
