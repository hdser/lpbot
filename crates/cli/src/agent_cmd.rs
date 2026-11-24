//! `lpbot agent next` and `lpbot agent decide`: a run decided one look at
//! a time by a person or a session (the agent mode of `lpbot_sim::agent`).
//!
//! `next` replays the run from its decisions up to the first look without
//! one and shows how the pool looks there; `decide` writes the decision for
//! that look and shows the next one. The decisions file is the run: `lpbot
//! run` on the same experiment writes its six tables.
//!
//! `decide` also appends to the run's reasoning log, next to the decisions
//! file: what the look showed, what was decided and why. The run never
//! reads it; it is there for whoever decides, and for reading the run
//! afterwards. The same goes, one JSON record a look, to the shared log
//! (`agent_log`) that other agents are handed (`agent_peers`).
//!
//! A forward run has a window open at the end (`to: open`). Before each
//! look, `next` and `decide` bring the run's pool and the gas prices up to
//! the node's head (`sync`); a look that is due is shown at the newest
//! block, and only while the node is at the head of the chain. When no
//! look is due yet, `next` says when by the wall clock, and with `--wait`
//! waits for it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::experiment::ResolvedRun;
use lpbot_data::backfill::{backfill_pools, first_block_at_or_after, AtHead, NodeSource, PoolRef};
use lpbot_data::day::Day;
use lpbot_data::snapshot::SnapshotStore;
use lpbot_data::store::{DayState, EventStore};
use lpbot_sim::agent::Observation;
use lpbot_sim::engine;

use crate::agent_log::{self, now_unix as now, Expectation, Identity, Source, Stance};
use crate::agent_peers;
use crate::chain_cmd::{connect_to, safe_head, HEAD_MARGIN};
use crate::gas_history_cmd::catch_up;
use crate::run_cmd::{
    agent_file, gas_from_chain, history_days, named_pool, prepare_one, run_window, Prepared,
    TIP_BLOCKS,
};

/// Seconds between two blocks.
const SLOT: u64 = 12;

/// What a look asks of the agent, as it is written next to the decisions.
fn pending_path(decisions: &Path) -> PathBuf {
    let mut name = decisions.as_os_str().to_owned();
    name.push(".pending");
    PathBuf::from(name)
}

/// The reasoning log next to the decisions file: `my-run.jsonl` keeps its
/// reasoning in `my-run.log.md`.
pub fn log_path(decisions: &Path) -> PathBuf {
    decisions.with_extension("log.md")
}

/// The look waiting for a decision: its time (the key of the decisions
/// file), the time of the block it was shown at, its number and day in the
/// run, the text it showed, and the first half of its shared record
/// (`agent_log::look_record`).
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub look: u64,
    pub at: Option<u64>,
    pub number: Option<u64>,
    pub day: Option<f64>,
    pub seen: Option<String>,
    pub record: Option<serde_json::Value>,
}

impl Pending {
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "look": self.look,
            "at": self.at,
            "number": self.number,
            "day": self.day,
            "seen": self.seen,
            "record": self.record,
        })
        .to_string()
    }

    /// Reads a pending file; one written before the log existed holds only
    /// the look's time.
    pub fn parse(text: &str) -> Option<Pending> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        Some(Pending {
            look: v.get("look")?.as_u64()?,
            at: v.get("at").and_then(serde_json::Value::as_u64),
            number: v.get("number").and_then(serde_json::Value::as_u64),
            day: v.get("day").and_then(serde_json::Value::as_f64),
            seen: v
                .get("seen")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            record: v.get("record").filter(|r| r.is_object()).cloned(),
        })
    }

    /// The look as the agent knows it: by number and day, never by time.
    fn label(&self) -> String {
        match (self.number, self.day) {
            (Some(n), Some(d)) => format!("Look {n}, day {d:.2}"),
            _ => "Look (number not recorded)".into(),
        }
    }
}

/// The log's entry for one decision, in Markdown.
pub fn log_entry(pending: &Pending, decision: &Decision, stance: Option<&Stance>) -> String {
    let mut text = String::new();
    let action = match (decision.action, decision.lower_pct, decision.upper_pct) {
        ("enter", Some(lower), Some(upper)) => format!("enter {lower:+.2}% to {upper:+.2}%"),
        (other, _, _) => other.to_string(),
    };
    let _ = writeln!(
        text,
        "## {}: {action}, next look in {}\n",
        pending.label(),
        decision.next
    );
    if let Some(seen) = &pending.seen {
        let _ = writeln!(text, "```text\n{}\n```\n", seen.trim_end());
    }
    if !decision.note.is_empty() {
        let _ = writeln!(text, "Note: {}\n", decision.note.trim());
    }
    if !decision.expect.is_empty() {
        let _ = writeln!(text, "Expected: {}.\n", decision.expect.describe());
    }
    if let Some(stance) = stance {
        let _ = writeln!(text, "Stance toward the peers: {}.\n", stance.describe());
    }
    let reasoning = decision.reasoning.trim();
    if reasoning.is_empty() {
        let _ = writeln!(text, "(No reasoning written.)\n");
    } else {
        let _ = writeln!(text, "{reasoning}\n");
    }
    text
}

/// Appends an entry to the log, starting the file with a header naming the
/// agent and its model when it is new.
pub fn append_log(log: &Path, who: &Identity, entry: &str) -> Result<()> {
    let mut content = std::fs::read_to_string(log).unwrap_or_default();
    if content.is_empty() {
        let _ = writeln!(
            content,
            "# Reasoning log of agent {} ({})\n\nDecisions: {}. One entry a decision: what the \
             look showed, what was decided and why; the same, one JSON record a look, is in {} \
             for other agents and tools. The run does not read either. Local only: keep them \
             under data/, which is not committed.\n",
            who.name,
            who.model,
            who.decisions.display(),
            agent_log::records_path(&who.decisions).display()
        );
    }
    content.push_str(entry);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(log, content).with_context(|| format!("writing {}", log.display()))
}

