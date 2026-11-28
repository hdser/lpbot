# Implementation plan: from forward runs to real positions

Written on 28 November 2026 at the owner's request. It covers phases 3 to 5 of the build plan in `docs/DESIGN.md` (simulation mode, custody and executor, live trials) and the selector that decides what trades. `docs/DESIGN.md` stays the source of truth for what the bot does; this file says in what order to build it, where each piece goes, and what must be shown before the next one starts. When a milestone lands, its behaviour moves into the design and its line here is marked done.

## Where it stands

What exists: the pool maths and replica, the stored history (now kept current by a nightly job), the replay engine, backtests, forward runs of frozen rules, the agent mode with panels and scoring, and `lpbot select` (walk-forward and the ladder). What does not: anything that touches funds. `crates/exec` is one empty trait, `crates/risk` holds two limits, and `lpbot run` refuses every mode but `backtest`.

What the research says, plainly: no rule and no agent has yet earned real positions. The one family that held up out of sample (dollars placed after a fall, only while the month is up) made about a point a year over cash on five to fifteen campaigns a run, and the forward runs are days old. So this plan builds the path to real positions and the measurement that decides who gets on it. It does not assume anything will pass.

That is consistent with the design: the path to small live trials is short on purpose, because several things are settled only with real positions (what a fill really costs, how late an action lands, whether the fee share matches). The plan therefore has two tracks that run side by side: the plumbing (milestones M1 to M9) and the evidence (E1 to E5).

## The target

```text
candidates                selector                 allocator            trader (one process, always up)
  rules  (experiments/*.yml)  ladder: gates    ->   approved plan   ->   for each candidate: a sleeve of capital
  agents (decisions files)    standings, shares     (owner signs off)      engine: the same code as the backtest
                                                                            | actions
                                                                           risk engine: shrink or veto
                                                                            | allowed actions
                                                                           executor: plan, simulate, bound,
                                                                             sign, submit, track, reconcile
                                                                            |
                                                              signer -> roles module -> Safe -> Uniswap
                                                                            ^
                              watchdog (another machine) ------------------ +   exit paths: automatic, operator, watchdog
```

One sentence each:

- A **candidate** is a run: a frozen rule file or an agent of a panel. Nothing new here.
- The **selector** holds every forward run against the owner's gates and proposes shares. It exists; it gains a schedule, demotion and an approval step.
- The **allocator** turns an approved plan into sleeves: how much of the Safe's capital each candidate may use.
- The **trader** is the engine following the chain's head instead of replaying stored days. In paper mode its positions are virtual; in live mode the executor carries them out.
- The **risk engine** sits between the engine and the executor and can only shrink or refuse.
- The **executor** is the only code that builds transactions; the **signer** is the only process that holds the key.

## Rules this plan keeps

- **Same code in every mode.** The engine that decides is the one the backtests ran. Only the clock, the data source and the executor differ. A paper run and a forward replay of the same file must agree block for block; a difference is a bug, and that is M2's gate.
- **Integer maths for amounts.** Everything the executor puts in a transaction comes from the ported pool maths, never from a float.
- **The chain is the source of truth.** After every transaction, and at every start, positions and balances are read back. Local files are a journal, not a state.
- **No custom contract.** A Safe with the Zodiac Roles module; the one non-Rust piece is the TypeScript script that applies the roles policy.
- **No paid RPC.** The local node decides; public endpoints cross-check and serve the exit-only fallback; private submission uses a free relay.
- **Funds are the owner's.** Creating the Safe, applying the policy, making and funding keys, and every approval of capital are the owner's steps. A session writes the code, the fork tests and the runbook, and never holds a key, a password or a seed. "What the owner does" below lists them.
- **New tables need the owner's yes.** The viewer knows sixteen tables. This plan proposes two more and builds neither until approved.

## The pieces

### 1. The trader: paper mode first (`crates/sim`, `crates/cli`)

Today a forward run is replayed from stored days each time `lpbot run` is called. The trader is that loop made continuous: `lpbot paper <experiment>` (and later `lpbot live`) follows the head with `lpbot_data::live::Follower`, which already keeps a finalized base, a tail of blocks and a rebuild on a reorg, and hands each block to the engine.

What has to change in the engine:

