#!/usr/bin/env bash
# A throwaway Grafana for looking at the dashboards of this working copy
# before they reach the node machine. It runs on this computer only, at
# http://127.0.0.1:3002, read-only and without a login, and is removed by
# `stop`. Nothing is kept: its own database lives inside the container.
#
#   make dashboards-preview                         # data from the viewer here
#   make dashboards-preview HOST=<user>@<node>      # data from the node machine
#   make dashboards-preview-stop
#
# With HOST it opens its own SSH tunnel to the node's Postgres and Prometheus
# (local ports 15432 and 19090) unless one is already open, reads the node's
# database password inside the command that uses it, never printing it, and
# closes only the tunnel it opened when stopped. Without HOST it reads the
# viewer this computer runs (`make viewer`): Postgres on 5432 and Prometheus
# on 9090.
#
# The dashboards are mounted from deploy/grafana/dashboards, so a change to a
# file shows within 30 seconds; reload the page.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME=lpbot-dashboards-preview
PORT=3002
STATE="$ROOT/logs/.dashboards-preview"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

image() {
  sed -n 's/^ *image: *\(grafana\/grafana:.*\)$/\1/p' "$ROOT/deploy/docker-compose.yml" | head -n 1
}

home_page() {
  sed -n 's/^ *GF_USERS_HOME_PAGE: *\(.*\)$/\1/p' "$ROOT/deploy/docker-compose.yml" | head -n 1
}

stop() {
  docker rm -f "$NAME" >/dev/null 2>&1 && echo "Stopped the preview." || echo "No preview was running."
  if [[ -f "$STATE/tunnel" ]]; then
    local tunnel
    tunnel=$(cat "$STATE/tunnel")
    pkill -f -x "$tunnel" 2>/dev/null && echo "Closed the tunnel it opened." || true
  fi
  rm -rf "$STATE"
}

start() {
  command -v docker >/dev/null 2>&1 || die "Docker is needed for the preview."
  local host="${HOST:-}" pg_port prom_port password
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  mkdir -p "$STATE/datasources"
  # A tunnel this script opened before stays recorded, so `stop` still
  # closes it after a second `start`.

  if [[ -n "$host" ]]; then
    pg_port=15432
    prom_port=19090
    local open
    open=$(lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | grep -cE ":($pg_port|$prom_port) " || true)
    if [[ "$open" -eq 0 ]]; then
      local tunnel="ssh -f -N -o ExitOnForwardFailure=yes -L $pg_port:127.0.0.1:5432 -L $prom_port:127.0.0.1:9090 $host"
      $tunnel || die "could not open the tunnel to $host"
      # pkill matches the running command line, which drops `-f`'s effect but
      # not its text: keep it exactly as typed.
      echo "$tunnel" > "$STATE/tunnel"
    elif [[ "$open" -lt 2 ]]; then
      die "only one of the ports $pg_port and $prom_port is taken; close that tunnel or open both"
    else
      echo "Using the tunnel already open on $pg_port and $prom_port."
    fi
    password=$(ssh -o BatchMode=yes "$host" "sed -n 's/^POSTGRES_PASSWORD=//p' ~/lpbot/deploy/.env | head -n 1")
    [[ -n "$password" ]] || die "could not read the node's database password"
  else
    pg_port=5432
    prom_port=9090
    password=$(sed -n 's/^POSTGRES_PASSWORD=//p' "$ROOT/deploy/.env" 2>/dev/null | head -n 1)
    [[ -n "$password" ]] || die "no POSTGRES_PASSWORD in deploy/.env: start the viewer with make viewer first, or give HOST=<user>@<node>"
  fi

  # The same data sources and uids as the node's, reached through the host.
  cat > "$STATE/datasources/datasources.yml" <<EOF
apiVersion: 1
datasources:
  - name: Prometheus
    uid: prometheus
    type: prometheus
    access: proxy
    url: http://host.docker.internal:$prom_port
    isDefault: true
  - name: Results
    uid: results
    type: postgres
    access: proxy
    url: host.docker.internal:$pg_port
    user: lpbot
    jsonData:
      database: lpbot
      sslmode: disable
      postgresVersion: 1700
    secureJsonData:
      password: \$POSTGRES_PASSWORD
EOF

  local home
  home=$(home_page)
  POSTGRES_PASSWORD="$password" docker run -d --rm --name "$NAME" \
    -p "127.0.0.1:$PORT:3000" \
    --add-host host.docker.internal:host-gateway \
    -e POSTGRES_PASSWORD \
    -e GF_AUTH_ANONYMOUS_ENABLED=true \
    -e GF_AUTH_ANONYMOUS_ORG_ROLE=Viewer \
    -e GF_AUTH_DISABLE_LOGIN_FORM=true \
    -e GF_SECURITY_ADMIN_PASSWORD="$(openssl rand -hex 16)" \
    -e GF_ANALYTICS_REPORTING_ENABLED=false \
    -e GF_ANALYTICS_CHECK_FOR_UPDATES=false \
    -e GF_NEWS_NEWS_FEED_ENABLED=false \
    ${home:+-e GF_USERS_HOME_PAGE="$home"} \
    -v "$ROOT/deploy/grafana/dashboards:/etc/grafana/provisioning/dashboards:ro" \
    -v "$STATE/datasources:/etc/grafana/provisioning/datasources:ro" \
    "$(image)" >/dev/null
  echo "Preview starting at http://127.0.0.1:$PORT (read-only, no login). Stop it with: make dashboards-preview-stop"
}

case "${1:-start}" in
  start) start ;;
  stop) stop ;;
  *) die "usage: $0 [start|stop]   (HOST=<user>@<node> for the node's data)" ;;
esac
