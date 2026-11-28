//! `lpbot gates`: every check against real chain data, in order, then the
//! backtest, the load into the viewer's database and the calibration.
//!
//! Each step is a command that also exists on its own. This one runs them
//! in the order they depend on each other, stops at the first that fails
//! and ends with a summary. Everything it stores is kept, so a second run
//! only does what is new.

use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::Address;
use anyhow::{anyhow, bail, Result};
use lpbot_chain::eth::SyncProgress;
use lpbot_chain::uniswap_v3::DiscoveredPool;
use lpbot_core::config::Mode;
use lpbot_core::experiment::ResolvedRun;
use lpbot_data::day::Day;
use lpbot_data::fee_check::{FeeCheckSummary, PositionReader};
use lpbot_data::snapshot::SnapshotStore;
use lpbot_sim::engine::Policy;

use crate::chain_cmd::{self, chain_addresses, Chain, HEAD_MARGIN};
use crate::{backfill_cmd, fees_cmd, gas_history_cmd, load_cmd, rank_cmd, run_cmd};

/// The steps in the order they run, each with what it shows.
pub const STEPS: [(&str, &str); 9] = [
    ("node", "the node answers, is synced and follows the chain"),
    ("pools", "the universe's pools, from the Uniswap v3 factory"),
    (
        "replica",
        "the replica against every pool over recent blocks (gates 1 and 2)",
    ),
    (
        "history",
        "pool history stored and replayed against the chain (gate 3)",
    ),
    (
        "gas",
        "each block's gas price stored, for the backtest to charge",
    ),
    (
        "fees",
        "the replica's fees against what real positions collected (gate 6)",
    ),
    ("backtest", "the experiment on the stored history (gate 7)"),
    // Before the calibration, which takes the longest and is not needed
    // to look at the runs.
    ("load", "the results into Postgres, for the dashboards"),
    (
        "calibrate",
        "the adaptive decision fitted and scored walk-forward",
    ),
];

/// A head older than this means the node is not following the chain.
const MAX_HEAD_AGE: u64 = 600;

/// How a step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass(String),
    /// Nothing wrong was found, but the check could not be decided.
    Open(String),
    /// The check failed. Nothing after it runs.
    Fail(String),
    /// Not run, on request or because there was nothing to do.
    Skipped(String),
}

/// Which steps to run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The first step to run; the ones before it are left out.
    pub start_at: Option<String>,
    /// Steps to leave out.
    pub skip: Vec<String>,
}

impl Plan {
    /// Refuses a name that is not a step, so a typo cannot pass unnoticed.
    pub fn check(&self, names: &[&str]) -> Result<()> {
        for name in self.start_at.iter().chain(&self.skip) {
            if !names.contains(&name.as_str()) {
                bail!("`{name}` is not a step; the steps are {}", names.join(", "));
            }
        }
        Ok(())
    }
}

/// What a step does with the context the steps share.
pub type Action<'a, C> = Box<dyn FnMut(&mut C) -> Result<Outcome> + 'a>;

/// One step of a sequence that shares a context.
pub struct Step<'a, C> {
    pub name: &'static str,
    pub title: &'static str,
    pub run: Action<'a, C>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StepResult {
    pub name: &'static str,
    /// `None` when an earlier step failed and this one never ran.
    pub outcome: Option<Outcome>,
    pub seconds: f64,
}

