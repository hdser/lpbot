#!/usr/bin/env bash
# Prepares a fresh Ubuntu Server machine to run the stack: Docker, the tools
# to build lpbot, and a firewall that lets in SSH and the node's peer-to-peer
# ports and nothing else. Run it once, as your normal user (it uses sudo).
# Safe to run again.
#
#   ./scripts/host-setup.sh
#
# LPBOT_DRY_RUN=1      print what it would do and change nothing
# LPBOT_SKIP_FIREWALL=1  leave the firewall alone

set -euo pipefail

say() { printf '\n==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

DRY="${LPBOT_DRY_RUN:-0}"
run() {
  if [[ "$DRY" == 1 ]]; then
    printf '   would run: %s\n' "$*"
  else
    "$@"
  fi
}

[[ "$(uname -s)" == Linux ]] || die "this script is for the Linux machine that runs the node, not for a laptop."
ME="$(id -un)"
[[ "$(id -u)" != 0 ]] || die "run it as your normal user, not as root; it calls sudo where needed."
command -v apt-get >/dev/null 2>&1 || die "this script supports Ubuntu and Debian (apt). Install Docker and Rust by hand on other systems."
# shellcheck disable=SC1091
. /etc/os-release
case "${ID:-}" in
  ubuntu | debian) ;;
  *) die "this script supports Ubuntu and Debian; found '${ID:-unknown}'." ;;
esac

say "Installing build tools."
run sudo apt-get update
run sudo apt-get install -y ca-certificates curl git openssl build-essential pkg-config ufw

if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  say "Docker and the Compose plugin are already installed."
else
  say "Installing Docker Engine from Docker's own package repository."
  run sudo install -m 0755 -d /etc/apt/keyrings
  run sudo curl -fsSL "https://download.docker.com/linux/${ID}/gpg" -o /etc/apt/keyrings/docker.asc
  run sudo chmod a+r /etc/apt/keyrings/docker.asc
  repo="deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/${ID} ${VERSION_CODENAME} stable"
  if [[ "$DRY" == 1 ]]; then
    printf '   would write /etc/apt/sources.list.d/docker.list: %s\n' "$repo"
  else
    printf '%s\n' "$repo" | sudo tee /etc/apt/sources.list.d/docker.list >/dev/null
  fi
  run sudo apt-get update
  run sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
fi

# Containers come back after a reboot only if Docker itself starts on boot.
run sudo systemctl enable --now docker

if id -nG "$ME" | tr ' ' '\n' | grep -qx docker; then
  say "$ME can already use Docker."
else
  say "Letting $ME use Docker without sudo."
  run sudo usermod -aG docker "$ME"
  relogin=1
fi

if command -v cargo >/dev/null 2>&1 || [[ -x "$HOME/.cargo/bin/cargo" ]]; then
  say "Rust is already installed."
else
  say "Installing Rust with rustup."
  if [[ "$DRY" == 1 ]]; then
    printf '   would run: curl https://sh.rustup.rs | sh -s -- -y --profile minimal\n'
  else
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
  fi
  relogin=1
fi

if [[ "${LPBOT_SKIP_FIREWALL:-0}" == 1 ]]; then
  say "Leaving the firewall alone."
else
  say "Firewall: SSH and the node's peer-to-peer ports in, everything else closed."
  # SSH first, so enabling the firewall cannot cut off the session doing it.
  run sudo ufw allow OpenSSH
  run sudo ufw allow 30303/tcp comment 'reth p2p'
  run sudo ufw allow 30303/udp comment 'reth discovery'
  run sudo ufw allow 9000/tcp comment 'lighthouse p2p'
  run sudo ufw allow 9000/udp comment 'lighthouse discovery'
  run sudo ufw --force enable
fi

cat <<EOF

The machine is ready.
EOF
if [[ "${relogin:-0}" == 1 ]]; then
  cat <<EOF
Log out and back in once, so your shell picks up Docker and Rust.
EOF
fi
cat <<EOF

Next, from the repository folder:

  LPBOT_DRY_RUN=1 ./scripts/up.sh   # creates deploy/.env
  nano deploy/.env                  # point DATA_DIR at the NVMe drive
  make snapshot                     # download the node's database (hours)
  make up                           # start everything
  make status                       # watch it catch up

RPC, database and dashboards listen on this machine only. From your laptop,
\`make tunnel HOST=<user>@<this machine>\` brings Grafana and the RPC to it.
EOF