- **Feed.** `Engine` takes its blocks from a slice of stored history. Give it a source trait with two implementations: stored days (today) and the follower. The catch-up in `agent_cmd::sync` already does the hard part for one pool.
- **Executor seam.** `Engine::execute` both decides amounts and books them. Split it: the engine emits an `Action` (the enum exists: open, close, flip, withdraw, recenter), an `Executor` returns a `Fill` (what went in and out, fees collected, gas paid, the block), and the engine books the fill. `SimExecutor` is today's code moved, so backtests do not change by a unit; M3's gate checks that.
- **Reorgs.** A decision made on a block that is later replaced is made again on the new one. In paper mode that is a rebuild from the finalized base, as the watcher does. In live mode an action already sent is tracked by its transaction, not by the block it was decided on.
- **Results.** The same six tables, written as the run goes, with the run's mode recorded. Agents in the trader read decisions exactly as a forward run does: the trader, not the agent's session, enforces stops, time limits and the loss limit between looks, so a session that fails to wake leaves a position guarded.

### 2. Sleeves: several candidates in one Safe (`crates/exec`, new `book` module)

One Safe holds every candidate's capital. Each candidate has a sleeve: a ledger entry of how much of each token is its own, and which position NFTs it opened. A position is attributed by the token id minted in the transaction the journal recorded for that candidate's campaign. Idle money is pooled in the Safe and split only in the ledger.

- The sum of the sleeves never exceeds what the Safe holds; reconciliation checks it at every start and after every transaction, and a shortfall halts new entries.
- An NFT in the Safe that no sleeve owns (the owner acting by hand) is reported and left alone.
- Resizing happens at campaign boundaries. An open campaign is never cut to rebalance; a demoted candidate's campaign is closed by its own exit rules or by the owner.

### 3. Selector to allocator (`crates/cli/src/select_cmd.rs`)

- `lpbot select list`: every candidate in one place, rule or agent: where it is defined, its rung, days forward, campaigns, result, what it still needs. This answers "what do we have that could be deployed" from one command.
- The ladder on a schedule: nightly after the history job, replay the frozen rules, then `lpbot select ladder`. Cron, as the history job is.
- **Approval.** `lpbot select ladder` writes a proposed plan. `lpbot select approve <plan>` records the owner's yes with the plan's hash; the trader reads only approved plans and refuses one whose hash does not match. A plan moves no money by itself.
- **Demotion.** A live candidate that falls back through a gate (drawdown, result, or live results that stop tracking its forward twin) returns to forward: no new entries, sleeve back to cash once its campaign ends.
- **Tracking.** Every live candidate keeps its forward run going on the same blocks. The gap between the two, campaign by campaign, is the measure of what the simulation misses, and it is a gate of its own (`max_tracking_gap_pct`, the owner's to set).

### 4. Risk engine (`crates/risk`)

The table in the design, in the order live-small needs it. Each check is a pure function with a test that trips it.

| First (before any real position) | Later |
| --- | --- |
| Notional caps: per pool, per candidate, in total | Inventory skew per token |
| Minimum range width | Depeg distance on correlated pairs |
| Pool price against its own time-weighted price and against a second pool of the pair | Exchange feed checks (no feed is stored yet) |
| Slippage bound on any swap leg | Event calendar |
| Gas cap; actions per pool per day | |
| Loss limit (exists) and the global pause flag | |
| Node behind the public head: exit-only | |
| Replica differs from the chain: halt the pool | |

### 5. Executor (`crates/exec`)

Seven steps, as the design lists them, each its own module so each can be tested alone.

1. **Plan.** From an action and the chain's state to exact calls: position manager `mint`, `increaseLiquidity`, `decreaseLiquidity`, `collect`, `burn`; router single-pool swap; token approvals; batched through the Safe's MultiSend and sent as one call to the roles module. Calldata encoding is hand-written against the ABIs with test vectors, as the pool maths was.
2. **Simulate.** The whole transaction against the local node's latest state, then the balances and the position read in the same simulated block (`eth_simulateV1` on Reth; revm in-process if the node's version lacks it; check before building). Reject when the outcome differs from the plan.
3. **Bound.** Minimum amounts, a price limit on the swap, a deadline, so the transaction reverts when the pool moved.
4. **Sign.** Hand the unsigned transaction to the signer (piece 7).
5. **Submit.** Private relay on mainnet for everything; the local node's public mempool only for an exit when the relay does not include it in time.
6. **Track.** Journal the intent, the hash and the receipt; own the nonce; replace a stuck transaction with a higher fee up to the cap. Exits ignore the cap.
7. **Reconcile.** Read positions and balances back; the fill the engine books is what the chain shows, not what the plan hoped.