/// Who decides the run: its agent entry, with its decisions file.
pub fn identity_of(run: &ResolvedRun) -> Result<Identity> {
    let strategy = run
        .config
        .strategies
        .first()
        .ok_or_else(|| anyhow!("the run has no strategy"))?;
    if agent_file(strategy).is_none() {
        bail!(
            "the experiment's strategy has no agent.decisions file; set it, such as \
             `agent.decisions: [data/agent/my-run.jsonl]` under `sweep`"
        );
    }
    agent_log::identity(strategy)?.ok_or_else(|| anyhow!("the run has no agent.decisions file"))
}

fn read_pending(decisions: &Path) -> Result<Pending> {
    let pending = pending_path(decisions);
    match std::fs::read_to_string(&pending) {
        Ok(text) => Pending::parse(&text).ok_or_else(|| {
            anyhow!(
                "{} is damaged; run `lpbot agent next` again",
                pending.display()
            )
        }),
        Err(_) => bail!(
            "no look is waiting for a decision; run `lpbot agent next <experiment>` first to see it"
        ),
    }
}

/// How `next` and `decide` treat a forward run, whose window is open at
/// the end.
#[derive(Debug, Clone)]
pub struct Forward {
    /// Bring the store up to the node's head before each look.
    pub sync: bool,
    /// The node's endpoint; `None` takes LPBOT_RPC, then the config's.
    pub rpc: Option<String>,
    /// How old the head may be, by the wall clock, for a look to be shown.
    pub max_lag: u64,
    /// How long `next` may wait for the next look to come due.
    pub wait: u64,
}

/// What a replay came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A look waits for a decision.
    Look(Observation),
    /// A forward run: no look is due within the stored history. The next is
    /// due at this time.
    Due(u64),
    /// The window is over, or the loss limit stopped the run.
    Over,
}

/// What a replay came to: a look, a look due later, or the end. `open`:
/// the window is open at the end.
pub fn outcome(out: &engine::Output, open: bool) -> Outcome {
    if let Some(look) = &out.pending {
        return Outcome::Look(look.clone());
    }
    match out.next_look {
        Some(due) if open && !out.summary.halted_by_loss_limit => Outcome::Due(due),
        _ => Outcome::Over,
    }
}

/// A span of time as a person says it: `40 s`, `12 min`, `3.2 h`, `4.2 d`.
pub fn span(seconds: u64) -> String {
    match seconds {
        s if s < 90 => format!("{s} s"),
        s if s < 90 * 60 => format!("{} min", (s + 30) / 60),
        s if s < 48 * 3_600 => format!("{:.1} h", s as f64 / 3_600.0),
        s => format!("{:.1} d", s as f64 / 86_400.0),
    }
}

/// A time as the wall clock shows it, in UTC: `2026-11-24 18:05 UTC`.
pub fn clock(time: u64) -> String {
    format!(
        "{} {:02}:{:02} UTC",
        Day::of_time(time),
        time % 86_400 / 3_600,
        time % 3_600 / 60
    )
}

/// The refusal for a look whose head is older than `max_lag`, or `None`.
pub fn stale(head_block: u64, head_time: u64, now: u64, max_lag: u64) -> Option<String> {
    let age = now.saturating_sub(head_time);
    (age > max_lag).then(|| {
        format!(
            "block {head_block}, the newest the store reaches, was made {} ago: the node is \
             behind the chain, so a look now would not be at the head. Wait until it has caught \
             up (`make status` on the node machine, or \"Block age\" on the live dashboard), then \
             run this again. --max-lag sets how old the head may be (now {})",
            span(age),
            span(max_lag)
        )
    })
}

/// What `next` says when no look is due yet.
pub fn due_message(number: usize, due: u64, newest: Option<(u64, u64)>, now: u64) -> String {
    let mut text = if due > now {
        format!(
            "No look is due yet. Look {number} is due at {} (by the wall clock), in {}.",
            clock(due),
            span(due - now)
        )
    } else {
        format!(
            "Look {number} is due since {}, {} ago; the store does not reach its block yet.",
            clock(due),
            span(now - due)
        )
    };
    if let Some((block, time)) = newest {
        let _ = write!(
            text,
            " The store reaches block {block}, made {} ago.",
            span(now.saturating_sub(time))
        );
    }
    let _ = write!(
        text,
        "\nNext look due: {}Z\nRun `lpbot agent next <experiment>` then: it brings the store up to \
         the head first. With `--wait <duration>` it waits for the look and shows it.",
        iso(due)
    );
    text
}

/// A time as `2026-11-24T18:05:00`, in UTC.
fn iso(time: u64) -> String {
    format!(
        "{}T{:02}:{:02}:{:02}",
        Day::of_time(time),
        time % 86_400 / 3_600,
        time % 3_600 / 60,
        time % 60
    )
}

/// How far a catch-up brought the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Synced {
    /// The block the store now reaches, a few behind the node's head, and
    /// its time.
    pub block: u64,
    pub time: u64,
    /// The finalized block: days and gas chunks close only up to it.
    pub final_block: u64,
    /// Days of the pool's events written, and gas blocks read.
    pub days: usize,
    pub gas_blocks: u64,
}

/// The first day from `wanted` on that is not stored complete, up to
/// `today`: where a catch-up starts.
pub fn first_open_day(
    store: &EventStore,
    chain: &str,
    pool: alloy_primitives::Address,
    wanted: Day,
    today: Day,
) -> Day {
    let mut day = wanted;
    while day < today && store.day_state(chain, pool, day) == DayState::Complete {
        day = day.next();
    }
    day
}

/// The newest block the node can serve logs and fees for, a few behind
/// its head, and its time. While it syncs, its block number can run ahead
/// of what it has executed (Reth reports its headers stage), so the block
/// it has processed bounds it.
fn served_head(client: &lpbot_chain::RpcClient) -> Result<(u64, u64)> {
    let latest = client.block_number()?;
    let served = match client.syncing()? {
        Some(progress) => {
            println!(
                "note: the node is still syncing (block {} of {}); the store goes as far as \
                 block {}",
                progress.current,
                progress.highest,
                latest.min(progress.current)
            );
            latest.min(progress.current)
        }
        None => latest,
    };
    let head = served.saturating_sub(HEAD_MARGIN);
    Ok((head, client.block_header(head)?.0))
}