/// Runs the steps in order and stops at the first that fails. A step that
/// returns an error counts as failed, with the error as its note.
pub fn run_steps<C>(
    steps: Vec<Step<'_, C>>,
    context: &mut C,
    plan: &Plan,
) -> Result<Vec<StepResult>> {
    let names: Vec<&str> = steps.iter().map(|step| step.name).collect();
    plan.check(&names)?;
    let first = plan
        .start_at
        .as_deref()
        .and_then(|name| names.iter().position(|n| *n == name))
        .unwrap_or(0);
    let total = steps.len();

    let mut results = Vec::with_capacity(total);
    let mut stopped = false;
    for (index, mut step) in steps.into_iter().enumerate() {
        let mut result = StepResult {
            name: step.name,
            outcome: None,
            seconds: 0.0,
        };
        if stopped {
            // Left as not run.
        } else if index < first {
            result.outcome = Some(Outcome::Skipped(format!("started at `{}`", names[first])));
        } else if plan.skip.iter().any(|name| name == step.name) {
            result.outcome = Some(Outcome::Skipped("left out on request".into()));
        } else {
            println!();
            println!("==> [{}/{total}] {}: {}", index + 1, step.name, step.title);
            let started = Instant::now();
            let outcome = match (step.run)(context) {
                Ok(outcome) => outcome,
                Err(error) => Outcome::Fail(format!("{error:#}")),
            };
            result.seconds = started.elapsed().as_secs_f64();
            println!("<== {}: {}", status(Some(&outcome)), note(Some(&outcome)));
            stopped = matches!(outcome, Outcome::Fail(_));
            result.outcome = Some(outcome);
        }
        results.push(result);
    }
    Ok(results)
}

fn status(outcome: Option<&Outcome>) -> &'static str {
    match outcome {
        Some(Outcome::Pass(_)) => "pass",
        Some(Outcome::Open(_)) => "open",
        Some(Outcome::Fail(_)) => "FAIL",
        Some(Outcome::Skipped(_)) => "left out",
        None => "not run",
    }
}

fn note(outcome: Option<&Outcome>) -> &str {
    match outcome {
        Some(
            Outcome::Pass(note)
            | Outcome::Open(note)
            | Outcome::Fail(note)
            | Outcome::Skipped(note),
        ) => note,
        None => "",
    }
}

/// `45 s`, `3 min 20 s` or `1 h 12 min`.
pub fn took(seconds: f64) -> String {
    let total = seconds.round() as u64;
    match total {
        0..=59 => format!("{total} s"),
        60..=3_599 => format!("{} min {} s", total / 60, total % 60),
        _ => format!("{} h {} min", total / 3_600, total % 3_600 / 60),
    }
}

/// The step that failed, if one did.
pub fn failed_step(results: &[StepResult]) -> Option<&'static str> {
    results
        .iter()
        .find(|r| matches!(r.outcome, Some(Outcome::Fail(_))))
        .map(|r| r.name)
}

/// The closing summary: one line per step, the counts, and what to do next.
pub fn summary(results: &[StepResult]) -> Vec<String> {
    let count = |wanted: &str| {
        results
            .iter()
            .filter(|r| status(r.outcome.as_ref()) == wanted)
            .count()
    };
    let mut lines = vec!["Summary".to_string(), String::new()];
    for result in results {
        let outcome = result.outcome.as_ref();
        let time = match outcome {
            Some(Outcome::Skipped(_)) | None => String::new(),
            Some(_) => took(result.seconds),
        };
        lines.push(
            format!(
                "  {:<10} {:<8} {:>11}   {}",
                result.name,
                status(outcome),
                time,
                note(outcome)
            )
            .trim_end()
            .to_string(),
        );
    }
    lines.push(String::new());
    lines.push(format!(
        "{} passed, {} open, {} failed, {} left out, {} not run, in {}.",
        count("pass"),
        count("open"),
        count("FAIL"),
        count("left out"),
        count("not run"),
        took(results.iter().map(|r| r.seconds).sum())
    ));
    if count("open") > 0 {
        lines.push(
            "Open: nothing wrong was found, but the check could not be decided. \
             Its note says what is missing."
                .to_string(),
        );
    }
    if let Some(name) = failed_step(results) {
        lines.push(format!(
            "Stopped at `{name}`. Once that is dealt with, `make gates STEP={name}` \
             carries on from there."
        ));
    }
    lines
}