The journal is append-only, one JSON line a step, under `data/journal/` (local, never committed). A crash between any two steps is recovered by reading the journal's last intent and the chain: a mint that landed is adopted, one that did not is planned again.

New dependencies this needs: transaction encoding and secp256k1 signing (the workspace has `alloy-primitives` only). Choose between the rest of alloy and `k256` with hand-written RLP when M5 starts; prefer the smaller.

### 6. Custody: the Safe and its roles policy (`deploy/custody/`)

The design's roles table, written as code: a TypeScript script that builds the policy and prints the transactions for the owners to sign.

- **Keeper:** position manager mint, increase, decrease, collect, burn; router single-pool swap; approvals to those two contracts. Recipient pinned to the Safe on every call that has one. Tokens limited to WETH, WBTC, USDC, USDT. Daily allowances on swap volume and on the number of position changes, sized to the run's capital.
- **Watchdog:** decrease and collect only, recipient pinned to the Safe.
- **Not allowed, and tested as such:** the position manager's own `multicall` (it would carry calls past the conditions), transfers of tokens or NFTs, any other target, any other recipient, native ETH.

The details of Roles v2 (the entry point's signature, how MultiSend is unwrapped, how allowances refill) are to be read from its documentation when M4 starts, not taken from this file.

### 7. Signer (`lpbot signer`, its own process)

Holds the keeper's encrypted keystore, listens on a local socket, and signs one kind of transaction: a call to the roles module's address on the configured chain, within a gas ceiling. Anything else is refused and logged. The keeper's wallet holds gas money only. The keystore's password is typed by the owner when the service starts; it is in no file and no environment a session can read.

### 8. Exit paths

- **Automatic:** a risk breach that says exit withdraws that candidate's positions.
- **Operator:** `lpbot exit-all` withdraws everything and sets the pause flag. From Telegram too, if the owner wants inbound commands (today the bot only sends).
- **Watchdog:** `lpbot watchdog` on a small machine elsewhere, with its own key and a public endpoint. The trader pushes a signed heartbeat to it every minute; when none arrives for the configured time and positions are open, it withdraws them. It decreases and collects only: tokens stay in the Safe as they are, so it ends the liquidity position, not the price exposure of whatever token the position was in. The home connection was down for four days in November 2026; the grace time is the owner's choice between pulling ranges on a short outage and leaving them unattended on a long one.

### 9. What is seen (`crates/sim` tables, Grafana)

- Live and paper runs use the six result tables, so the existing dashboards show them.
- Two tables proposed, not built without approval: `standings` (the ladder's last result: every candidate, its rung, what it needs, its share; replaced each run, as `agent_status` is) and `transactions` (the journal's on-chain steps: run, campaign, hash, block, gas, what moved), which is also the ledger export the design asks for.
- Alerts, as Grafana rules like the others: a transaction stuck or reverted, a reconciliation difference, the heartbeat missed, an exit triggered, a candidate demoted.

## Where the milestones stand

Updated 28 November 2026, on the branch `real-positions`.

| | State | What is built | What its gate still needs |
| --- | --- | --- | --- |
| M1 | Built | `lpbot select list` (every experiment file, forward or practice, with its agents and, given a plan, where its runs stand); `lpbot select approve` (the owner's yes, with the plan's SHA-256, beside the plan); `lpbot select check` and `select_ops::approved_plan`, which refuse a plan with no approval, changed since, or older than 36 hours; `deploy/selection/nightly.sh` (forward rules into `runs/forward-rules`, then the ladder) | The cron line on the node |
| M2 | Built | `lpbot paper <experiment>`: the engine as a stream (`lpbot_sim::engine::Stream`), a base fed final blocks and a copy fed the rest at every step, the tables written as it goes, the final steps journaled under `data/paper/`, and every `--check-every` steps a replay of the whole history compared with it | Two weeks on the node with every check equal, a reorg and a restart in them |
| M3 | Done | The seam: the engine records every change to its position as `lpbot_core::StepRecord` (mint, burn, swap), with no change to any result; the planner (`lpbot_exec::planner`) turns one action's records into the calls, bounded, and `verify` reads them back. On a fork of mainnet the planner's calls did on the deployed contracts what the engine counted, to the unit (`crates/exec/tests/fork.rs`) | Passed: 104 runs (the 78 of `regime-usd-usdc-weth-005`, the 14 of `forward-regime-usdc`, 12 agent runs of `agent-wording-a` and `-c`) rerun with the binary from before the split and after it wrote all six tables byte for byte the same |
| M4 | Started | The fork harness (Anvil from the local node, `crates/exec/tests/fork.rs`); the Roles v2 entry point `execTransactionWithRole(address,uint256,bytes,uint8,bytes32,bool)` (`0xc6fe8747`) found in the three Roles v2 mastercopies most deployed through the Zodiac factory | The policy as code (TypeScript, Zodiac Roles SDK: needs the owner's yes to install it), a Safe and Roles modifier on the fork, every call outside the allowlist refused |
| M5 | Started | The signer (`crates/signer`, `lpbot signer`): EIP-1559 signing matched byte for byte against `cast`, Web3 keystores, a policy that signs only calls to the roles module, a 0600 Unix socket; the journal (`lpbot_exec::journal`: append-and-sync before each step, and what to do with an action a crash left open); the submitter (`lpbot_exec::submit`: one nonce, replacement of a stuck transaction within the cap, an exit's own ceiling); reconciliation (`lpbot_exec::reconcile`: the position read back from the position manager and adopted only if it is the range the plan made with at least the liquidity it bounded), run on the fork after the mint and the burn | The kill-at-every-step test on the fork through the Safe |
| M6 | Started | The checks (`lpbot_risk::checks`: width, price against its average and its twin pool, swap impact, slippage bound, gas, actions per pool per day, node behind the public head, replica against the chain, sleeve caps); the pause flag (`lpbot_risk::pause`) and `lpbot pause`, `lpbot exit-all`, `lpbot resume` | The checks wired into the executor; the watchdog; the exit paths shown on the fork |
| M7 to M9 | Not started | | The owner's Safe, keys and funds |

## Milestones

Each has a gate; none starts before the one it depends on passes. "Cloud" means a session without chain access can do it; "laptop" needs the tunnel to the node; "owner" is a step only the owner takes.

| | Milestone | Where | Gate | Who | Size |
| --- | --- | --- | --- | --- | --- |
| M1 | `lpbot select list`; the ladder nightly; plan approval and its hash | `select_cmd.rs`, `deploy/` | Lists every rule and agent with its rung; the trader's reader refuses an unapproved or altered plan (unit tests) | Cloud | Small |
| M2 | Paper mode: the engine at the head, virtual positions, for rules and agents, as a service | `lpbot_sim::engine`, `lpbot_data::live`, new `paper_cmd.rs` | Two weeks on the node in which the paper run equals the forward replay of the same file, block for block; a reorg and a restart inside those weeks | Cloud to write, laptop to test, owner to start it on the node | Large |
| M3 | Executor seam and the planner: actions to exact calls, calldata, MultiSend | `lpbot_sim::engine`, `crates/exec` | Backtests unchanged to the unit after the split; over every campaign of the stored backtests the planner's amounts equal the engine's | Cloud | Large |
| M4 | Fork harness and the roles policy as code | `deploy/custody/`, `crates/exec/tests` | On a fork of mainnet from the local node: every call outside the allowlist is rejected; a full campaign runs through the Safe; the pool-maths fork test Phase 1 still lacks is written and passes | Laptop | Large |
| M5 | Signer, submitter, journal, reconcile | `crates/exec`, `lpbot signer` | On the fork: killed at each step and restarted, it never mints twice and never loses a position; a stuck transaction is replaced; the signer refuses everything but a roles call | Laptop | Large |
| M6 | Risk engine for live-small, pause flag, `exit-all`, watchdog | `crates/risk`, `crates/cli` | Every check has a test that trips it; all three exit paths shown on the fork | Cloud, then laptop | Medium |
| M7 | Real custody and the first transactions | The owner's Safe | On mainnet, simulated calls that must fail do fail; one campaign at dust size, every transaction confirmed by hand (`--confirm` prints the plan and waits), entered, collected and exited | Owner, with the runbook | Medium |
| M8 | Live, small: whatever passed the ladder gets the smallest share | Everything | Live results within the tracking gate of their forward twins; each exit path used once in production; one demotion carried out | Owner approves each plan | Medium |
| M9 | Shares by result; more tracks; `approve` mode over Telegram | `select_cmd.rs`, `crates/cli` | The owner's call, from M8's record | | |

M1, M2 and M3 do not depend on each other and can be built at the same time. M4 needs M3; M5 needs M4; M6 needs M2 and M3; M7 needs M5 and M6.

## The evidence track

The plumbing can be finished with nothing on the ladder's top rung. These decide whether anything gets there, and none waits for a milestone.

| | Work | Why |
| --- | --- | --- |
| E1 | Start the frozen rules and an agent panel forward on the node, and leave the files alone | Forward days are the only clean evidence, and they only accrue from the day they start |
| E2 | Finish the wording test; put the two or three wordings that models follow into the forward panel on all three models | A wording a model misreads wastes months of forward time |
| E3 | Backfill history before 2025 | Eighteen months give twelve scored periods; the regime gate trades five to fifteen times in them. More history is the only fast way to more campaigns |
| E4 | Condition the return odds on how far and how long price has been away (gap 1 of Phase 2) | The patience decision is the bot's core and still reads unconditional odds |
| E5 | Score each decision against the options not taken (gap 2) | It is how the adaptive decision is told from luck, for rules as it already is for agents |

A choice for the owner follows from this (below): whether M7 and M8 wait for a candidate that passed the gates, or whether one candidate is run at dust size as a trial of the pipes, labelled as that and outside the ladder.

## What the owner does, and what a session never does

A session writes code, tests, the policy script, the runbook, and operates the node within `deploy/NODE.md`. These steps are the owner's alone:

1. Create the Safe and choose its signers and threshold.
2. Enable the roles module and sign the policy transactions the script prints, after reading them.
3. Create the keeper and watchdog keys, type their passwords, fund the keeper with gas money.
4. Rent and set up the watchdog's machine.
5. Fund the Safe.
6. Set the ladder's gates, each track's capital and each run's loss limit.
7. Approve each plan (`lpbot select approve`) and, in M7, each transaction.

No committed file carries a real address of the Safe, a key or a password; config keeps its `"0x..."` placeholders and the real values live in `deploy/.env` and `data/` on the node.

## Decisions the owner needs to make

Before the milestone named, not all at once.

| Decision | Needed by | Note |
| --- | --- | --- |
| Approve the two tables, `standings` and `transactions`, or keep both as files | M1 and M5 | Without `standings` the ladder is a command and a JSON file; without `transactions` the journal is not on a dashboard |
| The ladder's gates, including the new tracking gate | M1 | The example file's values are placeholders |
| Capital for the first real campaigns and its loss limit | M7 | The design says campaign-sized; this plan suggests the smallest amount for which gas stays under about 1% of a campaign |
| Dust trial of the pipes before any candidate passes, or wait | M7 | The trial settles fill costs and latency sooner; it also puts money on a strategy no evidence supports, so it should be sized as a cost |
| Whether agents may hold live sleeves on the same gates as rules | M8 | An agent needs a session that wakes; the trader guards its position when it does not, but a missed look is a missed decision |
| Watchdog: where it runs and its grace time | M6 | Short pulls ranges on any outage; long leaves them unattended |
| Telegram commands in (`exit-all`, approvals), or outbound only | M6, M9 | Inbound commands are a second way to move the bot and need their own check of who is asking |
| Safe signers and threshold | M7 | One signer on a hardware wallet is the simplest; two of three survives a lost device |

## Risks of this plan itself

| Risk | What limits it |
| --- | --- |
| The pipes get built and nothing ever passes the gates | The evidence track runs from day one; M7's dust trial is optional and sized as a cost; nothing after M6 is built before there is a reason |
| The engine split (M3) quietly changes backtests | The gate is equality to the unit on every stored run, before and after |
| A policy written too broadly | Fork tests of everything that must fail, run again against the real Safe by simulation before funding |
| The simulation flatters fills | The tracking gate: a live candidate is measured against its own forward twin, campaign by campaign |
| One process holding too much | The signer is separate and signs one kind of call; the watchdog is on another machine with a narrower role; the owner can remove the keeper in one transaction |
| The box at home: heat, the drive, the connection | Already the suspects when the node falls behind; the watchdog exists for exactly this; `deploy/RUNBOOK.md` keeps what happened |