/// Brings a forward run's pool and the gas prices up to the node's head:
/// the days not yet stored complete, the day in progress up to the head,
/// and the gas of every block from the window's start. Days and gas
/// chunks close only at the finalized block; the rest is read again by the
/// next catch-up.
pub fn sync(run: &ResolvedRun, root: &Path, chain_name: &str, rpc: Option<&str>) -> Result<Synced> {
    let config = &run.config;
    let strategy = config
        .strategies
        .first()
        .ok_or_else(|| anyhow!("the run has no strategy"))?;
    let pool = named_pool(strategy)?.ok_or_else(|| {
        anyhow!(
            "a forward run names its pool, so the catch-up knows what to store: set `pool:` \
             in the strategy, or under `sweep`"
        )
    })?;
    let chain = connect_to(config, chain_name, rpc)?;
    let client = &chain.client;
    let node = NodeSource {
        client,
        head_margin: HEAD_MARGIN,
        log_chunk: 2_000,
    };
    let (head, head_time) = served_head(client)?;
    let safe = safe_head(client)?;
    if let Some(warning) = &safe.warning {
        println!("warning: {warning}");
    }
    let final_block = safe.block.min(head);
    let source = AtHead {
        source: &node,
        block: head,
        time: head_time,
        final_block: Some(final_block),
    };

    let (from, _) = run_window(config)?;
    let wanted = from.minus(history_days(strategy)?);
    let start = first_open_day(
        &EventStore::new(root),
        chain_name,
        pool,
        wanted,
        Day::of_time(head_time),
    );
    let snapshot = SnapshotStore::new(root)
        .latest_on_or_before(chain_name, pool, start)?
        .ok_or_else(|| {
            anyhow!(
                "pool {pool:#x} has no snapshot stored on or before {start}; run `lpbot backfill \
                 --pool {pool:#x} --from {wanted}` once first"
            )
        })?;
    let pool_ref = PoolRef {
        address: pool,
        token0: snapshot.token0,
        token1: snapshot.token1,
    };
    let report = backfill_pools(&source, root, chain_name, &[pool_ref], start, |_, _| {})?
        .pop()
        .expect("one outcome for the one pool")?;

    let gas_blocks = if gas_from_chain(config)? {
        let first = first_block_at_or_after(&source, from.start(), 1, head)?;
        catch_up(
            client,
            root,
            chain_name,
            first.saturating_sub(TIP_BLOCKS as u64),
            head,
            final_block,
        )?
    } else {
        0
    };
    Ok(Synced {
        block: head,
        time: head_time,
        final_block,
        days: report.written.len(),
        gas_blocks,
    })
}

/// Replays the run to its first look without a decision and shows it.
/// `None` once the window is over, or, in a forward run, when no look is
/// due yet: then it says when the next is, and with `forward.wait` waits
/// up to that long for it.
pub fn next(
    run: &ResolvedRun,
    root: &Path,
    chain: &str,
    forward: &Forward,
) -> Result<Option<Observation>> {
    let who = identity_of(run)?;
    let pending = pending_path(&who.decisions);
    let started = now();
    let mut said_waiting = false;
    loop {
        let open = run_window(&run.config)?.1.is_none();
        let synced = if open && forward.sync {
            // A node that is busy committing blocks can answer out of step
            // for a moment: one more try a block later.
            let rpc = forward.rpc.as_deref();
            let synced = sync(run, root, chain, rpc).or_else(|first| {
                println!("note: the catch-up failed ({first:#}); trying again in {SLOT} s");
                std::thread::sleep(Duration::from_secs(SLOT));
                sync(run, root, chain, rpc)
            });
            Some(synced.with_context(|| {
                "bringing the store up to the node's head before the look; is the node \
                 reachable (LPBOT_RPC, or --rpc) and synced (`make status`)? --no-sync replays \
                 what is stored"
            })?)
        } else {
            None
        };
        if let Some(synced) = &synced {
            println!(
                "Store brought up to block {}, made {} ago: {} day(s) of the pool's events \
                 written, the gas of {} block(s) read.",
                synced.block,
                span(now().saturating_sub(synced.time)),
                synced.days,
                synced.gas_blocks
            );
        }
        let prepared = prepare_one(run, root, chain)?;
        let out = prepared.replay(&prepared.settings)?;
        let head = synced.map(|s| (s.block, s.time)).or(prepared.newest);
        match outcome(&out, prepared.open) {
            Outcome::Look(observation) => {
                if prepared.open {
                    let (block, time) = head.unwrap_or((observation.block, observation.time));
                    if let Some(refusal) = stale(block, time, now(), forward.max_lag) {
                        let _ = std::fs::remove_file(&pending);
                        bail!("{refusal}");
                    }
                }
                show(run, &who, &prepared, &out, &observation)?;
                return Ok(Some(observation));
            }
            Outcome::Over => {
                let _ = std::fs::remove_file(&pending);
                over(run, &out, &who.decisions);
                return Ok(None);
            }
            Outcome::Due(due) => {
                let _ = std::fs::remove_file(&pending);
                let clock_now = now();
                let number = decisions_used(run)?.saturating_sub(out.unused_decisions) + 1;
                // A look that is due while the node lags is not coming.
                if due <= clock_now {
                    if let Some((block, time)) = synced.map(|s| (s.block, s.time)) {
                        if let Some(refusal) = stale(block, time, clock_now, forward.max_lag) {
                            bail!("{refusal}");
                        }
                    }
                }
                let deadline = started.saturating_add(forward.wait);
                if clock_now >= deadline {
                    println!("{}", due_message(number, due, head, clock_now));
                    if let Some((block, time)) = synced.map(|s| (s.block, s.time)) {
                        if stale(block, time, clock_now, forward.max_lag).is_some() {
                            println!(
                                "warning: the node is behind the chain; the look will be refused \
                                 until it has caught up (`make status`)"
                            );
                        }
                    }
                    return Ok(None);
                }
                if !said_waiting {
                    println!(
                        "Waiting for look {number}, due at {} (in {}), for at most {}.",
                        clock(due),
                        span(due.saturating_sub(clock_now)),
                        span(forward.wait)
                    );
                    said_waiting = true;
                }
                // The look's block reaches the store a few blocks after it
                // is made; until then, ask again every block.
                let ready = due + (HEAD_MARGIN + 1) * SLOT;
                let until = if ready > clock_now {
                    ready
                } else {
                    clock_now + SLOT
                };
                sleep_until(until.min(deadline));
            }
        }
    }
}

