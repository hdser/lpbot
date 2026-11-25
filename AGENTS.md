# Codex agents in lpbot

Choose the role before opening project files. A development or research session
must not later become a clean LP decider by reusing its context.

- **Development, code review, research and operations:** read `CLAUDE.md` and
  `docs/DESIGN.md`. Their project rules apply to Codex too. The design is the
  source of truth; report conflicts with the implementation before changing
  settled behavior.
- **LP decider:** start in a fresh context with only `lpbot agent brief` for the
  assigned experiment and agent. Read only the permitted `lpbot agent` output.
  Do not open the experiment, configuration, research, raw history, decisions,
  logs, scores or result tables, and do not look up prices or news.
- **Strategy reviewer:** distinguish research review from a blind review of a
  decision. Follow `docs/AGENT_WORKFLOW.md`; the proposed `agent review` commands
  are not implemented.

## Delegation

Use Codex subagents for independent development, code review, LP strategy
research and strategy review when they help the requested work. Project roles
live in `.codex/agents/`. Inherit the session's model and reasoning effort
unless the owner selects others. These roles do not schedule recurring work.

Give each developer a bounded task and ownership of files. Reviewers inspect
without editing. Integrate and validate changes in the coordinating session;
do not have several agents edit the same files or run heavy jobs together.
Start LP deciders without inherited development/research context and forward
only their generated brief and permitted tool output. Use a separate identity
and decisions path for each run. Never change an experiment after its first
decision.

## Code Review Rules

Prioritize reproducible correctness problems over style. For each finding,
give the trigger, impact, file/line and missing check. Separate confirmed
defects from assumptions or conflicting design text. Check integer pool math,
token order, fees and gas, reorg handling, atomic persistence, look-ahead and
the monotonic `as_of` boundary for agent information. Strategy claims need
out-of-sample evidence, costs and fixed-rule baselines.

## Validation

Work on a branch and follow the pull-request workflow in `CLAUDE.md`. Before a
commit, run `make check` (formatting, Clippy, workspace tests and experiment
planning). Add meaningful regression tests for behavior changes. A documentation
or role-only edit needs its links and configuration checked, without rebuilding
the node or running data-heavy experiments.

## Server work

For authorized operations, read `deploy/NODE.md` and `deploy/RUNBOOK.md`. Resolve
the SSH login from the ignored laptop file `data/NODE.local.md`; never put real
host/user/machine names into tracked files, commits or pull requests.

- Never print secrets, including `deploy/.env`, JWTs or Grafana passwords.
- Manage containers only through the compose file in `~/lpbot`. Get the owner's
  approval before stopping or recreating Reth, Lighthouse or Postgres, or adding
  database tables.
- Use `CARGO_BUILD_JOBS=2` on the server. Check existing load before heavy work.
  Long jobs go in their own named `tmux` sessions. Do not attach to, type into or
  stop another session, and never kill the tmux server.
- Leave runs in progress and their `data/agent/` files alone unless the owner
  specifically requests work on that run. Use `lpbot agent` to write decisions.
- Check available disk before substantial writes. Do not change the archive
  retention policy or invoke sudo on the owner's behalf.