/// Whether the node can be trusted to hold the chain up to `until`.
pub fn node_verdict(
    syncing: Option<SyncProgress>,
    head: u64,
    head_time: u64,
    now: u64,
    until: Day,
) -> Outcome {
    if let Some(progress) = syncing {
        let how_far = if progress.highest > 0 {
            format!(" (block {} of {})", progress.current, progress.highest)
        } else {
            String::new()
        };
        return Outcome::Fail(format!(
            "the node is still syncing{how_far}. `make status` shows how far it is; \
             run this again once it says synced"
        ));
    }
    let age = now.saturating_sub(head_time);
    if age > MAX_HEAD_AGE {
        return Outcome::Fail(format!(
            "the node's newest block, {head}, is {} old, so it is not following the chain: \
             it has not caught up yet, or the consensus client is not feeding it. \
             See `make status` and `make logs`",
            took(age as f64)
        ));
    }
    if head_time < until.end() {
        return Outcome::Fail(format!(
            "the experiment's window runs to {until}, which is not over yet; \
             a backtest needs a window that lies in the past"
        ));
    }
    Outcome::Pass(format!("synced, head block {head}, {age} s old"))
}

/// The first day of history the runs need and the last day of their
/// windows. History starts the longest zone lookback before a window.
pub fn span(runs: &[ResolvedRun]) -> Result<(Day, Day)> {
    let mut span: Option<(Day, Day)> = None;
    for run in runs {
        let strategy = run
            .config
            .strategies
            .first()
            .ok_or_else(|| anyhow!("the run has no strategy"))?;
        let (from, to) = run_cmd::window(&run.config)?;
        let longest = run_cmd::history_days(strategy)?;
        let start = from.minus(longest);
        span = Some(match span {
            None => (start, to),
            Some((first, last)) => (first.min(start), last.max(to)),
        });
    }
    span.ok_or_else(|| anyhow!("the experiment has no runs"))
}

/// What the fee check's counts mean for the gate.
pub fn fee_outcome(pool: &str, summary: &FeeCheckSummary, needed: usize) -> Outcome {
    let confirmed = summary.exact + summary.close;
    if summary.mismatched > 0 {
        Outcome::Fail(format!(
            "{pool}: {} position(s) collected something else than the replica computed",
            summary.mismatched
        ))
    } else if confirmed >= needed {
        Outcome::Pass(format!(
            "{pool}: {} emptied positions collected exactly what the replica computed, \
             {} within {}%, none contradicts it",
            summary.exact,
            summary.close,
            fees_cmd::TOLERANCE * 100.0
        ))
    } else {
        Outcome::Open(format!(
            "{pool}: {confirmed} emptied position(s) agree with the replica and none \
             contradicts it, but the check asks for {needed}; a longer history has more"
        ))
    }
}

/// One outcome for several: a failure outweighs an open check, which
/// outweighs a pass. The notes are kept, the worst first.
pub fn combine(outcomes: Vec<Outcome>) -> Outcome {
    let rank = |outcome: &Outcome| match outcome {
        Outcome::Fail(_) => 0,
        Outcome::Open(_) => 1,
        Outcome::Pass(_) => 2,
        Outcome::Skipped(_) => 3,
    };
    let mut sorted = outcomes;
    sorted.sort_by_key(rank);
    let notes = sorted
        .iter()
        .map(|outcome| note(Some(outcome)))
        .collect::<Vec<_>>()
        .join("; ");
    match sorted.first() {
        Some(Outcome::Fail(_)) => Outcome::Fail(notes),
        Some(Outcome::Open(_)) => Outcome::Open(notes),
        Some(Outcome::Pass(_)) => Outcome::Pass(notes),
        Some(Outcome::Skipped(_)) => Outcome::Skipped(notes),
        None => Outcome::Skipped("nothing to check".into()),
    }
}