/// Sleeps until `time` by the wall clock, a minute at most at a stretch, so
/// a machine that slept meanwhile does not oversleep the look.
fn sleep_until(time: u64) {
    loop {
        let now = now();
        if now >= time {
            return;
        }
        std::thread::sleep(Duration::from_secs((time - now).min(60)));
    }
}

/// How many decisions the decisions file holds.
fn decisions_used(run: &ResolvedRun) -> Result<usize> {
    let strategy = &run.config.strategies[0];
    Ok(crate::run_cmd::agent_plan(strategy)?.map_or(0, |plan| plan.decisions.len()))
}

/// Notes decisions the run did not ask for, if any.
fn note_unused(out: &engine::Output, decisions: &Path) {
    if out.unused_decisions > 0 {
        println!(
            "note: {} decision(s) at the end of {} answer looks the run did not ask for (the \
             settings or the history changed since); they are left out",
            out.unused_decisions,
            decisions.display()
        );
    }
}

/// Writes the pending look, with the first half of its shared record, and
/// prints it with the peers' records it hands over.
fn show(
    run: &ResolvedRun,
    who: &Identity,
    prepared: &Prepared,
    out: &engine::Output,
    observation: &Observation,
) -> Result<()> {
    let decisions = &who.decisions;
    note_unused(out, decisions);
    let strategy = &run.config.strategies[0];
    let pending = pending_path(decisions);
    if let Some(parent) = pending.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seen = describe(observation, &prepared.pool_name, &strategy.value_token);
    // What the look knows: the market up to the block it is shown at.
    let as_of = observation.time;
    let own = agent_log::read_records(&agent_log::records_path(decisions))?;
    let handover = agent_peers::handover(who, as_of, &agent_peers::already_shown(&own))?;
    let source = Source {
        identity: who,
        experiment: &run.experiment,
        run_id: &run.run_id,
        pool: &prepared.pool_name,
        value_token: &strategy.value_token,
    };
    let record = agent_log::look_record(&source, observation, as_of, &seen, &handover.shown);
    let waiting = Pending {
        look: observation.look,
        at: Some(observation.time),
        number: Some(observation.looks_before as u64 + 1),
        day: Some(observation.day),
        seen: Some(seen.clone()),
        record: Some(record),
    };
    std::fs::write(&pending, format!("{}\n", waiting.to_json()))
        .with_context(|| format!("writing {}", pending.display()))?;
    if prepared.open {
        println!(
            "At the head: block {}, made {} ago.",
            observation.block,
            span(now().saturating_sub(observation.time))
        );
    }
    print!("{seen}");
    if !who.peers.is_empty() {
        println!();
        print!("{}", handover.text);
    }
    println!();
    println!(
        "Decide with: lpbot agent decide <experiment> --action stay_out|hold|exit|enter \
         [--lower <pct> --upper <pct>] --next <duration> --note \"why, in a sentence\" \
         --reasoning \"what you saw and weighed\" [--expect-low <pct> --expect-high <pct> \
         --expect-confidence <0-1>] [--expect \"what you expect\"]"
    );
    if !handover.shown.is_empty() {
        println!(
            "This look showed your peers' records: add --stance own|copy|adapt|depart \
             [--toward <peer>]."
        );
    }
    println!(
        "Agent {} ({}). Your reasoning so far: {}",
        who.name,
        who.model,
        log_path(decisions).display()
    );
    Ok(())
}

/// What `next` says once the window is over or the loss limit stopped it.
fn over(run: &ResolvedRun, out: &engine::Output, decisions: &Path) {
    note_unused(out, decisions);
    let strategy = &run.config.strategies[0];
    let s = &out.summary;
    let how = if s.halted_by_loss_limit {
        "The loss limit stopped the run"
    } else {
        "The window is over"
    };
    println!(
        "{how}: {:+.2}% in {} ({:+.2}% in dollars; holding it made {:+.2}% in dollars), {} \
         campaign(s), fees {:.4}, gas {:.4}. `lpbot run` on the experiment writes the tables.",
        s.result_pct,
        strategy.value_token,
        s.result_pct_usd,
        s.hold_value_pct_usd,
        s.campaigns,
        s.fees,
        s.gas
    );
}

/// A decision for the look `lpbot agent next` last showed.
#[derive(Clone)]
pub struct Decision<'a> {
    pub action: &'a str,
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
    pub next: &'a str,
    pub note: &'a str,
    /// The reasoning, as long as it needs to be; it goes to the logs only.
    pub reasoning: &'a str,
    /// What the agent expects until its next look; it goes to the logs only.
    pub expect: Expectation,
    /// How the decision relates to the peers' records the look showed:
    /// `own`, `copy`, `adapt` or `depart`, and toward which peer.
    pub stance: Option<&'a str>,
    pub toward: Option<&'a str>,
}

/// The decision's line in the decisions file: the look it answers, the
/// time of the block it was shown at, when it was decided by the wall
/// clock, and the decision.
pub fn decision_line(
    look: u64,
    at: Option<u64>,
    decided: u64,
    decision: &Decision,
) -> Result<String> {
    if crate::run_cmd::parse_duration(decision.next).is_none() {
        bail!(
            "--next {} is not a duration such as 30m, 4h or 2d",
            decision.next
        );
    }
    let mut row = serde_json::json!({
        "look": look,
        "action": decision.action,
        "next": decision.next,
        "note": decision.note,
        "decided": decided,
    });
    if let Some(at) = at {
        row["at"] = serde_json::json!(at);
    }
    match decision.action {
        "enter" => {
            let (Some(lower), Some(upper)) = (decision.lower_pct, decision.upper_pct) else {
                bail!("an entry needs --lower and --upper, in percent from the price now");
            };
            row["lower_pct"] = serde_json::json!(lower);
            row["upper_pct"] = serde_json::json!(upper);
        }
        "stay_out" | "hold" | "exit" => {}
        other => bail!("--action {other}: use stay_out, hold, exit or enter"),
    }
    // The line must read back as it was meant.
    crate::run_cmd::parse_decisions(&row.to_string())?;
    Ok(row.to_string())
}

