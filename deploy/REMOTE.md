# Setting up the node machine

How to take a fresh machine to a running stack. `deploy/README.md` covers day-to-day use once it runs.

Nothing here has been run on a real machine yet. The scripts were checked in dry-run mode and against a stand-in for Docker. Read their output the first time, and tell the next Claude session what did not work.

## The machine

- **Disk:** 4 TB NVMe is enough for an archive node. Reth's archive database is about 2.3 TB on its current storage layout; the consensus client, pool history and Postgres add a few hundred GB; the node grows by one to two GB a day. Use a TLC drive: a QLC drive falls behind during sync.
- **Memory:** 32 GB is comfortable. 16 GB is the floor.
- **Network:** a wired connection. The first download is about 2 TB.
- **Power:** set the BIOS to power on after a power loss, so the machine comes back by itself.

## Standalone, or under a hypervisor

The scripts assume a machine that does this one job: Ubuntu Server LTS on the metal, Docker on top. That gives the node the NVMe drive directly, keeps the bot running when other servers are rebooted, and keeps the key that will later trade on a machine only you administer.

Under Proxmox it works too, as a VM with the NVMe drive passed through to it. Do not put the node's database on a thin-provisioned virtual disk or a ZFS volume: the write load of an archive node is what those handle worst.

## Steps

1. **Install Ubuntu Server LTS.** Put the data on the NVMe drive with ext4. If the system and the data share the drive, nothing more is needed. If the NVMe is a separate data drive, mount it (for example at `/data`) with the `noatime` option. During install, enable the OpenSSH server and add your public key.

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

   `LPBOT_DRY_RUN=1 ./scripts/host-setup.sh` shows what it would do first. Log out and back in afterwards.

4. **Say where the data lives.**

   ```sh
   LPBOT_DRY_RUN=1 ./scripts/up.sh    # creates deploy/.env with generated passwords
   nano deploy/.env                   # set DATA_DIR, for example /data/lpbot
   ```

5. **Download the node's database.** This replaces days of syncing with a download of a few hours:

   ```sh
   make snapshot
   ```

   If it is interrupted, `LPBOT_SNAPSHOT_RESUME=1 make snapshot` carries on. Run it inside `tmux` or `screen`, so a dropped SSH session does not stop it.

6. **Start everything.**

   ```sh
   make up
   make status      # repeat until both clients say they are synced
   ```

7. **Build the bot and run the checks** in `deploy/README.md`, "First check after the sync": `lpbot pools`, `lpbot verify-pool`, `make history`, `lpbot verify-fees`, `make backtest`, `make calibrate`, `make load`.

## Reaching it from your laptop

RPC, Postgres and Grafana listen on the machine itself and are not reachable from the network. That is deliberate: the RPC has no authentication. From the laptop, in a clone of this repository:

```sh
make tunnel HOST=<user>@<machine>
```

While that runs, Grafana is on http://127.0.0.1:3000 and the RPC on http://127.0.0.1:8545 on the laptop. The same works from outside the house over whatever already gets you an SSH session to the machine (a VPN such as WireGuard or Tailscale, or a jump host). Do not forward the SSH port on the router to this machine without key-only logins.

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

## Not set up yet

- An alert when the node falls behind or the disk fills. Prometheus already collects the numbers.
- Backups. Everything except `deploy/.env`, the `runs/` folder and, later, the bot's key can be rebuilt from the chain. Keep a copy of `deploy/.env` somewhere safe.
