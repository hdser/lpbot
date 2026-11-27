# Dashboards from other projects

`reth-node.json` is Reth's own overview dashboard, from `etc/grafana/dashboards/overview.json` in https://github.com/paradigmxyz/reth, which is licensed under Apache-2.0 or MIT at your option. It is changed so that Grafana can load it from a file and find it among ours: the two import-time placeholders are filled in (`instance` as the instance label, Grafana's built-in expression source); its title (`System: Reth internals`), id, tags (`lpbot`, `system`, `reth`) and description are ours; and the menu every lpbot dashboard starts with comes before its own link to its source.

It has not been opened against a running node yet. If panels are empty, compare with the copy in the Reth version that is running.