/// Writes the decision for the pending look, adds it to the reasoning log
/// and shows the next look.
pub fn decide(
    run: &ResolvedRun,
    root: &Path,
    chain: &str,
    decision: &Decision,
    forward: &Forward,
) -> Result<()> {
    let who = identity_of(run)?;
    let decisions = who.decisions.clone();
    let pending = pending_path(&decisions);
    let waiting = read_pending(&decisions)?;
    let look = waiting.look;
    // Everything is checked before anything is written.
    let decided = now();
    let line = decision_line(look, waiting.at, decided, decision)?;
    decision.expect.check()?;
    let shown_from = waiting
        .record
        .as_ref()
        .map(agent_log::shown_from)
        .unwrap_or_default();
    let stance = agent_log::stance(decision.stance, decision.toward, &shown_from, &who.peers)?;
    if let Some(parent) = decisions.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = std::fs::read_to_string(&decisions).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&line);
    content.push('\n');
    std::fs::write(&decisions, content)
        .with_context(|| format!("writing {}", decisions.display()))?;
    std::fs::remove_file(&pending).ok();
    let log = log_path(&decisions);
    append_log(&log, &who, &log_entry(&waiting, decision, stance.as_ref())).with_context(|| {
        format!(
            "the decision is written to {}, but not to the reasoning log",
            decisions.display()
        )
    })?;
    if let Some(mut record) = waiting.record.clone() {
        agent_log::complete(
            &mut record,
            &agent_log::Decided {
                action: decision.action,
                lower_pct: decision.lower_pct,
                upper_pct: decision.upper_pct,
                next: decision.next,
                note: decision.note,
                reasoning: decision.reasoning,
                expect: &decision.expect,
                stance: stance.as_ref(),
            },
            decided,
        );
        let records = agent_log::records_path(&decisions);
        agent_log::append_records(&records, &[record]).with_context(|| {
            format!(
                "the decision is written to {}, but not to the shared log {}",
                decisions.display(),
                records.display()
            )
        })?;
    }
    println!("{}: {}", waiting.label(), decision.action);
    println!();
    let forward = Forward {
        wait: 0,
        ..forward.clone()
    };
    next(run, root, chain, &forward).map(|_| ())
}

/// Shows again every record of the run's peers up to the look waiting for
/// a decision.
pub fn peers(run: &ResolvedRun) -> Result<()> {
    let who = identity_of(run)?;
    if who.peers.is_empty() {
        println!(
            "Agent {} reads no peers: its entry names none in agent.peers.",
            who.name
        );
        return Ok(());
    }
    let waiting = read_pending(&who.decisions)?;
    let as_of = waiting
        .record
        .as_ref()
        .and_then(|r| r.get("as_of"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            anyhow!("run `lpbot agent next` again: the waiting look predates the shared log")
        })?;
    let handover = agent_peers::handover(&who, as_of, &Default::default())?;
    println!("{}", waiting.label());
    print!("{}", handover.text);
    Ok(())
}

fn hours(seconds: u64) -> String {
    let h = seconds as f64 / 3_600.0;
    if h < 48.0 {
        format!("{h:.1} h")
    } else {
        format!("{:.1} d", h / 24.0)
    }
}