/// What a calibration shipped, from how often each choice won.
fn what_ships(counts: &[(&str, usize)]) -> String {
    counts
        .iter()
        .map(|(name, count)| match *name {
            "adaptive" => format!("the adaptive decision ships in {count}"),
            rule => format!("always-{rule} ships in {count}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The host of an endpoint, without the scheme, path or query: an
/// endpoint's path can hold a key, which must not end up in a log.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    rest.rsplit_once('@').map_or(rest, |(_, host)| host)
}

/// What the caller chooses. Not printable: `db` holds a password.
pub struct Options {
    pub chain_name: String,
    /// Node endpoint, when it is not the config's local one.
    pub rpc: Option<String>,
    /// Data folder with the stored history.
    pub root: PathBuf,
    /// Folder the result tables are written to.
    pub results: PathBuf,
    /// Database to load the results into; without one the load is left out.
    pub db: Option<String>,
    /// First day of history to store, when not what the runs need.
    pub history_from: Option<Day>,
    /// Blocks the replica check replays for every pool.
    pub blocks: u64,
    pub log_chunk: u64,
    /// Emptied positions the fee check needs to pass.
    pub positions: usize,
    pub fit_days: Option<u32>,
    pub score_days: Option<u32>,
}

/// Everything the steps share.
pub struct Gates {
    pub runs: Vec<ResolvedRun>,
    pub options: Options,
    chain: Option<Chain>,
    pools: Option<Vec<DiscoveredPool>>,
}

impl Gates {
    pub fn new(runs: Vec<ResolvedRun>, options: Options) -> Result<Self> {
        if runs.is_empty() {
            bail!("the experiment has no runs");
        }
        if runs.iter().any(|run| run.config.mode != Mode::Backtest) {
            bail!("the gates need an experiment with `mode: backtest`");
        }
        // Fails here, before anything runs, when a window or lookback is unreadable.
        span(&runs)?;
        Ok(Self {
            runs,
            options,
            chain: None,
            pools: None,
        })
    }

    /// The first day of history to store.
    pub fn history_start(&self) -> Result<Day> {
        match self.options.history_from {
            Some(day) => Ok(day),
            None => Ok(span(&self.runs)?.0),
        }
    }

    /// The node, connected on first use.
    fn chain(&mut self) -> Result<&Chain> {
        if self.chain.is_none() {
            let chain = chain_cmd::connect_to(
                &self.runs[0].config,
                &self.options.chain_name,
                self.options.rpc.as_deref(),
            )?;
            self.chain = Some(chain);
        }
        Ok(self.chain.as_ref().expect("connected above"))
    }

    /// The universe's pools, discovered on first use.
    fn discovered(&mut self) -> Result<Vec<DiscoveredPool>> {
        if self.pools.is_none() {
            let (_, found) = chain_cmd::discover(self.chain()?)?;
            self.pools = Some(found);
        }
        Ok(self.pools.clone().expect("discovered above"))
    }

    /// The pools the runs trade, each once, with the name the runs use.
    /// Runs choose their pool as `lpbot run` does: by the same key.
    fn traded_pools(&self) -> Result<Vec<(Address, String)>> {
        let snapshots = SnapshotStore::new(&self.options.root);
        let mut asked: Vec<run_cmd::PoolKey> = Vec::new();
        let mut pools: Vec<(Address, String)> = Vec::new();
        for run in &self.runs {
            let config = &run.config;
            let strategy = config
                .strategies
                .first()
                .ok_or_else(|| anyhow!("the run has no strategy"))?;
            let (from, _) = run_cmd::window(config)?;
            let key = run_cmd::pool_key(strategy, from)?;
            if asked.contains(&key) {
                continue;
            }
            asked.push(key);
            let (_, tokens) = chain_addresses(config, &self.options.chain_name)?;
            let (pool, _) = run_cmd::choose_pool(
                &self.options.root,
                &self.options.chain_name,
                &tokens,
                config,
                strategy,
                from,
            )?;
            if pools.iter().any(|(address, _)| *address == pool) {
                continue;
            }
            let snapshot = snapshots
                .latest_on_or_before(&self.options.chain_name, pool, from)?
                .ok_or_else(|| anyhow!("no stored snapshot for {pool:#x}"))?;
            let name = format!(
                "{}/{} {}",
                rank_cmd::token_of(&tokens, snapshot.token0)?.0,
                rank_cmd::token_of(&tokens, snapshot.token1)?.0,
                snapshot.state.fee_pips as f64 / 10_000.0
            );
            pools.push((pool, name));
        }
        Ok(pools)
    }

    fn node(&mut self) -> Result<Outcome> {
        let (_, until) = span(&self.runs)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let chain = self.chain()?;
        let syncing = chain.client.syncing()?;
        let head = chain.client.block_number()?;
        let (head_time, _) = chain.client.block_header(head)?;
        println!(
            "  {} answers as chain `{}`",
            host_of(chain.client.url()),
            chain.name
        );
        let verdict = node_verdict(syncing, head, head_time, now, until);
        if !matches!(verdict, Outcome::Pass(_)) {
            return Ok(verdict);
        }
        match &self.options.db {
            Some(url) => {
                if let Err(error) = load_cmd::connect(url) {
                    return Ok(Outcome::Fail(format!(
                        "{error}. The results are loaded into it after the backtest: \
                         start it with `make viewer` or `make up`"
                    )));
                }
                println!("  the results database answers");
            }
            None => println!("  no database given: the load after the backtest is left out"),
        }
        Ok(verdict)
    }

    fn pools(&mut self) -> Result<Outcome> {
        let pools = self.discovered()?;
        let chain = self.chain()?;
        for pool in &pools {
            println!(
                "  {:<18} {:#x}",
                chain_cmd::pair_label(chain, pool),
                pool.address
            );
        }
        if pools.is_empty() {
            return Ok(Outcome::Fail(
                "the factory has no pool for the universe's tokens; check the factory \
                 and token addresses in the config"
                    .into(),
            ));
        }
        Ok(Outcome::Pass(format!(
            "{} pools in the universe",
            pools.len()
        )))
    }

    fn replica(&mut self) -> Result<Outcome> {
        let pools = self.discovered()?;
        let (blocks, log_chunk) = (self.options.blocks, self.options.log_chunk);
        let chain = self.chain()?;
        // One end block for every pool, a few behind the head.
        let to = chain.client.block_number()?.saturating_sub(HEAD_MARGIN);
        let mut failed = Vec::new();
        for pool in &pools {
            let label = chain_cmd::pair_label(chain, pool);
            println!("  {label}");
            let address = format!("{:#x}", pool.address);
            match chain_cmd::verify(chain, &address, None, Some(to), blocks, log_chunk) {
                Ok(true) => {}
                Ok(false) => failed.push(label),
                Err(error) => {
                    println!("could not finish: {error:#}");
                    failed.push(label);
                }
            }
            println!();
        }
        if failed.is_empty() {
            Ok(Outcome::Pass(format!(
                "{} pools replayed over the last {blocks} blocks; every one matches the chain exactly",
                pools.len()
            )))
        } else {
            Ok(Outcome::Fail(format!(
                "{} of {} pools do not match the chain: {}. \
                 `lpbot verify-pool <address>` repeats one of them",
                failed.len(),
                pools.len(),
                failed.join(", ")
            )))
        }
    }

    fn history(&mut self) -> Result<Outcome> {
        let from = self.history_start()?;
        let (root, log_chunk) = (self.options.root.clone(), self.options.log_chunk);
        let chain = self.chain()?;
        let stored = backfill_cmd::run(
            chain,
            &backfill_cmd::BackfillArgs {
                pools: &[],
                from: Some(from),
                days: 0,
                resume: false,
                root: &root,
                log_chunk,
                verify: true,
            },
        )?;
        Ok(Outcome::Pass(format!(
            "{} pools stored from {} ({} logs new in this run); \
             every pool replays to its state on the chain",
            stored.pools, stored.from, stored.logs_written
        )))
    }

    fn gas(&mut self) -> Result<Outcome> {
        let from = self.history_start()?;
        let (root, chain_name) = (self.options.root.clone(), self.options.chain_name.clone());
        let log_chunk = self.options.log_chunk;
        let chain = self.chain()?;
        let client = &chain.client;
        let to = match client.finalized_block()? {
            Some((block, _)) => block,
            None => client.block_number()?.saturating_sub(64),
        };
        let source = lpbot_data::backfill::NodeSource {
            client,
            head_margin: 0,
            log_chunk,
        };
        let first = lpbot_data::backfill::first_block_at_or_after(&source, from.start(), 1, to)?;
        let done = gas_history_cmd::run(client, &root, &chain_name, first, to, to, 2)?;
        Ok(Outcome::Pass(format!(
            "gas prices of blocks {first} to {to} stored ({} chunk(s) read, {} already stored)",
            done.chunks, done.kept
        )))
    }

    fn fees(&mut self) -> Result<Outcome> {
        let mut outcomes = Vec::new();
        let root = self.options.root.clone();
        let chain_name = self.options.chain_name.clone();
        let positions = self.options.positions;
        for (pool, name) in self.traded_pools()? {
            println!("  {name}");
            // The node says which positions started empty and what the
            // pool still owes them. Without one the check still runs, but
            // confirms nothing, so the step stays open and cannot pass.
            let reader: Option<&dyn PositionReader> = match self.chain() {
                Ok(chain) => Some(&chain.client),
                Err(error) => {
                    println!("  no node ({error:#}): nothing the check finds is confirmed");
                    None
                }
            };
            let summary = fees_cmd::run_with(&root, &chain_name, pool, positions, reader)?;
            outcomes.push(fee_outcome(&name, &summary, self.options.positions));
        }
        Ok(combine(outcomes))
    }

    fn backtest(&mut self) -> Result<Outcome> {
        run_cmd::run(
            &self.runs,
            &self.options.root,
            &self.options.results,
            &self.options.chain_name,
        )?;
        Ok(Outcome::Pass(format!(
            "{} runs written to {}",
            self.runs.len(),
            self.options.results.display()
        )))
    }

    fn calibrate(&mut self) -> Result<Outcome> {
        let mut adaptive = 0;
        for run in &self.runs {
            if let Some(strategy) = run.config.strategies.first() {
                if matches!(run_cmd::policy(strategy)?, Policy::Adaptive(_)) {
                    adaptive += 1;
                }
            }
        }
        if adaptive == 0 {
            return Ok(Outcome::Skipped(
                "the experiment has no run with `patience.mode: adaptive`".into(),
            ));
        }
        let shipped = run_cmd::calibrate(
            &self.runs,
            &self.options.root,
            &self.options.results,
            &self.options.chain_name,
            self.options.fit_days,
            self.options.score_days,
        )?;
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for (_, ships) in &shipped {
            match counts.iter_mut().find(|(name, _)| *name == *ships) {
                Some((_, count)) => *count += 1,
                None => counts.push((ships, 1)),
            }
        }
        Ok(Outcome::Pass(format!(
            "{} adaptive run(s) scored out of sample against the fixed rules: {}",
            shipped.len(),
            what_ships(&counts)
        )))
    }

    fn load(&mut self) -> Result<Outcome> {
        let Some(url) = &self.options.db else {
            return Ok(Outcome::Skipped(
                "no database given; `make gates` takes it from deploy/.env".into(),
            ));
        };
        let report = load_cmd::run(url, &self.options.results)?;
        Ok(Outcome::Pass(format!(
            "{} runs, {} rows in Postgres; open the `lpbot: runs` dashboard",
            report.runs, report.rows
        )))
    }

    /// The steps, in the order of `STEPS`.
    pub fn steps<'a>() -> Vec<Step<'a, Gates>> {
        let run: [fn(&mut Gates) -> Result<Outcome>; 9] = [
            Gates::node,
            Gates::pools,
            Gates::replica,
            Gates::history,
            Gates::gas,
            Gates::fees,
            Gates::backtest,
            Gates::load,
            Gates::calibrate,
        ];
        STEPS
            .into_iter()
            .zip(run)
            .map(|((name, title), run)| Step {
                name,
                title,
                run: Box::new(run),
            })
            .collect()
    }
}

/// Runs the gates and prints the summary. An error names the step that
/// failed.
pub fn run(mut gates: Gates, plan: &Plan) -> Result<()> {
    println!(
        "history:    from {}, under {}",
        gates.history_start()?,
        gates.options.root.display()
    );
    println!("results:    {}", gates.options.results.display());

    let results = run_steps(Gates::steps(), &mut gates, plan)?;
    println!();
    for line in summary(&results) {
        println!("{line}");
    }
    match failed_step(&results) {
        Some(name) => bail!("stopped at `{name}`"),
        None => Ok(()),
    }
}

#[cfg(test)]
#[path = "gates_cmd_tests.rs"]
mod tests;
