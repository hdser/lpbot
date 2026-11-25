# Working with Codex agents

Codex handles development, reviews and LP strategy work through separate roles.
The coordinator owns the task, combines results and checks the final change.
`AGENTS.md` routes each role to its allowed sources; `CLAUDE.md` and
`docs/DESIGN.md` retain the shared project rules.

## Roles

| Role | Work | Result |
| --- | --- | --- |
| `lp_developer` | Implement a bounded Rust/configuration task in assigned files | Change, meaningful checks and limitations |
| `lp_code_reviewer` | Independently inspect a change or subsystem without editing | Reproducible findings with impact and file/line references |
| `lp_strategy_researcher` | Review completed research and design YAML experiments | Hypothesis, frozen settings, baselines, costs and acceptance criteria |
| `lp_decider` | Decide one assigned simulation through `lpbot agent` | Action, range, next look, reasoning and an expected price band |
| `lp_strategy_reviewer` | Challenge experiment design or review completed decisions | Independent assessment, evidence, uncertainty and proposed next test |

The project definitions are in `.codex/agents/`. They inherit the parent's model,
reasoning effort and permissions. A review role's no-edit rule is an instruction,
not a separate filesystem sandbox. The coordinator must supply the intended
scope and sources with each assignment.

For example, ask Codex to use `lp_developer` for an agreed change and
`lp_code_reviewer` to inspect it independently, or use
`lp_strategy_researcher` and `lp_strategy_reviewer` to propose and challenge the
next experiment before implementing it. These definitions are reusable roles,
not continuously running processes.

## Development and code review

Developers read the design, make a focused change on a branch and add tests for
changed behavior. Reviewers check the actual code and its callers, resolve
contradictory design statements and report only substantiated defects. The
coordinator verifies findings and runs the relevant checks before integration.
Use `make check` before a commit; use the node's runbook for checks that need
real chain data. Do not make the production node a build or test dependency for
unit tests.

## LP strategy research

Research agents may read `docs/RESEARCH.md`, completed experiment definitions
and results explicitly included in their assignment. They must not read or
change an active run merely because it is accessible. Define the pool/window
selection rule, capital, required loss limit, gas, latency, decision cadence and
acceptance criteria before scoring. Keep experiment variants in YAML and use
the shared engine.

Compare with cash, holding the value token, holding the other token where
relevant, and the fixed patience rules. Report value-token and dollar results,
drawdown, campaign/decision counts, fees and costs. Fit on earlier windows and
score on later ones; disclose repeated use of already studied history. A small
sample or an in-sample improvement does not establish a strategy edge.

## Forward simulations

The current forward agent workflow uses `mode: backtest` with an open-ended
window and virtual positions replayed against the chain head. The separate
`paper` and `live` execution modes are not implemented. Starting a forward
simulation does not fund a wallet or submit transactions.

1. The coordinator creates a new experiment with fresh decisions paths, using
   the actual start day and the owner's requested capital and loss limit (or
   clearly disclosed simulation inputs copied from the example).
2. Resolve the experiment with `lpbot plan`, check the server's health and obtain
   `lpbot agent brief <experiment> --agent <name>` for each decider.
3. Start each decider in a fresh context, without research, earlier review
   findings or the coordinator's history. With Codex collaboration tools use
   `fork_turns: "none"`. Supply only its brief and permitted command output.
4. One session writes each agent's run. The coordinator can transport the exact
   command output over SSH and record the decider's returned action unchanged.
   Serialize pool/gas catch-ups when agents share the lake. On the node export
   `LPBOT_DATA_DIR=/data/lpbot/lake`; use named `tmux` jobs for long commands.
5. Record the actual model identifier when available. If the client does not
   expose it, label the inherited Codex session explicitly; do not invent an id
   or present that run as a comparison of named models.
6. Use `agent next` to resume and respect its due time. A run left without a
   decider holds its state under the engine's limits; it does not keep making
   decisions by itself. State how wake-ups are arranged and what stops them.

`agent next` writes catch-up data and pending state. `inbox` and `answers` can
record what was read. They are not read-only inspection commands. Preserve
existing experiments and runs once decisions exist. A new Codex experiment
must not share decision paths with the running Claude panel.

The initial `agent-codex-panel-forward` comparison uses filtered peer records
only: `solo` decides alone and `reader` reads `solo`. Do not use `ask`, `inbox`,
`answer`, `answers` or board advice in this comparison, even though the generic
brief offers them. Keep this restriction in every decider assignment.

Score completed horizons with `agent evaluate --horizons next,1d`, then replay
with `lpbot run --results-dir <new directory>` and load those results for the
existing agent dashboard. Do not make scoring or loading overlap a heavy job.
The coordinator may run these operations. The evaluator also includes horizons
truncated at the stored head; its summary and dashboard do not filter them out.
For a fixed horizon, require the scored span to cover its full duration before
calling it complete, and label shorter terminal windows separately. A `next`
horizon is complete once the next decision exists. Keep outcomes later than a
decider's current `as_of` away from it; mature peer scores delivered by
`agent next` are permitted.

## Strategy and decision reviews

Research review can inspect a proposed experiment and completed findings. A
review of a decision should first form an independent view from the same look,
before seeing the author's reasoning, expectation or outcome. Then reveal
those records and score the reasoning and result separately. This is a manual
two-stage review until the designed `agent review next/write` commands exist;
do not claim the tools enforce blindness or write invented review records into
the run.

Do not let a decider review its own historical window before its decisions
finish. Do not feed hindsight into an active decider. Reviews of completed
periods can inform later research, with their time boundary recorded. The
structured strategy-note and lockstep historical-panel workflows are also
designed but not implemented.

Codex's documented [repository guidance](https://learn.chatgpt.com/docs/agent-configuration/agents-md)
and [custom agent definitions](https://learn.chatgpt.com/docs/agent-configuration/subagents)
describe how the local client discovers this setup.