/// The observation as text for the agent: what it sees and nothing else.
pub fn describe(o: &Observation, pool: &str, value_token: &str) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Look {}, day {:.2} of the run, on {pool}. Prices are the volatile token's, in percent from now.",
        o.looks_before + 1,
        o.day
    );
    let _ = writeln!(
        text,
        "Fee {:.2}%, one tick spacing {:.3}%. Gas {:.3} gwei: entering and leaving cost {:.3}% of the run.",
        o.fee_pct, o.spacing_pct, o.gas_gwei, o.round_trip_gas_pct
    );
    let _ = writeln!(
        text,
        "Run so far: {:+.2}% in {value_token}, {} campaign(s){}.",
        o.result_pct,
        o.campaigns,
        o.since_last_close
            .map(|s| format!(", the last ended {} ago", hours(s)))
            .unwrap_or_default()
    );
    match &o.position {
        Some(p) => {
            let _ = writeln!(
                text,
                "Position: {:+.2}% to {:+.2}%, {}, held {}, result {:+.3}%, fees {:+.3}%, {:.0}% in {value_token}.",
                p.lower_pct,
                p.upper_pct,
                p.state,
                hours(p.held_seconds),
                p.result_pct,
                p.fees_pct,
                p.value_share_pct
            );
        }
        None => {
            let _ = writeln!(text, "Position: none.");
        }
    }
    let _ = writeln!(
        text,
        "{:>5} {:>8} {:>8} {:>8} {:>10} {:>7} {:>9}",
        "scale", "change", "high", "low", "vol/day", "swaps", "volume"
    );
    for s in &o.scales {
        let _ = writeln!(
            text,
            "{:>5} {:>+8.2} {:>+8.2} {:>+8.2} {:>10} {:>7} {:>9}",
            s.label,
            s.change_pct,
            s.high_pct,
            s.low_pct,
            s.vol_pct_per_day
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "-".into()),
            s.swaps,
            s.volume_vs_30d
                .map(|v| format!("{v:.2}x"))
                .unwrap_or_else(|| "-".into())
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_sim::agent::{PositionView, ScaleView};

    #[test]
    fn a_decision_line_reads_back_and_bad_ones_are_refused() {
        let enter = Decision {
            action: "enter",
            lower_pct: Some(-3.0),
            upper_pct: Some(-1.0),
            next: "4h",
            note: "after a 4% fall",
            reasoning: "",
            expect: Expectation::default(),
            stance: None,
            toward: None,
        };
        let line = decision_line(1_800_000_000, None, 1_800_000_090, &enter).unwrap();
        let back = crate::run_cmd::parse_decisions(&line).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].look, 1_800_000_000);
        assert_eq!(back[0].at, None);
        assert!(line.contains("\"decided\":1800000090"), "{line}");
        let shown =
            decision_line(1_800_000_000, Some(1_800_000_060), 1_800_000_090, &enter).unwrap();
        let back = crate::run_cmd::parse_decisions(&shown).unwrap();
        assert_eq!(back[0].at, Some(1_800_000_060));
        assert_eq!(back[0].next_look, 4 * 3_600);
        assert_eq!(back[0].note, "after a 4% fall");
        let missing = Decision {
            lower_pct: None,
            ..enter.clone()
        };
        assert!(decision_line(1, None, 2, &missing).is_err());
        let wrong = Decision {
            action: "buy",
            ..enter.clone()
        };
        assert!(decision_line(1, None, 2, &wrong).is_err());
        let late = Decision {
            next: "soon",
            ..enter.clone()
        };
        assert!(decision_line(1, None, 2, &late).is_err());
        assert!(
            !line.contains("reasoning"),
            "the reasoning stays out of the decisions file: {line}"
        );
    }

    #[test]
    fn the_log_sits_next_to_the_decisions_file() {
        assert_eq!(
            log_path(Path::new("data/agent/usdc-weth-2026-07.jsonl")),
            PathBuf::from("data/agent/usdc-weth-2026-07.log.md")
        );
        assert_eq!(
            log_path(Path::new("data/agent/run")),
            PathBuf::from("data/agent/run.log.md")
        );
    }

    #[test]
    fn a_pending_look_reads_back_and_an_old_one_still_reads() {
        let p = Pending {
            look: 1_800_000_000,
            at: Some(1_800_000_004),
            number: Some(12),
            day: Some(3.25),
            seen: Some("Look 12, day 3.25\n  1h  -1.00\n".into()),
            record: Some(serde_json::json!({"kind": "look", "as_of": 1_800_000_004u64})),
        };
        assert_eq!(Pending::parse(&p.to_json()), Some(p));
        let old = Pending::parse("{\"look\": 7}\n").unwrap();
        assert_eq!(old.look, 7);
        assert_eq!((old.number, old.day, old.seen), (None, None, None));
        assert_eq!((old.at, old.record), (None, None));
        assert!(Pending::parse("{\"number\": 3}").is_none());
        assert!(Pending::parse("not json").is_none());
    }

    #[test]
    fn a_log_entry_shows_what_was_seen_what_was_done_and_why() {
        let p = Pending {
            look: 1_800_000_000,
            at: None,
            number: Some(12),
            day: Some(3.25),
            seen: Some("Look 12, day 3.25, on USDC/WETH 0.05.\n".into()),
            record: None,
        };
        let enter = Decision {
            action: "enter",
            lower_pct: Some(-3.0),
            upper_pct: Some(1.5),
            next: "1d",
            note: "quiet week",
            reasoning: "Daily volatility is 1.8%, well under the band.\n\nCentred, a little low.",
            expect: Expectation {
                low_pct: Some(-2.0),
                high_pct: Some(2.0),
                confidence: Some(0.65),
                text: String::new(),
            },
            stance: Some("adapt"),
            toward: None,
        };
        let stance = Stance {
            kind: "adapt".into(),
            toward: Some("a".into()),
        };
        let entry = log_entry(&p, &enter, Some(&stance));
        assert!(
            entry.starts_with("## Look 12, day 3.25: enter -3.00% to +1.50%, next look in 1d\n"),
            "{entry}"
        );
        assert!(
            entry.contains("```text\nLook 12, day 3.25, on USDC/WETH 0.05.\n```"),
            "{entry}"
        );
        assert!(entry.contains("Note: quiet week"), "{entry}");
        assert!(
            entry.contains(
                "Expected: price within -2.00% to +2.00% until the next look (65% sure)."
            ),
            "{entry}"
        );
        assert!(
            entry.contains("Stance toward the peers: adapt (a)."),
            "{entry}"
        );
        assert!(
            entry.contains("1.8%, well under the band.\n\nCentred"),
            "{entry}"
        );
        assert!(
            !entry.contains("1800000000"),
            "the look's time is not shown: {entry}"
        );

        let bare = Decision {
            action: "hold",
            lower_pct: None,
            upper_pct: None,
            note: "",
            reasoning: "  ",
            expect: Expectation::default(),
            ..enter
        };
        let old = Pending {
            number: None,
            day: None,
            seen: None,
            ..p
        };
        let entry = log_entry(&old, &bare, None);
        assert!(
            entry.starts_with("## Look (number not recorded): hold, next look in 1d"),
            "{entry}"
        );
        assert!(entry.contains("(No reasoning written.)"), "{entry}");
        assert!(
            !entry.contains("Note:")
                && !entry.contains("```")
                && !entry.contains("Expected:")
                && !entry.contains("Stance"),
            "{entry}"
        );
    }

    #[test]
    fn the_log_starts_with_a_header_and_grows_entry_by_entry() {
        let dir = std::env::temp_dir().join(format!("lpbot-agent-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let who = Identity {
            name: "run".into(),
            model: "claude-opus-5-5".into(),
            decisions: dir.join("agent/run.jsonl"),
            peers: Vec::new(),
        };
        let log = log_path(&who.decisions);
        append_log(&log, &who, "## Look 1\n\nfirst\n\n").unwrap();
        append_log(&log, &who, "## Look 2\n\nsecond\n\n").unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(
            text.starts_with("# Reasoning log of agent run (claude-opus-5-5)"),
            "{text}"
        );
        assert!(text.contains("run.log.jsonl"), "{text}");
        assert_eq!(text.matches("# Reasoning log").count(), 1, "{text}");
        let first = text.find("## Look 1").unwrap();
        let second = text.find("## Look 2").unwrap();
        assert!(first < second, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_observation_reads_as_text_with_every_scale() {
        let o = Observation {
            look: 42,
            looks_before: 2,
            day: 1.5,
            block: 7,
            time: 1_800_000_000,
            scales: vec![ScaleView {
                label: "1h",
                seconds: 3_600,
                change_pct: -1.25,
                high_pct: 0.5,
                low_pct: -0.25,
                vol_pct_per_day: Some(3.0),
                swaps: 120,
                volume_vs_30d: Some(1.8),
            }],
            fee_pct: 0.05,
            spacing_pct: 0.1,
            gas_gwei: 0.2,
            round_trip_gas_pct: 0.004,
            result_pct: 1.0,
            campaigns: 3,
            since_last_close: Some(7_200),
            position: Some(PositionView {
                lower_pct: -2.0,
                upper_pct: -0.5,
                state: "in_range".into(),
                held_seconds: 3_600,
                result_pct: 0.1,
                fees_pct: 0.2,
                value_share_pct: 60.0,
            }),
        };
        let text = describe(&o, "USDC/WETH 0.05", "USDC");
        assert!(text.contains("Look 3, day 1.50"), "{text}");
        assert!(!text.contains("42"), "the look's time is not shown: {text}");
        assert!(text.contains("-2.00% to -0.50%, in_range"), "{text}");
        assert!(text.contains("   1h    -1.25"), "{text}");
        assert!(text.contains("1.80x"), "{text}");
        assert!(text.contains("the last ended 2.0 h ago"), "{text}");
    }

    #[test]
    fn spans_and_clock_times_read_as_a_person_says_them() {
        assert_eq!(span(40), "40 s");
        assert_eq!(span(12 * 60 + 10), "12 min");
        assert_eq!(span(3 * 3_600 + 720), "3.2 h");
        assert_eq!(span(4 * 86_400 + 17_280), "4.2 d");
        // 2026-11-24 18:05:09 UTC
        let t = Day::of_time(1_795_000_000).start() + 18 * 3_600 + 5 * 60 + 9;
        assert!(clock(t).ends_with(" 18:05 UTC"), "{}", clock(t));
        assert!(iso(t).ends_with("T18:05:09"), "{}", iso(t));
        assert_eq!(&iso(t)[..10], &Day::of_time(t).to_string());
    }

    #[test]
    fn a_look_is_refused_once_the_head_is_older_than_the_lag_allowed() {
        let now = 1_800_000_000;
        assert_eq!(stale(100, now - 60, now, 300), None);
        assert_eq!(stale(100, now - 300, now, 300), None);
        let refusal = stale(100, now - 4 * 86_400, now, 300).expect("four days behind");
        assert!(refusal.starts_with("block 100, the newest"), "{refusal}");
        assert!(refusal.contains("4.0 d ago"), "{refusal}");
        assert!(refusal.contains("make status"), "{refusal}");
        // A head stamped after the wall clock is not stale.
        assert_eq!(stale(100, now + 5, now, 0), None);
    }

    #[test]
    fn a_look_not_due_yet_is_told_by_the_wall_clock() {
        let now = Day::of_time(1_800_000_000).start() + 14 * 3_600;
        let due = now + 3 * 3_600 + 720;
        let text = due_message(4, due, Some((26_500_000, now - 70)), now);
        assert!(
            text.starts_with("No look is due yet. Look 4 is due at "),
            "{text}"
        );
        assert!(
            text.contains(" 17:12 UTC (by the wall clock), in 3.2 h."),
            "{text}"
        );
        assert!(text.contains("block 26500000, made 70 s ago"), "{text}");
        assert!(
            text.contains(&format!("Next look due: {}Z", iso(due))),
            "{text}"
        );
        let late = due_message(4, now - 30, None, now);
        assert!(
            late.contains("is due since") && late.contains("30 s ago"),
            "{late}"
        );
        assert!(!late.contains("The store reaches"), "{late}");
    }

    #[test]
    fn a_catch_up_starts_at_the_first_day_not_stored_complete() {
        let dir = std::env::temp_dir().join(format!("lpbot-agent-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = EventStore::new(&dir);
        let pool = alloy_primitives::Address::repeat_byte(7);
        let day = |text: &str| text.parse::<Day>().unwrap();
        let (wanted, today) = (day("2026-11-20"), day("2026-11-24"));
        assert_eq!(
            first_open_day(&store, "ethereum", pool, wanted, today),
            wanted
        );
        for d in ["2026-11-20", "2026-11-21"] {
            store
                .write_day("ethereum", pool, day(d), &[], true)
                .unwrap();
        }
        store
            .write_day("ethereum", pool, day("2026-11-22"), &[], false)
            .unwrap();
        store
            .write_day("ethereum", pool, day("2026-11-23"), &[], true)
            .unwrap();
        assert_eq!(
            first_open_day(&store, "ethereum", pool, wanted, today),
            day("2026-11-22"),
            "a partial day is fetched again"
        );
        for d in ["2026-11-22", "2026-11-24"] {
            store
                .write_day("ethereum", pool, day(d), &[], true)
                .unwrap();
        }
        assert_eq!(
            first_open_day(&store, "ethereum", pool, wanted, today),
            today,
            "the day of the head is always fetched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The first demo run, its window open at the end, deciding into a
    /// file of its own and naming the demo pool.
    fn forward_demo(label: &str) -> (PathBuf, ResolvedRun) {
        let (root, mut runs, _) = crate::run_cmd::tests::demo(label, 12);
        let mut run = runs.swap_remove(0);
        run.config.window.as_mut().unwrap().to = lpbot_core::config::OPEN_END.into();
        let strategy = &mut run.config.strategies[0];
        let decisions = root.join("agent/forward.jsonl");
        strategy.extra.insert(
            "agent".into(),
            serde_yaml::from_str(&format!("{{decisions: \"{}\"}}", decisions.display())).unwrap(),
        );
        strategy.extra.insert(
            "pool".into(),
            serde_yaml::Value::String(crate::demo_cmd::DEMO_POOL.to_checksum(None)),
        );
        (root, run)
    }

    #[test]
    fn a_forward_run_looks_at_the_newest_block_and_acts_where_it_looked() {
        let (root, run) = forward_demo("forward");
        let lake = root.join("lake");
        let forward = Forward {
            sync: false,
            rpc: None,
            max_lag: u64::MAX,
            wait: 0,
        };
        let prepared = prepare_one(&run, &lake, "ethereum").unwrap();
        assert!(prepared.open);
        let (newest_block, newest_time) = prepared.newest.expect("the demo history has events");
        assert_eq!(
            Day::of_time(newest_time).to_string(),
            "2026-01-12",
            "an open window runs to the last stored day"
        );

        // The first look, due as the window starts, is shown at the newest block.
        let look = next(&run, &lake, "ethereum", &forward)
            .unwrap()
            .expect("a look");
        assert_eq!(look.look, "2026-01-08".parse::<Day>().unwrap().start());
        assert_eq!((look.block, look.time), (newest_block, newest_time));
        let decisions = root.join("agent/forward.jsonl");
        let waiting =
            Pending::parse(&std::fs::read_to_string(pending_path(&decisions)).unwrap()).unwrap();
        assert_eq!(waiting.at, Some(newest_time));

        // Decided, it is acted on there, and the next look is due an hour later.
        let enter = Decision {
            action: "enter",
            lower_pct: Some(-1.0),
            upper_pct: Some(1.0),
            next: "1h",
            note: "a try",
            reasoning: "",
            expect: Expectation::default(),
            stance: None,
            toward: None,
        };
        decide(&run, &lake, "ethereum", &enter, &forward).unwrap();
        let written = std::fs::read_to_string(&decisions).unwrap();
        let back = crate::run_cmd::parse_decisions(&written).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!((back[0].look, back[0].at), (look.look, Some(newest_time)));
        assert!(!pending_path(&decisions).exists(), "no look waits");
        let prepared = prepare_one(&run, &lake, "ethereum").unwrap();
        let out = prepared.replay(&prepared.settings).unwrap();
        assert_eq!(outcome(&out, true), Outcome::Due(newest_time + 3_600));
        assert_eq!(out.unused_decisions, 0);
        assert!(out.campaigns.is_empty(), "no block is left to enter on");
        // The shared record is as of the block the look was shown at.
        let records = agent_log::read_records(&agent_log::records_path(&decisions)).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].get("as_of").and_then(serde_json::Value::as_u64),
            Some(newest_time)
        );
        assert!(
            decide(&run, &lake, "ethereum", &enter, &forward).is_err(),
            "nothing to decide"
        );

        // `lpbot run` writes the six tables of what is stored so far.
        let results = root.join("runs");
        crate::run_cmd::run(std::slice::from_ref(&run), &lake, &results, "ethereum").unwrap();
        for table in lpbot_sim::RESULT_TABLES {
            assert!(
                lpbot_sim::tables::table_path(&results, table, &run.run_id).is_file(),
                "{table}"
            );
        }

        // A head older than the lag allowed is refused, and leaves no look.
        std::fs::remove_file(&decisions).unwrap();
        let strict = Forward {
            max_lag: 60,
            ..forward
        };
        let refused = next(&run, &lake, "ethereum", &strict)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("behind the chain"), "{refused}");
        assert!(!pending_path(&decisions).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_forward_run_is_scored_from_where_each_look_was_seen() {
        let (root, run) = forward_demo("forward-eval");
        let lake = root.join("lake");
        let prepared = prepare_one(&run, &lake, "ethereum").unwrap();
        let (_, newest) = prepared.newest.unwrap();
        let block_at = |time: u64| {
            let events = prepared.events();
            events[events.partition_point(|e| e.time < time)].time
        };
        // Both looks seen late: the first a day after it was due, the
        // second twelve hours after.
        let first_look = "2026-01-08".parse::<Day>().unwrap().start();
        let first_at = block_at(first_look + 86_400);
        let second_look = first_at + 6 * 3_600;
        let second_at = block_at(second_look + 12 * 3_600);
        let decisions = root.join("agent/forward.jsonl");
        std::fs::create_dir_all(decisions.parent().unwrap()).unwrap();
        std::fs::write(
            &decisions,
            format!(
                "{{\"look\":{first_look},\"at\":{first_at},\"action\":\"enter\",\
                 \"lower_pct\":-1.0,\"upper_pct\":1.0,\"next\":\"6h\",\"note\":\"\"}}\n\
                 {{\"look\":{second_look},\"at\":{second_at},\"action\":\"exit\",\
                 \"next\":\"1d\",\"note\":\"\"}}\n"
            ),
        )
        .unwrap();
        let who = identity_of(&run).unwrap();
        let horizons = crate::agent_eval::parse_horizons("next,6h").unwrap();
        let report =
            crate::agent_eval::evaluate(&run, &who, &lake, "ethereum", &horizons, 2).unwrap();
        assert_eq!(report.scored, 2);
        let scores = agent_log::read_records(&agent_log::scores_path(&decisions)).unwrap();
        let score = |number: u64, horizon: &str| {
            scores
                .iter()
                .find(|r| r["number"] == number && r["horizon"] == horizon)
                .unwrap_or_else(|| panic!("no score for {number} over {horizon}"))
                .clone()
        };
        let as_of = |r: &serde_json::Value, key: &str| r[key].as_u64().unwrap();
        // Each decision is scored from the block its look was seen at.
        assert_eq!(as_of(&score(1, "next"), "decided_as_of"), first_at);
        assert_eq!(as_of(&score(2, "next"), "decided_as_of"), second_at);
        // Until the next look: where that look was seen, not when it was due.
        assert_eq!(as_of(&score(1, "next"), "as_of"), second_at);
        // A fixed horizon ends when it is due.
        let six = as_of(&score(1, "6h"), "as_of");
        assert!(six >= first_at + 6 * 3_600 && six < second_at, "{six}");
        // Nothing ends past the newest stored block.
        assert!(scores.iter().all(|r| as_of(r, "as_of") <= newest));
        let _ = std::fs::remove_dir_all(&root);
    }
}
