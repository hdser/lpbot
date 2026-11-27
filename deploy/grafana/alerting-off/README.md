# Alerting off

Grafana reads its alert rules, contact point and policies from the folder that
`GRAFANA_ALERTING` names in `deploy/.env`. This empty folder is the default, so a
machine without that setting (the laptop's viewer) loads no alerts and needs no
Telegram keys. On the node machine `GRAFANA_ALERTING=./grafana/alerting` turns
them on; `deploy/RUNBOOK.md`, "Alerts on Telegram", has the steps.
